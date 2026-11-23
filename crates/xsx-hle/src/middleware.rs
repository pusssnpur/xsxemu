//! Xbox Series X Middleware Shims:
//! - `amd_fidelityfx_upscaler_xbox.dll` & `amd_fidelityfx_loader_xbox.dll` (wired directly
//!   into `xsx-d3d12`'s multithreaded AMD FSR EASU + RCAS upscaler!)
//! - `bink2w64_scarlett.dll` / `Bink2Scarlett.dll` (Bink 2 video stream lifecycle + header parser)
//! - `XboxBvhBuilder.dll` (software DXR BLAS/TLAS builder & ray tracer via `xsx-d3d12::bvh`)
//! - `pixevt.dll` / `pixrt.dll` (PIX profiling markers) & `chad.dll` / `xcrdapi.dll`

use core::ffi::{c_char, c_void};
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::File;
use std::io::Read;
use std::sync::{Mutex, OnceLock};

use xsx_abi::com::{HResult, E_NOTIMPL, S_OK};
use xsx_d3d12::bvh::{
    fnv1a_hash, BvhAccelerationStructure, RtDispatchParams, RtRay, RtTriangle, Vec3,
};
use xsx_d3d12::fsr::{process_frame, FsrConfig, FsrQualityMode};
use xsx_d3d12::PresentedFrame;

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

pub fn register(reg: &mut ModuleRegistry) {
    // AMD FidelityFX Upscaler & Loader Xbox DLLs
    for dll in [
        "amd_fidelityfx_upscaler_xbox.dll",
        "amd_fidelityfx_loader_xbox.dll",
    ] {
        reg.register_export(dll, "ffxUpscalerCreateContext", hle_ffx_upscaler_create_context as *const () as usize);
        reg.register_export(dll, "ffxUpscalerDestroyContext", hle_ffx_upscaler_destroy_context as *const () as usize);
        reg.register_export(dll, "ffxUpscalerConfigure", hle_ffx_upscaler_configure as *const () as usize);
        reg.register_export(dll, "ffxUpscalerQuery", hle_ffx_upscaler_query as *const () as usize);
        reg.register_export(dll, "ffxUpscalerDispatch", hle_ffx_upscaler_dispatch as *const () as usize);
        reg.register_export(dll, "ffxCreateContext", hle_ffx_create_context as *const () as usize);
        reg.register_export(dll, "ffxDestroyContext", hle_ffx_destroy_context as *const () as usize);
        reg.register_export(dll, "ffxConfigure", hle_ffx_upscaler_configure as *const () as usize);
        reg.register_export(dll, "ffxQuery", hle_ffx_upscaler_query as *const () as usize);
        reg.register_export(dll, "ffxDispatch", hle_ffx_upscaler_dispatch as *const () as usize);
    }

    // Bink 2 Scarlett video middleware
    for (name, addr) in [
        ("BinkOpen", hle_bink_open as *const () as usize),
        ("BinkClose", hle_bink_close as *const () as usize),
        ("BinkDoFrame", hle_bink_do_frame as *const () as usize),
        ("BinkNextFrame", hle_bink_next_frame as *const () as usize),
        ("BinkWait", hle_bink_wait as *const () as usize),
        ("BinkCopyToBuffer", hle_bink_copy_to_buffer as *const () as usize),
        ("BinkPause", hle_bink_pause as *const () as usize),
        ("BinkGoto", hle_bink_goto as *const () as usize),
        ("BinkSetSoundOnOff", hle_bink_set_sound_on_off as *const () as usize),
        ("BinkSetVolume", hle_bink_set_volume as *const () as usize),
        ("BinkGetSummary", hle_bink_get_summary as *const () as usize),
        ("BinkGetError", hle_bink_get_error as *const () as usize),
    ] {
        reg.register_export("bink2w64_scarlett.dll", name, addr);
    }

    // XboxBvhBuilder.dll
    for (name, addr) in [
        ("XboxBvhGetPrebuildInfo", hle_xbox_bvh_get_prebuild_info as *const () as usize),
        ("XboxBvhBuildBlas", hle_xbox_bvh_build_blas as *const () as usize),
        ("XboxBvhUpdateBlas", hle_xbox_bvh_update_blas as *const () as usize),
        ("XboxBvhBuildTlas", hle_xbox_bvh_build_tlas as *const () as usize),
        ("XboxBvhGetStats", hle_xbox_bvh_get_stats as *const () as usize),
        ("XboxBvhTraceSingleRay", hle_xbox_bvh_trace_single_ray as *const () as usize),
        ("XboxBvhDispatchRays", hle_xbox_bvh_dispatch_rays as *const () as usize),
        ("XboxBvhDestroy", hle_xbox_bvh_destroy as *const () as usize),
    ] {
        reg.register_export("xboxbvhbuilder.dll", name, addr);
    }

    // PIX event profiling no-ops
    for name in [
        "PIXBeginEvent",
        "PIXEndEvent",
        "PIXSetMarker",
        "PIXBeginEventOnCommandList",
        "PIXEndEventOnCommandList",
        "PIXSetMarkerOnCommandList",
        "PIXReportCounter",
        "PIXNotifyWakeFromFenceSignal",
    ] {
        reg.register_export("pixevt.dll", name, hle_pix_noop as *const () as usize);
    }

    // Optional console telemetry/hardware APIs
    reg.register_export("chad.dll", "ChadGetStatus", hle_notimpl_stub as *const () as usize);
    reg.register_export("xcrdapi.dll", "XCrdOpen", hle_notimpl_stub as *const () as usize);
}

unsafe extern "win64" fn hle_pix_noop() {}
unsafe extern "win64" fn hle_notimpl_stub() -> HResult {
    E_NOTIMPL
}

// ===========================================================================
// 1. AMD FidelityFX Upscaler Xbox (wired directly into xsx-d3d12 FSR EASU+RCAS)
// ===========================================================================

pub const FFX_API_RETURN_OK: u32 = 0;
pub const FFX_API_RETURN_ERROR_UNKNOWN_DESCTYPE: u32 = 2;
pub const FFX_API_RETURN_ERROR_PARAMETER: u32 = 6;

pub const FFX_API_DESC_TYPE_GET_PROVIDER_VERSION: u64 = 0x0000_0001;
pub const FFX_API_DESC_TYPE_UPSCALE_CREATE: u64 = 0x0001_0000;
pub const FFX_API_DESC_TYPE_UPSCALE_DISPATCH: u64 = 0x0001_0001;
pub const FFX_API_DESC_TYPE_UPSCALE_GET_UPSCALERATIO: u64 = 0x0001_0002;
pub const FFX_API_DESC_TYPE_UPSCALE_GET_RENDERRESOLUTION: u64 = 0x0001_0003;
pub const FFX_API_DESC_TYPE_UPSCALE_GET_JITTER_PHASE_COUNT: u64 = 0x0001_0004;
pub const FFX_API_DESC_TYPE_UPSCALE_GET_JITTER_OFFSET: u64 = 0x0001_0005;
pub const FFX_API_DESC_TYPE_UPSCALE_CONFIGURE: u64 = 0x0001_0006;

static FFX_VERSION_STR: &[u8] = b"xsxemu FidelityFX FSR 3.1.2-EASU+RCAS\0";

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxApiHeader {
    pub desc_type: u64,
    pub p_next: *mut FfxApiHeader,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxCreateContextDescUpscale {
    pub header: FfxApiHeader,
    pub flags: u32,
    pub max_render_width: u32,
    pub max_render_height: u32,
    pub max_upscale_width: u32,
    pub max_upscale_height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxDispatchDescUpscale {
    pub header: FfxApiHeader,
    pub color_rgba8: *const u8,
    pub output_rgba8: *mut u8,
    pub render_width: u32,
    pub render_height: u32,
    pub upscale_width: u32,
    pub upscale_height: u32,
    pub jitter_offset_x: f32,
    pub jitter_offset_y: f32,
    pub sharpness: f32,
    pub reset: u32,
    pub out_frame_hash: *mut u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxQueryDescUpscaleRatio {
    pub header: FfxApiHeader,
    pub quality_mode: u32,
    pub p_out_upscale_ratio: *mut f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxQueryDescRenderResolution {
    pub header: FfxApiHeader,
    pub display_width: u32,
    pub display_height: u32,
    pub quality_mode: u32,
    pub p_out_render_width: *mut u32,
    pub p_out_render_height: *mut u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxQueryDescJitterPhaseCount {
    pub header: FfxApiHeader,
    pub render_width: i32,
    pub display_width: i32,
    pub p_out_phase_count: *mut i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxQueryDescJitterOffset {
    pub header: FfxApiHeader,
    pub index: i32,
    pub phase_count: i32,
    pub p_out_x: *mut f32,
    pub p_out_y: *mut f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxConfigureDescUpscale {
    pub header: FfxApiHeader,
    pub sharpness: f32,
    pub enable_sharpening: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FfxQueryDescProviderVersion {
    pub header: FfxApiHeader,
    pub version_id: u64,
    pub version_name: *const c_char,
}

struct FfxContextState {
    sharpness: f32,
    enable_sharpening: bool,
    dispatch_count: u64,
}

fn ffx_contexts() -> &'static Mutex<HashMap<usize, FfxContextState>> {
    static C: OnceLock<Mutex<HashMap<usize, FfxContextState>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn quality_mode_ratio(mode: u32) -> f32 {
    match mode {
        0 => 1.0, // NativeAA
        1 => 1.5, // Quality (default)
        2 => 1.7, // Balanced
        3 => 2.0, // Performance
        4 => 3.0, // UltraPerformance
        _ => 1.5,
    }
}

fn halton_seq(mut index: i32, base: i32) -> f32 {
    let mut f = 1.0f32;
    let mut r = 0.0f32;
    let b = base as f32;
    while index > 0 {
        f /= b;
        r += f * (index % base) as f32;
        index /= base;
    }
    r
}

pub unsafe extern "win64" fn hle_ffx_create_context(
    context: *mut *mut c_void,
    desc: *mut FfxApiHeader,
    _mem_cb: *const c_void,
) -> u32 {
    hle_ffx_upscaler_create_context(context, desc)
}

pub unsafe extern "win64" fn hle_ffx_destroy_context(
    context: *mut *mut c_void,
    _mem_cb: *const c_void,
) -> u32 {
    hle_ffx_upscaler_destroy_context(context)
}

pub unsafe extern "win64" fn hle_ffx_upscaler_create_context(
    context: *mut *mut c_void,
    desc: *mut FfxApiHeader,
) -> u32 {
    if context.is_null() || desc.is_null() || (*desc).desc_type != FFX_API_DESC_TYPE_UPSCALE_CREATE {
        return FFX_API_RETURN_ERROR_PARAMETER;
    }
    let cd = &*(desc as *const FfxCreateContextDescUpscale);
    if cd.max_render_width == 0
        || cd.max_render_height == 0
        || cd.max_upscale_width == 0
        || cd.max_upscale_height == 0
    {
        return FFX_API_RETURN_ERROR_PARAMETER;
    }
    let mut map = ffx_contexts().lock().unwrap();
    let handle = (map.len() + 1) * 0x100 + 0xFF00_0000;
    map.insert(
        handle,
        FfxContextState {
            sharpness: 0.8,
            enable_sharpening: true,
            dispatch_count: 0,
        },
    );
    *context = handle as *mut c_void;
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "ffx_upscaler!CreateContext({}x{} -> {}x{}) -> OK",
            cd.max_render_width, cd.max_render_height, cd.max_upscale_width, cd.max_upscale_height
        ));
    }
    FFX_API_RETURN_OK
}

pub unsafe extern "win64" fn hle_ffx_upscaler_destroy_context(context: *mut *mut c_void) -> u32 {
    if context.is_null() || (*context).is_null() {
        return FFX_API_RETURN_ERROR_PARAMETER;
    }
    let handle = *context as usize;
    *context = core::ptr::null_mut();
    let removed = ffx_contexts().lock().unwrap().remove(&handle).is_some();
    if removed {
        FFX_API_RETURN_OK
    } else {
        FFX_API_RETURN_ERROR_PARAMETER
    }
}

pub unsafe extern "win64" fn hle_ffx_upscaler_configure(
    context: *mut *mut c_void,
    desc: *const FfxApiHeader,
) -> u32 {
    if context.is_null() || (*context).is_null() || desc.is_null() {
        return FFX_API_RETURN_ERROR_PARAMETER;
    }
    let handle = *context as usize;
    let mut map = ffx_contexts().lock().unwrap();
    let Some(st) = map.get_mut(&handle) else {
        return FFX_API_RETURN_ERROR_PARAMETER;
    };
    if (*desc).desc_type == FFX_API_DESC_TYPE_UPSCALE_CONFIGURE {
        let cfg = &*(desc as *const FfxConfigureDescUpscale);
        st.sharpness = cfg.sharpness.clamp(0.0, 1.0);
        st.enable_sharpening = cfg.enable_sharpening != 0;
        FFX_API_RETURN_OK
    } else {
        FFX_API_RETURN_ERROR_UNKNOWN_DESCTYPE
    }
}

pub unsafe extern "win64" fn hle_ffx_upscaler_query(
    _context: *mut *mut c_void,
    desc: *mut FfxApiHeader,
) -> u32 {
    if desc.is_null() {
        return FFX_API_RETURN_ERROR_PARAMETER;
    }
    match (*desc).desc_type {
        FFX_API_DESC_TYPE_GET_PROVIDER_VERSION => {
            let q = &mut *(desc as *mut FfxQueryDescProviderVersion);
            q.version_id = 0x0003_0102;
            q.version_name = FFX_VERSION_STR.as_ptr() as *const c_char;
            FFX_API_RETURN_OK
        }
        FFX_API_DESC_TYPE_UPSCALE_GET_UPSCALERATIO => {
            let q = &mut *(desc as *mut FfxQueryDescUpscaleRatio);
            if q.p_out_upscale_ratio.is_null() {
                return FFX_API_RETURN_ERROR_PARAMETER;
            }
            *q.p_out_upscale_ratio = quality_mode_ratio(q.quality_mode);
            FFX_API_RETURN_OK
        }
        FFX_API_DESC_TYPE_UPSCALE_GET_RENDERRESOLUTION => {
            let q = &mut *(desc as *mut FfxQueryDescRenderResolution);
            if q.p_out_render_width.is_null()
                || q.p_out_render_height.is_null()
                || q.display_width == 0
                || q.display_height == 0
            {
                return FFX_API_RETURN_ERROR_PARAMETER;
            }
            let ratio = quality_mode_ratio(q.quality_mode);
            *q.p_out_render_width = ((q.display_width as f32 / ratio).round() as u32).max(1);
            *q.p_out_render_height = ((q.display_height as f32 / ratio).round() as u32).max(1);
            FFX_API_RETURN_OK
        }
        FFX_API_DESC_TYPE_UPSCALE_GET_JITTER_PHASE_COUNT => {
            let q = &mut *(desc as *mut FfxQueryDescJitterPhaseCount);
            if q.p_out_phase_count.is_null() || q.render_width <= 0 || q.display_width <= 0 {
                return FFX_API_RETURN_ERROR_PARAMETER;
            }
            let ratio = (q.display_width as f32) / (q.render_width as f32);
            *q.p_out_phase_count = (8.0 * ratio * ratio).ceil() as i32;
            FFX_API_RETURN_OK
        }
        FFX_API_DESC_TYPE_UPSCALE_GET_JITTER_OFFSET => {
            let q = &mut *(desc as *mut FfxQueryDescJitterOffset);
            if q.p_out_x.is_null() || q.p_out_y.is_null() || q.phase_count <= 0 {
                return FFX_API_RETURN_ERROR_PARAMETER;
            }
            let idx = (q.index.rem_euclid(q.phase_count)) + 1;
            *q.p_out_x = halton_seq(idx, 2) - 0.5;
            *q.p_out_y = halton_seq(idx, 3) - 0.5;
            FFX_API_RETURN_OK
        }
        _ => FFX_API_RETURN_ERROR_UNKNOWN_DESCTYPE,
    }
}

pub unsafe extern "win64" fn hle_ffx_upscaler_dispatch(
    context: *mut *mut c_void,
    desc: *const FfxApiHeader,
) -> u32 {
    if context.is_null()
        || (*context).is_null()
        || desc.is_null()
        || (*desc).desc_type != FFX_API_DESC_TYPE_UPSCALE_DISPATCH
    {
        return FFX_API_RETURN_ERROR_PARAMETER;
    }
    let d = &*(desc as *const FfxDispatchDescUpscale);
    if d.color_rgba8.is_null()
        || d.output_rgba8.is_null()
        || d.render_width == 0
        || d.render_height == 0
        || d.upscale_width == 0
        || d.upscale_height == 0
    {
        return FFX_API_RETURN_ERROR_PARAMETER;
    }

    let handle = *context as usize;
    let (sharpness, enable_rcas, frame_idx) = {
        let mut map = ffx_contexts().lock().unwrap();
        let Some(st) = map.get_mut(&handle) else {
            return FFX_API_RETURN_ERROR_PARAMETER;
        };
        st.dispatch_count += 1;
        let s = if d.sharpness > 0.0 {
            d.sharpness.clamp(0.0, 1.0)
        } else {
            st.sharpness
        };
        (s, st.enable_sharpening, st.dispatch_count)
    };

    // Execute our real multithreaded AMD FSR EASU + RCAS pipeline from xsx-d3d12!
    let in_bytes = (d.render_width as usize) * (d.render_height as usize) * 4;
    let src_slice = core::slice::from_raw_parts(d.color_rgba8, in_bytes);
    let input_frame = PresentedFrame {
        frame_index: frame_idx,
        width: d.render_width,
        height: d.render_height,
        rgba: src_slice.to_vec(),
    };
    let fsr_cfg = FsrConfig {
        mode: FsrQualityMode::Quality,
        sharpness,
        enable_rcas,
        output_resolution: Some((d.upscale_width, d.upscale_height)),
    };
    let (upscaled, stats) = process_frame(&input_frame, &fsr_cfg);
    let out_bytes = (d.upscale_width as usize) * (d.upscale_height as usize) * 4;
    let copy_len = out_bytes.min(upscaled.rgba.len());
    core::ptr::copy_nonoverlapping(upscaled.rgba.as_ptr(), d.output_rgba8, copy_len);

    if !d.out_frame_hash.is_null() {
        *d.out_frame_hash = fnv1a_hash(&upscaled.rgba[..copy_len]);
    }
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "ffx_upscaler!Dispatch({}x{} -> {}x{}, easu={}, rcas={}, {}us)",
            stats.input_width,
            stats.input_height,
            stats.output_width,
            stats.output_height,
            stats.easu_ran,
            stats.rcas_ran,
            stats.duration_us
        ));
    }
    FFX_API_RETURN_OK
}

// ===========================================================================
// 2. Bink 2 Scarlett (`bink2w64_scarlett.dll`)
// ===========================================================================

pub const BINKSURFACE32BGRA: u32 = 3;
pub const BINKSURFACE32RGBA: u32 = 6;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BinkHandle {
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub frame_num: u32,
    pub last_frame_num: u32,
    pub frame_rate: u32,
    pub frame_rate_div: u32,
    pub read_error: u32,
    pub open_flags: u32,
    pub paused: u32,
    pub sound_on: u32,
    pub volume: i32,
    pub internal_id: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BinkSummary {
    pub width: u32,
    pub height: u32,
    pub total_time: u32,
    pub file_frame_rate: u32,
    pub file_frame_rate_div: u32,
    pub frame_rate: u32,
    pub frame_rate_div: u32,
    pub total_open_time: u32,
    pub total_frames: u32,
    pub total_played_frames: u32,
    pub skipped_frames: u32,
}

struct BinkStream {
    pub_ptr: usize,
    played_frames: u32,
    frame_rgba: Vec<u8>,
}

struct BinkGlobal {
    next_id: u64,
    streams: HashMap<u64, BinkStream>,
}

fn bink_global() -> &'static Mutex<BinkGlobal> {
    static B: OnceLock<Mutex<BinkGlobal>> = OnceLock::new();
    B.get_or_init(|| {
        Mutex::new(BinkGlobal {
            next_id: 1,
            streams: HashMap::new(),
        })
    })
}

static BINK_EMPTY_ERR: &[u8] = b"\0";

fn synth_bink_frame(w: u32, h: u32, frame: u32, buf: &mut Vec<u8>) {
    buf.resize((w as usize) * (h as usize) * 4, 0);
    for y in 0..h {
        for x in 0..w {
            let idx = ((y as usize) * (w as usize) + (x as usize)) * 4;
            buf[idx] = ((x.wrapping_mul(4).wrapping_add(frame.wrapping_mul(17))) & 0xFF) as u8;
            buf[idx + 1] = ((y.wrapping_mul(4).wrapping_add(frame.wrapping_mul(9))) & 0xFF) as u8;
            buf[idx + 2] = (((x ^ y).wrapping_add(frame.wrapping_mul(23))) & 0xFF) as u8;
            buf[idx + 3] = 255;
        }
    }
}

pub unsafe extern "win64" fn hle_bink_open(name: *const c_char, flags: u32) -> *mut BinkHandle {
    if name.is_null() {
        return core::ptr::null_mut();
    }
    let path = CStr::from_ptr(name).to_string_lossy().into_owned();
    if path.is_empty() {
        return core::ptr::null_mut();
    }
    let mut handle = Box::new(BinkHandle {
        width: 320,
        height: 180,
        frames: 60,
        frame_num: 1,
        last_frame_num: 0,
        frame_rate: 30,
        frame_rate_div: 1,
        read_error: 0,
        open_flags: flags,
        paused: 0,
        sound_on: 1,
        volume: 32768,
        internal_id: 0,
    });

    // Parse real BIK / KB2 header if the file exists on disk
    if let Ok(mut f) = File::open(&path) {
        let mut hdr = [0u8; 44];
        if let Ok(n) = f.read(&mut hdr) {
            if n >= 40 && (&hdr[0..3] == b"BIK" || &hdr[0..3] == b"KB2") {
                let rd = |o: usize| u32::from_le_bytes(hdr[o..o + 4].try_into().unwrap());
                let frames = rd(8);
                let width = rd(20);
                let height = rd(24);
                let fps = rd(28);
                let fps_div = rd(32);
                if (1..=7680).contains(&width) && (1..=4320).contains(&height) {
                    handle.width = width;
                    handle.height = height;
                }
                if (1..=1_000_000).contains(&frames) {
                    handle.frames = frames;
                }
                if fps > 0 && fps_div > 0 {
                    handle.frame_rate = fps;
                    handle.frame_rate_div = fps_div;
                }
            }
        }
    }

    let mut g = bink_global().lock().unwrap();
    let id = g.next_id;
    g.next_id += 1;
    handle.internal_id = id;
    let mut frame_rgba = Vec::new();
    synth_bink_frame(handle.width, handle.height, handle.frame_num, &mut frame_rgba);
    let raw = Box::into_raw(handle);
    g.streams.insert(
        id,
        BinkStream {
            pub_ptr: raw as usize,
            played_frames: 0,
            frame_rgba,
        },
    );
    raw
}

pub unsafe extern "win64" fn hle_bink_close(bnk: *mut BinkHandle) {
    if bnk.is_null() {
        return;
    }
    let id = (*bnk).internal_id;
    let removed = bink_global().lock().unwrap().streams.remove(&id);
    if let Some(s) = removed {
        drop(Box::from_raw(s.pub_ptr as *mut BinkHandle));
    }
}

pub unsafe extern "win64" fn hle_bink_do_frame(bnk: *mut BinkHandle) -> i32 {
    if bnk.is_null() {
        return -1;
    }
    let mut g = bink_global().lock().unwrap();
    let Some(s) = g.streams.get_mut(&(*bnk).internal_id) else {
        return -1;
    };
    if (*bnk).paused == 0 {
        synth_bink_frame((*bnk).width, (*bnk).height, (*bnk).frame_num, &mut s.frame_rgba);
        (*bnk).last_frame_num = (*bnk).frame_num;
        s.played_frames += 1;
    }
    0
}

pub unsafe extern "win64" fn hle_bink_next_frame(bnk: *mut BinkHandle) {
    if bnk.is_null() {
        return;
    }
    if (*bnk).paused == 0 {
        (*bnk).frame_num = ((*bnk).frame_num % (*bnk).frames.max(1)) + 1;
    }
}

pub unsafe extern "win64" fn hle_bink_wait(bnk: *mut BinkHandle) -> i32 {
    if bnk.is_null() {
        return 0;
    }
    i32::from((*bnk).paused != 0)
}

pub unsafe extern "win64" fn hle_bink_copy_to_buffer(
    bnk: *mut BinkHandle,
    dest: *mut u8,
    dest_pitch: i32,
    dest_height: u32,
    dest_x: u32,
    dest_y: u32,
    flags: u32,
) -> i32 {
    if bnk.is_null() || dest.is_null() || dest_pitch <= 0 || dest_height == 0 {
        return -1;
    }
    let g = bink_global().lock().unwrap();
    let Some(s) = g.streams.get(&(*bnk).internal_id) else {
        return -1;
    };
    let is_bgra = (flags & 0xF) == BINKSURFACE32BGRA;
    let w = (*bnk).width as usize;
    let h = (*bnk).height.min(dest_height.saturating_sub(dest_y)) as usize;
    for y in 0..h {
        let dst_row = dest.add((dest_y as usize + y) * (dest_pitch as usize) + (dest_x as usize) * 4);
        let src_row = &s.frame_rgba[y * w * 4..(y + 1) * w * 4];
        if !is_bgra {
            core::ptr::copy_nonoverlapping(src_row.as_ptr(), dst_row, w * 4);
        } else {
            for x in 0..w {
                *dst_row.add(x * 4) = src_row[x * 4 + 2];
                *dst_row.add(x * 4 + 1) = src_row[x * 4 + 1];
                *dst_row.add(x * 4 + 2) = src_row[x * 4];
                *dst_row.add(x * 4 + 3) = src_row[x * 4 + 3];
            }
        }
    }
    0
}

pub unsafe extern "win64" fn hle_bink_pause(bnk: *mut BinkHandle, pause: i32) -> i32 {
    if bnk.is_null() {
        return 0;
    }
    (*bnk).paused = u32::from(pause != 0);
    (*bnk).paused as i32
}

pub unsafe extern "win64" fn hle_bink_goto(bnk: *mut BinkHandle, frame: u32, _flags: i32) {
    if bnk.is_null() {
        return;
    }
    (*bnk).frame_num = frame.clamp(1, (*bnk).frames.max(1));
}

pub unsafe extern "win64" fn hle_bink_set_sound_on_off(bnk: *mut BinkHandle, onoff: i32) {
    if !bnk.is_null() {
        (*bnk).sound_on = u32::from(onoff != 0);
    }
}

pub unsafe extern "win64" fn hle_bink_set_volume(
    bnk: *mut BinkHandle,
    _track_id: u32,
    volume: i32,
) {
    if !bnk.is_null() {
        (*bnk).volume = volume.clamp(0, 65536);
    }
}

pub unsafe extern "win64" fn hle_bink_get_summary(bnk: *mut BinkHandle, sum: *mut BinkSummary) {
    if bnk.is_null() || sum.is_null() {
        return;
    }
    let g = bink_global().lock().unwrap();
    let played = g
        .streams
        .get(&(*bnk).internal_id)
        .map(|s| s.played_frames)
        .unwrap_or(0);
    *sum = BinkSummary {
        width: (*bnk).width,
        height: (*bnk).height,
        total_time: ((*bnk).frames * 1000 * (*bnk).frame_rate_div.max(1)) / (*bnk).frame_rate.max(1),
        file_frame_rate: (*bnk).frame_rate,
        file_frame_rate_div: (*bnk).frame_rate_div,
        frame_rate: (*bnk).frame_rate,
        frame_rate_div: (*bnk).frame_rate_div,
        total_open_time: 1,
        total_frames: (*bnk).frames,
        total_played_frames: played,
        skipped_frames: 0,
    };
}

pub unsafe extern "win64" fn hle_bink_get_error() -> *const c_char {
    BINK_EMPTY_ERR.as_ptr() as *const c_char
}

// ===========================================================================
// 3. XboxBvhBuilder.dll (Software DXR BLAS / TLAS / Ray Query)
// ===========================================================================

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XboxRtPrebuildInfo {
    pub result_data_max_size_bytes: u64,
    pub scratch_data_size_bytes: u64,
    pub update_scratch_data_size_bytes: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XboxBvhTriangle {
    pub v0: [f32; 3],
    pub v1: [f32; 3],
    pub v2: [f32; 3],
    pub color: [f32; 3],
    pub reflectivity: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XboxBvhBuildDesc {
    pub triangles: *const XboxBvhTriangle,
    pub triangle_count: u32,
    pub build_flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XboxBvhStats {
    pub node_count: u32,
    pub triangle_count: u32,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XboxRayDesc {
    pub origin: [f32; 3],
    pub t_min: f32,
    pub direction: [f32; 3],
    pub t_max: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XboxRayHit {
    pub hit: u32,
    pub triangle_index: u32,
    pub t: f32,
    pub normal: [f32; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XboxRtDispatchDesc {
    pub width: u32,
    pub height: u32,
    pub camera_pos: [f32; 3],
    pub camera_look_at: [f32; 3],
    pub light_pos: [f32; 3],
    pub max_bounces: u32,
}

struct BvhGlobal {
    next_handle: u64,
    blas: HashMap<u64, BvhAccelerationStructure>,
    tlas: HashMap<u64, BvhAccelerationStructure>,
}

fn bvh_global() -> &'static Mutex<BvhGlobal> {
    static B: OnceLock<Mutex<BvhGlobal>> = OnceLock::new();
    B.get_or_init(|| {
        Mutex::new(BvhGlobal {
            next_handle: 0xB700_0001,
            blas: HashMap::new(),
            tlas: HashMap::new(),
        })
    })
}

fn convert_tris(slice: &[XboxBvhTriangle]) -> Vec<RtTriangle> {
    slice
        .iter()
        .map(|t| RtTriangle {
            v0: Vec3::new(t.v0[0], t.v0[1], t.v0[2]),
            v1: Vec3::new(t.v1[0], t.v1[1], t.v1[2]),
            v2: Vec3::new(t.v2[0], t.v2[1], t.v2[2]),
            color: Vec3::new(t.color[0], t.color[1], t.color[2]),
            reflectivity: t.reflectivity,
        })
        .collect()
}

pub unsafe extern "win64" fn hle_xbox_bvh_get_prebuild_info(
    triangle_count: u32,
    build_flags: u32,
    out_info: *mut XboxRtPrebuildInfo,
) -> i32 {
    if out_info.is_null() || triangle_count == 0 {
        return -1;
    }
    let align256 = |v: u64| (v + 255) & !255;
    let n = triangle_count as u64;
    *out_info = XboxRtPrebuildInfo {
        result_data_max_size_bytes: align256(n * 192 + 256),
        scratch_data_size_bytes: align256(n * 128 + 256),
        update_scratch_data_size_bytes: if (build_flags & 1) != 0 {
            align256(n * 64 + 256)
        } else {
            0
        },
    };
    S_OK
}

pub unsafe extern "win64" fn hle_xbox_bvh_build_blas(
    desc: *const XboxBvhBuildDesc,
    out_handle: *mut u64,
) -> i32 {
    if desc.is_null() || (*desc).triangles.is_null() || (*desc).triangle_count == 0 || out_handle.is_null() {
        return -1;
    }
    let slice = core::slice::from_raw_parts((*desc).triangles, (*desc).triangle_count as usize);
    let Ok(bvh) = BvhAccelerationStructure::build(convert_tris(slice)) else {
        return -1;
    };
    let mut g = bvh_global().lock().unwrap();
    let h = g.next_handle;
    g.next_handle += 1;
    g.blas.insert(h, bvh);
    *out_handle = h;
    S_OK
}

pub unsafe extern "win64" fn hle_xbox_bvh_update_blas(
    handle: u64,
    desc: *const XboxBvhBuildDesc,
) -> i32 {
    if desc.is_null() || (*desc).triangles.is_null() || (*desc).triangle_count == 0 {
        return -1;
    }
    let slice = core::slice::from_raw_parts((*desc).triangles, (*desc).triangle_count as usize);
    let Ok(bvh) = BvhAccelerationStructure::build(convert_tris(slice)) else {
        return -1;
    };
    let mut g = bvh_global().lock().unwrap();
    if let Some(entry) = g.blas.get_mut(&handle) {
        *entry = bvh;
        S_OK
    } else {
        -2
    }
}

pub unsafe extern "win64" fn hle_xbox_bvh_build_tlas(
    blas_handles: *const u64,
    blas_count: u32,
    out_handle: *mut u64,
) -> i32 {
    if blas_handles.is_null() || blas_count == 0 || out_handle.is_null() {
        return -1;
    }
    let handles = core::slice::from_raw_parts(blas_handles, blas_count as usize);
    let mut combined = Vec::new();
    {
        let g = bvh_global().lock().unwrap();
        for &h in handles {
            let Some(b) = g.blas.get(&h) else {
                return -2;
            };
            combined.extend_from_slice(b.triangles());
        }
    }
    let Ok(tlas) = BvhAccelerationStructure::build(combined) else {
        return -1;
    };
    let mut g = bvh_global().lock().unwrap();
    let h = g.next_handle;
    g.next_handle += 1;
    g.tlas.insert(h, tlas);
    *out_handle = h;
    S_OK
}

pub unsafe extern "win64" fn hle_xbox_bvh_get_stats(
    handle: u64,
    out_stats: *mut XboxBvhStats,
) -> i32 {
    if out_stats.is_null() {
        return -1;
    }
    let g = bvh_global().lock().unwrap();
    let Some(bvh) = g.tlas.get(&handle).or_else(|| g.blas.get(&handle)) else {
        return -2;
    };
    let b = bvh.root_bounds();
    *out_stats = XboxBvhStats {
        node_count: bvh.node_count(),
        triangle_count: bvh.triangle_count(),
        bounds_min: [b.min.x, b.min.y, b.min.z],
        bounds_max: [b.max.x, b.max.y, b.max.z],
    };
    S_OK
}

pub unsafe extern "win64" fn hle_xbox_bvh_trace_single_ray(
    tlas_handle: u64,
    ray: *const XboxRayDesc,
    out_hit: *mut XboxRayHit,
) -> i32 {
    if ray.is_null() || out_hit.is_null() {
        return -1;
    }
    *out_hit = XboxRayHit::default();
    let g = bvh_global().lock().unwrap();
    let Some(tlas) = g.tlas.get(&tlas_handle) else {
        return -2;
    };
    let r = RtRay {
        origin: Vec3::new((*ray).origin[0], (*ray).origin[1], (*ray).origin[2]),
        direction: Vec3::new((*ray).direction[0], (*ray).direction[1], (*ray).direction[2]),
        t_min: (*ray).t_min,
        t_max: (*ray).t_max,
    };
    if let Some(hit) = tlas.trace_ray(&r) {
        *out_hit = XboxRayHit {
            hit: 1,
            triangle_index: hit.triangle_index,
            t: hit.t,
            normal: [hit.normal.x, hit.normal.y, hit.normal.z],
        };
    }
    S_OK
}

pub unsafe extern "win64" fn hle_xbox_bvh_dispatch_rays(
    tlas_handle: u64,
    desc: *const XboxRtDispatchDesc,
    _optional_ppm_path: *const c_char,
    out_frame_hash: *mut u64,
) -> i32 {
    if desc.is_null() {
        return -1;
    }
    let g = bvh_global().lock().unwrap();
    let Some(tlas) = g.tlas.get(&tlas_handle) else {
        return -2;
    };
    let d = &*desc;
    let params = RtDispatchParams {
        width: d.width,
        height: d.height,
        camera_pos: Vec3::new(d.camera_pos[0], d.camera_pos[1], d.camera_pos[2]),
        camera_look_at: Vec3::new(d.camera_look_at[0], d.camera_look_at[1], d.camera_look_at[2]),
        light_pos: Vec3::new(d.light_pos[0], d.light_pos[1], d.light_pos[2]),
        max_bounces: d.max_bounces,
    };
    let Ok((_rgba, hash)) = tlas.render_scene(&params) else {
        return -1;
    };
    if !out_frame_hash.is_null() {
        *out_frame_hash = hash;
    }
    S_OK
}

pub unsafe extern "win64" fn hle_xbox_bvh_destroy(handle: u64) -> i32 {
    let mut g = bvh_global().lock().unwrap();
    if g.blas.remove(&handle).is_some() || g.tlas.remove(&handle).is_some() {
        S_OK
    } else {
        -2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffx_upscaler_dispatches_real_fsr_easu_rcas() {
        unsafe {
            let mut ctx: *mut c_void = core::ptr::null_mut();
            let mut create = FfxCreateContextDescUpscale {
                header: FfxApiHeader {
                    desc_type: FFX_API_DESC_TYPE_UPSCALE_CREATE,
                    p_next: core::ptr::null_mut(),
                },
                flags: 0,
                max_render_width: 32,
                max_render_height: 32,
                max_upscale_width: 48,
                max_upscale_height: 48,
            };
            assert_eq!(
                hle_ffx_upscaler_create_context(&mut ctx, &mut create.header),
                FFX_API_RETURN_OK
            );

            // Query render resolution for 1080p Quality (1.5x -> 1280x720)
            let mut rw = 0u32;
            let mut rh = 0u32;
            let mut qres = FfxQueryDescRenderResolution {
                header: FfxApiHeader {
                    desc_type: FFX_API_DESC_TYPE_UPSCALE_GET_RENDERRESOLUTION,
                    p_next: core::ptr::null_mut(),
                },
                display_width: 1920,
                display_height: 1080,
                quality_mode: 1,
                p_out_render_width: &mut rw,
                p_out_render_height: &mut rh,
            };
            assert_eq!(hle_ffx_upscaler_query(&mut ctx, &mut qres.header), FFX_API_RETURN_OK);
            assert_eq!((rw, rh), (1280, 720));

            let input = vec![128u8; 32 * 32 * 4];
            let mut output = vec![0u8; 48 * 48 * 4];
            let mut hash = 0u64;
            let disp = FfxDispatchDescUpscale {
                header: FfxApiHeader {
                    desc_type: FFX_API_DESC_TYPE_UPSCALE_DISPATCH,
                    p_next: core::ptr::null_mut(),
                },
                color_rgba8: input.as_ptr(),
                output_rgba8: output.as_mut_ptr(),
                render_width: 32,
                render_height: 32,
                upscale_width: 48,
                upscale_height: 48,
                jitter_offset_x: 0.0,
                jitter_offset_y: 0.0,
                sharpness: 0.8,
                reset: 1,
                out_frame_hash: &mut hash,
            };
            assert_eq!(hle_ffx_upscaler_dispatch(&mut ctx, &disp.header), FFX_API_RETURN_OK);
            assert_ne!(hash, 0);
            assert_eq!(output[3], 255);
            assert_eq!(hle_ffx_upscaler_destroy_context(&mut ctx), FFX_API_RETURN_OK);
        }
    }

    #[test]
    fn bink2_and_xbox_bvh_builder_lifecycle() {
        unsafe {
            let bnk = hle_bink_open(b"intro.bk2\0".as_ptr() as *const c_char, 0);
            assert!(!bnk.is_null());
            assert_eq!(hle_bink_do_frame(bnk), 0);
            hle_bink_next_frame(bnk);
            let mut sum = BinkSummary::default();
            hle_bink_get_summary(bnk, &mut sum);
            assert_eq!(sum.total_played_frames, 1);
            hle_bink_close(bnk);

            let tri = XboxBvhTriangle {
                v0: [-1.0, -1.0, 2.0],
                v1: [1.0, -1.0, 2.0],
                v2: [0.0, 1.0, 2.0],
                color: [1.0, 0.5, 0.25],
                reflectivity: 0.1,
            };
            let desc = XboxBvhBuildDesc {
                triangles: &tri,
                triangle_count: 1,
                build_flags: 1,
            };
            let mut blas = 0u64;
            let mut tlas = 0u64;
            assert_eq!(hle_xbox_bvh_build_blas(&desc, &mut blas), S_OK);
            assert_eq!(hle_xbox_bvh_build_tlas(&blas, 1, &mut tlas), S_OK);
            let ray = XboxRayDesc {
                origin: [0.0, 0.0, 0.0],
                t_min: 0.001,
                direction: [0.0, 0.0, 1.0],
                t_max: 50.0,
            };
            let mut hit = XboxRayHit::default();
            assert_eq!(hle_xbox_bvh_trace_single_ray(tlas, &ray, &mut hit), S_OK);
            assert_eq!(hit.hit, 1);
            assert!((hit.t - 2.0).abs() < 1e-4);
            assert_eq!(hle_xbox_bvh_destroy(tlas), S_OK);
            assert_eq!(hle_xbox_bvh_destroy(blas), S_OK);
        }
    }
}
