//! Xbox Series X (`d3d12_xs.dll`) Root Signature sanitization & v1.2 -> v1.1 downgrade.
//!
//! Real Xbox Series X titles set console-specific bits in the high byte of
//! `D3D12_ROOT_SIGNATURE_FLAGS` (observed `0x0300_0000`), which the PC Windows `d3d12.dll`
//! serializer and deserializer reject with `E_INVALIDARG`.
//!
//! This module strips the Xbox high byte on:
//! - Legacy `D3D12RootSignatureDesc`
//! - Versioned `D3D12VersionedRootSignatureDesc` (1.0, 1.1, 1.2)
//! - Serialized `DXBC` containers holding an `RTS0` chunk (recomputing the exact Microsoft
//!   DXBC MD5-variant digest so `D3D12CreateRootSignatureDeserializer` accepts the blob)
//!
//! It also downgrades version 1.2 descriptions to 1.1 when no static sampler uses 1.2 flags,
//! supporting Windows 10 hosts whose built-in `d3d12.dll` only serializes up to 1.1.

use xsx_abi::d3d12::{
    D3D12RootSignatureDesc, D3D12StaticSamplerDesc, D3D12StaticSamplerDesc1,
    D3D12VersionedRootSignatureDesc, D3D_ROOT_SIGNATURE_VERSION_1_0,
    D3D_ROOT_SIGNATURE_VERSION_1_1, D3D_ROOT_SIGNATURE_VERSION_1_2,
    XBOX_ROOT_SIGNATURE_FLAG_HIGH_BYTE,
};

use crate::shader::compute_dxbc_hash;

/// Clear the Xbox-only high byte (`0xFF00_0000`) from a root signature flags word.
#[inline]
pub const fn clear_xbox_root_sig_flags(flags: u32) -> u32 {
    flags & !XBOX_ROOT_SIGNATURE_FLAG_HIGH_BYTE
}

/// Copy a legacy `D3D12RootSignatureDesc` with Xbox high-byte flags cleared.
pub fn sanitize_root_signature_desc(desc: &D3D12RootSignatureDesc) -> D3D12RootSignatureDesc {
    let mut out = *desc;
    out.flags = clear_xbox_root_sig_flags(out.flags);
    out
}

/// Copy a `D3D12VersionedRootSignatureDesc` with Xbox high-byte flags cleared.
/// Returns `None` if `desc.version` is unrecognized (caller should forward unchanged).
pub fn sanitize_versioned_root_signature_desc(
    desc: &D3D12VersionedRootSignatureDesc,
) -> Option<D3D12VersionedRootSignatureDesc> {
    match desc.version {
        D3D_ROOT_SIGNATURE_VERSION_1_0
        | D3D_ROOT_SIGNATURE_VERSION_1_1
        | D3D_ROOT_SIGNATURE_VERSION_1_2 => {
            let mut out = *desc;
            out.desc.flags = clear_xbox_root_sig_flags(out.desc.flags);
            Some(out)
        }
        _ => None,
    }
}

/// Downgrade a sanitized Root Signature 1.2 description to 1.1 when no static sampler
/// sets non-zero v1.2 `flags`. Returns the downgraded v1.1 descriptor and backing sampler
/// storage (whose lifetime must outlive the returned `D3D12VersionedRootSignatureDesc`).
///
/// # Safety
/// If `desc.desc.num_static_samplers > 0`, `desc.desc.p_static_samplers` must point to a
/// valid array of `num_static_samplers` `D3D12StaticSamplerDesc1` structs.
pub unsafe fn downgrade_root_signature_1_2_to_1_1(
    desc: &D3D12VersionedRootSignatureDesc,
) -> Option<(D3D12VersionedRootSignatureDesc, Vec<D3D12StaticSamplerDesc>)> {
    if desc.version != D3D_ROOT_SIGNATURE_VERSION_1_2 {
        return None;
    }
    let count = desc.desc.num_static_samplers as usize;
    if count > 0 && desc.desc.p_static_samplers.is_null() {
        return None;
    }
    let mut samplers = Vec::with_capacity(count);
    if count > 0 {
        let src = core::slice::from_raw_parts(
            desc.desc.p_static_samplers as *const D3D12StaticSamplerDesc1,
            count,
        );
        for s in src {
            if s.flags != 0 {
                return None;
            }
            samplers.push(s.base);
        }
    }

    let mut v11 = D3D12VersionedRootSignatureDesc {
        version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        _pad: 0,
        desc: D3D12RootSignatureDesc {
            num_parameters: desc.desc.num_parameters,
            _pad0: 0,
            p_parameters: desc.desc.p_parameters,
            num_static_samplers: desc.desc.num_static_samplers,
            _pad1: 0,
            p_static_samplers: if samplers.is_empty() {
                core::ptr::null()
            } else {
                samplers.as_ptr() as *const core::ffi::c_void
            },
            flags: clear_xbox_root_sig_flags(desc.desc.flags),
            _pad2: 0,
        },
    };
    // Ensure pointer points to the Vec buffer after returning: caller must pin or update ptr
    // if moved; however Vec's heap allocation address stays stable across moves!
    if !samplers.is_empty() {
        v11.desc.p_static_samplers = samplers.as_ptr() as *const core::ffi::c_void;
    }
    Some((v11, samplers))
}

/// Inspect a serialized `DXBC` root signature blob (`RTS0` chunk). If the `RTS0` header's
/// `flags` word (at payload offset +20) has any Xbox high-byte bits set, return a rewritten
/// copy with those bits cleared and the 16-byte Microsoft DXBC MD5-variant checksum updated.
///
/// Returns `None` if the blob is not DXBC, has no `RTS0` chunk, or already has no Xbox flags.
pub fn sanitize_rts0_blob(blob: &[u8]) -> Option<Vec<u8>> {
    if blob.len() < 36 || &blob[0..4] != b"DXBC" {
        return None;
    }
    let chunk_count = u32::from_le_bytes(blob[28..32].try_into().ok()?) as usize;
    if chunk_count == 0 || chunk_count > 16 || 32 + chunk_count * 4 > blob.len() {
        return None;
    }

    for i in 0..chunk_count {
        let off = u32::from_le_bytes(blob[32 + i * 4..36 + i * 4].try_into().ok()?) as usize;
        if off > blob.len() || blob.len() - off < 8 + 24 {
            continue;
        }
        if &blob[off..off + 4] != b"RTS0" {
            continue;
        }
        let flags_off = off + 8 + 20;
        let flags = u32::from_le_bytes(blob[flags_off..flags_off + 4].try_into().ok()?);
        let cleared = clear_xbox_root_sig_flags(flags);
        if cleared == flags {
            return None;
        }

        let mut out = blob.to_vec();
        out[flags_off..flags_off + 4].copy_from_slice(&cleared.to_le_bytes());
        let digest = compute_dxbc_hash(&out[20..]);
        out[4..20].copy_from_slice(&digest);
        return Some(out);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shader::ShaderContainer;

    #[test]
    fn clears_xbox_high_byte_on_versioned_descs_and_rts0_blobs() {
        let mut desc = D3D12VersionedRootSignatureDesc {
            version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            _pad: 0,
            desc: D3D12RootSignatureDesc::default(),
        };
        desc.desc.flags = 0x0300_0001; // Xbox high byte + ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
        let clean = sanitize_versioned_root_signature_desc(&desc).unwrap();
        assert_eq!(clean.desc.flags, 0x0000_0001);

        // Build synthetic DXBC/RTS0 blob with 0x0300_0005 at payload+20
        let mut rts0 = vec![0u8; 32];
        rts0[0..4].copy_from_slice(&2u32.to_le_bytes()); // version 1.1
        rts0[20..24].copy_from_slice(&0x0300_0005u32.to_le_bytes());
        let dxbc = ShaderContainer::build(&[(*b"RTS0", &rts0)]);

        let patched = sanitize_rts0_blob(&dxbc).expect("should rewrite RTS0 blob");
        let parsed = ShaderContainer::parse(&patched).unwrap();
        assert_eq!(parsed.digest, compute_dxbc_hash(&patched[20..]));
        let patched_flags = u32::from_le_bytes(parsed.chunks[0].data[20..24].try_into().unwrap());
        assert_eq!(patched_flags, 0x0000_0005);
        // Second pass returns None (already clean)
        assert!(sanitize_rts0_blob(&patched).is_none());
    }

    #[test]
    fn downgrades_v12_root_signature_without_sampler_flags_to_v11() {
        let s1 = [D3D12StaticSamplerDesc1 {
            base: D3D12StaticSamplerDesc {
                filter: 0x15,
                shader_register: 3,
                ..Default::default()
            },
            flags: 0,
        }];
        let desc = D3D12VersionedRootSignatureDesc {
            version: D3D_ROOT_SIGNATURE_VERSION_1_2,
            _pad: 0,
            desc: D3D12RootSignatureDesc {
                num_parameters: 0,
                _pad0: 0,
                p_parameters: core::ptr::null(),
                num_static_samplers: 1,
                _pad1: 0,
                p_static_samplers: s1.as_ptr() as *const core::ffi::c_void,
                flags: 0x0300_0001,
                _pad2: 0,
            },
        };
        let (v11, samplers) = unsafe { downgrade_root_signature_1_2_to_1_1(&desc).unwrap() };
        assert_eq!(v11.version, D3D_ROOT_SIGNATURE_VERSION_1_1);
        assert_eq!(v11.desc.flags, 0x1);
        assert_eq!(samplers.len(), 1);
        assert_eq!(samplers[0].shader_register, 3);

        // Non-zero sampler flags must be refused
        let s1_flagged = [D3D12StaticSamplerDesc1 {
            base: D3D12StaticSamplerDesc::default(),
            flags: 1,
        }];
        let mut desc_flagged = desc;
        desc_flagged.desc.p_static_samplers = s1_flagged.as_ptr() as *const core::ffi::c_void;
        assert!(unsafe { downgrade_root_signature_1_2_to_1_1(&desc_flagged) }.is_none());
    }
}
