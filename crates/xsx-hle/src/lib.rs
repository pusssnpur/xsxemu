//! High-Level Emulation (HLE) of Xbox Series X OS & Microsoft GDK runtime DLLs.
//!
//! Implements Win64 ABI thunks and COM objects for:
//! - `kernelx.dll` / `kernelbase.dll` / `kernel32.dll` / `api-ms-win-core-*` (incl. RTC, CoW VFS, pseudo-handle loader)
//! - `xmem.dll` (`XMemAlloc`, `XMemFree`, `XMemVirtualAlloc`, `XMemVirtualQuery`, `XMemGetWorkingSetStatistics`)
//! - `ucrtbase.dll` / `api-ms-win-crt-*` / `msvcrt.dll` (guest command-line & CRT startup)
//! - `xgameruntime.dll` (`InitializeApiImpl*`, `QueryApiImpl` COM singletons, `XAsync`/`XTaskQueue`, `XSystem`, `XPackage`, `XUser`, `XGameSave`, `XStore`)
//! - `d3d12_x.dll` / `d3d12_xs.dll` / `d3d12.dll` (84-slot measured Xbox ABI proxy + root signature sanitizer)
//! - `xinput1_4.dll` / `gameinput.dll`
//! - `xaudio2_9.dll` / `xaudio2_9redist.dll` (multi-voice stereo AudioMixer + Windows winmm playback + WAV export)
//! - `dstorage_x.dll` / `dstorage_xs.dll` / `dstorage.dll`

pub mod audio_mixer;
pub mod d3d12_x;
pub mod dstorage;
pub mod input;
pub mod kernelx;
pub mod process;
pub mod registry;
pub mod rtc;
pub mod runtime;
pub mod services;
pub mod startup_trace;
pub mod vfs;
pub mod xaudio2;
pub mod xgameruntime;
pub mod xmem;

pub use audio_mixer::{generate_sine_pcm16, AudioMixer};
pub use input::VirtualGamepadState;
pub use process::{last_guest_exit_code, set_guest_command_line};
pub use registry::{
    classify_import_return_class, get_unimplemented_policy, normalize_dll,
    set_unimplemented_policy, ModuleRegistry, ReturnClass, UnimplementedPolicy,
};
pub use rtc::RtcConfig;
pub use runtime::{GuestRuntime, RuntimeGuard};
pub use startup_trace::StartupTracer;
pub use vfs::{VfsDirEntry, VirtualFs, XboxDrive};
