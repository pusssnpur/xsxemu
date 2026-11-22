//! `xgameruntime.dll` (Microsoft GDK GameRuntime) HLE thunks.

use core::ffi::c_void;

use xsx_abi::com::{HResult, E_INVALIDARG, S_OK};
use xsx_abi::gdk::{Handle, XSystemAnalyticsInfo, XSystemDeviceType, XVersion};

use crate::registry::ModuleRegistry;
use crate::runtime::{current_runtime, KernelObject};

pub fn register(reg: &mut ModuleRegistry) {
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
        "XSystemGetDeviceType",
        hle_xsystem_get_device_type as *const () as usize,
    );
    reg.register_export(
        "xgameruntime.dll",
        "XSystemGetAnalyticsInfo",
        hle_xsystem_get_analytics_info as *const () as usize,
    );
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
}

unsafe extern "win64" fn hle_xgame_runtime_initialize() -> HResult {
    if let Some(rt) = current_runtime() {
        rt.set_game_runtime_initialized(true);
        rt.trace("xgameruntime!XGameRuntimeInitialize() -> S_OK");
    }
    S_OK
}

unsafe extern "win64" fn hle_xgame_runtime_uninitialize() {
    if let Some(rt) = current_runtime() {
        rt.set_game_runtime_initialized(false);
        rt.trace("xgameruntime!XGameRuntimeUninitialize()");
    }
}

unsafe extern "win64" fn hle_xsystem_get_device_type() -> u32 {
    if let Some(rt) = current_runtime() {
        rt.trace("xgameruntime!XSystemGetDeviceType() -> XboxScarlettAnaconda (Series X)");
    }
    XSystemDeviceType::XboxScarlettAnaconda as u32
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

unsafe extern "win64" fn hle_xtask_queue_create(
    work_dispatch_mode: u32,
    completion_dispatch_mode: u32,
    queue_out: *mut Handle,
) -> HResult {
    if queue_out.is_null() {
        return E_INVALIDARG;
    }
    if let Some(rt) = current_runtime() {
        let h = rt.insert_handle(KernelObject::TaskQueue {
            work_mode: work_dispatch_mode,
            completion_mode: completion_dispatch_mode,
        });
        *queue_out = h;
        rt.trace(format!(
            "xgameruntime!XTaskQueueCreate(work={work_dispatch_mode}, comp={completion_dispatch_mode}) -> {h:?}"
        ));
        S_OK
    } else {
        E_INVALIDARG
    }
}

unsafe extern "win64" fn hle_xtask_queue_close_handle(queue: *mut c_void) {
    if let Some(rt) = current_runtime() {
        rt.close_handle(queue);
        rt.trace(format!("xgameruntime!XTaskQueueCloseHandle({queue:?})"));
    }
}

fn copy_cstr(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len().saturating_sub(1));
    dst[..n].copy_from_slice(&src[..n]);
    dst[n] = 0;
}
