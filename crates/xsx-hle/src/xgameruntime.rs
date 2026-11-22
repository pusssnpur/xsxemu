//! `xgameruntime.dll` (Microsoft GDK GameRuntime) HLE thunks & `QueryApiImpl` COM singletons.
//!
//! Real Xbox Series X titles linked against `xgameruntime.lib` dynamically load
//! `xgameruntime.dll` and `GetProcAddress` these five internal bootstrap functions:
//! - `InitializeApiImpl` / `InitializeApiImplEx` / `InitializeApiImplEx2` / `UninitializeApiImpl`
//! - `QueryApiImpl(class_id, interface_id, out)`
//!
//! `QueryApiImpl` hands out process-wide COM singletons matching the exact measured vtable
//! layouts of `xgameruntime.dll`:
//! - `IXThreadingImpl` (`CLSID_XTHREADING_IMPL`, 31 slots): `XAsync*`, `XTaskQueue*`, `XThread*`
//! - `IXErrorImpl` (`CLSID_XERROR_IMPL`, 6 slots): `XErrorReport`, `XErrorSetCallback`, `XErrorSetOptions`
//! - `IXNetworkingImpl2` (`CLSID_XNETWORKING_IMPL`, 21 slots): offline connectivity & cert verification
//!
//! Direct named exports (`XGameRuntimeInitialize`, `XSystem*`, `XPackage*`, `XTaskQueue*`,
//! `XAsync*`) are served by the exact same underlying implementations.

use core::ffi::{c_char, c_void};
use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CStr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use xsx_abi::com::{
    Guid, HResult, CLSID_XERROR_IMPL, CLSID_XGAMEEVENT_API, CLSID_XGAMERUNTIME_FEATURE_API,
    CLSID_XGAMESAVE_API, CLSID_XGAME_API, CLSID_XNETWORKING_IMPL, CLSID_XPACKAGE_API,
    CLSID_XSYSTEM_API, CLSID_XTHREADING_IMPL, CLSID_XUSER_API, E_ABORT, E_INSUFFICIENT_BUFFER,
    E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_NOT_SUPPORTED, E_PENDING, E_POINTER,
    IID_IUNKNOWN, IID_XGAMESAVE_LATER, IID_XGAME_LATER, IID_XPACKAGE_LATER, IID_XSYSTEM_LATER,
    IID_XUSER_GAMERTAG, IID_XUSER_LATER, S_OK,
};
use xsx_abi::gdk::{
    ApiImplOptions, Handle, XAsyncBlock, XNetworkingConnectivityHint, XSystemAnalyticsInfo,
    XSystemDeviceType, XTaskQueueCallback, XTaskQueueRegistrationToken, XVersion,
};

use crate::registry::ModuleRegistry;
use crate::runtime::{current_runtime, KernelObject};
use crate::services;

const CONSOLE_ID_STR: &[u8] = b"00000000.00000000.00000000.00000000.00\0";
const SANDBOX_ID_STR: &[u8] = b"RETAIL\0";
const USER_LOCALE_STR: &[u8] = b"en-US\0";
const DEFAULT_PACKAGE_ID: &[u8] = b"XsxEmuPackageIdentifier000000000\0";

static RUNTIME_REFS: AtomicI32 = AtomicI32::new(0);
static API_REFS: AtomicI32 = AtomicI32::new(0);
static NEXT_NET_TOKEN: AtomicU64 = AtomicU64::new(1);
static NEXT_USER_TOKEN: AtomicU64 = AtomicU64::new(1);
static NEXT_PKG_TOKEN: AtomicU64 = AtomicU64::new(1);
static PROCESS_TASK_QUEUE: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_TITLE_ID: AtomicU32 = AtomicU32::new(0x7530_1234);

/// Configure the active Xbox TitleId (typically loaded from `MicrosoftGame.Config`).
pub fn set_active_title_id(title_id: u32) {
    ACTIVE_TITLE_ID.store(title_id, Ordering::SeqCst);
}

/// Read the active Xbox TitleId reported by `XGameGetXboxTitleId`.
pub fn get_active_title_id() -> u32 {
    ACTIVE_TITLE_ID.load(Ordering::SeqCst)
}

thread_local! {
    static TIME_SENSITIVE: Cell<bool> = const { Cell::new(false) };
}

// ---------------------------------------------------------------------------
// In-process XTaskQueue & XAsync engine
// ---------------------------------------------------------------------------

struct QueuedCallback {
    callback: XTaskQueueCallback,
    context: usize,
}

#[derive(Default)]
struct TaskQueueState {
    work_mode: u32,
    completion_mode: u32,
    work_items: VecDeque<QueuedCallback>,
    completion_items: VecDeque<QueuedCallback>,
    terminated: bool,
}

#[derive(Debug, Clone)]
struct AsyncState {
    status: HResult,
    result_data: Vec<u8>,
    result_size: usize,
    completed: bool,
    canceled: bool,
}

fn queues() -> &'static Mutex<HashMap<usize, TaskQueueState>> {
    static Q: OnceLock<Mutex<HashMap<usize, TaskQueueState>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(HashMap::new()))
}

fn async_blocks() -> &'static Mutex<HashMap<usize, AsyncState>> {
    static A: OnceLock<Mutex<HashMap<usize, AsyncState>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register(reg: &mut ModuleRegistry) {
    // xgameruntime.lib dynamic bootstrap entrypoints
    reg.register_export(
        "xgameruntime.dll",
        "InitializeApiImpl",
        hle_initialize_api_impl as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "InitializeApiImplEx",
        hle_initialize_api_impl_ex as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "InitializeApiImplEx2",
        hle_initialize_api_impl_ex2 as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "UninitializeApiImpl",
        hle_uninitialize_api_impl as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "QueryApiImpl",
        hle_query_api_impl as *const () as usize,
    );

    // Core XGameRuntime, XGame, XSystem, Feature & Event
    reg.register_export(
        "xgameruntime.dll",
        "XGameRuntimeInitialize",
        hle_xgame_runtime_initialize as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XGameRuntimeUninitialize",
        hle_xgame_runtime_uninitialize as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XGameRuntimeIsFeatureAvailable",
        hle_xgame_runtime_is_feature_available as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XGameGetXboxTitleId",
        hle_xgame_get_xbox_title_id as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XGameEventWrite",
        hle_xgame_event_write as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XSystemGetDeviceType",
        hle_xsystem_get_device_type as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XSystemGetConsoleId",
        hle_xsystem_get_console_id as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XSystemGetXboxLiveSandboxId",
        hle_xsystem_get_sandbox_id as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XSystemGetAppMemoryUsageLimit",
        hle_xsystem_get_app_memory_usage_limit as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XSystemGetAppMemoryUsage",
        hle_xsystem_get_app_memory_usage as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XSystemGetAnalyticsInfo",
        hle_xsystem_get_analytics_info as *const () as usize,
    );

    // XPackage
    reg.register_export(
        "xgameruntime.dll",
        "XPackageGetCurrentProcessPackageIdentifier",
        hle_xpackage_get_identifier as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XPackageGetMountPath",
        hle_xpackage_get_mount_path as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XPackageIsPackagedProcess",
        hle_xpackage_is_packaged_process as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XPackageGetUserLocale",
        hle_xpackage_get_user_locale as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XPackageEnumeratePackages",
        hle_xpackage_enumerate_packages as *const () as usize,
    );

    // XTaskQueue
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueCreate",
        hle_xtask_queue_create as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueCloseHandle",
        hle_xtask_queue_close_handle as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueSubmitCallback",
        hle_xtask_queue_submit_callback as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueSubmitDelayedCallback",
        hle_xtask_queue_submit_delayed_callback as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueDispatch",
        hle_xtask_queue_dispatch as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueTerminate",
        hle_xtask_queue_terminate as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueGetCurrentProcessTaskQueue",
        hle_xtask_queue_get_process_queue as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XTaskQueueSetCurrentProcessTaskQueue",
        hle_xtask_queue_set_process_queue as *const () as usize,
    );

    // XAsync
    reg.register_export(
        "xgameruntime.dll",
        "XAsyncGetStatus",
        hle_xasync_get_status as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XAsyncGetResultSize",
        hle_xasync_get_result_size as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XAsyncCancel",
        hle_xasync_cancel as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XAsyncBegin",
        hle_xasync_begin as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XAsyncSchedule",
        hle_xasync_schedule as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XAsyncComplete",
        hle_xasync_complete as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XAsyncGetResult",
        hle_xasync_get_result as *const () as usize,
    );
}

// ---------------------------------------------------------------------------
// xgameruntime.lib bootstrap exports (InitializeApiImpl* / QueryApiImpl)
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_initialize_api_impl(gdk_ver: u32, gs_ver: u32) -> HResult {
    API_REFS.fetch_add(1, Ordering::Relaxed);
    if let Some(rt) = current_runtime() {
        rt.set_game_runtime_initialized(true);
        rt.trace(format!(
            "xgameruntime!InitializeApiImpl(gdk={gdk_ver}, gs={gs_ver}) -> S_OK"
        ));
    }
    S_OK
}

unsafe extern "win64" fn hle_initialize_api_impl_ex(
    gdk_ver: u32,
    gs_ver: u32,
    mode: u8,
) -> HResult {
    API_REFS.fetch_add(1, Ordering::Relaxed);
    if let Some(rt) = current_runtime() {
        rt.set_game_runtime_initialized(true);
        rt.trace(format!(
            "xgameruntime!InitializeApiImplEx(gdk={gdk_ver}, gs={gs_ver}, mode=0x{mode:X}) -> S_OK"
        ));
    }
    S_OK
}

unsafe extern "win64" fn hle_initialize_api_impl_ex2(
    gdk_ver: u32,
    gs_ver: u32,
    mode: u8,
    options: *const ApiImplOptions,
) -> HResult {
    API_REFS.fetch_add(1, Ordering::Relaxed);
    if let Some(rt) = current_runtime() {
        rt.set_game_runtime_initialized(true);
        let inline = !options.is_null() && (*options).is_inline != 0;
        rt.trace(format!(
            "xgameruntime!InitializeApiImplEx2(gdk={gdk_ver}, gs={gs_ver}, mode=0x{mode:X}, inline={inline}) -> S_OK"
        ));
    }
    S_OK
}

unsafe extern "win64" fn hle_uninitialize_api_impl() -> HResult {
    let mut prev = API_REFS.load(Ordering::Relaxed);
    while prev > 0
        && API_REFS
            .compare_exchange_weak(prev, prev - 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        prev = API_REFS.load(Ordering::Relaxed);
    }
    S_OK
}

// ---------------------------------------------------------------------------
// QueryApiImpl COM Singletons (IXThreadingImpl / IXErrorImpl / IXNetworkingImpl2)
// ---------------------------------------------------------------------------

#[repr(transparent)]
#[derive(Clone, Copy)]
struct ComSlot(*const c_void);
unsafe impl Sync for ComSlot {}

#[repr(C)]
struct RuntimeSingleton {
    lp_vtbl: *const ComSlot,
    class_id: Guid,
    alt_iid: Guid,
}
unsafe impl Sync for RuntimeSingleton {}

unsafe extern "win64" fn singleton_qi(
    this: *mut c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    *out = core::ptr::null_mut();
    if riid.is_null() {
        return E_POINTER;
    }
    let obj = &*(this as *const RuntimeSingleton);
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == obj.class_id || iid == obj.alt_iid {
        *out = this;
        S_OK
    } else if obj.class_id == CLSID_XUSER_API && iid == IID_XUSER_GAMERTAG {
        *out = &SINGLETON_USER_GAMERTAG as *const RuntimeSingleton as *mut c_void;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn singleton_add_ref(_this: *mut c_void) -> u32 {
    2
}

unsafe extern "win64" fn singleton_release(_this: *mut c_void) -> u32 {
    1
}

// --- IXThreadingImpl (31 slots) ---
unsafe extern "win64" fn thr_xasync_get_status(
    _this: *mut c_void,
    ab: *mut XAsyncBlock,
    wait: bool,
) -> HResult {
    hle_xasync_get_status(ab, wait)
}
unsafe extern "win64" fn thr_xasync_get_result_size(
    _this: *mut c_void,
    ab: *mut XAsyncBlock,
    sz: *mut usize,
) -> HResult {
    hle_xasync_get_result_size(ab, sz)
}
unsafe extern "win64" fn thr_xasync_cancel(_this: *mut c_void, ab: *mut XAsyncBlock) {
    hle_xasync_cancel(ab);
}
unsafe extern "win64" fn thr_xasync_run(
    _this: *mut c_void,
    ab: *mut XAsyncBlock,
    work: *mut c_void,
) -> HResult {
    if ab.is_null() {
        return E_INVALIDARG;
    }
    if !work.is_null() {
        let f: unsafe extern "win64" fn(*mut XAsyncBlock) -> HResult = core::mem::transmute(work);
        let hr = f(ab);
        hle_xasync_complete(ab, hr, 0);
        hr
    } else {
        hle_xasync_complete(ab, S_OK, 0);
        S_OK
    }
}
unsafe extern "win64" fn thr_xasync_begin(
    _this: *mut c_void,
    ab: *mut XAsyncBlock,
    id: *const c_void,
    id_name: *const c_char,
    ctx: *mut c_void,
    provider: *mut c_void,
) -> HResult {
    hle_xasync_begin(ab, id, id_name, ctx, provider)
}
unsafe extern "win64" fn thr_xasync_schedule(
    _this: *mut c_void,
    ab: *mut XAsyncBlock,
    delay_ms: u32,
) -> HResult {
    hle_xasync_schedule(ab, delay_ms)
}
unsafe extern "win64" fn thr_xasync_complete(
    _this: *mut c_void,
    ab: *mut XAsyncBlock,
    hr: HResult,
    size: usize,
) {
    hle_xasync_complete(ab, hr, size);
}
unsafe extern "win64" fn thr_xasync_get_result(
    _this: *mut c_void,
    ab: *mut XAsyncBlock,
    id: *const c_void,
    cap: usize,
    buf: *mut c_void,
    used: *mut usize,
) -> HResult {
    hle_xasync_get_result(ab, id, cap, buf, used)
}
unsafe extern "win64" fn thr_tq_create(
    _this: *mut c_void,
    wm: u32,
    cm: u32,
    out: *mut Handle,
) -> HResult {
    hle_xtask_queue_create(wm, cm, out)
}
unsafe extern "win64" fn thr_tq_dispatch(
    _this: *mut c_void,
    q: Handle,
    port: u32,
    timeout_ms: u32,
) -> bool {
    hle_xtask_queue_dispatch(q, port, timeout_ms)
}
unsafe extern "win64" fn thr_tq_close(_this: *mut c_void, q: Handle) {
    hle_xtask_queue_close_handle(q);
}
unsafe extern "win64" fn thr_tq_submit(
    _this: *mut c_void,
    q: Handle,
    port: u32,
    ctx: *mut c_void,
    cb: Option<XTaskQueueCallback>,
) -> HResult {
    hle_xtask_queue_submit_callback(q, port, ctx, cb)
}
unsafe extern "win64" fn thr_tq_submit_delayed(
    _this: *mut c_void,
    q: Handle,
    port: u32,
    delay_ms: u32,
    ctx: *mut c_void,
    cb: Option<XTaskQueueCallback>,
) -> HResult {
    hle_xtask_queue_submit_delayed_callback(q, port, delay_ms, ctx, cb)
}
unsafe extern "win64" fn thr_tq_terminate(
    _this: *mut c_void,
    q: Handle,
    wait: bool,
    ctx: *mut c_void,
    cb: *mut c_void,
) -> HResult {
    hle_xtask_queue_terminate(q, wait, ctx, cb)
}
unsafe extern "win64" fn thr_tq_get_process(_this: *mut c_void, out: *mut Handle) -> bool {
    hle_xtask_queue_get_process_queue(out)
}
unsafe extern "win64" fn thr_tq_set_process(_this: *mut c_void, q: Handle) {
    hle_xtask_queue_set_process_queue(q);
}
unsafe extern "win64" fn thr_set_time_sensitive(_this: *mut c_void, val: bool) -> HResult {
    TIME_SENSITIVE.with(|c| c.set(val));
    S_OK
}
unsafe extern "win64" fn thr_assert_not_time_sensitive(_this: *mut c_void) {}
unsafe extern "win64" fn thr_is_time_sensitive(_this: *mut c_void) -> bool {
    TIME_SENSITIVE.with(|c| c.get())
}
unsafe extern "win64" fn thr_stub_zero(_this: *mut c_void) -> u64 {
    0
}

static THREADING_VTBL: [ComSlot; 31] = [
    ComSlot(singleton_qi as *const c_void),               // 0
    ComSlot(singleton_add_ref as *const c_void),          // 1
    ComSlot(singleton_release as *const c_void),          // 2
    ComSlot(thr_xasync_get_status as *const c_void),      // 3
    ComSlot(thr_xasync_get_result_size as *const c_void), // 4
    ComSlot(thr_xasync_cancel as *const c_void),          // 5
    ComSlot(thr_xasync_run as *const c_void),             // 6
    ComSlot(thr_xasync_begin as *const c_void),           // 7
    ComSlot(thr_stub_zero as *const c_void),              // 8: XAsyncBeginAlloc
    ComSlot(thr_xasync_schedule as *const c_void),        // 9
    ComSlot(thr_xasync_complete as *const c_void),        // 10
    ComSlot(thr_xasync_get_result as *const c_void),      // 11
    ComSlot(thr_tq_create as *const c_void),              // 12
    ComSlot(thr_stub_zero as *const c_void),              // 13: CreateComposite
    ComSlot(thr_stub_zero as *const c_void),              // 14: GetPort
    ComSlot(thr_stub_zero as *const c_void),              // 15: DuplicateHandle
    ComSlot(thr_tq_dispatch as *const c_void),            // 16
    ComSlot(thr_tq_close as *const c_void),               // 17
    ComSlot(thr_tq_submit as *const c_void),              // 18
    ComSlot(thr_tq_submit_delayed as *const c_void),      // 19
    ComSlot(thr_stub_zero as *const c_void),              // 20: RegisterWaiter
    ComSlot(thr_stub_zero as *const c_void),              // 21: UnregisterWaiter
    ComSlot(thr_tq_terminate as *const c_void),           // 22
    ComSlot(thr_stub_zero as *const c_void),              // 23: RegisterMonitor
    ComSlot(thr_stub_zero as *const c_void),              // 24: UnregisterMonitor
    ComSlot(thr_tq_get_process as *const c_void),         // 25
    ComSlot(thr_tq_set_process as *const c_void),         // 26
    ComSlot(thr_set_time_sensitive as *const c_void),     // 27
    ComSlot(thr_stub_zero as *const c_void),              // 28: XTaskQueueIsEmpty
    ComSlot(thr_assert_not_time_sensitive as *const c_void), // 29
    ComSlot(thr_is_time_sensitive as *const c_void),      // 30
];

// --- IXErrorImpl (6 slots) ---
unsafe extern "win64" fn err_report(_this: *mut c_void, hr: HResult, msg: *const c_char) {
    let text = if msg.is_null() {
        "(null)".to_string()
    } else {
        CStr::from_ptr(msg).to_string_lossy().into_owned()
    };
    if let Some(rt) = current_runtime() {
        rt.trace(format!("xgameruntime!XErrorReport(0x{:08X}, {text})", hr as u32));
    }
}
unsafe extern "win64" fn err_set_callback(_this: *mut c_void, _cb: *mut c_void, _ctx: *mut c_void) {}
unsafe extern "win64" fn err_set_options(_this: *mut c_void, _o1: u32, _o2: u32) -> HResult {
    S_OK
}

static ERROR_VTBL: [ComSlot; 6] = [
    ComSlot(singleton_qi as *const c_void),
    ComSlot(singleton_add_ref as *const c_void),
    ComSlot(singleton_release as *const c_void),
    ComSlot(err_report as *const c_void),       // 3
    ComSlot(err_set_callback as *const c_void), // 4
    ComSlot(err_set_options as *const c_void),  // 5
];

// --- IXNetworkingImpl2 (21 slots) ---
unsafe extern "win64" fn net_query_sec_async(
    _this: *mut c_void,
    _url: *const u16,
    ab: *mut XAsyncBlock,
) -> HResult {
    if !ab.is_null() {
        hle_xasync_complete(ab, S_OK, 0);
    }
    S_OK
}
unsafe extern "win64" fn net_query_sec_size(
    _this: *mut c_void,
    _ab: *mut XAsyncBlock,
    size: *mut usize,
) -> HResult {
    if !size.is_null() {
        *size = 0;
    }
    S_OK
}
unsafe extern "win64" fn net_query_sec_result(
    _this: *mut c_void,
    _ab: *mut XAsyncBlock,
    _cap: usize,
    used: *mut usize,
    _buf: *mut u8,
    info: *mut *mut c_void,
) -> HResult {
    if !used.is_null() {
        *used = 0;
    }
    if !info.is_null() {
        *info = core::ptr::null_mut();
    }
    S_OK
}
unsafe extern "win64" fn net_verify_cert(_this: *mut c_void, _req: *mut c_void, _sec: *const c_void) -> HResult {
    S_OK
}
unsafe extern "win64" fn net_get_connectivity_hint(
    _this: *mut c_void,
    hint: *mut XNetworkingConnectivityHint,
) -> HResult {
    if hint.is_null() {
        return E_POINTER;
    }
    *hint = XNetworkingConnectivityHint {
        connectivity_level_hint: 3, // InternetAccess
        connectivity_cost_hint: 1,  // Unrestricted
        iana_interface_type: 6,     // MIB_IF_TYPE_ETHERNET
        network_initialized: 1,
        approaching_data_limit: 0,
        over_data_limit: 0,
        roaming: 0,
    };
    S_OK
}
unsafe extern "win64" fn net_register_hint_changed(
    _this: *mut c_void,
    _q: Handle,
    _ctx: *mut c_void,
    _cb: *mut c_void,
    token: *mut XTaskQueueRegistrationToken,
) -> HResult {
    if !token.is_null() {
        (*token).token = NEXT_NET_TOKEN.fetch_add(1, Ordering::Relaxed);
    }
    S_OK
}
unsafe extern "win64" fn net_unregister_hint_changed(
    _this: *mut c_void,
    _token: XTaskQueueRegistrationToken,
    _wait: bool,
) -> bool {
    true
}

static NETWORKING_VTBL: [ComSlot; 21] = [
    ComSlot(singleton_qi as *const c_void),               // 0
    ComSlot(singleton_add_ref as *const c_void),          // 1
    ComSlot(singleton_release as *const c_void),          // 2
    ComSlot(thr_stub_zero as *const c_void),              // 3
    ComSlot(thr_stub_zero as *const c_void),              // 4
    ComSlot(thr_stub_zero as *const c_void),              // 5
    ComSlot(thr_stub_zero as *const c_void),              // 6
    ComSlot(thr_stub_zero as *const c_void),              // 7
    ComSlot(thr_stub_zero as *const c_void),              // 8
    ComSlot(thr_stub_zero as *const c_void),              // 9
    ComSlot(thr_stub_zero as *const c_void),              // 10
    ComSlot(net_query_sec_async as *const c_void),        // 11
    ComSlot(net_query_sec_size as *const c_void),         // 12
    ComSlot(net_query_sec_result as *const c_void),       // 13
    ComSlot(net_verify_cert as *const c_void),            // 14
    ComSlot(net_get_connectivity_hint as *const c_void),  // 15
    ComSlot(net_register_hint_changed as *const c_void),  // 16
    ComSlot(net_unregister_hint_changed as *const c_void), // 17
    ComSlot(thr_stub_zero as *const c_void),              // 18
    ComSlot(thr_stub_zero as *const c_void),              // 19
    ComSlot(thr_stub_zero as *const c_void),              // 20
];

// --- IXGameApiImpl (4 slots: slot 3 = XGameGetXboxTitleId) ---
unsafe extern "win64" fn api_game_get_title_id(_this: *mut c_void, out: *mut u32) -> HResult {
    hle_xgame_get_xbox_title_id(out)
}
static GAME_API_VTBL: [ComSlot; 4] = [
    ComSlot(singleton_qi as *const c_void),
    ComSlot(singleton_add_ref as *const c_void),
    ComSlot(singleton_release as *const c_void),
    ComSlot(api_game_get_title_id as *const c_void), // 3
];

// --- IXSystemApiImpl (5 slots: slot 3 = GetConsoleId, slot 4 = GetXboxLiveSandboxId) ---
unsafe extern "win64" fn api_sys_get_console_id(
    _this: *mut c_void,
    size: usize,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    hle_xsystem_get_console_id(size, buf, used)
}
unsafe extern "win64" fn api_sys_get_sandbox_id(
    _this: *mut c_void,
    size: usize,
    buf: *mut u8,
    used: *mut usize,
) -> HResult {
    hle_xsystem_get_sandbox_id(size, buf, used)
}
static SYSTEM_API_VTBL: [ComSlot; 5] = [
    ComSlot(singleton_qi as *const c_void),
    ComSlot(singleton_add_ref as *const c_void),
    ComSlot(singleton_release as *const c_void),
    ComSlot(api_sys_get_console_id as *const c_void), // 3
    ComSlot(api_sys_get_sandbox_id as *const c_void), // 4
];

// --- IXUserApiImpl (37 slots) + IXUserGamertagImpl (4 slots) ---
unsafe extern "win64" fn api_notimpl(_this: *mut c_void) -> HResult {
    E_NOTIMPL
}
unsafe extern "win64" fn api_user_dup_handle(
    _this: *mut c_void,
    user: Handle,
    dup: *mut Handle,
) -> HResult {
    if dup.is_null() {
        return E_INVALIDARG;
    }
    *dup = user;
    S_OK
}
unsafe extern "win64" fn api_user_close_handle(_this: *mut c_void, user: Handle) {
    services::hle_xuser_close_handle(user);
}
unsafe extern "win64" fn api_user_compare(_this: *mut c_void, a: Handle, b: Handle) -> i32 {
    if a == b { 0 } else if a < b { -1 } else { 1 }
}
unsafe extern "win64" fn api_user_get_max_users(_this: *mut c_void, out: *mut u32) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 4;
    S_OK
}
unsafe extern "win64" fn api_user_add_async(
    _this: *mut c_void,
    options: u32,
    ab: *mut xsx_abi::services::XAsyncBlock,
) -> HResult {
    services::hle_xuser_add_async(options, ab)
}
unsafe extern "win64" fn api_user_add_result(
    _this: *mut c_void,
    ab: *mut xsx_abi::services::XAsyncBlock,
    out: *mut Handle,
) -> HResult {
    services::hle_xuser_add_result(ab, out)
}
unsafe extern "win64" fn api_user_get_local_id(
    _this: *mut c_void,
    _user: Handle,
    out: *mut u64,
) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 1;
    S_OK
}
unsafe extern "win64" fn api_user_get_id(
    _this: *mut c_void,
    user: Handle,
    out: *mut u64,
) -> HResult {
    services::hle_xuser_get_id(user, out)
}
unsafe extern "win64" fn api_user_get_is_guest(
    _this: *mut c_void,
    _user: Handle,
    out: *mut bool,
) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = false;
    S_OK
}
unsafe extern "win64" fn api_user_get_state(
    _this: *mut c_void,
    _user: Handle,
    out: *mut u32,
) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 0; // XUserState::SignedIn
    S_OK
}
unsafe extern "win64" fn api_user_get_gamertag_component(
    _this: *mut c_void,
    user: Handle,
    component: u32,
    size: usize,
    buf: *mut c_char,
    used: *mut usize,
) -> HResult {
    const MIN_BYTES: [usize; 4] = [16, 97, 15, 101];
    if !used.is_null() {
        *used = 0;
    }
    if component > 3 {
        return E_INVALIDARG;
    }
    if size < MIN_BYTES[component as usize] {
        return E_INSUFFICIENT_BUFFER;
    }
    services::hle_xuser_get_gamertag(user, component, size, buf, used)
}
unsafe extern "win64" fn api_user_get_gamertag_classic(
    this: *mut c_void,
    user: Handle,
    size: usize,
    buf: *mut c_char,
    used: *mut usize,
) -> HResult {
    api_user_get_gamertag_component(this, user, 0, size, buf, used)
}
unsafe extern "win64" fn api_user_get_age_group(
    _this: *mut c_void,
    _user: Handle,
    out: *mut u32,
) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = 3; // XUserAgeGroup::Adult
    S_OK
}
unsafe extern "win64" fn api_user_check_privilege(
    _this: *mut c_void,
    _user: Handle,
    _options: u32,
    _privilege: u32,
    has_priv: *mut bool,
    reason: *mut u32,
) -> HResult {
    if !has_priv.is_null() {
        *has_priv = true;
    }
    if !reason.is_null() {
        *reason = 0;
    }
    S_OK
}
unsafe extern "win64" fn api_user_reg_change(
    _this: *mut c_void,
    _q: Handle,
    _ctx: *mut c_void,
    cb: *mut c_void,
    token: *mut XTaskQueueRegistrationToken,
) -> HResult {
    if cb.is_null() || token.is_null() {
        return E_INVALIDARG;
    }
    (*token).token = NEXT_USER_TOKEN.fetch_add(1, Ordering::Relaxed);
    S_OK
}
unsafe extern "win64" fn api_user_unreg_change(
    _this: *mut c_void,
    token: XTaskQueueRegistrationToken,
    _wait: bool,
) -> bool {
    token.token != 0
}

static USER_API_VTBL: [ComSlot; 37] = [
    ComSlot(singleton_qi as *const c_void),               // 0
    ComSlot(singleton_add_ref as *const c_void),          // 1
    ComSlot(singleton_release as *const c_void),          // 2
    ComSlot(api_user_dup_handle as *const c_void),        // 3
    ComSlot(api_user_close_handle as *const c_void),      // 4
    ComSlot(api_user_compare as *const c_void),           // 5
    ComSlot(api_user_get_max_users as *const c_void),     // 6
    ComSlot(api_user_add_async as *const c_void),         // 7
    ComSlot(api_user_add_result as *const c_void),        // 8
    ComSlot(api_user_get_local_id as *const c_void),      // 9
    ComSlot(api_notimpl as *const c_void),                // 10
    ComSlot(api_user_get_id as *const c_void),            // 11
    ComSlot(api_notimpl as *const c_void),                // 12
    ComSlot(api_user_get_is_guest as *const c_void),      // 13
    ComSlot(api_user_get_state as *const c_void),         // 14
    ComSlot(api_user_get_gamertag_classic as *const c_void), // 15
    ComSlot(api_notimpl as *const c_void),                // 16
    ComSlot(api_notimpl as *const c_void),                // 17
    ComSlot(api_notimpl as *const c_void),                // 18
    ComSlot(api_user_get_age_group as *const c_void),     // 19
    ComSlot(api_user_check_privilege as *const c_void),   // 20
    ComSlot(api_notimpl as *const c_void),                // 21
    ComSlot(api_notimpl as *const c_void),                // 22
    ComSlot(api_notimpl as *const c_void),                // 23
    ComSlot(api_notimpl as *const c_void),                // 24
    ComSlot(api_notimpl as *const c_void),                // 25
    ComSlot(api_notimpl as *const c_void),                // 26
    ComSlot(api_notimpl as *const c_void),                // 27
    ComSlot(api_notimpl as *const c_void),                // 28
    ComSlot(api_notimpl as *const c_void),                // 29
    ComSlot(api_notimpl as *const c_void),                // 30
    ComSlot(api_notimpl as *const c_void),                // 31
    ComSlot(api_notimpl as *const c_void),                // 32
    ComSlot(api_user_reg_change as *const c_void),        // 33
    ComSlot(api_user_unreg_change as *const c_void),      // 34
    ComSlot(api_notimpl as *const c_void),                // 35
    ComSlot(api_notimpl as *const c_void),                // 36
];

static USER_GAMERTAG_VTBL: [ComSlot; 4] = [
    ComSlot(singleton_qi as *const c_void),
    ComSlot(singleton_add_ref as *const c_void),
    ComSlot(singleton_release as *const c_void),
    ComSlot(api_user_get_gamertag_component as *const c_void), // 3
];

// --- IXGameSaveApiImpl (33 slots) ---
unsafe extern "win64" fn api_save_init_provider(
    _this: *mut c_void,
    user: Handle,
    scid: *const c_char,
    sync: i32,
    out: *mut Handle,
) -> HResult {
    services::hle_xgamesave_init_provider(user, scid, sync, out)
}
unsafe extern "win64" fn api_save_close(_this: *mut c_void, h: Handle) {
    services::hle_xgamesave_close_handle(h);
}
unsafe extern "win64" fn api_save_remaining_quota(
    _this: *mut c_void,
    _provider: Handle,
    quota: *mut i64,
) -> HResult {
    if quota.is_null() {
        return E_INVALIDARG;
    }
    *quota = 256 * 1024 * 1024;
    S_OK
}
unsafe extern "win64" fn api_save_create_container(
    _this: *mut c_void,
    provider: Handle,
    name: *const c_char,
    out: *mut Handle,
) -> HResult {
    services::hle_xgamesave_create_container(provider, name, out)
}
unsafe extern "win64" fn api_save_create_update(
    _this: *mut c_void,
    container: Handle,
    display_name: *const c_char,
    out: *mut Handle,
) -> HResult {
    services::hle_xgamesave_create_update(container, display_name, out)
}
unsafe extern "win64" fn api_save_submit_blob_write(
    _this: *mut c_void,
    update: Handle,
    name: *const c_char,
    data: *const u8,
    count: usize,
) -> HResult {
    services::hle_xgamesave_submit_blob_write(update, name, data, count)
}
unsafe extern "win64" fn api_save_submit_update(_this: *mut c_void, update: Handle) -> HResult {
    services::hle_xgamesave_submit_update(update)
}

static GAMESAVE_API_VTBL: [ComSlot; 33] = [
    ComSlot(singleton_qi as *const c_void),               // 0
    ComSlot(singleton_add_ref as *const c_void),          // 1
    ComSlot(singleton_release as *const c_void),          // 2
    ComSlot(api_save_init_provider as *const c_void),     // 3
    ComSlot(api_notimpl as *const c_void),                // 4
    ComSlot(api_notimpl as *const c_void),                // 5
    ComSlot(api_save_close as *const c_void),             // 6
    ComSlot(api_save_remaining_quota as *const c_void),   // 7
    ComSlot(api_notimpl as *const c_void),                // 8
    ComSlot(api_notimpl as *const c_void),                // 9
    ComSlot(api_notimpl as *const c_void),                // 10
    ComSlot(api_notimpl as *const c_void),                // 11
    ComSlot(api_notimpl as *const c_void),                // 12
    ComSlot(api_notimpl as *const c_void),                // 13
    ComSlot(api_notimpl as *const c_void),                // 14
    ComSlot(api_notimpl as *const c_void),                // 15
    ComSlot(api_save_create_container as *const c_void),  // 16
    ComSlot(api_save_close as *const c_void),             // 17
    ComSlot(api_notimpl as *const c_void),                // 18
    ComSlot(api_notimpl as *const c_void),                // 19
    ComSlot(api_notimpl as *const c_void),                // 20
    ComSlot(api_notimpl as *const c_void),                // 21
    ComSlot(api_notimpl as *const c_void),                // 22
    ComSlot(api_save_create_update as *const c_void),     // 23
    ComSlot(api_save_close as *const c_void),             // 24
    ComSlot(api_save_submit_blob_write as *const c_void), // 25
    ComSlot(api_notimpl as *const c_void),                // 26
    ComSlot(api_save_submit_update as *const c_void),     // 27
    ComSlot(api_notimpl as *const c_void),                // 28
    ComSlot(api_notimpl as *const c_void),                // 29
    ComSlot(api_notimpl as *const c_void),                // 30
    ComSlot(api_notimpl as *const c_void),                // 31
    ComSlot(api_save_remaining_quota as *const c_void),   // 32
];

// --- IXPackageApiImpl (39 slots) ---
unsafe extern "win64" fn api_pkg_get_user_locale(
    _this: *mut c_void,
    size: usize,
    buf: *mut c_char,
) -> HResult {
    hle_xpackage_get_user_locale(size, buf)
}
unsafe extern "win64" fn api_pkg_enumerate(
    _this: *mut c_void,
    kind: u32,
    scope: u32,
    ctx: *mut c_void,
    cb: *mut c_void,
) -> HResult {
    hle_xpackage_enumerate_packages(kind, scope, ctx, cb)
}
unsafe extern "win64" fn api_pkg_reg_installed(
    _this: *mut c_void,
    _q: Handle,
    _ctx: *mut c_void,
    cb: *mut c_void,
    token: *mut XTaskQueueRegistrationToken,
) -> HResult {
    if cb.is_null() || token.is_null() {
        return E_INVALIDARG;
    }
    (*token).token = NEXT_PKG_TOKEN.fetch_add(1, Ordering::Relaxed);
    S_OK
}

static PACKAGE_API_VTBL: [ComSlot; 39] = [
    ComSlot(singleton_qi as *const c_void),            // 0
    ComSlot(singleton_add_ref as *const c_void),       // 1
    ComSlot(singleton_release as *const c_void),       // 2
    ComSlot(api_notimpl as *const c_void),             // 3
    ComSlot(api_notimpl as *const c_void),             // 4
    ComSlot(api_notimpl as *const c_void),             // 5
    ComSlot(api_notimpl as *const c_void),             // 6
    ComSlot(api_notimpl as *const c_void),             // 7
    ComSlot(api_notimpl as *const c_void),             // 8
    ComSlot(api_notimpl as *const c_void),             // 9
    ComSlot(api_notimpl as *const c_void),             // 10
    ComSlot(api_pkg_get_user_locale as *const c_void), // 11
    ComSlot(api_notimpl as *const c_void),             // 12
    ComSlot(api_notimpl as *const c_void),             // 13
    ComSlot(api_notimpl as *const c_void),             // 14
    ComSlot(api_notimpl as *const c_void),             // 15
    ComSlot(api_notimpl as *const c_void),             // 16
    ComSlot(api_notimpl as *const c_void),             // 17
    ComSlot(api_notimpl as *const c_void),             // 18
    ComSlot(api_notimpl as *const c_void),             // 19
    ComSlot(api_pkg_enumerate as *const c_void),       // 20
    ComSlot(api_pkg_reg_installed as *const c_void),   // 21
    ComSlot(api_notimpl as *const c_void),             // 22
    ComSlot(api_notimpl as *const c_void),             // 23
    ComSlot(api_notimpl as *const c_void),             // 24
    ComSlot(api_notimpl as *const c_void),             // 25
    ComSlot(api_notimpl as *const c_void),             // 26
    ComSlot(api_notimpl as *const c_void),             // 27
    ComSlot(api_pkg_enumerate as *const c_void),       // 28
    ComSlot(api_pkg_reg_installed as *const c_void),   // 29
    ComSlot(api_notimpl as *const c_void),             // 30
    ComSlot(api_notimpl as *const c_void),             // 31
    ComSlot(api_notimpl as *const c_void),             // 32
    ComSlot(api_notimpl as *const c_void),             // 33
    ComSlot(api_notimpl as *const c_void),             // 34
    ComSlot(api_pkg_enumerate as *const c_void),       // 35
    ComSlot(api_pkg_reg_installed as *const c_void),   // 36
    ComSlot(api_notimpl as *const c_void),             // 37
    ComSlot(api_notimpl as *const c_void),             // 38
];

// --- IXGameRuntimeFeatureImpl (4 slots) & IXGameEventImpl (4 slots) ---
unsafe extern "win64" fn api_feature_is_available(_this: *mut c_void, feature: u32) -> bool {
    hle_xgame_runtime_is_feature_available(feature)
}
static FEATURE_API_VTBL: [ComSlot; 4] = [
    ComSlot(singleton_qi as *const c_void),
    ComSlot(singleton_add_ref as *const c_void),
    ComSlot(singleton_release as *const c_void),
    ComSlot(api_feature_is_available as *const c_void), // 3
];

unsafe extern "win64" fn api_event_write(
    _this: *mut c_void,
    user: Handle,
    scid: *const c_char,
    session_id: *const c_char,
    event_name: *const c_char,
    dims: *const c_char,
    meas: *const c_char,
) -> HResult {
    hle_xgame_event_write(user, scid, session_id, event_name, dims, meas)
}
static GAMEEVENT_API_VTBL: [ComSlot; 4] = [
    ComSlot(singleton_qi as *const c_void),
    ComSlot(singleton_add_ref as *const c_void),
    ComSlot(singleton_release as *const c_void),
    ComSlot(api_event_write as *const c_void), // 3
];

static SINGLETON_THREADING: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: THREADING_VTBL.as_ptr(),
    class_id: CLSID_XTHREADING_IMPL,
    alt_iid: CLSID_XTHREADING_IMPL,
};
static SINGLETON_ERROR: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: ERROR_VTBL.as_ptr(),
    class_id: CLSID_XERROR_IMPL,
    alt_iid: CLSID_XERROR_IMPL,
};
static SINGLETON_NETWORKING: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: NETWORKING_VTBL.as_ptr(),
    class_id: CLSID_XNETWORKING_IMPL,
    alt_iid: CLSID_XNETWORKING_IMPL,
};
static SINGLETON_GAME: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: GAME_API_VTBL.as_ptr(),
    class_id: CLSID_XGAME_API,
    alt_iid: IID_XGAME_LATER,
};
static SINGLETON_SYSTEM: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: SYSTEM_API_VTBL.as_ptr(),
    class_id: CLSID_XSYSTEM_API,
    alt_iid: IID_XSYSTEM_LATER,
};
static SINGLETON_USER: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: USER_API_VTBL.as_ptr(),
    class_id: CLSID_XUSER_API,
    alt_iid: IID_XUSER_LATER,
};
static SINGLETON_USER_GAMERTAG: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: USER_GAMERTAG_VTBL.as_ptr(),
    class_id: IID_XUSER_GAMERTAG,
    alt_iid: IID_XUSER_GAMERTAG,
};
static SINGLETON_GAMESAVE: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: GAMESAVE_API_VTBL.as_ptr(),
    class_id: CLSID_XGAMESAVE_API,
    alt_iid: IID_XGAMESAVE_LATER,
};
static SINGLETON_PACKAGE: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: PACKAGE_API_VTBL.as_ptr(),
    class_id: CLSID_XPACKAGE_API,
    alt_iid: IID_XPACKAGE_LATER,
};
static SINGLETON_FEATURE: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: FEATURE_API_VTBL.as_ptr(),
    class_id: CLSID_XGAMERUNTIME_FEATURE_API,
    alt_iid: CLSID_XGAMERUNTIME_FEATURE_API,
};
static SINGLETON_GAMEEVENT: RuntimeSingleton = RuntimeSingleton {
    lp_vtbl: GAMEEVENT_API_VTBL.as_ptr(),
    class_id: CLSID_XGAMEEVENT_API,
    alt_iid: CLSID_XGAMEEVENT_API,
};

unsafe extern "win64" fn hle_query_api_impl(
    class_id: *const Guid,
    interface_id: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    if !out.is_null() {
        *out = core::ptr::null_mut();
    }
    if class_id.is_null() || interface_id.is_null() || out.is_null() {
        return E_NOT_SUPPORTED;
    }
    let cid = *class_id;
    let target: Option<*const RuntimeSingleton> = if cid == CLSID_XTHREADING_IMPL {
        Some(&SINGLETON_THREADING)
    } else if cid == CLSID_XERROR_IMPL {
        Some(&SINGLETON_ERROR)
    } else if cid == CLSID_XNETWORKING_IMPL {
        Some(&SINGLETON_NETWORKING)
    } else if cid == CLSID_XGAME_API {
        Some(&SINGLETON_GAME)
    } else if cid == CLSID_XSYSTEM_API {
        Some(&SINGLETON_SYSTEM)
    } else if cid == CLSID_XUSER_API {
        Some(&SINGLETON_USER)
    } else if cid == CLSID_XGAMESAVE_API {
        Some(&SINGLETON_GAMESAVE)
    } else if cid == CLSID_XPACKAGE_API {
        Some(&SINGLETON_PACKAGE)
    } else if cid == CLSID_XGAMERUNTIME_FEATURE_API {
        Some(&SINGLETON_FEATURE)
    } else if cid == CLSID_XGAMEEVENT_API {
        Some(&SINGLETON_GAMEEVENT)
    } else {
        None
    };

    if let Some(s) = target {
        return singleton_qi(s as *mut c_void, interface_id, out);
    }
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "xgameruntime!QueryApiImpl(class={:08X}-{:04X}-{:04X}) -> E_NOT_SUPPORTED",
            cid.data1, cid.data2, cid.data3
        ));
    }
    E_NOT_SUPPORTED
}

// ---------------------------------------------------------------------------
// Core XGameRuntime / XGame / XSystem / XPackage
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_xgame_get_xbox_title_id(title_id: *mut u32) -> HResult {
    if title_id.is_null() {
        return E_INVALIDARG;
    }
    let tid = get_active_title_id();
    *title_id = tid;
    if let Some(rt) = current_runtime() {
        rt.trace(format!("xgameruntime!XGameGetXboxTitleId() -> 0x{tid:08X}"));
    }
    S_OK
}

unsafe extern "win64" fn hle_xgame_runtime_is_feature_available(feature: u32) -> bool {
    feature <= 21
}

unsafe extern "win64" fn hle_xgame_event_write(
    user: Handle,
    _scid: *const c_char,
    _session_id: *const c_char,
    event_name: *const c_char,
    _dims: *const c_char,
    _meas: *const c_char,
) -> HResult {
    if user.is_null() || event_name.is_null() {
        return E_INVALIDARG;
    }
    S_OK
}

unsafe extern "win64" fn hle_xsystem_get_sandbox_id(
    size: usize,
    out: *mut u8,
    used: *mut usize,
) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    if size < SANDBOX_ID_STR.len() {
        return E_INSUFFICIENT_BUFFER;
    }
    core::ptr::copy_nonoverlapping(SANDBOX_ID_STR.as_ptr(), out, SANDBOX_ID_STR.len());
    if !used.is_null() {
        *used = SANDBOX_ID_STR.len();
    }
    S_OK
}

unsafe extern "win64" fn hle_xpackage_get_user_locale(
    size: usize,
    out: *mut c_char,
) -> HResult {
    if out.is_null() || size < USER_LOCALE_STR.len() {
        return E_INSUFFICIENT_BUFFER;
    }
    core::ptr::copy_nonoverlapping(USER_LOCALE_STR.as_ptr(), out as *mut u8, USER_LOCALE_STR.len());
    S_OK
}

unsafe extern "win64" fn hle_xpackage_enumerate_packages(
    _kind: u32,
    _scope: u32,
    _ctx: *mut c_void,
    callback: *mut c_void,
) -> HResult {
    if callback.is_null() {
        return E_INVALIDARG;
    }
    S_OK
}

unsafe extern "win64" fn hle_xgame_runtime_initialize() -> HResult {
    RUNTIME_REFS.fetch_add(1, Ordering::Relaxed);
    if let Some(rt) = current_runtime() {
        rt.set_game_runtime_initialized(true);
        rt.trace("xgameruntime!XGameRuntimeInitialize() -> S_OK");
    }
    S_OK
}

unsafe extern "win64" fn hle_xgame_runtime_uninitialize() {
    let prev = RUNTIME_REFS.fetch_sub(1, Ordering::Relaxed);
    if prev <= 1 {
        RUNTIME_REFS.store(0, Ordering::Relaxed);
        if let Some(rt) = current_runtime() {
            rt.set_game_runtime_initialized(false);
            rt.trace("xgameruntime!XGameRuntimeUninitialize()");
        }
    }
}

unsafe extern "win64" fn hle_xsystem_get_device_type() -> u32 {
    if let Some(rt) = current_runtime() {
        rt.trace("xgameruntime!XSystemGetDeviceType() -> XboxScarlettAnaconda (Series X)");
    }
    XSystemDeviceType::XboxScarlettAnaconda as u32
}

unsafe extern "win64" fn hle_xsystem_get_console_id(
    size: usize,
    out: *mut u8,
    used: *mut usize,
) -> HResult {
    if out.is_null() {
        return E_INVALIDARG;
    }
    if size < CONSOLE_ID_STR.len() {
        return E_INSUFFICIENT_BUFFER;
    }
    core::ptr::copy_nonoverlapping(CONSOLE_ID_STR.as_ptr(), out, CONSOLE_ID_STR.len());
    if !used.is_null() {
        *used = CONSOLE_ID_STR.len();
    }
    S_OK
}

unsafe extern "win64" fn hle_xsystem_get_app_memory_usage_limit() -> u64 {
    13_500_000_000 // ~13.5 GB title memory budget on Xbox Series X
}

unsafe extern "win64" fn hle_xsystem_get_app_memory_usage() -> u64 {
    128 * 1024 * 1024
}

unsafe extern "win64" fn hle_xsystem_get_analytics_info(out_info: *mut XSystemAnalyticsInfo) {
    if out_info.is_null() {
        return;
    }
    let mut info = XSystemAnalyticsInfo {
        os_version: XVersion {
            major: 10,
            minor: 0,
            build: 25398,
            revision: 0,
        },
        hosting_os_version: XVersion {
            major: 10,
            minor: 0,
            build: 25398,
            revision: 0,
        },
        family: [0; 64],
        form: [0; 64],
    };
    copy_cstr(&mut info.family, b"Xbox");
    copy_cstr(&mut info.form, b"Xbox Series X");
    *out_info = info;
    if let Some(rt) = current_runtime() {
        rt.trace("xgameruntime!XSystemGetAnalyticsInfo() -> Xbox Series X");
    }
}

unsafe extern "win64" fn hle_xpackage_get_identifier(size: usize, buf: *mut u8) -> HResult {
    if buf.is_null() || size == 0 {
        return E_INVALIDARG;
    }
    if size < DEFAULT_PACKAGE_ID.len() {
        return E_INSUFFICIENT_BUFFER;
    }
    core::ptr::copy_nonoverlapping(DEFAULT_PACKAGE_ID.as_ptr(), buf, DEFAULT_PACKAGE_ID.len());
    S_OK
}

unsafe extern "win64" fn hle_xpackage_get_mount_path(
    _mount: Handle,
    path_size: usize,
    path_out: *mut u8,
) -> HResult {
    if path_out.is_null() || path_size == 0 {
        return E_INVALIDARG;
    }
    let mount_str = current_runtime()
        .map(|rt| rt.vfs().game_root.to_string_lossy().into_owned())
        .unwrap_or_else(|| "GAME:\\".to_string());
    let bytes = mount_str.as_bytes();
    if path_size < bytes.len() + 1 {
        return E_INSUFFICIENT_BUFFER;
    }
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), path_out, bytes.len());
    *path_out.add(bytes.len()) = 0;
    S_OK
}

unsafe extern "win64" fn hle_xpackage_is_packaged_process() -> bool {
    true
}

// ---------------------------------------------------------------------------
// XTaskQueue & XAsync implementation
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_xtask_queue_create(
    work_dispatch_mode: u32,
    completion_dispatch_mode: u32,
    queue_out: *mut Handle,
) -> HResult {
    if queue_out.is_null() {
        return E_INVALIDARG;
    }
    let h = if let Some(rt) = current_runtime() {
        let h = rt.insert_handle(KernelObject::TaskQueue {
            work_mode: work_dispatch_mode,
            completion_mode: completion_dispatch_mode,
        });
        rt.trace(format!(
            "xgameruntime!XTaskQueueCreate(work={work_dispatch_mode}, comp={completion_dispatch_mode}) -> {h:?}"
        ));
        h
    } else {
        static FALLBACK_ID: AtomicUsize = AtomicUsize::new(0x7000_0000);
        FALLBACK_ID.fetch_add(16, Ordering::Relaxed) as Handle
    };
    queues().lock().unwrap().insert(
        h as usize,
        TaskQueueState {
            work_mode: work_dispatch_mode,
            completion_mode: completion_dispatch_mode,
            ..Default::default()
        },
    );
    *queue_out = h;
    S_OK
}

unsafe extern "win64" fn hle_xtask_queue_close_handle(queue: *mut c_void) {
    queues().lock().unwrap().remove(&(queue as usize));
    if let Some(rt) = current_runtime() {
        rt.close_handle(queue);
        rt.trace(format!("xgameruntime!XTaskQueueCloseHandle({queue:?})"));
    }
}

unsafe extern "win64" fn hle_xtask_queue_submit_callback(
    queue: Handle,
    port: u32,
    context: *mut c_void,
    callback: Option<XTaskQueueCallback>,
) -> HResult {
    let Some(cb) = callback else {
        return E_INVALIDARG;
    };
    let mut immediate = false;
    {
        let mut map = queues().lock().unwrap();
        let Some(q) = map.get_mut(&(queue as usize)) else {
            return E_INVALIDARG;
        };
        if q.terminated {
            return E_ABORT;
        }
        let mode = if port == 0 { q.work_mode } else { q.completion_mode };
        // Mode 3 = Immediate; Mode 0/1 = ThreadPool (execute inline in offline deterministic runtime)
        if mode == 3 {
            immediate = true;
        } else if port == 0 {
            q.work_items.push_back(QueuedCallback {
                callback: cb,
                context: context as usize,
            });
        } else {
            q.completion_items.push_back(QueuedCallback {
                callback: cb,
                context: context as usize,
            });
        }
    }
    if immediate {
        cb(context, false);
    }
    S_OK
}

unsafe extern "win64" fn hle_xtask_queue_submit_delayed_callback(
    queue: Handle,
    port: u32,
    _delay_ms: u32,
    context: *mut c_void,
    callback: Option<XTaskQueueCallback>,
) -> HResult {
    hle_xtask_queue_submit_callback(queue, port, context, callback)
}

unsafe extern "win64" fn hle_xtask_queue_dispatch(
    queue: Handle,
    port: u32,
    _timeout_ms: u32,
) -> bool {
    let item = {
        let mut map = queues().lock().unwrap();
        let Some(q) = map.get_mut(&(queue as usize)) else {
            return false;
        };
        if port == 0 {
            q.work_items.pop_front()
        } else {
            q.completion_items.pop_front()
        }
    };
    if let Some(it) = item {
        (it.callback)(it.context as *mut c_void, false);
        true
    } else {
        false
    }
}

unsafe extern "win64" fn hle_xtask_queue_terminate(
    queue: Handle,
    _wait: bool,
    context: *mut c_void,
    callback: *mut c_void,
) -> HResult {
    let drained: Vec<QueuedCallback> = {
        let mut map = queues().lock().unwrap();
        let Some(q) = map.get_mut(&(queue as usize)) else {
            return E_INVALIDARG;
        };
        q.terminated = true;
        q.work_items
            .drain(..)
            .chain(q.completion_items.drain(..))
            .collect()
    };
    for it in drained {
        (it.callback)(it.context as *mut c_void, true);
    }
    if !callback.is_null() {
        let f: unsafe extern "win64" fn(*mut c_void) = core::mem::transmute(callback);
        f(context);
    }
    S_OK
}

unsafe extern "win64" fn hle_xtask_queue_get_process_queue(out: *mut Handle) -> bool {
    if out.is_null() {
        return false;
    }
    let mut ptr = PROCESS_TASK_QUEUE.load(Ordering::Acquire);
    if ptr == 0 {
        let mut q = core::ptr::null_mut();
        if hle_xtask_queue_create(0, 0, &mut q) == S_OK {
            let _ = PROCESS_TASK_QUEUE.compare_exchange(
                0,
                q as usize,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            ptr = PROCESS_TASK_QUEUE.load(Ordering::Acquire);
        }
    }
    *out = ptr as Handle;
    ptr != 0
}

unsafe extern "win64" fn hle_xtask_queue_set_process_queue(queue: Handle) {
    PROCESS_TASK_QUEUE.store(queue as usize, Ordering::Release);
}

unsafe extern "win64" fn hle_xasync_begin(
    ab: *mut XAsyncBlock,
    _identity: *const c_void,
    _identity_name: *const c_char,
    _context: *mut c_void,
    _provider: *mut c_void,
) -> HResult {
    if ab.is_null() {
        return E_INVALIDARG;
    }
    async_blocks().lock().unwrap().insert(
        ab as usize,
        AsyncState {
            status: E_PENDING,
            result_data: Vec::new(),
            result_size: 0,
            completed: false,
            canceled: false,
        },
    );
    S_OK
}

unsafe extern "win64" fn hle_xasync_schedule(ab: *mut XAsyncBlock, _delay_ms: u32) -> HResult {
    if ab.is_null() {
        return E_INVALIDARG;
    }
    S_OK
}

unsafe extern "win64" fn hle_xasync_complete(
    ab: *mut XAsyncBlock,
    result: HResult,
    required_buffer_size: usize,
) {
    if ab.is_null() {
        return;
    }
    {
        let mut map = async_blocks().lock().unwrap();
        let entry = map.entry(ab as usize).or_insert(AsyncState {
            status: E_PENDING,
            result_data: Vec::new(),
            result_size: 0,
            completed: false,
            canceled: false,
        });
        entry.status = result;
        entry.result_size = required_buffer_size;
        entry.completed = true;
    }
    if let Some(cb) = (*ab).callback {
        cb(ab);
    }
}

unsafe extern "win64" fn hle_xasync_get_status(ab: *mut XAsyncBlock, _wait: bool) -> HResult {
    if ab.is_null() {
        return E_INVALIDARG;
    }
    let map = async_blocks().lock().unwrap();
    match map.get(&(ab as usize)) {
        Some(st) if st.canceled => E_ABORT,
        Some(st) if st.completed => st.status,
        Some(_) => E_PENDING,
        None => S_OK,
    }
}

unsafe extern "win64" fn hle_xasync_get_result_size(
    ab: *mut XAsyncBlock,
    buffer_size: *mut usize,
) -> HResult {
    if ab.is_null() || buffer_size.is_null() {
        return E_INVALIDARG;
    }
    let map = async_blocks().lock().unwrap();
    match map.get(&(ab as usize)) {
        Some(st) if st.completed => {
            *buffer_size = st.result_size;
            st.status
        }
        Some(_) => E_PENDING,
        None => {
            *buffer_size = 0;
            S_OK
        }
    }
}

unsafe extern "win64" fn hle_xasync_get_result(
    ab: *mut XAsyncBlock,
    _identity: *const c_void,
    buffer_size: usize,
    buffer: *mut c_void,
    buffer_used: *mut usize,
) -> HResult {
    if ab.is_null() {
        return E_INVALIDARG;
    }
    let map = async_blocks().lock().unwrap();
    let Some(st) = map.get(&(ab as usize)) else {
        if !buffer_used.is_null() {
            *buffer_used = 0;
        }
        return S_OK;
    };
    if !st.completed {
        return E_PENDING;
    }
    if buffer_size < st.result_size {
        return E_INSUFFICIENT_BUFFER;
    }
    if !buffer.is_null() && !st.result_data.is_empty() {
        let n = st.result_data.len().min(buffer_size);
        core::ptr::copy_nonoverlapping(st.result_data.as_ptr(), buffer as *mut u8, n);
    }
    if !buffer_used.is_null() {
        *buffer_used = st.result_size;
    }
    st.status
}

unsafe extern "win64" fn hle_xasync_cancel(ab: *mut XAsyncBlock) {
    if ab.is_null() {
        return;
    }
    if let Some(st) = async_blocks().lock().unwrap().get_mut(&(ab as usize)) {
        st.canceled = true;
        st.completed = true;
        st.status = E_ABORT;
    }
}

fn copy_cstr(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_api_impl_serves_threading_error_and_networking_singletons() {
        unsafe {
            assert_eq!(hle_initialize_api_impl_ex2(10, 1, 0, core::ptr::null()), S_OK);

            // Query IXThreadingImpl and invoke slot 12 (XTaskQueueCreate) + slot 27/30 (TimeSensitive)
            let mut thr: *mut c_void = core::ptr::null_mut();
            assert_eq!(
                hle_query_api_impl(&CLSID_XTHREADING_IMPL, &CLSID_XTHREADING_IMPL, &mut thr),
                S_OK
            );
            assert!(!thr.is_null());
            let vtbl = &**(thr as *const *const [ComSlot; 31]);
            let set_ts: unsafe extern "win64" fn(*mut c_void, bool) -> HResult =
                core::mem::transmute(vtbl[27].0);
            let is_ts: unsafe extern "win64" fn(*mut c_void) -> bool =
                core::mem::transmute(vtbl[30].0);
            assert!(!is_ts(thr));
            assert_eq!(set_ts(thr, true), S_OK);
            assert!(is_ts(thr));
            assert_eq!(set_ts(thr, false), S_OK);

            // Query IXNetworkingImpl2 and invoke slot 15 (XNetworkingGetConnectivityHint)
            let mut net: *mut c_void = core::ptr::null_mut();
            assert_eq!(
                hle_query_api_impl(&CLSID_XNETWORKING_IMPL, &CLSID_XNETWORKING_IMPL, &mut net),
                S_OK
            );
            let nvtbl = &**(net as *const *const [ComSlot; 21]);
            let get_hint: unsafe extern "win64" fn(
                *mut c_void,
                *mut XNetworkingConnectivityHint,
            ) -> HResult = core::mem::transmute(nvtbl[15].0);
            let mut hint = XNetworkingConnectivityHint::default();
            assert_eq!(get_hint(net, &mut hint), S_OK);
            assert_eq!(hint.connectivity_level_hint, 3);
            assert_eq!(hint.network_initialized, 1);

            // Unknown class -> E_NOT_SUPPORTED
            let mut unknown: *mut c_void = core::ptr::null_mut();
            let bogus = Guid::new(1, 2, 3, [0; 8]);
            assert_eq!(hle_query_api_impl(&bogus, &bogus, &mut unknown), E_NOT_SUPPORTED);
        }
    }
}
