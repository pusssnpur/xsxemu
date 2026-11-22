//! Xbox Series X hardware BCPack MDU rANS entropy decoder (MIT / Apache-2.0).
//!
//! Ported from Microsoft's MIT-licensed `silicon-rans` hardware MDU reference via
//! `bcpack-rans` (MIT OR Apache-2.0; see `THIRD_PARTY_NOTICES.md`). Used by DirectStorage
//! (`dstorage_xs.dll`) and texture streaming on Xbox Series X|S to decompress BC1–BC7 streams.

#![allow(dead_code)]

pub mod rans;
pub mod tables;

pub use rans::{decode_lanes, is_legal_end_state, DecodeError, Model};
