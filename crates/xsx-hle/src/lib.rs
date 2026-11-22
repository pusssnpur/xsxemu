//! High-Level Emulation (HLE) of Xbox Series X OS & Microsoft GDK runtime DLLs.
//!
//! Implements Win64 ABI thunks and COM objects for:
//! - `kernelx.dll` / `kernelbase.dll` / `kernel32.dll` / `api-ms-win-core-*` (incl. RTC & VFS)
//! - `xgameruntime.dll` (`XGameRuntime`, `XSystem`, `XTaskQueue`, `XUser`, `XGameSave`, `XStore`)
//! - `d3d12_x.dll` / `d3d12_xs.dll` / `d3d12.dll`
//! - `xinput1_4.dll` / `gameinput.dll`
//! - `xaudio2_9.dll` (multi-voice stereo AudioMixer + Windows winmm playback + WAV export)
//! - `dstorage_x.dll` / `dstorage.dll`

pub mod audio_mixer;
pub mod d3d12_x;
pub mod dstorage;
pub mod input;
pub mod kernelx;
pub mod registry;
pub mod rtc;
pub mod runtime;
pub mod services;
pub mod vfs;
pub mod xaudio2;
pub mod xgameruntime;

pub use audio_mixer::{generate_sine_pcm16, AudioMixer};
pub use input::VirtualGamepadState;
pub use registry::ModuleRegistry;
pub use rtc::RtcConfig;
pub use runtime::{GuestRuntime, RuntimeGuard};
pub use vfs::VirtualFs;
