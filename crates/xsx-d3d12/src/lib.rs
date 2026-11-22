//! Xbox Series X Direct3D 12.x (`d3d12_x.dll`) translation layer and GPU backends.
//!
//! Provides:
//! - Xbox D3D12.x -> PC Direct3D 12 parameter/heap/`PresentX` translation (`translate`)
//! - Binary-compatible Win64 COM objects (`com_objects`)
//! - Native Windows x86_64 Direct3D 12 backend (`native_win`)
//! - Cross-platform software D3D12 command executor & rasterizer (`software`) for CI and verification

pub mod backend;
pub mod com_objects;
pub mod fsr;
pub mod native_win;
pub mod software;
pub mod translate;

pub use backend::{
    BackendKind, D3D12Translator, GpuBackend, GpuCommand, PresentedFrame, ResourceKind,
    TranslatorConfig,
};
pub use com_objects::create_device_x_com;
pub use fsr::{FsrConfig, FsrDispatchConstants, FsrFrameStats, FsrQualityMode};
pub use software::SoftwareD3D12Backend;
