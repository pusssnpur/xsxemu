//! `XAudio2_9.dll` HLE audio engine with `IXAudio2`, `IXAudio2MasteringVoice`,
//! and `IXAudio2SourceVoice` COM vtables and PCM mixing capture.

use core::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use xsx_abi::com::{
    Guid, HResult, IUnknownVtbl, E_INVALIDARG, E_NOINTERFACE, E_POINTER, IID_IUNKNOWN,
    IID_IXAUDIO2, S_OK,
};
use xsx_abi::services::{
    IXAudio2MasteringVoiceVtbl, IXAudio2SourceVoiceVtbl, IXAudio2Vtbl, WaveFormatEx,
    XAudio2Buffer, XAudio2VoiceState,
};

use crate::registry::ModuleRegistry;
use crate::runtime::{current_runtime, GuestRuntime};

pub fn register(reg: &mut ModuleRegistry) {
    for dll in ["xaudio2_9.dll", "xaudio2_9redist.dll"] {
        reg.register_export(dll, "XAudio2Create", hle_xaudio2_create as *const () as usize);
        reg.register_ordinal(dll, 1, hle_xaudio2_create as *const () as usize);
    }
}

unsafe extern "win64" fn hle_xaudio2_create(
    pp_xaudio2: *mut *mut c_void,
    _flags: u32,
    _xaudio2_processor: u32,
) -> HResult {
    if pp_xaudio2.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_INVALIDARG;
    };
    rt.trace("xaudio2_9!XAudio2Create() -> S_OK");
    let obj = Box::new(ComXAudio2 {
        lp_vtbl: &XAUDIO2_VTBL,
        ref_count: AtomicU32::new(1),
        runtime: rt,
        running: AtomicBool::new(true),
    });
    *pp_xaudio2 = Box::into_raw(obj) as *mut c_void;
    S_OK
}

// ===========================================================================
// IXAudio2
// ===========================================================================

#[repr(C)]
struct ComXAudio2 {
    lp_vtbl: *const IXAudio2Vtbl,
    ref_count: AtomicU32,
    runtime: Arc<GuestRuntime>,
    running: AtomicBool,
}

static XAUDIO2_VTBL: IXAudio2Vtbl = IXAudio2Vtbl {
    base: IUnknownVtbl {
        query_interface: xa_qi,
        add_ref: xa_add_ref,
        release: xa_release,
    },
    create_mastering_voice: xa_create_mastering_voice,
    create_source_voice: xa_create_source_voice,
    start_engine: xa_start_engine,
    stop_engine: xa_stop_engine,
};

unsafe extern "win64" fn xa_qi(
    this: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == IID_IXAUDIO2 {
        xa_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        *ppv = ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn xa_add_ref(this: *mut c_void) -> u32 {
    let obj = &*(this as *const ComXAudio2);
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn xa_release(this: *mut c_void) -> u32 {
    let obj = &*(this as *const ComXAudio2);
    let prev = obj.ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComXAudio2));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn xa_create_mastering_voice(
    this: *mut c_void,
    pp_mastering_voice: *mut *mut c_void,
    input_channels: u32,
    input_sample_rate: u32,
    _flags: u32,
) -> HResult {
    if this.is_null() || pp_mastering_voice.is_null() {
        return E_INVALIDARG;
    }
    let xa = &*(this as *const ComXAudio2);
    let ch = if input_channels == 0 { 2 } else { input_channels };
    let sr = if input_sample_rate == 0 { 48_000 } else { input_sample_rate };
    xa.runtime.trace(format!(
        "IXAudio2::CreateMasteringVoice(channels={ch}, sample_rate={sr}Hz)"
    ));
    let obj = Box::new(ComMasteringVoice {
        lp_vtbl: &MASTERING_VTBL,
        ref_count: AtomicU32::new(1),
        _channels: ch,
        _sample_rate: sr,
    });
    *pp_mastering_voice = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn xa_create_source_voice(
    this: *mut c_void,
    pp_source_voice: *mut *mut c_void,
    p_source_format: *const WaveFormatEx,
    _flags: u32,
) -> HResult {
    if this.is_null() || pp_source_voice.is_null() || p_source_format.is_null() {
        return E_INVALIDARG;
    }
    let xa = &*(this as *const ComXAudio2);
    let fmt = *p_source_format;
    let ch = fmt.n_channels;
    let rate = fmt.n_samples_per_sec;
    let bits = fmt.w_bits_per_sample;
    xa.runtime.trace(format!(
        "IXAudio2::CreateSourceVoice(ch={ch}, rate={rate}Hz, bits={bits})"
    ));
    let obj = Box::new(ComSourceVoice {
        lp_vtbl: &SOURCE_VTBL,
        ref_count: AtomicU32::new(1),
        runtime: Arc::clone(&xa.runtime),
        format: fmt,
        playing: AtomicBool::new(false),
        buffers_queued: AtomicU32::new(0),
        samples_played: AtomicU64::new(0),
    });
    *pp_source_voice = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn xa_start_engine(this: *mut c_void) -> HResult {
    if !this.is_null() {
        let xa = &*(this as *const ComXAudio2);
        xa.running.store(true, Ordering::SeqCst);
    }
    S_OK
}

unsafe extern "win64" fn xa_stop_engine(this: *mut c_void) {
    if !this.is_null() {
        let xa = &*(this as *const ComXAudio2);
        xa.running.store(false, Ordering::SeqCst);
    }
}

// ===========================================================================
// IXAudio2MasteringVoice
// ===========================================================================

#[repr(C)]
struct ComMasteringVoice {
    lp_vtbl: *const IXAudio2MasteringVoiceVtbl,
    ref_count: AtomicU32,
    _channels: u32,
    _sample_rate: u32,
}

static MASTERING_VTBL: IXAudio2MasteringVoiceVtbl = IXAudio2MasteringVoiceVtbl {
    base: IUnknownVtbl {
        query_interface: mv_qi,
        add_ref: mv_add_ref,
        release: mv_release,
    },
    set_volume: mv_set_volume,
    destroy_voice: mv_destroy,
};

unsafe extern "win64" fn mv_qi(this: *mut c_void, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    if *riid == IID_IUNKNOWN {
        mv_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        *ppv = ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn mv_add_ref(this: *mut c_void) -> u32 {
    (*(this as *const ComMasteringVoice))
        .ref_count
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "win64" fn mv_release(this: *mut c_void) -> u32 {
    let prev = (*(this as *const ComMasteringVoice))
        .ref_count
        .fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComMasteringVoice));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn mv_set_volume(_this: *mut c_void, volume: f32) -> HResult {
    if let Some(rt) = current_runtime() {
        rt.set_master_volume(volume);
    }
    S_OK
}

unsafe extern "win64" fn mv_destroy(this: *mut c_void) {
    if !this.is_null() {
        mv_release(this);
    }
}

// ===========================================================================
// IXAudio2SourceVoice
// ===========================================================================

#[repr(C)]
struct ComSourceVoice {
    lp_vtbl: *const IXAudio2SourceVoiceVtbl,
    ref_count: AtomicU32,
    runtime: Arc<GuestRuntime>,
    format: WaveFormatEx,
    playing: AtomicBool,
    buffers_queued: AtomicU32,
    samples_played: AtomicU64,
}

static SOURCE_VTBL: IXAudio2SourceVoiceVtbl = IXAudio2SourceVoiceVtbl {
    base: IUnknownVtbl {
        query_interface: sv_qi,
        add_ref: sv_add_ref,
        release: sv_release,
    },
    start: sv_start,
    stop: sv_stop,
    submit_source_buffer: sv_submit_buffer,
    get_state: sv_get_state,
    destroy_voice: sv_destroy,
};

unsafe extern "win64" fn sv_qi(this: *mut c_void, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if ppv.is_null() || riid.is_null() {
        return E_POINTER;
    }
    if *riid == IID_IUNKNOWN {
        sv_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        *ppv = ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn sv_add_ref(this: *mut c_void) -> u32 {
    (*(this as *const ComSourceVoice))
        .ref_count
        .fetch_add(1, Ordering::Relaxed)
        + 1
}

unsafe extern "win64" fn sv_release(this: *mut c_void) -> u32 {
    let prev = (*(this as *const ComSourceVoice))
        .ref_count
        .fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComSourceVoice));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn sv_start(this: *mut c_void, _flags: u32) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let sv = &*(this as *const ComSourceVoice);
    sv.playing.store(true, Ordering::SeqCst);
    sv.runtime.trace("IXAudio2SourceVoice::Start()");
    S_OK
}

unsafe extern "win64" fn sv_stop(this: *mut c_void, _flags: u32) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let sv = &*(this as *const ComSourceVoice);
    sv.playing.store(false, Ordering::SeqCst);
    S_OK
}

unsafe extern "win64" fn sv_submit_buffer(
    this: *mut c_void,
    p_buffer: *const XAudio2Buffer,
) -> HResult {
    if this.is_null() || p_buffer.is_null() {
        return E_INVALIDARG;
    }
    let sv = &*(this as *const ComSourceVoice);
    let buf = &*p_buffer;
    if !buf.p_audio_data.is_null() && buf.audio_bytes > 0 {
        let pcm = std::slice::from_raw_parts(buf.p_audio_data, buf.audio_bytes as usize);
        let ch = sv.format.n_channels;
        let rate = sv.format.n_samples_per_sec;
        let mixed_frames = sv.runtime.mix_audio_pcm(pcm, ch, rate, 1.0);
        sv.buffers_queued.fetch_add(1, Ordering::Relaxed);
        sv.samples_played.fetch_add(mixed_frames, Ordering::Relaxed);
        sv.runtime.trace(format!(
            "IXAudio2SourceVoice::SubmitSourceBuffer(bytes={}, ch={ch}, rate={rate}Hz -> mixed_48k_frames={mixed_frames})",
            buf.audio_bytes
        ));
    }
    S_OK
}

unsafe extern "win64" fn sv_get_state(this: *mut c_void, p_state: *mut XAudio2VoiceState) {
    if this.is_null() || p_state.is_null() {
        return;
    }
    let sv = &*(this as *const ComSourceVoice);
    *p_state = XAudio2VoiceState {
        p_current_buffer_context: 0,
        buffers_queued: sv.buffers_queued.load(Ordering::Relaxed),
        _pad: 0,
        samples_played: sv.samples_played.load(Ordering::Relaxed),
    };
}

unsafe extern "win64" fn sv_destroy(this: *mut c_void) {
    if !this.is_null() {
        sv_release(this);
    }
}
