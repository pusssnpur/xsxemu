//! Clean-room synthetic Xbox Series X PE32+ (`AMD64`) test binary generator.
//!
//! Emits spec-compliant x86_64 PE32+ executables and DLLs across all roadmap phases:
//! - `hello_xsx.exe`: Phase 1 vertical-slice GDK + D3D12.x RGB triangle sample
//! - `xsx_helper.dll` + `phase2_xsx.exe`: Phase 2 multi-module runtime suite
//! - `full_stack_xsx.exe`: Phase 3 + Phase 4 full-stack suite (DXBC shader stripping,
//!   PSO/RootSignature, Compute Dispatch, FSR, DirectStorage, XInput, XAudio2, XUser, XGameSave, XStore)

pub mod full_stack_suite;
pub mod pe_builder;
pub mod phase2_suite;
pub mod real_game_suite;
pub mod x64_asm;

pub use full_stack_suite::{
    build_full_stack_xsx_pe, FULLSTACK_ASSET_PAYLOAD, FULLSTACK_SAVE_PAYLOAD,
};
pub use pe_builder::build_hello_xsx_pe;
pub use phase2_suite::{
    build_helper_dll_pe, build_phase2_xsx_pe, HELPER_MAGIC, PHASE2_FILE_PAYLOAD, WORKER_EXIT_CODE,
};
pub use real_game_suite::{
    build_real_game_xsx_pe, REALGAME_EXIT_CODE, REALGAME_OVERLAY_CONFIG, REALGAME_PKG_CONFIG,
};
