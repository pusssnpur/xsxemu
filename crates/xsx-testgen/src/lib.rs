//! Clean-room synthetic Xbox Series X PE32+ (`AMD64`) test binary generator.
//!
//! Emits spec-compliant x86_64 PE32+ executables and DLLs:
//! - `hello_xsx.exe`: vertical-slice GDK + D3D12.x RGB triangle sample
//! - `xsx_helper.dll` + `phase2_xsx.exe`: Phase 2 multi-module suite exercising
//!   secondary DLL loading, Export Tables, TLS callbacks, `.pdata`, virtual Xbox
//!   `T:\` file I/O, real multithreading, and default-enabled FSR upscaling.

pub mod pe_builder;
pub mod phase2_suite;
pub mod x64_asm;

pub use pe_builder::build_hello_xsx_pe;
pub use phase2_suite::{
    build_helper_dll_pe, build_phase2_xsx_pe, HELPER_MAGIC, PHASE2_FILE_PAYLOAD, WORKER_EXIT_CODE,
};
