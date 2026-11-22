//! Active guest runtime state (handles, VFS, threads, sync primitives, TLS, modules,
//! Phase 4 input/audio/dstorage/saves state, and D3D12/FSR translator).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use xsx_abi::gdk::{Handle, INFINITE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use xsx_abi::services::{XInputGamepad, XInputVibration};
use xsx_d3d12::translate::HostDeviceCreateConfig;
use xsx_d3d12::{D3D12Translator, FsrFrameStats, PresentedFrame, TranslatorConfig};
use xsx_loader::LoadedModuleSet;

use crate::audio_mixer::AudioMixer;
use crate::input::VirtualGamepadState;
use crate::rtc::RtcConfig;
use crate::services::{SaveContainerState, SaveUpdateState};
use crate::startup_trace::StartupTracer;
use crate::vfs::{VfsDirEntry, VirtualFs};

#[derive(Debug)]
pub struct FindSearchState {
    pub entries: Vec<VfsDirEntry>,
    pub cursor: usize,
}

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
    FindSearch(Arc<Mutex<FindSearchState>>),
    Thread(Arc<ThreadState>),
    TaskQueue {
        work_mode: u32,
        completion_mode: u32,
    },
    User {
        xuid: u64,
        gamertag: String,
    },
    SaveProvider {
        root_dir: PathBuf,
    },
    SaveContainer(SaveContainerState),
    SaveUpdate(SaveUpdateState),
    StoreContext,
}

thread_local! {
    static THREAD_TLS_SLOTS: RefCell<HashMap<u32, usize>> = RefCell::new(HashMap::new());
}

/// Process-scoped runtime environment active while executing a guest Xbox PE64 binary.
pub struct GuestRuntime {
    pub translator_config: TranslatorConfig,
    pub vfs: VirtualFs,
    pub trace_hle: bool,
    rtc: Mutex<RtcConfig>,
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
    startup_tracer: Mutex<Option<Arc<StartupTracer>>>,

    // Phase 4 & 5 state: Virtual gamepads, multi-voice AudioMixer, DirectStorage telemetry
    gamepads: Mutex<[VirtualGamepadState; 4]>,
    audio_mixer: Mutex<AudioMixer>,
    dstorage_bytes_read: AtomicU64,
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
        Self::with_options(translator_config, vfs, RtcConfig::default(), false, trace_hle)
    }

    pub fn with_options(
        translator_config: TranslatorConfig,
        vfs: VirtualFs,
        rtc: RtcConfig,
        live_audio: bool,
        trace_hle: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            translator_config,
            vfs,
            trace_hle,
            rtc: Mutex::new(rtc),
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
            startup_tracer: Mutex::new(None),
            gamepads: Mutex::new([
                VirtualGamepadState::connected_default(),
                VirtualGamepadState::default(),
                VirtualGamepadState::default(),
                VirtualGamepadState::default(),
            ]),
            audio_mixer: Mutex::new(AudioMixer::new(live_audio)),
            dstorage_bytes_read: AtomicU64::new(0),
        })
    }

    pub fn vfs(&self) -> &VirtualFs {
        &self.vfs
    }

    pub fn set_startup_tracer(&self, tracer: Arc<StartupTracer>) {
        *self.startup_tracer.lock().unwrap() = Some(tracer);
    }

    pub fn startup_tracer(&self) -> Option<Arc<StartupTracer>> {
        self.startup_tracer.lock().unwrap().clone()
    }

    pub fn get_find_search(&self, h: Handle) -> Option<Arc<Mutex<FindSearchState>>> {
        match self.handles.lock().unwrap().get(&(h as usize)) {
            Some(KernelObject::FindSearch(fs)) => Some(Arc::clone(fs)),
            _ => None,
        }
    }

    pub fn rtc(&self) -> RtcConfig {
        *self.rtc.lock().unwrap()
    }

    pub fn set_rtc(&self, cfg: RtcConfig) {
        *self.rtc.lock().unwrap() = cfg;
    }

    pub fn attach_modules(&self, modules: Arc<Mutex<LoadedModuleSet>>) {
        *self.modules.lock().unwrap() = Some(modules);
    }

    pub fn modules(&self) -> Option<Arc<Mutex<LoadedModuleSet>>> {
        self.modules.lock().unwrap().clone()
    }

    // -----------------------------------------------------------------------
    // Phase 4 & 5 Gamepad, AudioMixer, DirectStorage, and Save helpers
    // -----------------------------------------------------------------------

    pub fn set_virtual_gamepad(&self, user_index: u32, gamepad: XInputGamepad) {
        if let Some(slot) = self.gamepads.lock().unwrap().get_mut(user_index as usize) {
            slot.connected = true;
            slot.packet_number = slot.packet_number.wrapping_add(1);
            slot.gamepad = gamepad;
        }
    }

    pub fn get_gamepad_state(&self, user_index: u32) -> VirtualGamepadState {
        self.gamepads
            .lock()
            .unwrap()
            .get(user_index as usize)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_gamepad_vibration(&self, user_index: u32, vib: XInputVibration) {
        if let Some(slot) = self.gamepads.lock().unwrap().get_mut(user_index as usize) {
            slot.last_vibration = vib;
        }
    }

    pub fn set_master_volume(&self, volume: f32) {
        self.audio_mixer.lock().unwrap().set_master_volume(volume);
    }

    pub fn mix_audio_pcm(
        &self,
        pcm: &[u8],
        src_channels: u16,
        src_sample_rate: u32,
        voice_volume: f32,
    ) -> u64 {
        self.audio_mixer
            .lock()
            .unwrap()
            .submit_pcm16(pcm, src_channels, src_sample_rate, voice_volume)
    }

    pub fn audio_samples_submitted(&self) -> u64 {
        self.audio_mixer.lock().unwrap().total_frames_mixed()
    }

    pub fn audio_pcm_len(&self) -> usize {
        self.audio_mixer.lock().unwrap().pcm_byte_len()
    }

    pub fn write_audio_wav(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.audio_mixer.lock().unwrap().write_wav(path)
    }

    pub fn record_dstorage_bytes(&self, bytes: u64) {
        self.dstorage_bytes_read.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn dstorage_bytes_read(&self) -> u64 {
        self.dstorage_bytes_read.load(Ordering::Relaxed)
    }

    pub fn get_save_provider_root(&self, h: Handle) -> Option<PathBuf> {
        match self.handles.lock().unwrap().get(&(h as usize)) {
            Some(KernelObject::SaveProvider { root_dir }) => Some(root_dir.clone()),
            _ => None,
        }
    }

    pub fn get_save_container(&self, h: Handle) -> Option<SaveContainerState> {
        match self.handles.lock().unwrap().get(&(h as usize)) {
            Some(KernelObject::SaveContainer(c)) => Some(c.clone()),
            _ => None,
        }
    }

    pub fn stage_save_blob_write(&self, update: Handle, name: String, data: Vec<u8>) -> bool {
        if let Some(KernelObject::SaveUpdate(upd)) =
            self.handles.lock().unwrap().get_mut(&(update as usize))
        {
            upd.writes.insert(name, data);
            true
        } else {
            false
        }
    }

    pub fn commit_save_update(&self, update: Handle) -> std::io::Result<usize> {
        let mut map = self.handles.lock().unwrap();
        let Some(KernelObject::SaveUpdate(upd)) = map.get_mut(&(update as usize)) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid save update handle",
            ));
        };
        fs::create_dir_all(&upd.container_dir)?;
        let mut count = 0usize;
        for (name, data) in std::mem::take(&mut upd.writes) {
            fs::write(upd.container_dir.join(name), data)?;
            count += 1;
        }
        for del in std::mem::take(&mut upd.deletes) {
            let _ = fs::remove_file(upd.container_dir.join(del));
        }
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // Core Runtime State
    // -----------------------------------------------------------------------

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
        if let Some(st) = self.startup_tracer() {
            st.record_debug_string(&s);
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
        let mut buf = map.remove(&(old_ptr as usize)).unwrap_or_default();
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
