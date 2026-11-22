//! `d3d12_x.dll` / `d3d12_xs.dll` entrypoints wired into `xsx-d3d12`.
//!
//! Exports both:
//! - Real Xbox Series X (`d3d12_xs.dll`):
//!   - `D3D12XboxCreateDevice(adapter, params, riid, out)` -> 84-slot measured Xbox ABI proxy
//!     (slots 62, 67 FSPX native compute PSO, 76 resource layout rewrite, RAX-return heap 9/10)
//!   - `D3D12SerializeVersionedRootSignature` / `D3D12SerializeRootSignature` (strips Xbox
//!     high-byte `0xFF00_0000` flags & downgrades v1.2 -> v1.1 when safe)
//!   - `D3D12CreateRootSignatureDeserializer` / `D3D12CreateVersionedRootSignatureDeserializer`
//!     (rewrites `DXBC`/`RTS0` Xbox flags & recomputes Microsoft DXBC MD5-variant checksum)
//! - Clean-room Phase 1–5 synthetic ABI:
//!   - `D3D12XBOX_CreateDevice(params, riid, out)` -> `ID3D12DeviceX` COM object
//!   - `D3D12CreateDevice(adapter, min_feature_level, riid, out)`

use core::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use xsx_abi::com::{
    Guid, HResult, IUnknownVtbl, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_POINTER, IID_IUNKNOWN,
    S_OK,
};
use xsx_abi::d3d12::{
    D3D12RootSignatureDesc, D3D12VersionedRootSignatureDesc, D3D12XboxCreateDeviceParameters,
};
use xsx_d3d12::rootsig::{
    sanitize_root_signature_desc, sanitize_rts0_blob, sanitize_versioned_root_signature_desc,
};
use xsx_d3d12::shader::ShaderContainer;
use xsx_d3d12::translate::translate_create_device_params;
use xsx_d3d12::{create_device_x_com, create_xs_device_proxy};

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

pub fn register(reg: &mut ModuleRegistry) {
    // Phase 1-5 clean-room entrypoints
    reg.register_export(
        "d3d12_x.dll",
        "D3D12XBOX_CreateDevice",
        hle_d3d12xbox_create_device as *const () as usize,
    );
    reg.register_export(
        "d3d12_x.dll",
        "D3D12CreateDevice",
        hle_d3d12_create_device as *const () as usize,
    );

    // Real measured Xbox Series X (d3d12_xs.dll) entrypoints
    reg.register_export(
        "d3d12_x.dll",
        "D3D12XboxCreateDevice",
        hle_d3d12_xbox_create_device_xs as *const () as usize,
    );
    reg.register_export(
        "d3d12_x.dll",
        "D3D12SerializeRootSignature",
        hle_d3d12_serialize_root_signature as *const () as usize,
    );
    reg.register_export(
        "d3d12_x.dll",
        "D3D12SerializeVersionedRootSignature",
        hle_d3d12_serialize_versioned_root_signature as *const () as usize,
    );
    reg.register_export(
        "d3d12_x.dll",
        "D3D12CreateRootSignatureDeserializer",
        hle_d3d12_create_root_signature_deserializer as *const () as usize,
    );
    reg.register_export(
        "d3d12_x.dll",
        "D3D12CreateVersionedRootSignatureDeserializer",
        hle_d3d12_create_root_signature_deserializer as *const () as usize,
    );
}

// ---------------------------------------------------------------------------
// Minimal ID3DBlob COM object returned by serialized root signature exports
// ---------------------------------------------------------------------------

#[repr(C)]
struct ComBlobVtbl {
    base: IUnknownVtbl,
    get_buffer_pointer: unsafe extern "win64" fn(this: *mut c_void) -> *const c_void,
    get_buffer_size: unsafe extern "win64" fn(this: *mut c_void) -> usize,
}

#[repr(C)]
struct ComBlob {
    lp_vtbl: *const ComBlobVtbl,
    ref_count: AtomicU32,
    bytes: Vec<u8>,
}

unsafe extern "win64" fn blob_qi(
    this: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = core::ptr::null_mut();
    if riid.is_null() {
        return E_POINTER;
    }
    if *riid == IID_IUNKNOWN {
        blob_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn blob_add_ref(this: *mut c_void) -> u32 {
    let b = &*(this as *const ComBlob);
    b.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn blob_release(this: *mut c_void) -> u32 {
    let b = &*(this as *const ComBlob);
    let prev = b.ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this as *mut ComBlob));
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn blob_get_ptr(this: *mut c_void) -> *const c_void {
    let b = &*(this as *const ComBlob);
    b.bytes.as_ptr() as *const c_void
}

unsafe extern "win64" fn blob_get_size(this: *mut c_void) -> usize {
    let b = &*(this as *const ComBlob);
    b.bytes.len()
}

static BLOB_VTBL: ComBlobVtbl = ComBlobVtbl {
    base: IUnknownVtbl {
        query_interface: blob_qi,
        add_ref: blob_add_ref,
        release: blob_release,
    },
    get_buffer_pointer: blob_get_ptr,
    get_buffer_size: blob_get_size,
};

fn make_blob(bytes: Vec<u8>) -> *mut c_void {
    Box::into_raw(Box::new(ComBlob {
        lp_vtbl: &BLOB_VTBL,
        ref_count: AtomicU32::new(1),
        bytes,
    })) as *mut c_void
}

// ---------------------------------------------------------------------------
// Device Creation & Root Signature Exports
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_d3d12xbox_create_device(
    p_parameters: *const D3D12XboxCreateDeviceParameters,
    _riid: *const Guid,
    pp_device: *mut *mut c_void,
) -> HResult {
    if pp_device.is_null() {
        return E_INVALIDARG;
    }
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let params = if p_parameters.is_null() {
        None
    } else {
        Some(&*p_parameters)
    };
    let host_cfg = translate_create_device_params(params);
    match rt.create_or_get_d3d12(host_cfg) {
        Ok(translator) => {
            rt.trace(format!(
                "d3d12_x!D3D12XBOX_CreateDevice() -> backend={}",
                translator.backend_name()
            ));
            *pp_device = create_device_x_com(translator);
            S_OK
        }
        Err(e) => {
            rt.trace(format!("d3d12_x!D3D12XBOX_CreateDevice FAILED: {e}"));
            E_FAIL
        }
    }
}

/// Real measured Xbox Series X `d3d12_xs.dll!D3D12XboxCreateDevice(adapter, params, riid, out)`.
/// Returns the 84-slot measured Xbox Series X Device ABI Proxy (slots 62, 67, 76 & RAX Heap 9/10).
unsafe extern "win64" fn hle_d3d12_xbox_create_device_xs(
    _adapter: *mut c_void,
    params: *const D3D12XboxCreateDeviceParameters,
    riid: *const Guid,
    pp_device: *mut *mut c_void,
) -> HResult {
    if pp_device.is_null() || riid.is_null() {
        return E_POINTER;
    }
    *pp_device = core::ptr::null_mut();
    let Some(rt) = current_runtime() else {
        return E_FAIL;
    };
    let p_opt = if params.is_null() { None } else { Some(&*params) };
    let host_cfg = translate_create_device_params(p_opt);
    match rt.create_or_get_d3d12(host_cfg) {
        Ok(translator) => {
            rt.trace(format!(
                "d3d12_xs!D3D12XboxCreateDevice() -> Xbox-ABI proxy (backend={})",
                translator.backend_name()
            ));
            *pp_device = create_xs_device_proxy(translator);
            S_OK
        }
        Err(e) => {
            rt.trace(format!("d3d12_xs!D3D12XboxCreateDevice FAILED: {e}"));
            E_FAIL
        }
    }
}

unsafe extern "win64" fn hle_d3d12_create_device(
    _p_adapter: *mut c_void,
    _min_feature_level: u32,
    riid: *const Guid,
    pp_device: *mut *mut c_void,
) -> HResult {
    hle_d3d12xbox_create_device(core::ptr::null(), riid, pp_device)
}

/// Serialize a root signature into a valid `DXBC`/`RTS0` container with Xbox high-byte flags cleared.
fn emit_sanitized_rts0_blob(version: u32, num_params: u32, num_samplers: u32, flags: u32) -> Vec<u8> {
    let mut rts0 = vec![0u8; 24];
    rts0[0..4].copy_from_slice(&version.to_le_bytes());
    rts0[4..8].copy_from_slice(&num_params.to_le_bytes());
    rts0[8..12].copy_from_slice(&24u32.to_le_bytes());
    rts0[12..16].copy_from_slice(&num_samplers.to_le_bytes());
    rts0[16..20].copy_from_slice(&24u32.to_le_bytes());
    rts0[20..24].copy_from_slice(&flags.to_le_bytes());
    ShaderContainer::build(&[(*b"RTS0", &rts0)])
}

unsafe extern "win64" fn hle_d3d12_serialize_root_signature(
    desc: *const D3D12RootSignatureDesc,
    version: u32,
    pp_blob: *mut *mut c_void,
    pp_error: *mut *mut c_void,
) -> HResult {
    if !pp_error.is_null() {
        *pp_error = core::ptr::null_mut();
    }
    if desc.is_null() || pp_blob.is_null() {
        return E_INVALIDARG;
    }
    let clean = sanitize_root_signature_desc(&*desc);
    let dxbc = emit_sanitized_rts0_blob(
        version,
        clean.num_parameters,
        clean.num_static_samplers,
        clean.flags,
    );
    *pp_blob = make_blob(dxbc);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "d3d12_xs!D3D12SerializeRootSignature(ver={version}, flags=0x{:X} -> 0x{:X})",
            (*desc).flags,
            clean.flags
        ));
    }
    S_OK
}

unsafe extern "win64" fn hle_d3d12_serialize_versioned_root_signature(
    desc: *const D3D12VersionedRootSignatureDesc,
    pp_blob: *mut *mut c_void,
    pp_error: *mut *mut c_void,
) -> HResult {
    if !pp_error.is_null() {
        *pp_error = core::ptr::null_mut();
    }
    if desc.is_null() || pp_blob.is_null() {
        return E_INVALIDARG;
    }
    let raw = &*desc;
    let clean = sanitize_versioned_root_signature_desc(raw).unwrap_or(*raw);
    let dxbc = emit_sanitized_rts0_blob(
        clean.version,
        clean.desc.num_parameters,
        clean.desc.num_static_samplers,
        clean.desc.flags,
    );
    *pp_blob = make_blob(dxbc);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "d3d12_xs!D3D12SerializeVersionedRootSignature(ver={}, flags=0x{:X} -> 0x{:X})",
            clean.version, raw.desc.flags, clean.desc.flags
        ));
    }
    S_OK
}

unsafe extern "win64" fn hle_d3d12_create_root_signature_deserializer(
    data: *const c_void,
    size: usize,
    _riid: *const Guid,
    pp_out: *mut *mut c_void,
) -> HResult {
    if pp_out.is_null() {
        return E_POINTER;
    }
    *pp_out = core::ptr::null_mut();
    if data.is_null() || size < 32 {
        return E_INVALIDARG;
    }
    let slice = core::slice::from_raw_parts(data as *const u8, size);
    let sanitized = sanitize_rts0_blob(slice).unwrap_or_else(|| slice.to_vec());
    *pp_out = make_blob(sanitized);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "d3d12_xs!D3D12CreateRootSignatureDeserializer(size={size}) -> S_OK"
        ));
    }
    S_OK
}
