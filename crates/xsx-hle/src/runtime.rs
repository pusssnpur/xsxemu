//! Active guest runtime state (handles, debug logs, timing, and D3D12 translator).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use xsx_abi::gdk::Handle;
use xsx_d3d12::translate::HostDeviceCreateConfig;
use xsx_d3d12::{D3D12Translator, PresentedFrame, TranslatorConfig};

#[derive(Debug, Clone)]
pub enum KernelObject {
    Event {
        manual_reset: bool,
        signaled: bool,
    },
    TaskQueue {
        work_mode: u32,
        completion_mode: u32,
    },
}

/// Process-scoped runtime environment active while executing a guest Xbox PE64 binary.
pub struct GuestRuntime {
    pub translator_config: TranslatorConfig,
    pub trace_hle: bool,
    start_time: Instant,
    last_error: AtomicU32,
    game_runtime_initialized: AtomicBool,
    next_handle: AtomicU64,
    handles: Mutex<HashMap<usize, KernelObject>>,
    debug_messages: Mutex<Vec<String>>,
    hle_trace: Mutex<Vec<String>>,
    d3d12: Mutex<Option<Arc<D3D12Translator>>>,
    heap_allocs: Mutex<HashMap<usize, Vec<u8>>>,
}

impl GuestRuntime {
    pub fn new(translator_config: TranslatorConfig, trace_hle: bool) -> Arc<Self> {
        Arc::new(Self {
            translator_config,
            trace_hle,
            start_time: Instant::now(),
            last_error: AtomicU32::new(0),
            game_runtime_initialized: AtomicBool::new(false),
            next_handle: AtomicU64::new(0x1000),
            handles: Mutex::new(HashMap::new()),
            debug_messages: Mutex::new(Vec::new()),
            hle_trace: Mutex::new(Vec::new()),
            d3d12: Mutex::new(None),
            heap_allocs: Mutex::new(HashMap::new()),
        })
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

    pub fn with_handle_mut<R>(&self, h: Handle, f: impl FnOnce(&mut KernelObject) -> R) -> Option<R> {
        self.handles.lock().unwrap().get_mut(&(h as usize)).map(f)
    }

    pub fn alloc_virtual(&self, size: usize) -> *mut u8 {
        let mut buf = vec![0u8; size.max(16)];
        let ptr = buf.as_mut_ptr();
        self.heap_allocs.lock().unwrap().insert(ptr as usize, buf);
        ptr
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
}

static ACTIVE_RUNTIME: OnceLock<RwLock<Option<Arc<GuestRuntime>>>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<Arc<GuestRuntime>>> {
    ACTIVE_RUNTIME.get_or_init(|| RwLock::new(None))
}

/// RAII guard that binds a `GuestRuntime` as the active global HLE context.
pub struct RuntimeGuard {
    _private: (),
}

impl RuntimeGuard {
    pub fn activate(runtime: Arc<GuestRuntime>) -> Self {
        *slot().write().unwrap() = Some(runtime);
        Self { _private: () }
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
