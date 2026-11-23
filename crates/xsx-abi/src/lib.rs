//! Clean-room Win64 / Xbox Series X|S binary ABI definitions.
//!
//! All types in this crate use `#[repr(C)]` and `extern "win64"` calling
//! conventions so that guest PE32+ (AMD64) executables can invoke HLE thunks
//! and COM vtables identically on both Windows x86_64 and Linux x86_64 hosts.

pub mod com;
pub mod d3d12;
pub mod gdk;
pub mod services;
