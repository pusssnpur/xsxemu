//! Core GPU backend abstraction, command IR, framebuffer representation, UMA staging,
//! shader translation telemetry, and `D3D12Translator`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use xsx_abi::d3d12::{
    D3D12HeapProperties, D3D12Rect, D3D12ResourceDesc, D3D12ResourceDimension,
    D3D12VertexBufferView, D3D12Viewport,
};

use crate::fsr::{self, FsrComputePlan, FsrConfig, FsrFrameStats, FsrTemporalInputs};
use crate::native_win;
use crate::shader::{TranslatedShader, DXBC_MAGIC};
use crate::shader_cache::ShaderCache;
use crate::software::SoftwareD3D12Backend;
use crate::staging::UmaStagingRing;
use crate::translate::{translate_heap_properties, HostDeviceCreateConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Use native Windows D3D12 on Windows x86_64, software D3D12 otherwise.
    Auto,
    /// Force native Windows D3D12 (`d3d12.dll` + `dxgi.dll`).
    NativeD3D12,
    /// Force software D3D12 command executor & rasterizer (works headless on all OSes).
    Software,
}

#[derive(Debug, Clone)]
pub struct TranslatorConfig {
    pub backend: BackendKind,
    pub trace_calls: bool,
    pub default_width: u32,
    pub default_height: u32,
    /// AMD FSR upscaling & RCAS sharpening pipeline (enabled by default at Quality 1.5x).
    pub fsr: FsrConfig,
    /// Show a visible Win32 presentation window on Windows x86_64 (false = hidden/headless).
    pub windowed: bool,
    /// Optional on-disk shader cache directory for translated Xbox -> PC DXBC/DXIL containers.
    pub shader_cache_dir: Option<PathBuf>,
}

impl Default for TranslatorConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::Auto,
            trace_calls: false,
            default_width: 640,
            default_height: 360,
            fsr: FsrConfig::default(),
            windowed: false,
            shader_cache_dir: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Buffer { size_bytes: usize },
    Texture2D { width: u32, height: u32, format: u32 },
}

/// Recorded Direct3D 12 graphics & compute commands.
#[derive(Debug, Clone)]
pub enum GpuCommand {
    ResourceBarrier {
        resource_id: u64,
        before: u32,
        after: u32,
    },
    SetViewport(D3D12Viewport),
    SetScissor(D3D12Rect),
    SetRenderTarget {
        rtv_handle: usize,
    },
    ClearRenderTarget {
        rtv_handle: usize,
        color: [f32; 4],
        rects: Vec<D3D12Rect>,
    },
    SetPrimitiveTopology(u32),
    SetVertexBuffer {
        slot: u32,
        view: D3D12VertexBufferView,
    },
    DrawInstanced {
        vertex_count: u32,
        instance_count: u32,
        start_vertex: u32,
        start_instance: u32,
    },
    SetPipelineState {
        pso_id: u64,
    },
    SetGraphicsRootSignature {
        root_sig_id: u64,
    },
    SetComputeRootSignature {
        root_sig_id: u64,
    },
    Dispatch {
        x: u32,
        y: u32,
        z: u32,
    },
    CopyResource {
        dst_id: u64,
        src_id: u64,
    },
}

/// Captured RGBA8 framebuffer produced by `PresentX` (after default FSR processing).
#[derive(Debug, Clone)]
pub struct PresentedFrame {
    pub frame_index: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl PresentedFrame {
    /// Save this RGBA8 framebuffer as a binary PPM (`P6`) image.
    pub fn write_ppm(&self, path: &Path) -> io::Result<()> {
        let mut f = File::create(path)?;
        write!(f, "P6\n{} {}\n255\n", self.width, self.height)?;
        let mut rgb = Vec::with_capacity((self.width * self.height * 3) as usize);
        for px in self.rgba.chunks_exact(4) {
            rgb.push(px[0]);
            rgb.push(px[1]);
            rgb.push(px[2]);
        }
        f.write_all(&rgb)
    }

    /// Encode this RGBA8 framebuffer as an uncompressed 24-bit Windows BMP byte buffer.
    pub fn encode_bmp(&self) -> Vec<u8> {
        let row_stride = ((self.width * 3 + 3) & !3) as usize;
        let pixel_bytes = row_stride * self.height as usize;
        let file_size = 54 + pixel_bytes;

        let mut buf = Vec::with_capacity(file_size);
        buf.extend_from_slice(b"BM");
        buf.extend_from_slice(&(file_size as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&54u32.to_le_bytes());
        buf.extend_from_slice(&40u32.to_le_bytes());
        buf.extend_from_slice(&(self.width as i32).to_le_bytes());
        buf.extend_from_slice(&(self.height as i32).to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&24u16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&(pixel_bytes as u32).to_le_bytes());
        buf.extend_from_slice(&2835i32.to_le_bytes());
        buf.extend_from_slice(&2835i32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());

        for y in (0..self.height as usize).rev() {
            let row_start = y * self.width as usize * 4;
            for x in 0..self.width as usize {
                let off = row_start + x * 4;
                buf.push(self.rgba[off + 2]);
                buf.push(self.rgba[off + 1]);
                buf.push(self.rgba[off]);
            }
            for _ in (self.width as usize * 3)..row_stride {
                buf.push(0);
            }
        }
        buf
    }

    /// Save this RGBA8 framebuffer as an uncompressed 24-bit Windows BMP image.
    pub fn write_bmp(&self, path: &Path) -> io::Result<()> {
        let buf = self.encode_bmp();
        let mut f = File::create(path)?;
        f.write_all(&buf)
    }

    pub fn pixel_rgba(&self, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[idx],
            self.rgba[idx + 1],
            self.rgba[idx + 2],
            self.rgba[idx + 3],
        ]
    }
}

/// Host GPU backend interface implemented by `NativeD3D12Backend` (Windows) and `SoftwareD3D12Backend`.
pub trait GpuBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn create_resource(
        &mut self,
        id: u64,
        heap_props: &D3D12HeapProperties,
        heap_flags: u32,
        desc: &D3D12ResourceDesc,
        initial_state: u32,
    ) -> Result<u64, String>;

    fn map_resource(&mut self, id: u64) -> Result<*mut u8, String>;
    fn unmap_resource(&mut self, id: u64);

    fn create_rtv(&mut self, rtv_handle: usize, resource_id: u64);

    fn execute_commands(&mut self, commands: &[GpuCommand]) -> Result<(), String>;

    fn wait_frame_latency(&mut self, _timeout_ms: u32) {}

    fn present_resource(
        &mut self,
        resource_id: Option<u64>,
        frame_index: u64,
        sync_interval: u32,
        fsr_plan: Option<&FsrComputePlan>,
    ) -> Result<Option<PresentedFrame>, String>;
}

/// Central thread-safe Xbox `d3d12_x` translation state shared by all live COM objects.
pub struct D3D12Translator {
    config: TranslatorConfig,
    backend: Mutex<Box<dyn GpuBackend>>,
    next_id: AtomicU64,
    next_frame_token: AtomicU64,
    next_rtv_handle: AtomicU64,
    dispatch_count: AtomicU64,
    fences: Mutex<HashMap<u64, u64>>,
    staging_ring: Mutex<UmaStagingRing>,
    shader_cache: Mutex<ShaderCache>,
    translated_shaders: Mutex<Vec<TranslatedShader>>,
    presented_frames: Mutex<Vec<PresentedFrame>>,
    fsr_stats: Mutex<Vec<FsrFrameStats>>,
    trace_log: Mutex<Vec<String>>,
    backend_name: &'static str,
}

impl D3D12Translator {
    pub fn new(
        config: TranslatorConfig,
        device_cfg: HostDeviceCreateConfig,
    ) -> Result<Arc<Self>, String> {
        let backend: Box<dyn GpuBackend> = match config.backend {
            BackendKind::Software => Box::new(SoftwareD3D12Backend::new(
                config.default_width,
                config.default_height,
            )),
            BackendKind::NativeD3D12 => native_win::create_native_backend(
                config.default_width,
                config.default_height,
                device_cfg,
                config.windowed,
            )?,
            BackendKind::Auto => {
                #[cfg(target_os = "windows")]
                {
                    match native_win::create_native_backend(
                        config.default_width,
                        config.default_height,
                        device_cfg,
                        config.windowed,
                    ) {
                        Ok(b) => b,
                        Err(_) => Box::new(SoftwareD3D12Backend::new(
                            config.default_width,
                            config.default_height,
                        )),
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = device_cfg;
                    Box::new(SoftwareD3D12Backend::new(
                        config.default_width,
                        config.default_height,
                    ))
                }
            }
        };

        let backend_name = backend.name();
        let shader_cache = ShaderCache::new(config.shader_cache_dir.clone());
        Ok(Arc::new(Self {
            config,
            backend: Mutex::new(backend),
            next_id: AtomicU64::new(1),
            next_frame_token: AtomicU64::new(1),
            next_rtv_handle: AtomicU64::new(0x1000_0000),
            dispatch_count: AtomicU64::new(0),
            fences: Mutex::new(HashMap::new()),
            staging_ring: Mutex::new(UmaStagingRing::new(4 * 1024 * 1024)),
            shader_cache: Mutex::new(shader_cache),
            translated_shaders: Mutex::new(Vec::new()),
            presented_frames: Mutex::new(Vec::new()),
            fsr_stats: Mutex::new(Vec::new()),
            trace_log: Mutex::new(Vec::new()),
            backend_name,
        }))
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    pub fn fsr_config(&self) -> &FsrConfig {
        &self.config.fsr
    }

    pub fn fsr_stats(&self) -> Vec<FsrFrameStats> {
        self.fsr_stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn translated_shaders(&self) -> Vec<TranslatedShader> {
        self.translated_shaders
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Return `(cache_hits, cache_misses)` for the shader translation cache.
    pub fn shader_cache_stats(&self) -> (u64, u64) {
        let c = self.shader_cache.lock().unwrap();
        (c.hits(), c.misses())
    }

    pub fn dispatch_count(&self) -> u64 {
        self.dispatch_count.load(Ordering::Relaxed)
    }

    pub fn alloc_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn next_frame_token(&self) -> u64 {
        self.next_frame_token.fetch_add(1, Ordering::Relaxed)
    }

    pub fn wait_frame_event(&self, timeout_ms: u32) -> u64 {
        self.backend.lock().unwrap().wait_frame_latency(timeout_ms);
        self.next_frame_token()
    }

    pub fn alloc_descriptor_range(&self, count: u32, increment: usize) -> usize {
        let span = (count.max(1) as u64) * (increment as u64);
        self.next_rtv_handle.fetch_add(span, Ordering::Relaxed) as usize
    }

    pub fn record_trace(&self, msg: impl Into<String>) {
        let s = msg.into();
        if self.config.trace_calls {
            eprintln!("[d3d12_x] {s}");
        }
        if let Ok(mut log) = self.trace_log.lock() {
            log.push(s);
        }
    }

    pub fn trace_log(&self) -> Vec<String> {
        self.trace_log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    pub fn presented_frames(&self) -> Vec<PresentedFrame> {
        self.presented_frames
            .lock()
            .map(|f| f.clone())
            .unwrap_or_default()
    }

    pub fn create_root_signature(&self, blob: &[u8]) -> u64 {
        let id = self.alloc_id();
        self.record_trace(format!(
            "ID3D12DeviceX::CreateRootSignature(id={id}, blob_bytes={})",
            blob.len()
        ));
        id
    }

    fn inspect_and_translate_shader(&self, stage: &str, bytecode: &[u8]) {
        if bytecode.len() >= 4 && bytecode[0..4] == DXBC_MAGIC {
            let res = self.shader_cache.lock().unwrap().get_or_translate(bytecode);
            match res {
                Ok(tr) => {
                    let (hits, misses) = self.shader_cache_stats();
                    self.record_trace(format!(
                        "ShaderTranslate({stage}): dxil={}, stripped={:?}, retained={:?} (cache hits={hits}, misses={misses})",
                        tr.has_dxil, tr.stripped_chunks, tr.retained_chunks
                    ));
                    self.translated_shaders.lock().unwrap().push(tr);
                }
                Err(e) => {
                    self.record_trace(format!("ShaderTranslate({stage}): passthrough ({e})"));
                }
            }
        }
    }

    pub fn create_graphics_pso(&self, vs: &[u8], ps: &[u8]) -> u64 {
        let id = self.alloc_id();
        self.inspect_and_translate_shader("VS", vs);
        self.inspect_and_translate_shader("PS", ps);
        self.record_trace(format!(
            "ID3D12DeviceX::CreateGraphicsPipelineState(id={id}, vs={}B, ps={}B)",
            vs.len(),
            ps.len()
        ));
        id
    }

    pub fn create_compute_pso(&self, cs: &[u8]) -> u64 {
        let id = self.alloc_id();
        self.inspect_and_translate_shader("CS", cs);
        self.record_trace(format!(
            "ID3D12DeviceX::CreateComputePipelineState(id={id}, cs={}B)",
            cs.len()
        ));
        id
    }

    pub fn create_committed_resource(
        &self,
        heap_props: &D3D12HeapProperties,
        xbox_heap_flags: u32,
        desc: &D3D12ResourceDesc,
        initial_state: u32,
    ) -> Result<(u64, u64, ResourceKind), String> {
        let id = self.alloc_id();
        let (host_props, host_flags) = translate_heap_properties(heap_props, xbox_heap_flags);
        let kind = if desc.dimension == D3D12ResourceDimension::Texture2D as u32 {
            ResourceKind::Texture2D {
                width: desc.width as u32,
                height: desc.height,
                format: desc.format,
            }
        } else {
            ResourceKind::Buffer {
                size_bytes: desc.width as usize,
            }
        };

        let gpu_va = self
            .backend
            .lock()
            .unwrap()
            .create_resource(id, &host_props, host_flags, desc, initial_state)?;

        self.record_trace(format!(
            "CreateCommittedResource(id={id}, kind={kind:?}, xbox_flags=0x{xbox_heap_flags:X} -> host_flags=0x{host_flags:X}, gpu_va=0x{gpu_va:X})"
        ));
        Ok((id, gpu_va, kind))
    }

    pub fn stage_uma_upload(&self, dst_resource_id: u64, data: &[u8], fence_value: u64) -> Result<(), String> {
        let mut ring = self.staging_ring.lock().unwrap();
        let alloc = ring.stage_copy(data, 256, dst_resource_id, 0, fence_value)?;
        self.record_trace(format!(
            "UmaStagingRing::stage(res={dst_resource_id}, bytes={}, off=0x{:X}, fence={fence_value})",
            data.len(),
            alloc.offset
        ));
        Ok(())
    }

    pub fn map_resource(&self, id: u64) -> Result<*mut u8, String> {
        self.backend.lock().unwrap().map_resource(id)
    }

    pub fn unmap_resource(&self, id: u64) {
        self.backend.lock().unwrap().unmap_resource(id);
    }

    pub fn create_rtv(&self, rtv_handle: usize, resource_id: u64) {
        self.record_trace(format!(
            "CreateRenderTargetView(rtv=0x{rtv_handle:X}, resource_id={resource_id})"
        ));
        self.backend
            .lock()
            .unwrap()
            .create_rtv(rtv_handle, resource_id);
    }

    pub fn create_fence(&self, initial_value: u64) -> u64 {
        let id = self.alloc_id();
        self.fences.lock().unwrap().insert(id, initial_value);
        self.record_trace(format!("CreateFence(id={id}, initial={initial_value})"));
        id
    }

    pub fn signal_fence(&self, id: u64, value: u64) {
        if let Some(slot) = self.fences.lock().unwrap().get_mut(&id) {
            *slot = value;
        }
        let retired = self.staging_ring.lock().unwrap().retire_up_to(value);
        self.record_trace(format!(
            "CommandQueue::Signal(fence={id}, value={value}, staging_retired={retired})"
        ));
    }

    pub fn fence_value(&self, id: u64) -> u64 {
        self.fences.lock().unwrap().get(&id).copied().unwrap_or(0)
    }

    pub fn execute_command_list(&self, commands: &[GpuCommand]) -> Result<(), String> {
        let dispatches = commands
            .iter()
            .filter(|c| matches!(c, GpuCommand::Dispatch { .. }))
            .count() as u64;
        if dispatches > 0 {
            self.dispatch_count.fetch_add(dispatches, Ordering::Relaxed);
        }
        let pending_copies = self.staging_ring.lock().unwrap().drain_pending_copies();
        if !pending_copies.is_empty() {
            self.record_trace(format!(
                "UmaStagingRing::flush({} pending GPU copies)",
                pending_copies.len()
            ));
        }
        self.record_trace(format!(
            "CommandQueue::ExecuteCommandLists(commands={}, dispatches={dispatches})",
            commands.len()
        ));
        self.backend.lock().unwrap().execute_commands(commands)
    }

    pub fn present_x(
        &self,
        resource_id: Option<u64>,
        frame_token: u64,
        sync_interval: u32,
    ) -> Result<(), String> {
        self.record_trace(format!(
            "CommandQueue::PresentX(resource={resource_id:?}, token={frame_token}, sync={sync_interval})"
        ));
        let frame_idx = self.presented_frames.lock().unwrap().len() as u64;
        let fsr_plan = FsrComputePlan::build(
            &self.config.fsr,
            self.config.default_width,
            self.config.default_height,
            frame_idx,
            FsrTemporalInputs::default(),
        );
        if let Some(raw_frame) = self.backend.lock().unwrap().present_resource(
            resource_id,
            frame_idx,
            sync_interval,
            Some(&fsr_plan),
        )? {
            let (final_frame, stats) = fsr::process_frame(&raw_frame, &self.config.fsr);
            if self.config.fsr.is_active() {
                self.record_trace(format!(
                    "FSR({}): {}x{} -> {}x{} (easu={}, rcas={}, sharpness={:.2}, {}us)",
                    stats.mode.name(),
                    stats.input_width,
                    stats.input_height,
                    stats.output_width,
                    stats.output_height,
                    stats.easu_ran,
                    stats.rcas_ran,
                    stats.sharpness,
                    stats.duration_us
                ));
            }
            self.fsr_stats.lock().unwrap().push(stats);
            self.presented_frames.lock().unwrap().push(final_frame);
        }
        Ok(())
    }
}
