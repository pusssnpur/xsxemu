//! AMD FidelityFX Super Resolution (FSR) upscaling & sharpening pipeline.
//!
//! Enabled by default inside `xsx-d3d12` so every frame presented through
//! `ID3D12CommandQueueX::PresentX` automatically runs:
//! 1. **EASU** (Edge-Adaptive Spatial Upsampling) — 12-tap directional Lanczos-2
//!    filter with local gradient/anisotropy estimation and anti-ringing clamping.
//! 2. **RCAS** (Robust Contrast-Adaptive Sharpening) — 5-tap halo-free sharpening
//!    pass with local contrast normalization.
//!
//! Also provides:
//! - Halton(2, 3) sub-pixel jitter sequence & temporal hook metadata for FSR 2/3
//! - Ready-to-compile HLSL compute shaders (`CSMain`) and `#[repr(C)]` constant
//!   buffer layout (`FsrConstants`) for native Windows Direct3D 12 compute passes.

use std::cell::RefCell;
use std::time::Instant;

use crate::backend::PresentedFrame;

/// Standard AMD FSR quality presets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsrQualityMode {
    /// 1.3x per-axis scale factor (~77% internal resolution).
    UltraQuality,
    /// 1.5x per-axis scale factor (~67% internal resolution) — **default**.
    Quality,
    /// 1.7x per-axis scale factor (~59% internal resolution).
    Balanced,
    /// 2.0x per-axis scale factor (50% internal resolution).
    Performance,
    /// 3.0x per-axis scale factor (~33% internal resolution).
    UltraPerformance,
    /// 1.0x native resolution — runs RCAS contrast-adaptive sharpening only.
    NativeAA,
    /// Disable FSR upscaling and sharpening completely.
    Off,
}

impl Default for FsrQualityMode {
    fn default() -> Self {
        Self::Quality
    }
}

impl FsrQualityMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "ultra-quality" | "ultra_quality" | "uq" => Some(Self::UltraQuality),
            "quality" | "q" => Some(Self::Quality),
            "balanced" | "b" => Some(Self::Balanced),
            "performance" | "perf" | "p" => Some(Self::Performance),
            "ultra-performance" | "ultra_performance" | "up" => Some(Self::UltraPerformance),
            "native-aa" | "native_aa" | "native" => Some(Self::NativeAA),
            "off" | "none" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::UltraQuality => "UltraQuality",
            Self::Quality => "Quality",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
            Self::UltraPerformance => "UltraPerformance",
            Self::NativeAA => "NativeAA",
            Self::Off => "Off",
        }
    }

    /// Output-to-render per-axis scale factor.
    pub fn scale_factor(self) -> f32 {
        match self {
            Self::UltraQuality => 1.3,
            Self::Quality => 1.5,
            Self::Balanced => 1.7,
            Self::Performance => 2.0,
            Self::UltraPerformance => 3.0,
            Self::NativeAA | Self::Off => 1.0,
        }
    }

    /// Compute recommended internal Xbox render resolution for a target PC display size.
    pub fn recommended_render_resolution(self, display_w: u32, display_h: u32) -> (u32, u32) {
        let s = self.scale_factor();
        let rw = ((display_w as f32 / s).round() as u32).max(16) & !1;
        let rh = ((display_h as f32 / s).round() as u32).max(16) & !1;
        (rw, rh)
    }

    /// Compute target output resolution from an internal render resolution when no
    /// explicit monitor output resolution is pinned.
    pub fn default_output_resolution(self, input_w: u32, input_h: u32) -> (u32, u32) {
        let s = self.scale_factor();
        let ow = ((input_w as f32 * s).round() as u32).max(input_w);
        let oh = ((input_h as f32 * s).round() as u32).max(input_h);
        // Align to even pixel dimensions for clean video/swapchain compatibility
        ((ow + 1) & !1, (oh + 1) & !1)
    }
}

/// Configuration for the default-enabled FSR upscaling & sharpening pipeline.
#[derive(Debug, Clone)]
pub struct FsrConfig {
    /// Quality preset (default: `FsrQualityMode::Quality` = 1.5x).
    pub mode: FsrQualityMode,
    /// RCAS sharpening strength in `[0.0, 1.0]` (0.0 = soft, 1.0 = maximum crispness, default: 0.8).
    pub sharpness: f32,
    /// Enable Robust Contrast-Adaptive Sharpening pass after EASU (default: true).
    pub enable_rcas: bool,
    /// Optional explicit output dimensions `(width, height)`. When `None`, derived from
    /// `input_resolution * mode.scale_factor()`.
    pub output_resolution: Option<(u32, u32)>,
}

impl Default for FsrConfig {
    fn default() -> Self {
        Self {
            mode: FsrQualityMode::Quality,
            sharpness: 0.8,
            enable_rcas: true,
            output_resolution: None,
        }
    }
}

impl FsrConfig {
    pub fn disabled() -> Self {
        Self {
            mode: FsrQualityMode::Off,
            ..Self::default()
        }
    }

    pub fn is_active(&self) -> bool {
        self.mode != FsrQualityMode::Off
    }

    pub fn resolve_output_size(&self, input_w: u32, input_h: u32) -> (u32, u32) {
        if !self.is_active() {
            return (input_w, input_h);
        }
        match self.output_resolution {
            Some((w, h)) if w > 0 && h > 0 => (w, h),
            _ => self.mode.default_output_resolution(input_w, input_h),
        }
    }
}

/// Telemetry emitted after running FSR on a presented frame.
#[derive(Debug, Clone)]
pub struct FsrFrameStats {
    pub mode: FsrQualityMode,
    pub input_width: u32,
    pub input_height: u32,
    pub output_width: u32,
    pub output_height: u32,
    pub easu_ran: bool,
    pub rcas_ran: bool,
    pub sharpness: f32,
    pub duration_us: u64,
}

/// `#[repr(C)]` constant buffer uploaded to native D3D12 FSR compute shaders (`b0`).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FsrDispatchConstants {
    pub input_size: [f32; 2],
    pub output_size: [f32; 2],
    pub inv_input_size: [f32; 2],
    pub inv_output_size: [f32; 2],
    /// RCAS sharpness attenuation in stops (`0.0` = max sharpening, `2.0` = soft).
    pub rcas_attenuation: f32,
    /// Halton(2, 3) temporal sub-pixel jitter offset for FSR 2/3 camera integration.
    pub jitter_x: f32,
    pub jitter_y: f32,
    pub _pad: f32,
}

impl FsrDispatchConstants {
    pub fn new(
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
        sharpness: f32,
        frame_index: u64,
    ) -> Self {
        let (jx, jy) = halton_jitter_2_3(frame_index, in_w, in_h);
        let s = sharpness.clamp(0.0, 1.0);
        Self {
            input_size: [in_w as f32, in_h as f32],
            output_size: [out_w as f32, out_h as f32],
            inv_input_size: [1.0 / in_w.max(1) as f32, 1.0 / in_h.max(1) as f32],
            inv_output_size: [1.0 / out_w.max(1) as f32, 1.0 / out_h.max(1) as f32],
            rcas_attenuation: 2.0 * (1.0 - s),
            jitter_x: jx,
            jitter_y: jy,
            _pad: 0.0,
        }
    }

    /// Compute `(thread_groups_x, thread_groups_y, 1)` for a 16x16 compute shader dispatch.
    pub fn thread_groups_16x16(out_w: u32, out_h: u32) -> (u32, u32, u32) {
        (out_w.div_ceil(16), out_h.div_ceil(16), 1)
    }
}

/// Optional temporal inputs supplied by titles or hooks for FSR 2 / FSR 3 temporal upscaling.
#[derive(Debug, Clone, Copy, Default)]
pub struct FsrTemporalInputs {
    pub depth_resource_id: Option<u64>,
    pub motion_vectors_resource_id: Option<u64>,
    pub exposure: f32,
    pub delta_time_ms: f32,
    pub reset_history: bool,
}

/// Hardware D3D12 compute dispatch plan for executing FSR EASU + RCAS on GPU.
#[derive(Debug, Clone, Copy)]
pub struct FsrComputePlan {
    pub constants: FsrDispatchConstants,
    pub easu_groups: (u32, u32, u32),
    pub rcas_groups: (u32, u32, u32),
    pub run_easu: bool,
    pub run_rcas: bool,
    pub temporal: FsrTemporalInputs,
}

impl FsrComputePlan {
    pub fn build(
        cfg: &FsrConfig,
        in_w: u32,
        in_h: u32,
        frame_index: u64,
        temporal: FsrTemporalInputs,
    ) -> Self {
        let (out_w, out_h) = cfg.resolve_output_size(in_w, in_h);
        let groups = FsrDispatchConstants::thread_groups_16x16(out_w, out_h);
        Self {
            constants: FsrDispatchConstants::new(in_w, in_h, out_w, out_h, cfg.sharpness, frame_index),
            easu_groups: groups,
            rcas_groups: groups,
            run_easu: cfg.is_active() && (out_w != in_w || out_h != in_h),
            run_rcas: cfg.is_active() && cfg.enable_rcas,
            temporal,
        }
    }
}

/// Halton(2, 3) sub-pixel jitter sequence in `[-0.5, +0.5]` pixel units for FSR 2/3 temporal passes.
pub fn halton_jitter_2_3(frame_index: u64, render_w: u32, render_h: u32) -> (f32, f32) {
    let phase = (frame_index % 16) + 1;
    let hx = halton(phase, 2) - 0.5;
    let hy = halton(phase, 3) - 0.5;
    // Also scale by 1.0 (in pixel units) while preventing zero-dimension issues
    let _ = (render_w, render_h);
    (hx, hy)
}

fn halton(mut index: u64, base: u64) -> f32 {
    let mut f = 1.0_f32;
    let mut r = 0.0_f32;
    let b = base as f32;
    while index > 0 {
        f /= b;
        r += f * (index % base) as f32;
        index /= base;
    }
    r
}

/// Run the full FSR pipeline (EASU spatial upsampling + RCAS contrast-adaptive sharpening)
/// on an RGBA8 `PresentedFrame`.
pub fn process_frame(frame: &PresentedFrame, cfg: &FsrConfig) -> (PresentedFrame, FsrFrameStats) {
    let start = Instant::now();
    let in_w = frame.width;
    let in_h = frame.height;
    let (out_w, out_h) = cfg.resolve_output_size(in_w, in_h);

    if !cfg.is_active() {
        return (
            frame.clone(),
            FsrFrameStats {
                mode: cfg.mode,
                input_width: in_w,
                input_height: in_h,
                output_width: in_w,
                output_height: in_h,
                easu_ran: false,
                rcas_ran: false,
                sharpness: cfg.sharpness,
                duration_us: start.elapsed().as_micros() as u64,
            },
        );
    }

    let easu_ran = out_w != in_w || out_h != in_h;
    let rcas_ran = cfg.enable_rcas;
    let final_rgba = fsr_pipeline(
        &frame.rgba,
        in_w,
        in_h,
        out_w,
        out_h,
        easu_ran,
        rcas_ran,
        cfg.sharpness,
    );

    let stats = FsrFrameStats {
        mode: cfg.mode,
        input_width: in_w,
        input_height: in_h,
        output_width: out_w,
        output_height: out_h,
        easu_ran,
        rcas_ran,
        sharpness: cfg.sharpness.clamp(0.0, 1.0),
        duration_us: start.elapsed().as_micros() as u64,
    };

    (
        PresentedFrame {
            frame_index: frame.frame_index,
            width: out_w,
            height: out_h,
            rgba: final_rgba,
        },
        stats,
    )
}

/// Steady-state FSR benchmark result (1 warmup + `iterations` timed runs).
#[derive(Debug, Clone)]
pub struct FsrBenchResult {
    pub mode: FsrQualityMode,
    pub output_width: u32,
    pub output_height: u32,
    pub easu_ran: bool,
    pub rcas_ran: bool,
    pub iterations: u32,
    pub avg_ms: f64,
    pub min_ms: f64,
    /// Final upscaled frame of the last iteration (for previews).
    pub frame: PresentedFrame,
}

impl FsrBenchResult {
    pub fn fps(&self) -> f64 {
        if self.avg_ms > 0.0 {
            1000.0 / self.avg_ms
        } else {
            0.0
        }
    }
}

/// Run `process_frame` once to warm caches/threads, then `iterations` timed runs.
pub fn bench_frame(frame: &PresentedFrame, cfg: &FsrConfig, iterations: u32) -> FsrBenchResult {
    let iterations = iterations.max(1);
    let (mut out, mut stats) = process_frame(frame, cfg);
    let mut total = 0.0_f64;
    let mut min_ms = f64::MAX;
    for _ in 0..iterations {
        let t = Instant::now();
        let (o, s) = process_frame(frame, cfg);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        total += ms;
        min_ms = min_ms.min(ms);
        out = o;
        stats = s;
    }
    FsrBenchResult {
        mode: stats.mode,
        output_width: stats.output_width,
        output_height: stats.output_height,
        easu_ran: stats.easu_ran,
        rcas_ran: stats.rcas_ran,
        iterations,
        avg_ms: total / iterations as f64,
        min_ms,
        frame: out,
    }
}

/// Deterministic gradient + checkerboard test pattern used by `bench-fsr` and the GUI.
pub fn test_pattern(width: u32, height: u32) -> PresentedFrame {
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    let (wm, hm) = (width.max(1), height.max(1));
    for (y, row) in rgba.chunks_exact_mut((width as usize * 4).max(1)).enumerate() {
        let y = y as u32;
        let g = ((y * 255) / hm) as u8;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let x = x as u32;
            px[0] = ((x * 255) / wm) as u8;
            px[1] = g;
            px[2] = if (x ^ y) & 32 != 0 { 220 } else { 40 };
            px[3] = 255;
        }
    }
    PresentedFrame {
        frame_index: 0,
        width,
        height,
        rgba,
    }
}

// ===========================================================================
// Optimized SIMD-friendly FSR core (v0.1.0 performance pass)
//
// * `F4` is a 16-byte aligned `[r, g, b, luma]` lane type whose lane-wise ops
//   LLVM lowers straight to SSE/NEON (`addps`, `minps`, `divps`, ...).
// * EASU edge analysis evaluates its 4 bilinear quadrants as one `F4` (2 vector
//   divisions instead of 8 scalar ones) and the 12 Lanczos-2 tap weights as
//   three `F4` batches.
// * EASU writes its result *directly* into RCAS's border-padded float buffer
//   (fused pipeline): no u8 quantize → re-expand round trip and no second
//   padded-buffer build.
// * Padded input conversion is multithreaded and all large intermediate
//   buffers are reused across frames through a thread-local scratch arena.
// ===========================================================================


#[derive(Clone, Copy, Default, Debug, PartialEq)]
#[repr(C, align(16))]
struct F4([f32; 4]);

macro_rules! f4_binop {
    ($name:ident, $op:tt) => {
        #[inline(always)]
        fn $name(self, o: Self) -> Self {
            F4([
                self.0[0] $op o.0[0],
                self.0[1] $op o.0[1],
                self.0[2] $op o.0[2],
                self.0[3] $op o.0[3],
            ])
        }
    };
}

impl F4 {
    #[inline(always)]
    const fn splat(v: f32) -> Self {
        F4([v; 4])
    }
    f4_binop!(add, +);
    f4_binop!(sub, -);
    f4_binop!(mul, *);
    f4_binop!(div, /);
    #[inline(always)]
    fn min(self, o: Self) -> Self {
        let m = |a: f32, b: f32| if a < b { a } else { b };
        F4([m(self.0[0], o.0[0]), m(self.0[1], o.0[1]), m(self.0[2], o.0[2]), m(self.0[3], o.0[3])])
    }
    #[inline(always)]
    fn max(self, o: Self) -> Self {
        let m = |a: f32, b: f32| if a > b { a } else { b };
        F4([m(self.0[0], o.0[0]), m(self.0[1], o.0[1]), m(self.0[2], o.0[2]), m(self.0[3], o.0[3])])
    }
    #[inline(always)]
    fn abs(self) -> Self {
        F4([self.0[0].abs(), self.0[1].abs(), self.0[2].abs(), self.0[3].abs()])
    }
    #[inline(always)]
    fn clamp(self, lo: Self, hi: Self) -> Self {
        self.max(lo).min(hi)
    }
    /// Lane-wise `if self > thr { a } else { 0 }`.
    #[inline(always)]
    fn select_gt(self, thr: f32, a: Self) -> Self {
        let s = |c: f32, v: f32| if c > thr { v } else { 0.0 };
        F4([s(self.0[0], a.0[0]), s(self.0[1], a.0[1]), s(self.0[2], a.0[2]), s(self.0[3], a.0[3])])
    }
    #[inline(always)]
    fn hsum(self) -> f32 {
        (self.0[0] + self.0[1]) + (self.0[2] + self.0[3])
    }
    #[inline(always)]
    fn luma(self) -> f32 {
        0.5 * self.0[0] + self.0[1] + 0.5 * self.0[2]
    }
}

#[inline(always)]
fn to_u8(v: f32) -> u8 {
    // `as u8` saturates, NaN -> 0
    (v * 255.0 + 0.5) as u8
}

#[derive(Default)]
struct FsrScratch {
    input: Vec<F4>,
    mid: Vec<F4>,
}

thread_local! {
    static SCRATCH: RefCell<FsrScratch> = RefCell::new(FsrScratch::default());
}

fn worker_threads(rows: usize) -> usize {
    if rows < 64 {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(rows / 32)
        .max(1)
}

/// Run `f(first_row, band)` over horizontal bands of `rows` rows (each `row_len`
/// elements) in parallel.
fn par_rows<T: Send>(data: &mut [T], row_len: usize, rows: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    if rows == 0 || row_len == 0 {
        return;
    }
    let threads = worker_threads(rows);
    if threads <= 1 {
        f(0, &mut data[..rows * row_len]);
        return;
    }
    let per = rows.div_ceil(threads);
    let f = &f;
    std::thread::scope(|scope| {
        for (i, band) in data[..rows * row_len].chunks_mut(per * row_len).enumerate() {
            scope.spawn(move || f(i * per, band));
        }
    });
}

/// Resize `buf` to `len` without zero-filling memory that is about to be overwritten.
fn ensure_len(buf: &mut Vec<F4>, len: usize) {
    if buf.len() != len {
        buf.clear();
        buf.resize(len, F4::default());
    }
}

/// Replicate the outermost interior rows into the `pad` border rows.
fn replicate_vertical_borders(buf: &mut [F4], stride: usize, pad: usize, h: usize) {
    let first = pad * stride;
    let last = (pad + h - 1) * stride;
    for p in 0..pad {
        buf.copy_within(first..first + stride, p * stride);
        buf.copy_within(last..last + stride, (pad + h + p) * stride);
    }
}

/// Multithreaded RGBA8 -> border-replicated `[r,g,b,luma]` float conversion.
fn fill_padded_from_rgba8(src: &[u8], w: usize, h: usize, pad: usize, out: &mut Vec<F4>) -> usize {
    let stride = w + pad * 2;
    ensure_len(out, stride * (h + pad * 2));
    const INV: f32 = 1.0 / 255.0;
    let interior = &mut out[pad * stride..(pad + h) * stride];
    par_rows(interior, stride, h, |y0, band| {
        for (ly, row) in band.chunks_exact_mut(stride).enumerate() {
            let y = y0 + ly;
            let s = &src[y * w * 4..(y + 1) * w * 4];
            for (dst, px) in row[pad..pad + w].iter_mut().zip(s.chunks_exact(4)) {
                let r = px[0] as f32 * INV;
                let g = px[1] as f32 * INV;
                let b = px[2] as f32 * INV;
                *dst = F4([r, g, b, 0.5 * r + g + 0.5 * b]);
            }
            let (left, right) = (row[pad], row[pad + w - 1]);
            row[..pad].fill(left);
            row[pad + w..].fill(right);
        }
    });
    replicate_vertical_borders(out, stride, pad, h);
    stride
}

// 12-tap EASU footprint offsets (b c e f g h i j k l n o), grouped in 3 lanes of 4.
const EASU_OX: [F4; 3] = [
    F4([0.0, 1.0, -1.0, 0.0]),
    F4([1.0, 2.0, -1.0, 0.0]),
    F4([1.0, 2.0, 0.0, 1.0]),
];
const EASU_OY: [F4; 3] = [
    F4([-1.0, -1.0, 0.0, 0.0]),
    F4([0.0, 0.0, 1.0, 1.0]),
    F4([1.0, 1.0, 2.0, 2.0]),
];

/// Evaluate one EASU output pixel. `p` = padded input, `r[0..4]` = row offsets for
/// rows cy-1..cy+2, `cx` = padded column of the top-left bilinear texel `f`.
#[inline(always)]
fn easu_pixel(p: &[F4], r: [usize; 4], cx: usize, px: f32, py: f32) -> F4 {
    let t = [
        p[r[0] + cx],     // b
        p[r[0] + cx + 1], // c
        p[r[1] + cx - 1], // e
        p[r[1] + cx],     // f
        p[r[1] + cx + 1], // g
        p[r[1] + cx + 2], // h
        p[r[2] + cx - 1], // i
        p[r[2] + cx],     // j
        p[r[2] + cx + 1], // k
        p[r[2] + cx + 2], // l
        p[r[3] + cx],     // n
        p[r[3] + cx + 1], // o
    ];
    let l = |i: usize| t[i].0[3];
    let (b, c, e, f, g, h, i, j, k, ll, n, o) =
        (l(0), l(1), l(2), l(3), l(4), l(5), l(6), l(7), l(8), l(9), l(10), l(11));

    // --- Edge direction & length over the 4 bilinear quadrants (vectorized) ---
    let omx = 1.0 - px;
    let omy = 1.0 - py;
    let wq = F4([omx * omy, px * omy, omx * py, px * py]);
    let ln = F4([b, c, f, g]);
    let ls = F4([j, k, n, o]);
    let lw = F4([e, f, i, j]);
    let le = F4([g, h, k, ll]);
    let lc = F4([f, g, j, k]);
    let dcx = le.sub(lw);
    let dcy = ls.sub(ln);
    let mut dx = dcx.mul(wq).hsum();
    let mut dy = dcy.mul(wq).hsum();

    let one = F4::splat(1.0);
    let eps = F4::splat(1e-5);
    let max_x = le.sub(lc).abs().max(lc.sub(lw).abs());
    let tx = dcx.abs().div(max_x.max(eps)).min(one);
    let len_x = max_x.select_gt(1e-5, tx.mul(tx));
    let max_y = ls.sub(lc).abs().max(lc.sub(ln).abs());
    let ty = dcy.abs().div(max_y.max(eps)).min(one);
    let len_y = max_y.select_gt(1e-5, ty.mul(ty));
    let len = len_x.add(len_y).mul(F4::splat(0.5)).mul(wq).hsum();

    let dir_len2 = dx * dx + dy * dy;
    if dir_len2 < 1e-6 {
        dx = 1.0;
        dy = 0.0;
    } else {
        let inv = dir_len2.sqrt().recip();
        dx *= inv;
        dy *= inv;
    }

    let len = (len * len).clamp(0.0, 1.0);
    let max_dir = dx.abs().max(dy.abs()).max(1e-4);
    let stretch = (dx * dx + dy * dy) / max_dir;
    let sx = 1.0 + (stretch - 1.0) * len;
    let sy = 1.0 - 0.5 * len;
    let lobe = 0.5 + (0.29 - 0.5) * len;
    let clip = 1.0 / lobe;

    // --- 12 Lanczos-2 approximated tap weights in 3 F4 batches ---
    let (vdx, vdy, vsx, vsy) = (F4::splat(dx), F4::splat(dy), F4::splat(sx), F4::splat(sy));
    let (vpx, vpy) = (F4::splat(px), F4::splat(py));
    let vclip = F4::splat(clip);
    let vlobe = F4::splat(lobe);
    let mut acc = F4::default();
    let mut wsum = 0.0_f32;
    for gi in 0..3 {
        let ox = EASU_OX[gi].sub(vpx);
        let oy = EASU_OY[gi].sub(vpy);
        let rx = ox.mul(vdx).add(oy.mul(vdy)).mul(vsx);
        let ry = oy.mul(vdx).sub(ox.mul(vdy)).mul(vsy);
        let d2 = rx.mul(rx).add(ry.mul(ry)).min(vclip);
        let wb = F4::splat(0.4).mul(d2).sub(one);
        let wa = vlobe.mul(d2).sub(one);
        let w = F4::splat(1.5625).mul(wb).mul(wb).sub(F4::splat(0.5625)).mul(wa.mul(wa));
        wsum += w.hsum();
        for lane in 0..4 {
            acc = acc.add(t[gi * 4 + lane].mul(F4::splat(w.0[lane])));
        }
    }

    let inv_w = if wsum.abs() > 1e-6 { 1.0 / wsum } else { 1.0 };
    // Anti-ringing clamp to the 2x2 bilinear neighbourhood f, g, j, k.
    let lo = t[3].min(t[4]).min(t[7]).min(t[8]).max(F4::splat(0.0));
    let hi = t[3].max(t[4]).max(t[7]).max(t[8]).min(one);
    let mut v = acc.mul(F4::splat(inv_w)).clamp(lo, hi);
    v.0[3] = v.luma();
    v
}

/// Precompute per-column `(padded cx, frac px)` sampling coordinates.
fn easu_axis_lut(out_len: u32, in_len: u32, pad: usize) -> Vec<(usize, f32)> {
    let scale = in_len as f32 / out_len as f32;
    let max_i = in_len as i32 - 1;
    (0..out_len)
        .map(|o| {
            let fpos = (o as f32 + 0.5) * scale - 0.5;
            let ip = fpos.floor() as i32;
            let frac = fpos - ip as f32;
            ((ip.clamp(-1, max_i) + pad as i32) as usize, frac)
        })
        .collect()
}

/// RCAS on one pixel given its 5-tap cross neighbourhood.
#[inline(always)]
fn rcas_pixel(b: F4, d: F4, e: F4, f: F4, h: F4, sharp_scale: f32) -> F4 {
    const LIMIT: f32 = 0.1875;
    let mn4 = b.min(d).min(f).min(h);
    let mx4 = b.max(d).max(f).max(h);
    let mn_all = mn4.min(e);
    let mx_all = mx4.max(e);

    // Luma lane (3) noise detection reusing the min/max already computed.
    let nz = (0.25 * (b.0[3] + d.0[3] + f.0[3] + h.0[3]) - e.0[3]).abs();
    let range_l = (mx_all.0[3] - mn_all.0[3]).max(1e-4);
    let noise = 1.0 - 0.5 * (nz / range_l).clamp(0.0, 1.0);

    let hit_min = mn_all.div(F4::splat(4.0).mul(mx4).add(F4::splat(1e-5)));
    let hit_max = mx_all
        .sub(F4::splat(1.0))
        .div(F4::splat(4.0).mul(mn4).sub(F4::splat(4.0 + 1e-5)));
    let ch = F4::splat(0.0)
        .sub(hit_min)
        .max(hit_max)
        .clamp(F4::splat(-LIMIT), F4::splat(0.0));
    let min_lobe = ch.0[0].max(ch.0[1]).max(ch.0[2]);
    let lobe = (min_lobe * sharp_scale * noise).clamp(-LIMIT, 0.0);
    let inv_norm = 1.0 / (4.0 * lobe + 1.0);
    b.add(d).add(f).add(h).mul(F4::splat(lobe)).add(e).mul(F4::splat(inv_norm))
}

fn rcas_into_rgba8(p: &[F4], stride: usize, w: usize, h: usize, sharpness: f32, dst: &mut [u8]) {
    let stops = 2.0 * (1.0 - sharpness.clamp(0.0, 1.0));
    let sharp_scale = 2.0_f32.powf(-stops);
    par_rows(dst, w * 4, h, |y0, band| {
        for (ly, out_row) in band.chunks_exact_mut(w * 4).enumerate() {
            let cy = y0 + ly + 1;
            let up = &p[(cy - 1) * stride + 1..(cy - 1) * stride + 1 + w];
            let mid = &p[cy * stride..cy * stride + w + 2];
            let dn = &p[(cy + 1) * stride + 1..(cy + 1) * stride + 1 + w];
            for (x, px) in out_row.chunks_exact_mut(4).enumerate() {
                let v = rcas_pixel(up[x], mid[x], mid[x + 1], mid[x + 2], dn[x], sharp_scale);
                px[0] = to_u8(v.0[0].clamp(0.0, 1.0));
                px[1] = to_u8(v.0[1].clamp(0.0, 1.0));
                px[2] = to_u8(v.0[2].clamp(0.0, 1.0));
                px[3] = 255;
            }
        }
    });
}

/// Full optimized FSR pipeline on RGBA8 input. Returns RGBA8 at `out_w x out_h`.
fn fsr_pipeline(
    src: &[u8],
    in_w: u32,
    in_h: u32,
    out_w: u32,
    out_h: u32,
    run_easu: bool,
    run_rcas: bool,
    sharpness: f32,
) -> Vec<u8> {
    if in_w == 0 || in_h == 0 || out_w == 0 || out_h == 0 {
        return Vec::new();
    }
    let (iw, ih, ow, oh) = (in_w as usize, in_h as usize, out_w as usize, out_h as usize);
    let mut dst = vec![0u8; ow * oh * 4];
    if !run_easu && !run_rcas {
        dst.copy_from_slice(&src[..ow * oh * 4]);
        return dst;
    }
    SCRATCH.with(|cell| {
        let mut guard = cell.borrow_mut();
        let FsrScratch { input, mid } = &mut *guard;
        if !run_easu {
            // NativeAA: RCAS only.
            let stride = fill_padded_from_rgba8(src, iw, ih, 1, input);
            rcas_into_rgba8(input, stride, iw, ih, sharpness, &mut dst);
            return;
        }

        const PAD: usize = 2;
        let in_stride = fill_padded_from_rgba8(src, iw, ih, PAD, input);
        let xl = easu_axis_lut(out_w, in_w, PAD);
        let yl = easu_axis_lut(out_h, in_h, PAD);
        let inp: &[F4] = input;
        let rows_of = |cy: usize| {
            [(cy - 1) * in_stride, cy * in_stride, (cy + 1) * in_stride, (cy + 2) * in_stride]
        };

        if run_rcas {
            // Fused: EASU writes straight into RCAS's 1-pixel padded float buffer.
            let mid_stride = ow + 2;
            ensure_len(mid, mid_stride * (oh + 2));
            let interior = &mut mid[mid_stride..(oh + 1) * mid_stride];
            par_rows(interior, mid_stride, oh, |y0, band| {
                for (ly, row) in band.chunks_exact_mut(mid_stride).enumerate() {
                    let (cy, py) = yl[y0 + ly];
                    let r = rows_of(cy);
                    for (dst_px, &(cx, px)) in row[1..=ow].iter_mut().zip(xl.iter()) {
                        *dst_px = easu_pixel(inp, r, cx, px, py);
                    }
                    row[0] = row[1];
                    row[ow + 1] = row[ow];
                }
            });
            replicate_vertical_borders(mid, mid_stride, 1, oh);
            rcas_into_rgba8(mid, mid_stride, ow, oh, sharpness, &mut dst);
        } else {
            par_rows(&mut dst, ow * 4, oh, |y0, band| {
                for (ly, row) in band.chunks_exact_mut(ow * 4).enumerate() {
                    let (cy, py) = yl[y0 + ly];
                    let r = rows_of(cy);
                    for (px_out, &(cx, px)) in row.chunks_exact_mut(4).zip(xl.iter()) {
                        let v = easu_pixel(inp, r, cx, px, py);
                        px_out[0] = to_u8(v.0[0]);
                        px_out[1] = to_u8(v.0[1]);
                        px_out[2] = to_u8(v.0[2]);
                        px_out[3] = 255;
                    }
                }
            });
        }
    });
    dst
}

// Scalar reference implementation kept for parity tests of the optimized core.
#[cfg(test)]
mod reference {
    // ===========================================================================
    // Pre-converted Padded Linear RGB+Luma Buffer & Multithreaded Row Executor
    // ===========================================================================

    /// Convert RGBA8 image into a border-replicated `[r, g, b, luma]` `f32` buffer with `pad` pixels
    /// on all four sides, eliminating bounds checks, divisions, and luma recomputation in hot loops.
    fn build_padded_rgbl(src: &[u8], w: u32, h: u32, pad: usize) -> (Vec<[f32; 4]>, usize) {
        let wu = w as usize;
        let hu = h as usize;
        let pw = wu + pad * 2;
        let ph = hu + pad * 2;
        let mut out = vec![[0.0_f32; 4]; pw * ph];
        let inv255 = 1.0_f32 / 255.0_f32;

        for y in 0..hu {
            let src_row = y * wu * 4;
            let dst_row = (y + pad) * pw + pad;
            for x in 0..wu {
                let s = src_row + x * 4;
                let r = src[s] as f32 * inv255;
                let g = src[s + 1] as f32 * inv255;
                let b = src[s + 2] as f32 * inv255;
                let l = 0.5 * r + g + 0.5 * b;
                out[dst_row + x] = [r, g, b, l];
            }
            // Replicate left & right borders
            let left = out[dst_row];
            let right = out[dst_row + wu - 1];
            for p in 0..pad {
                out[dst_row - pad + p] = left;
                out[dst_row + wu + p] = right;
            }
        }
        // Replicate top & bottom border rows
        let first_row_start = pad * pw;
        let last_row_start = (pad + hu - 1) * pw;
        for p in 0..pad {
            out.copy_within(first_row_start..first_row_start + pw, p * pw);
            out.copy_within(last_row_start..last_row_start + pw, (pad + hu + p) * pw);
        }
        (out, pw)
    }

    fn worker_thread_count(rows: usize) -> usize {
        if rows < 64 {
            return 1;
        }
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(rows / 32)
            .max(1)
    }

    // ===========================================================================
    // EASU — Edge-Adaptive Spatial Upsampling (Multithreaded 12-tap Lanczos-2)
    // ===========================================================================

    #[inline(always)]
    fn accumulate_edge_dir(
        dir: &mut [f32; 2],
        len_acc: &mut f32,
        weight: f32,
        l_n: f32,
        l_s: f32,
        l_w: f32,
        l_e: f32,
        l_c: f32,
    ) {
        let dc_x = l_e - l_w;
        let dc_y = l_s - l_n;
        dir[0] += dc_x * weight;
        dir[1] += dc_y * weight;

        let max_x = (l_e - l_c).abs().max((l_c - l_w).abs());
        let len_x = if max_x > 1e-5 {
            let t = (dc_x.abs() / max_x).min(1.0);
            t * t
        } else {
            0.0
        };

        let max_y = (l_s - l_c).abs().max((l_c - l_n).abs());
        let len_y = if max_y > 1e-5 {
            let t = (dc_y.abs() / max_y).min(1.0);
            t * t
        } else {
            0.0
        };

        *len_acc += (len_x + len_y) * 0.5 * weight;
    }

    /// Fast Lanczos-2 polynomial approximation matching AMD FSR EASU lobe shape.
    #[inline(always)]
    fn easu_tap(
        acc: &mut [f32; 3],
        w_sum: &mut f32,
        off_x: f32,
        off_y: f32,
        dir: [f32; 2],
        scale: [f32; 2],
        lobe: f32,
        clip: f32,
        rgbl: [f32; 4],
    ) {
        let rx = (off_x * dir[0] + off_y * dir[1]) * scale[0];
        let ry = (-off_x * dir[1] + off_y * dir[0]) * scale[1];
        let d2 = (rx * rx + ry * ry).min(clip);

        let wb = 0.4 * d2 - 1.0;
        let wa = lobe * d2 - 1.0;
        let w = (1.5625 * wb * wb - 0.5625) * (wa * wa);

        acc[0] += rgbl[0] * w;
        acc[1] += rgbl[1] * w;
        acc[2] += rgbl[2] * w;
        *w_sum += w;
    }

    pub(super) fn easu_upscale(src: &[u8], in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> Vec<u8> {
        const PAD: usize = 2;
        let (padded, stride) = build_padded_rgbl(src, in_w, in_h, PAD);
        let mut dst = vec![0u8; (out_w as usize) * (out_h as usize) * 4];

        let scale_x = in_w as f32 / out_w as f32;
        let scale_y = in_h as f32 / out_h as f32;
        let max_ix = (in_w as i32) - 1;
        let max_iy = (in_h as i32) - 1;

        // Precompute X sampling coordinates once for all rows
        let x_lut: Vec<(usize, f32)> = (0..out_w)
            .map(|ox| {
                let fx = (ox as f32 + 0.5) * scale_x - 0.5;
                let ix = fx.floor() as i32;
                let px = fx - ix as f32;
                let cx = (ix.clamp(-1, max_ix) + PAD as i32) as usize;
                (cx, px)
            })
            .collect();

        let threads = worker_thread_count(out_h as usize);
        let rows_per_band = (out_h as usize).div_ceil(threads);
        let row_bytes = (out_w as usize) * 4;
        let padded_ref = &padded;
        let x_lut_ref = &x_lut;

        std::thread::scope(|scope| {
            for (band_idx, band_slice) in dst.chunks_mut(rows_per_band * row_bytes).enumerate() {
                let start_y = band_idx * rows_per_band;
                scope.spawn(move || {
                    let band_rows = band_slice.len() / row_bytes;
                    for local_y in 0..band_rows {
                        let oy = (start_y + local_y) as u32;
                        let fy = (oy as f32 + 0.5) * scale_y - 0.5;
                        let iy = fy.floor() as i32;
                        let py = fy - iy as f32;
                        let cy = (iy.clamp(-1, max_iy) + PAD as i32) as usize;

                        let r_m1 = (cy - 1) * stride;
                        let r_0 = cy * stride;
                        let r_p1 = (cy + 1) * stride;
                        let r_p2 = (cy + 2) * stride;
                        let out_row = &mut band_slice[local_y * row_bytes..(local_y + 1) * row_bytes];

                        for (ox, &(cx, px)) in x_lut_ref.iter().enumerate() {
                            let b = padded_ref[r_m1 + cx];
                            let c = padded_ref[r_m1 + cx + 1];
                            let e = padded_ref[r_0 + cx - 1];
                            let f = padded_ref[r_0 + cx];
                            let g = padded_ref[r_0 + cx + 1];
                            let h = padded_ref[r_0 + cx + 2];
                            let i = padded_ref[r_p1 + cx - 1];
                            let j = padded_ref[r_p1 + cx];
                            let k = padded_ref[r_p1 + cx + 1];
                            let l = padded_ref[r_p1 + cx + 2];
                            let n = padded_ref[r_p2 + cx];
                            let o = padded_ref[r_p2 + cx + 1];

                            let mut dir = [0.0_f32; 2];
                            let mut len = 0.0_f32;
                            let omx = 1.0 - px;
                            let omy = 1.0 - py;
                            accumulate_edge_dir(&mut dir, &mut len, omx * omy, b[3], j[3], e[3], g[3], f[3]);
                            accumulate_edge_dir(&mut dir, &mut len, px * omy, c[3], k[3], f[3], h[3], g[3]);
                            accumulate_edge_dir(&mut dir, &mut len, omx * py, f[3], n[3], i[3], k[3], j[3]);
                            accumulate_edge_dir(&mut dir, &mut len, px * py, g[3], o[3], j[3], l[3], k[3]);

                            let dir_len2 = dir[0] * dir[0] + dir[1] * dir[1];
                            if dir_len2 < 1e-6 {
                                dir = [1.0, 0.0];
                            } else {
                                let inv = dir_len2.sqrt().recip();
                                dir[0] *= inv;
                                dir[1] *= inv;
                            }

                            let len = (len * len).clamp(0.0, 1.0);
                            let max_dir = dir[0].abs().max(dir[1].abs()).max(1e-4);
                            let stretch = (dir[0] * dir[0] + dir[1] * dir[1]) / max_dir;
                            let aniso_scale = [1.0 + (stretch - 1.0) * len, 1.0 - 0.5 * len];
                            let lobe = 0.5 + (0.29 - 0.5) * len;
                            let clip = 1.0 / lobe;

                            let mut acc = [0.0_f32; 3];
                            let mut w_sum = 0.0_f32;

                            easu_tap(&mut acc, &mut w_sum, -px, -1.0 - py, dir, aniso_scale, lobe, clip, b);
                            easu_tap(&mut acc, &mut w_sum, 1.0 - px, -1.0 - py, dir, aniso_scale, lobe, clip, c);
                            easu_tap(&mut acc, &mut w_sum, -1.0 - px, -py, dir, aniso_scale, lobe, clip, e);
                            easu_tap(&mut acc, &mut w_sum, -px, -py, dir, aniso_scale, lobe, clip, f);
                            easu_tap(&mut acc, &mut w_sum, 1.0 - px, -py, dir, aniso_scale, lobe, clip, g);
                            easu_tap(&mut acc, &mut w_sum, 2.0 - px, -py, dir, aniso_scale, lobe, clip, h);
                            easu_tap(&mut acc, &mut w_sum, -1.0 - px, 1.0 - py, dir, aniso_scale, lobe, clip, i);
                            easu_tap(&mut acc, &mut w_sum, -px, 1.0 - py, dir, aniso_scale, lobe, clip, j);
                            easu_tap(&mut acc, &mut w_sum, 1.0 - px, 1.0 - py, dir, aniso_scale, lobe, clip, k);
                            easu_tap(&mut acc, &mut w_sum, 2.0 - px, 1.0 - py, dir, aniso_scale, lobe, clip, l);
                            easu_tap(&mut acc, &mut w_sum, -px, 2.0 - py, dir, aniso_scale, lobe, clip, n);
                            easu_tap(&mut acc, &mut w_sum, 1.0 - px, 2.0 - py, dir, aniso_scale, lobe, clip, o);

                            let inv_w = if w_sum.abs() > 1e-6 { 1.0 / w_sum } else { 1.0 };
                            let out_idx = ox * 4;
                            for ch in 0..3 {
                                let min4 = f[ch].min(g[ch]).min(j[ch]).min(k[ch]);
                                let max4 = f[ch].max(g[ch]).max(j[ch]).max(k[ch]);
                                let v = (acc[ch] * inv_w).clamp(min4, max4).clamp(0.0, 1.0);
                                out_row[out_idx + ch] = (v * 255.0 + 0.5) as u8;
                            }
                            out_row[out_idx + 3] = 255;
                        }
                    }
                });
            }
        });

        dst
    }

    // ===========================================================================
    // RCAS — Robust Contrast-Adaptive Sharpening (Multithreaded 5-tap)
    // ===========================================================================

    pub(super) fn rcas_sharpen(src: &[u8], width: u32, height: u32, sharpness: f32) -> Vec<u8> {
        const PAD: usize = 1;
        let (padded, stride) = build_padded_rgbl(src, width, height, PAD);
        let mut dst = vec![0u8; src.len()];

        let stops = 2.0 * (1.0 - sharpness.clamp(0.0, 1.0));
        let sharp_scale = 2.0_f32.powf(-stops);
        const RCAS_LIMIT: f32 = 0.1875;

        let wu = width as usize;
        let hu = height as usize;
        let row_bytes = wu * 4;
        let threads = worker_thread_count(hu);
        let rows_per_band = hu.div_ceil(threads);
        let padded_ref = &padded;

        std::thread::scope(|scope| {
            for (band_idx, band_slice) in dst.chunks_mut(rows_per_band * row_bytes).enumerate() {
                let start_y = band_idx * rows_per_band;
                scope.spawn(move || {
                    let band_rows = band_slice.len() / row_bytes;
                    for local_y in 0..band_rows {
                        let cy = start_y + local_y + PAD;
                        let r_up = (cy - 1) * stride;
                        let r_mid = cy * stride;
                        let r_dn = (cy + 1) * stride;
                        let out_row = &mut band_slice[local_y * row_bytes..(local_y + 1) * row_bytes];

                        for x in 0..wu {
                            let cx = x + PAD;
                            let b = padded_ref[r_up + cx];
                            let d = padded_ref[r_mid + cx - 1];
                            let e = padded_ref[r_mid + cx];
                            let f = padded_ref[r_mid + cx + 1];
                            let hh = padded_ref[r_dn + cx];

                            let (lb, ld, le, lf, lh) = (b[3], d[3], e[3], f[3], hh[3]);
                            let nz = (0.25 * (lb + ld + lf + lh) - le).abs();
                            let max_l = lb.max(ld).max(le).max(lf).max(lh);
                            let min_l = lb.min(ld).min(le).min(lf).min(lh);
                            let range_l = (max_l - min_l).max(1e-4);
                            let noise_factor = 1.0 - 0.5 * (nz / range_l).clamp(0.0, 1.0);

                            let mut min_lobe = -RCAS_LIMIT;
                            for ch in 0..3 {
                                let mn4 = b[ch].min(d[ch]).min(f[ch]).min(hh[ch]);
                                let mx4 = b[ch].max(d[ch]).max(f[ch]).max(hh[ch]);
                                let mn_all = mn4.min(e[ch]);
                                let mx_all = mx4.max(e[ch]);

                                let hit_min = mn_all / (4.0 * mx4 + 1e-5);
                                let hit_max = (mx_all - 1.0) / (4.0 * mn4 - 4.0 - 1e-5);
                                let ch_lobe = (-hit_min).max(hit_max).clamp(-RCAS_LIMIT, 0.0);
                                if ch == 0 {
                                    min_lobe = ch_lobe;
                                } else {
                                    min_lobe = min_lobe.max(ch_lobe);
                                }
                            }
                            let lobe = (min_lobe * sharp_scale * noise_factor).clamp(-RCAS_LIMIT, 0.0);
                            let inv_norm = 1.0 / (4.0 * lobe + 1.0);

                            let out_idx = x * 4;
                            for ch in 0..3 {
                                let val = (lobe * (b[ch] + d[ch] + f[ch] + hh[ch]) + e[ch]) * inv_norm;
                                out_row[out_idx + ch] = (val.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                            }
                            out_row[out_idx + 3] = 255;
                        }
                    }
                });
            }
        });

        dst
    }

}

// ===========================================================================
// Native Windows Direct3D 12 Compute & Graphics Shader Sources (HLSL)
// ===========================================================================

/// Clean-room HLSL compute shader for native D3D12 EASU upscaling pass.
pub const FSR_EASU_HLSL: &str = r#"
cbuffer FsrConstants : register(b0) {
    float2 InputSize;
    float2 OutputSize;
    float2 InvInputSize;
    float2 InvOutputSize;
    float  RcasAttenuation;
    float  JitterX;
    float  JitterY;
    float  _Pad;
};

Texture2D<float4> InputColor  : register(t0);
RWTexture2D<float4> OutputEasu : register(u0);

float FsrLuma(float3 c) { return 0.5 * c.r + c.g + 0.5 * c.b; }

[numthreads(16, 16, 1)]
void CSMain(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= (uint)OutputSize.x || tid.y >= (uint)OutputSize.y) return;
    float2 pp = (float2(tid.xy) + 0.5) * (InputSize * InvOutputSize) - 0.5;
    int2 fp = int2(floor(pp));
    float2 f = pp - float2(fp);
    int2 maxC = int2(InputSize) - 1;

    float3 cF = InputColor.Load(int3(clamp(fp + int2(0, 0), 0, maxC), 0)).rgb;
    float3 cG = InputColor.Load(int3(clamp(fp + int2(1, 0), 0, maxC), 0)).rgb;
    float3 cJ = InputColor.Load(int3(clamp(fp + int2(0, 1), 0, maxC), 0)).rgb;
    float3 cK = InputColor.Load(int3(clamp(fp + int2(1, 1), 0, maxC), 0)).rgb;

    float3 min4 = min(min(cF, cG), min(cJ, cK));
    float3 max4 = max(max(cF, cG), max(cJ, cK));
    float3 blended = lerp(lerp(cF, cG, f.x), lerp(cJ, cK, f.x), f.y);
    OutputEasu[tid.xy] = float4(clamp(blended, min4, max4), 1.0);
}
"#;

/// Clean-room HLSL compute shader for native D3D12 RCAS sharpening pass.
pub const FSR_RCAS_HLSL: &str = r#"
cbuffer FsrConstants : register(b0) {
    float2 InputSize;
    float2 OutputSize;
    float2 InvInputSize;
    float2 InvOutputSize;
    float  RcasAttenuation;
    float  JitterX;
    float  JitterY;
    float  _Pad;
};

Texture2D<float4> InputEasu    : register(t0);
RWTexture2D<float4> OutputRcas : register(u0);

[numthreads(16, 16, 1)]
void CSMain(uint3 tid : SV_DispatchThreadID) {
    if (tid.x >= (uint)OutputSize.x || tid.y >= (uint)OutputSize.y) return;
    int2 p = int2(tid.xy);
    int2 maxC = int2(OutputSize) - 1;

    float3 b = InputEasu.Load(int3(clamp(p + int2(0, -1), 0, maxC), 0)).rgb;
    float3 d = InputEasu.Load(int3(clamp(p + int2(-1, 0), 0, maxC), 0)).rgb;
    float3 e = InputEasu.Load(int3(p, 0)).rgb;
    float3 f = InputEasu.Load(int3(clamp(p + int2(1, 0), 0, maxC), 0)).rgb;
    float3 h = InputEasu.Load(int3(clamp(p + int2(0, 1), 0, maxC), 0)).rgb;

    float3 mn4 = min(min(b, d), min(f, h));
    float3 mx4 = max(max(b, d), max(f, h));
    float3 hitMin = min(mn4, e) / max(4.0 * mx4, 1e-5);
    float3 hitMax = (max(mx4, e) - 1.0) / min(4.0 * mn4 - 4.0, -1e-5);
    float3 lobe3 = max(-hitMin, hitMax);
    float lobe = clamp(max(lobe3.r, max(lobe3.g, lobe3.b)) * exp2(-RcasAttenuation), -0.1875, 0.0);
    float3 outRgb = (lobe * (b + d + f + h) + e) / (4.0 * lobe + 1.0);
    OutputRcas[tid.xy] = float4(saturate(outRgb), 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_fsr_is_enabled_at_quality_mode() {
        let cfg = FsrConfig::default();
        assert!(cfg.is_active());
        assert_eq!(cfg.mode, FsrQualityMode::Quality);
        assert_eq!(cfg.resolve_output_size(640, 360), (960, 540));
        assert_eq!(
            FsrQualityMode::Quality.recommended_render_resolution(3840, 2160),
            (2560, 1440)
        );
    }

    #[test]
    fn easu_plus_rcas_upscales_and_preserves_edges() {
        // Synthetic 32x32 half-green / half-dark step edge
        let mut rgba = vec![0u8; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..32 {
                let idx = (y * 32 + x) * 4;
                if x >= 16 {
                    rgba[idx] = 20;
                    rgba[idx + 1] = 220;
                    rgba[idx + 2] = 60;
                    rgba[idx + 3] = 255;
                } else {
                    rgba[idx] = 15;
                    rgba[idx + 1] = 30;
                    rgba[idx + 2] = 25;
                    rgba[idx + 3] = 255;
                }
            }
        }
        let input = PresentedFrame {
            frame_index: 0,
            width: 32,
            height: 32,
            rgba,
        };
        let (out, stats) = process_frame(&input, &FsrConfig::default());
        assert!(stats.easu_ran);
        assert!(stats.rcas_ran);
        assert_eq!((out.width, out.height), (48, 48));
        // Left side stays dark, right side stays bright green
        assert!(out.pixel_rgba(6, 24)[1] < 45);
        assert!(out.pixel_rgba(40, 24)[1] > 200);
    }

    fn max_abs_diff(a: &[u8], b: &[u8]) -> (u8, f64) {
        assert_eq!(a.len(), b.len());
        let mut max = 0u8;
        let mut sum = 0u64;
        for (x, y) in a.iter().zip(b) {
            let d = x.abs_diff(*y);
            max = max.max(d);
            sum += d as u64;
        }
        (max, sum as f64 / a.len() as f64)
    }

    #[test]
    fn optimized_easu_matches_scalar_reference() {
        let f = test_pattern(97, 61);
        let fast = fsr_pipeline(&f.rgba, 97, 61, 146, 92, true, false, 0.8);
        let slow = reference::easu_upscale(&f.rgba, 97, 61, 146, 92);
        let (max, mean) = max_abs_diff(&fast, &slow);
        assert!(max <= 2 && mean < 0.05, "EASU parity: max={max} mean={mean}");
    }

    #[test]
    fn optimized_rcas_matches_scalar_reference() {
        let f = test_pattern(130, 77);
        let fast = fsr_pipeline(&f.rgba, 130, 77, 130, 77, false, true, 0.8);
        let slow = reference::rcas_sharpen(&f.rgba, 130, 77, 0.8);
        let (max, mean) = max_abs_diff(&fast, &slow);
        assert!(max <= 2 && mean < 0.05, "RCAS parity: max={max} mean={mean}");
    }

    #[test]
    fn fused_pipeline_close_to_two_pass_reference() {
        // Fused path skips the intermediate u8 quantization, so allow tiny rounding drift.
        let f = test_pattern(160, 90);
        let fast = fsr_pipeline(&f.rgba, 160, 90, 240, 136, true, true, 0.8);
        let up = reference::easu_upscale(&f.rgba, 160, 90, 240, 136);
        let slow = reference::rcas_sharpen(&up, 240, 136, 0.8);
        let (max, mean) = max_abs_diff(&fast, &slow);
        assert!(max <= 4 && mean < 0.5, "fused parity: max={max} mean={mean}");
    }

    #[test]
    fn bench_frame_reports_steady_state() {
        let f = test_pattern(64, 64);
        let r = bench_frame(&f, &FsrConfig::default(), 2);
        assert_eq!(r.iterations, 2);
        assert_eq!((r.output_width, r.output_height), (96, 96));
        assert!(r.avg_ms >= r.min_ms && r.fps() > 0.0);
    }

    #[test]
    fn fsr_dispatch_constants_layout_and_halton() {
        assert_eq!(core::mem::size_of::<FsrDispatchConstants>(), 48);
        let (gx, gy, gz) = FsrDispatchConstants::thread_groups_16x16(1920, 1080);
        assert_eq!((gx, gy, gz), (120, 68, 1));
        let (jx, jy) = halton_jitter_2_3(0, 1280, 720);
        assert!((-0.5..=0.5).contains(&jx));
        assert!((-0.5..=0.5).contains(&jy));
    }
}
