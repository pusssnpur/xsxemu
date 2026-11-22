//! Xbox Series X Direct3D 12.x (`d3d12_x.dll`) translation layer, multithreaded default
//! AMD FSR upscaling pipeline, DXBC/DXIL shader translator + persistent cache, UMA staging
//! ring, and true hardware Windows / software GPU backends.

pub mod backend;
pub mod com_objects;
pub mod fsr;
pub mod native_win;
pub mod shader;
pub mod shader_cache;
pub mod software;
pub mod staging;
pub mod translate;

pub use backend::{
    BackendKind, D3D12Translator, GpuBackend, GpuCommand, PresentedFrame, ResourceKind,
    TranslatorConfig,
};
pub use com_objects::create_device_x_com;
pub use fsr::{
    FsrComputePlan, FsrConfig, FsrDispatchConstants, FsrFrameStats, FsrQualityMode,
    FsrTemporalInputs,
};
pub use shader::{compute_dxbc_hash, translate_xbox_shader_container, ShaderContainer, TranslatedShader};
pub use shader_cache::ShaderCache;
pub use software::SoftwareD3D12Backend;
pub use staging::UmaStagingRing;
