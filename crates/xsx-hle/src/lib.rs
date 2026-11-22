//! High-Level Emulation (HLE) of Xbox Series X OS & Microsoft GDK runtime DLLs.
//!
//! Implements Win64 ABI thunks for:
//! - `kernelx.dll` / `kernelbase.dll` / `kernel32.dll` / `api-ms-win-core-*`
//! - `xgameruntime.dll`
//! - `d3d12_x.dll` / `d3d12_xs.dll` / `d3d12.dll`

pub mod d3d12_x;
pub mod kernelx;
pub mod registry;
pub mod runtime;
pub mod vfs;
pub mod xgameruntime;

pub use registry::ModuleRegistry;
pub use runtime::{GuestRuntime, RuntimeGuard};
pub use vfs::VirtualFs;
