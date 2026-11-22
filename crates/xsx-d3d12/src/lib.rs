//! Xbox Series X Direct3D 12.x (`d3d12_x.dll`) translation layer, default AMD FSR
//! upscaling pipeline, DXBC/DXIL shader container translator, UMA staging ring,
//! and native Windows / software GPU backends.

pub mod backend;
pub mod com_objects;
pub mod fsr;
pub mod native_win;
pub mod shader;
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
pub use shader::{translate_xbox_shader_container, ShaderContainer, TranslatedShader};
pub use software::SoftwareD3D12Backend;
pub use staging::UmaStagingRing;
