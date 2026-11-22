//! Clean-room synthetic Xbox Series X PE32+ (`AMD64`) test binary generator.
//!
//! Emits a spec-compliant x86_64 PE32+ executable (`hello_xsx.exe`) with real
//! x86_64 machine code, `.idata` imports from `xgameruntime.dll`, `kernelx.dll`,
//! and `d3d12_x.dll`, and `.reloc` base relocations that renders an RGB triangle
//! via COM vtable calls and presents via `ID3D12CommandQueueX::PresentX`.

pub mod pe_builder;
pub mod x64_asm;

pub use pe_builder::build_hello_xsx_pe;
