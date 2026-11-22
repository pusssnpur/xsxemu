//! Active guest runtime state (handles, VFS, threads, sync primitives, TLS, modules, and D3D12/FSR).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use xsx_abi::gdk::{Handle, INFINITE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use xsx_d3d12::translate::HostDeviceCreateConfig;
use xsx_d3d12::{D3D12Translator, FsrFrameStats, PresentedFrame, TranslatorConfig};
use xsx_loader::LoadedModuleSet;

use crate::vfs::VirtualFs;

#[derive(Debug)]
pub struct EventState {
    pub manual_reset: bool,
    pub signaled: bool,
}

#[derive(Debug)]
pub struct ThreadState {
    pub done: Mutex<bool>,
    pub condvar: Condvar,
    pub exit_code: AtomicU32,
}

#[derive(Debug, Default)]
pub struct SrwState {
    pub readers: u32,
    pub writer: bool,
}

#[derive(Debug, Default)]
pub struct CritState {
    pub owner: Option<ThreadId>,
    pub recursion: u32,
}

pub enum KernelObject {
    Event(Arc<(Mutex<EventState>, Condvar)>),
    File(Arc<Mutex<File>>),
    Thread(Arc<ThreadState>),
    TaskQueue {
        work_mode: u32,
        completion_mode: u32,
    },
}

thread_local! {
    static THREAD_TLS_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

/// Process-scoped runtime environment active while executing a guest Xbox PE64 binary.
pub struct GuestRuntime {
    pub translator_config: TranslatorConfig,
    pub vfs: VirtualFs,
    pub trace_hle: bool,
    start_time: Instant,
    last_error: AtomicU32,
    game_runtime_initialized: AtomicBool,
    next_handle: AtomicU64,
    next_tls_slot: AtomicU32,
    handles: Mutex<HashMap<usize, KernelObject>>,
    srw_locks: Mutex<HashMap<usize, Arc<(Mutex<SrwState>, Condvar)>>>,
    crit_sections: Mutex<HashMap<usize, Arc<(Mutex<CritState>, Condvar)>>>,
    debug_messages: Mutex<Vec<String>>,
    hle_trace: Mutex<Vec<String>>,
    d3d12: Mutex<Option<Arc<D3D12Translator>>>,
    heap_allocs: Mutex<HashMap<usize, Vec<u8>>>,
    modules: Mutex<Option<Arc<Mutex<LoadedModuleSet>>>>,
}

impl GuestRuntime {
    pub fn new(translator_config: TranslatorConfig, trace_hle: bool) -> Arc<Self> {
        Self::with_vfs(translator_config, VirtualFs::default_temp(), trace_hle)
    }

    pub fn with_vfs(
        translator_config: TranslatorConfig,
        vfs: VirtualFs,
        trace_hle: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            translator_config,
            vfs,
            trace_hle,
            start_time: Instant::now(),
            last_error: AtomicU32::new(0),
            game_runtime_initialized: AtomicBool::new(false),
            next_handle: AtomicU64::new(0x1000),
            next_tls_slot: AtomicU32::new(1),
            handles: Mutex::new(HashMap::new()),
            srw_locks: Mutex::new(HashMap::new()),
            crit_sections: Mutex::new(HashMap::new()),
            debug_messages: Mutex::new(Vec::new()),
            hle_trace: Mutex::new(Vec::new()),
            d3d12: Mutex::new(None),
            heap_allocs: Mutex::new(HashMap::new()),
            modules: Mutex::new(None),
        })
    }

    pub fn attach_modules(&self, modules: Arc<Mutex<LoadedModuleSet>>) {
        *self.modules.lock().unwrap() = Some(modules);
    }

    pub fn modules(&self) -> Option<Arc<Mutex<LoadedModuleSet>>> {
        self.modules.lock().unwrap().clone()
    }

    pub fn trace(&self, msg: impl Into<String>) {
        let s = msg.into();
        if self.trace_hle {
            eprintln!("[hle] {s}");
        }
        if let Ok(mut log) = self.hle_trace.lock() {
            log.push(s);
        }
    }

    pub fn log_debug_string(&self, s: String) {
        if self.trace_hle {
            eprintln!("[guest-debug] {s}");
        }
        self.debug_messages.lock().unwrap().push(s);
    }

    pub fn debug_messages(&self) -> Vec<String> {
        self.debug_messages.lock().unwrap().clone()
    }

    pub fn hle_trace(&self) -> Vec<String> {
        self.hle_trace.lock().unwrap().clone()
    }

    pub fn elapsed_us(&self) -> u64 {
        self.start_time.elapsed().as_micros() as u64
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    pub fn set_last_error(&self, err: u32) {
        self.last_error.store(err, Ordering::Relaxed);
    }

    pub fn get_last_error(&self) -> u32 {
        self.last_error.load(Ordering::Relaxed)
    }

    pub fn set_game_runtime_initialized(&self, v: bool) {
        self.game_runtime_initialized.store(v, Ordering::SeqCst);
    }

    pub fn is_game_runtime_initialized(&self) -> bool {
        self.game_runtime_initialized.load(Ordering::SeqCst)
    }

    pub fn insert_handle(&self, obj: KernelObject) -> Handle {
        let h = self.next_handle.fetch_add(4, Ordering::Relaxed) as usize;
        self.handles.lock().unwrap().insert(h, obj);
        h as Handle
    }

    pub fn close_handle(&self, h: Handle) -> bool {
        self.handles.lock().unwrap().remove(&(h as usize)).is_some()
    }

    pub fn get_event(&self, h: Handle) -> Option<Arc<(Mutex<EventState>, Condvar)>> {
        match self.handles.lock().unwrap().get(&(h as usize)) {
            Some(KernelObject::Event(ev)) => Some(Arc::clone(ev)),
            _ => None,
        }
    }

    pub fn get_file(&self, h: Handle) -> Option<Arc<Mutex<File>>> {
        match self.handles.lock().unwrap().get(&(h as usize)) {
            Some(KernelObject::File(f)) => Some(Arc::clone(f)),
            _ => None,
        }
    }

    pub fn get_thread(&self, h: Handle) -> Option<Arc<ThreadState>> {
        match self.handles.lock().unwrap().get(&(h as usize)) {
            Some(KernelObject::Thread(t)) => Some(Arc::clone(t)),
            _ => None,
        }
    }

    pub fn wait_handle(&self, h: Handle, timeout_ms: u32) -> Option<u32> {
        let infinite = timeout_ms == INFINITE;
        let deadline = if infinite {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
        };

        if let Some(ev) = self.get_event(h) {
            let (lock, cvar) = &*ev;
            let mut st = lock.lock().unwrap();
            loop {
                if st.signaled {
                    if !st.manual_reset {
                        st.signaled = false;
                    }
                    return Some(WAIT_OBJECT_0);
                }
                if let Some(dl) = deadline {
                    let now = Instant::now();
                    if now >= dl {
                        return Some(WAIT_TIMEOUT);
                    }
                    let (guard, res) = cvar.wait_timeout(st, dl - now).unwrap();
                    st = guard;
                    if res.timed_out() && !st.signaled {
                        return Some(WAIT_TIMEOUT);
                    }
                } else {
                    st = cvar.wait(st).unwrap();
                }
            }
        }

        if let Some(th) = self.get_thread(h) {
            let mut done = th.done.lock().unwrap();
            loop {
                if *done {
                    return Some(WAIT_OBJECT_0);
                }
                if let Some(dl) = deadline {
                    let now = Instant::now();
                    if now >= dl {
                        return Some(WAIT_TIMEOUT);
                    }
                    let (guard, res) = th.condvar.wait_timeout(done, dl - now).unwrap();
                    done = guard;
                    if res.timed_out() && !*done {
                        return Some(WAIT_TIMEOUT);
                    }
                } else {
                    done = th.condvar.wait(done).unwrap();
                }
            }
        }

        None
    }

    pub fn get_or_init_srw(&self, addr: usize) -> Arc<(Mutex<SrwState>, Condvar)> {
        let mut map = self.srw_locks.lock().unwrap();
        Arc::clone(
            map.entry(addr)
                .or_insert_with(|| Arc::new((Mutex::new(SrwState::default()), Condvar::new()))),
        )
    }

    pub fn get_or_init_crit(&self, addr: usize) -> Arc<(Mutex<CritState>, Condvar)> {
        let mut map = self.crit_sections.lock().unwrap();
        Arc::clone(
            map.entry(addr)
                .or_insert_with(|| Arc::new((Mutex::new(CritState::default()), Condvar::new()))),
        )
    }

    pub fn remove_crit(&self, addr: usize) {
        self.crit_sections.lock().unwrap().remove(&addr);
    }

    pub fn alloc_tls_slot(&self) -> u32 {
        self.next_tls_slot.fetch_add(1, Ordering::Relaxed)
    }

    pub fn set_tls_value(slot: u32, val: usize) {
        THREAD_TLS_SLOTS.with(|m| {
            m.borrow_mut().insert(slot, val);
        });
    }

    pub fn get_tls_value(slot: u32) -> usize {
        THREAD_TLS_SLOTS.with(|m| m.borrow().get(&slot).copied().unwrap_or(0))
    }

    pub fn free_tls_slot(slot: u32) {
        THREAD_TLS_SLOTS.with(|m| {
            m.borrow_mut().remove(&slot);
        });
    }

    pub fn alloc_virtual(&self, size: usize) -> *mut u8 {
        let mut buf = vec![0u8; size.max(16)];
        let ptr = buf.as_mut_ptr();
        self.heap_allocs.lock().unwrap().insert(ptr as usize, buf);
        ptr
    }

    pub fn realloc_virtual(&self, old_ptr: *mut u8, new_size: usize) -> *mut u8 {
        let mut map = self.heap_allocs.lock().unwrap();
        let mut buf = map
            .remove(&(old_ptr as usize))
            .unwrap_or_default();
        buf.resize(new_size.max(16), 0);
        let ptr = buf.as_mut_ptr();
        map.insert(ptr as usize, buf);
        ptr
    }

    pub fn virtual_size(&self, ptr: *mut u8) -> usize {
        self.heap_allocs
            .lock()
            .unwrap()
            .get(&(ptr as usize))
            .map(|b| b.len())
            .unwrap_or(0)
    }

    pub fn free_virtual(&self, ptr: *mut u8) -> bool {
        self.heap_allocs
            .lock()
            .unwrap()
            .remove(&(ptr as usize))
            .is_some()
    }

    pub fn create_or_get_d3d12(
        &self,
        device_cfg: HostDeviceCreateConfig,
    ) -> Result<Arc<D3D12Translator>, String> {
        let mut guard = self.d3d12.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            return Ok(Arc::clone(existing));
        }
        let translator = D3D12Translator::new(self.translator_config.clone(), device_cfg)?;
        *guard = Some(Arc::clone(&translator));
        Ok(translator)
    }

    pub fn d3d12_translator(&self) -> Option<Arc<D3D12Translator>> {
        self.d3d12.lock().unwrap().clone()
    }

    pub fn presented_frames(&self) -> Vec<PresentedFrame> {
        self.d3d12_translator()
            .map(|t| t.presented_frames())
            .unwrap_or_default()
    }

    pub fn fsr_stats(&self) -> Vec<FsrFrameStats> {
        self.d3d12_translator()
            .map(|t| t.fsr_stats())
            .unwrap_or_default()
    }

    pub fn current_thread_u32() -> u32 {
        // Stable non-zero 32-bit hash of the OS ThreadId
        let id = thread::current().id();
        let dbg = format!("{id:?}");
        let mut h: u32 = 2166136261;
        for b in dbg.bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(16777619);
        }
        h.max(1)
    }
}

static ACTIVE_RUNTIME: OnceLock<RwLock<Option<Arc<GuestRuntime>>>> = OnceLock::new();
static SESSION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<Arc<GuestRuntime>>> {
    ACTIVE_RUNTIME.get_or_init(|| RwLock::new(None))
}

fn session_lock() -> &'static Mutex<()> {
    SESSION_LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard that binds a `GuestRuntime` as the active global HLE context.
/// Serializes distinct guest sessions across threads while allowing worker
/// threads spawned by the active guest to access `current_runtime()` concurrently.
pub struct RuntimeGuard {
    _session: std::sync::MutexGuard<'static, ()>,
}

impl RuntimeGuard {
    pub fn activate(runtime: Arc<GuestRuntime>) -> Self {
        let session = session_lock().lock().unwrap_or_else(|e| e.into_inner());
        *slot().write().unwrap() = Some(runtime);
        Self { _session: session }
    }
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        *slot().write().unwrap() = None;
    }
}

pub fn current_runtime() -> Option<Arc<GuestRuntime>> {
    slot().read().unwrap().clone()
}
