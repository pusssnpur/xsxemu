//! `d3d12_x.dll` / `d3d12_xs.dll` entrypoints wired into `xsx-d3d12`.

use core::ffi::c_void;

use xsx_abi::com::{Guid, HResult, E_FAIL, E_INVALIDARG, S_OK};
use xsx_abi::d3d12::D3D12XboxCreateDeviceParameters;
use xsx_d3d12::create_device_x_com;
use xsx_d3d12::translate::translate_create_device_params;

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

pub fn register(reg: &mut ModuleRegistry) {
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
}

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

unsafe extern "win64" fn hle_d3d12_create_device(
    _p_adapter: *mut c_void,
    _min_feature_level: u32,
    riid: *const Guid,
    pp_device: *mut *mut c_void,
) -> HResult {
    hle_d3d12xbox_create_device(core::ptr::null(), riid, pp_device)
}
