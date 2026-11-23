//! `dxgi.dll` HLE module for Xbox Series X|S titles that enumerate adapters or create DXGI factories
//! alongside `d3d12_x.dll` / `d3d12_xs.dll`.

use core::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use xsx_abi::com::{Guid, HResult, IUnknownVtbl, E_NOINTERFACE, E_POINTER, IID_IUNKNOWN, S_OK};

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

const DXGI_ERROR_NOT_FOUND: i32 = 0x887A_0002u32 as i32;

/// `DXGI_ADAPTER_DESC1` (Win64 ABI, 312 bytes).
#[repr(C)]
pub struct DxgiAdapterDesc1 {
    pub description: [u16; 128],
    pub vendor_id: u32,
    pub device_id: u32,
    pub sub_sys_id: u32,
    pub revision: u32,
    pub dedicated_video_memory: usize,
    pub dedicated_system_memory: usize,
    pub shared_system_memory: usize,
    pub adapter_luid_low: u32,
    pub adapter_luid_high: i32,
    pub flags: u32,
}

pub fn register(reg: &mut ModuleRegistry) {
    reg.register_export(
        "dxgi.dll",
        "CreateDXGIFactory",
        hle_create_dxgi_factory1 as *const () as usize,
    );
    reg.register_export(
        "dxgi.dll",
        "CreateDXGIFactory1",
        hle_create_dxgi_factory1 as *const () as usize,
    );
    reg.register_export(
        "dxgi.dll",
        "CreateDXGIFactory2",
        hle_create_dxgi_factory2 as *const () as usize,
    );
    reg.register_export(
        "dxgi.dll",
        "DXGIDeclareAdapterRemovalSupport",
        hle_declare_adapter_removal_support as *const () as usize,
    );
    reg.register_export(
        "dxgi.dll",
        "DXGIGetDebugInterface1",
        hle_get_debug_interface1 as *const () as usize,
    );
}

// ---------------------------------------------------------------------------
// IDXGIAdapter1 COM object
// ---------------------------------------------------------------------------

#[repr(C)]
struct DxgiAdapterVtbl {
    base: IUnknownVtbl,
    // IDXGIObject (slots 3..6)
    set_private_data: unsafe extern "win64" fn(*mut c_void, *const Guid, u32, *const c_void) -> HResult,
    set_private_data_iface: unsafe extern "win64" fn(*mut c_void, *const Guid, *mut c_void) -> HResult,
    get_private_data: unsafe extern "win64" fn(*mut c_void, *const Guid, *mut u32, *mut c_void) -> HResult,
    get_parent: unsafe extern "win64" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult,
    // IDXGIAdapter (slots 7..9)
    enum_outputs: unsafe extern "win64" fn(*mut c_void, u32, *mut *mut c_void) -> HResult,
    get_desc: unsafe extern "win64" fn(*mut c_void, *mut c_void) -> HResult,
    check_interface_support: unsafe extern "win64" fn(*mut c_void, *const Guid, *mut i64) -> HResult,
    // IDXGIAdapter1 (slot 10)
    get_desc1: unsafe extern "win64" fn(*mut c_void, *mut DxgiAdapterDesc1) -> HResult,
}

#[repr(C)]
struct DxgiAdapter {
    vtbl: *const DxgiAdapterVtbl,
    refs: AtomicU32,
}

unsafe extern "win64" fn adapter_qi(this: *mut c_void, _riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    let obj = &*(this as *const DxgiAdapter);
    obj.refs.fetch_add(1, Ordering::Relaxed);
    *ppv = this;
    S_OK
}

unsafe extern "win64" fn adapter_add_ref(this: *mut c_void) -> u32 {
    (*(this as *const DxgiAdapter)).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn adapter_release(this: *mut c_void) -> u32 {
    let prev = (*(this as *const DxgiAdapter)).refs.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        drop(Box::from_raw(this as *mut DxgiAdapter));
    }
    prev - 1
}

unsafe extern "win64" fn stub_not_found(_this: *mut c_void, _a: u32, _b: *mut *mut c_void) -> HResult {
    DXGI_ERROR_NOT_FOUND
}
unsafe extern "win64" fn stub_ok_3(_this: *mut c_void, _a: *const Guid, _b: u32, _c: *const c_void) -> HResult {
    S_OK
}
unsafe extern "win64" fn stub_ok_2(_this: *mut c_void, _a: *const Guid, _b: *mut c_void) -> HResult {
    S_OK
}
unsafe extern "win64" fn stub_get_private(_this: *mut c_void, _a: *const Guid, _b: *mut u32, _c: *mut c_void) -> HResult {
    DXGI_ERROR_NOT_FOUND
}
unsafe extern "win64" fn stub_get_parent(_this: *mut c_void, _riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if !ppv.is_null() {
        *ppv = core::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "win64" fn adapter_get_desc1(_this: *mut c_void, desc: *mut DxgiAdapterDesc1) -> HResult {
    if desc.is_null() {
        return E_POINTER;
    }
    let out = &mut *desc;
    *out = core::mem::zeroed();
    let name = "AMD Radeon Scarlett (xsxemu RDNA 2)";
    for (i, ch) in name.encode_utf16().take(127).enumerate() {
        out.description[i] = ch;
    }
    out.vendor_id = 0x1002; // AMD
    out.device_id = 0x13FE; // Scarlett GPU
    out.dedicated_video_memory = 10 * 1024 * 1024 * 1024usize;
    out.shared_system_memory = 3 * 1024 * 1024 * 1024usize;
    out.adapter_luid_low = 0x0001_3370;
    S_OK
}

unsafe extern "win64" fn adapter_get_desc(this: *mut c_void, desc: *mut c_void) -> HResult {
    if desc.is_null() {
        return E_POINTER;
    }
    let mut d1: DxgiAdapterDesc1 = core::mem::zeroed();
    let hr = adapter_get_desc1(this, &mut d1);
    if hr == S_OK {
        // DXGI_ADAPTER_DESC is identical to DXGI_ADAPTER_DESC1 without the trailing `flags` field.
        let copy_len = core::mem::size_of::<DxgiAdapterDesc1>() - 4;
        core::ptr::copy_nonoverlapping(&d1 as *const _ as *const u8, desc as *mut u8, copy_len);
    }
    hr
}

unsafe extern "win64" fn adapter_check_iface(
    _this: *mut c_void,
    _guid: *const Guid,
    ver: *mut i64,
) -> HResult {
    if !ver.is_null() {
        *ver = 0x001E_0000_0001_0000;
    }
    S_OK
}

static ADAPTER_VTBL: DxgiAdapterVtbl = DxgiAdapterVtbl {
    base: IUnknownVtbl {
        query_interface: adapter_qi,
        add_ref: adapter_add_ref,
        release: adapter_release,
    },
    set_private_data: stub_ok_3,
    set_private_data_iface: stub_ok_2,
    get_private_data: stub_get_private,
    get_parent: stub_get_parent,
    enum_outputs: stub_not_found,
    get_desc: adapter_get_desc,
    check_interface_support: adapter_check_iface,
    get_desc1: adapter_get_desc1,
};

// ---------------------------------------------------------------------------
// IDXGIFactory4 COM object
// ---------------------------------------------------------------------------

#[repr(C)]
struct DxgiFactoryVtbl {
    base: IUnknownVtbl,
    set_private_data: unsafe extern "win64" fn(*mut c_void, *const Guid, u32, *const c_void) -> HResult,
    set_private_data_iface: unsafe extern "win64" fn(*mut c_void, *const Guid, *mut c_void) -> HResult,
    get_private_data: unsafe extern "win64" fn(*mut c_void, *const Guid, *mut u32, *mut c_void) -> HResult,
    get_parent: unsafe extern "win64" fn(*mut c_void, *const Guid, *mut *mut c_void) -> HResult,
    enum_adapters: unsafe extern "win64" fn(*mut c_void, u32, *mut *mut c_void) -> HResult,
    make_window_association: unsafe extern "win64" fn(*mut c_void, usize, u32) -> HResult,
    get_window_association: unsafe extern "win64" fn(*mut c_void, *mut usize) -> HResult,
    create_swap_chain: unsafe extern "win64" fn(*mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> HResult,
    create_software_adapter: unsafe extern "win64" fn(*mut c_void, usize, *mut *mut c_void) -> HResult,
    // IDXGIFactory1 (slots 12..13)
    enum_adapters1: unsafe extern "win64" fn(*mut c_void, u32, *mut *mut c_void) -> HResult,
    is_current: unsafe extern "win64" fn(*mut c_void) -> i32,
}

#[repr(C)]
struct DxgiFactory {
    vtbl: *const DxgiFactoryVtbl,
    refs: AtomicU32,
}

unsafe extern "win64" fn factory_qi(this: *mut c_void, riid: *const Guid, ppv: *mut *mut c_void) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    let _ = riid;
    let obj = &*(this as *const DxgiFactory);
    obj.refs.fetch_add(1, Ordering::Relaxed);
    *ppv = this;
    S_OK
}

unsafe extern "win64" fn factory_add_ref(this: *mut c_void) -> u32 {
    (*(this as *const DxgiFactory)).refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn factory_release(this: *mut c_void) -> u32 {
    let prev = (*(this as *const DxgiFactory)).refs.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        drop(Box::from_raw(this as *mut DxgiFactory));
    }
    prev - 1
}

unsafe extern "win64" fn factory_enum_adapters(
    _this: *mut c_void,
    index: u32,
    pp_adapter: *mut *mut c_void,
) -> HResult {
    if pp_adapter.is_null() {
        return E_POINTER;
    }
    if index > 0 {
        *pp_adapter = core::ptr::null_mut();
        return DXGI_ERROR_NOT_FOUND;
    }
    let adapter = Box::new(DxgiAdapter {
        vtbl: &ADAPTER_VTBL,
        refs: AtomicU32::new(1),
    });
    *pp_adapter = Box::into_raw(adapter) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn factory_make_window_assoc(_this: *mut c_void, _hwnd: usize, _flags: u32) -> HResult {
    S_OK
}
unsafe extern "win64" fn factory_get_window_assoc(_this: *mut c_void, hwnd: *mut usize) -> HResult {
    if !hwnd.is_null() {
        *hwnd = 0;
    }
    S_OK
}
unsafe extern "win64" fn factory_create_swap_chain(
    _this: *mut c_void,
    _dev: *mut c_void,
    _desc: *const c_void,
    pp: *mut *mut c_void,
) -> HResult {
    if !pp.is_null() {
        *pp = core::ptr::null_mut();
    }
    S_OK
}
unsafe extern "win64" fn factory_is_current(_this: *mut c_void) -> i32 {
    1
}

unsafe extern "win64" fn factory_create_sw_adapter(
    _this: *mut c_void,
    _hmod: usize,
    pp: *mut *mut c_void,
) -> HResult {
    if !pp.is_null() {
        *pp = core::ptr::null_mut();
    }
    DXGI_ERROR_NOT_FOUND
}

static FACTORY_VTBL: DxgiFactoryVtbl = DxgiFactoryVtbl {
    base: IUnknownVtbl {
        query_interface: factory_qi,
        add_ref: factory_add_ref,
        release: factory_release,
    },
    set_private_data: stub_ok_3,
    set_private_data_iface: stub_ok_2,
    get_private_data: stub_get_private,
    get_parent: stub_get_parent,
    enum_adapters: factory_enum_adapters,
    make_window_association: factory_make_window_assoc,
    get_window_association: factory_get_window_assoc,
    create_swap_chain: factory_create_swap_chain,
    create_software_adapter: factory_create_sw_adapter,
    enum_adapters1: factory_enum_adapters,
    is_current: factory_is_current,
};

unsafe extern "win64" fn hle_create_dxgi_factory1(_riid: *const Guid, pp_factory: *mut *mut c_void) -> HResult {
    if pp_factory.is_null() {
        return E_POINTER;
    }
    if let Some(rt) = current_runtime() {
        rt.trace("dxgi!CreateDXGIFactory1()");
    }
    let f = Box::new(DxgiFactory {
        vtbl: &FACTORY_VTBL,
        refs: AtomicU32::new(1),
    });
    *pp_factory = Box::into_raw(f) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn hle_create_dxgi_factory2(
    _flags: u32,
    riid: *const Guid,
    pp_factory: *mut *mut c_void,
) -> HResult {
    hle_create_dxgi_factory1(riid, pp_factory)
}

unsafe extern "win64" fn hle_declare_adapter_removal_support() -> HResult {
    S_OK
}

unsafe extern "win64" fn hle_get_debug_interface1(
    _flags: u32,
    _riid: *const Guid,
    pp: *mut *mut c_void,
) -> HResult {
    if !pp.is_null() {
        *pp = core::ptr::null_mut();
    }
    let _ = IID_IUNKNOWN;
    E_NOINTERFACE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dxgi_factory_enumerates_scarlett_rdna2_adapter() {
        unsafe {
            let mut factory: *mut c_void = core::ptr::null_mut();
            assert_eq!(hle_create_dxgi_factory2(0, &IID_IUNKNOWN, &mut factory), S_OK);
            assert!(!factory.is_null());

            let f = &*(factory as *const DxgiFactory);
            let mut adapter: *mut c_void = core::ptr::null_mut();
            assert_eq!(((*f.vtbl).enum_adapters1)(factory, 0, &mut adapter), S_OK);
            assert!(!adapter.is_null());

            let a = &*(adapter as *const DxgiAdapter);
            let mut desc: DxgiAdapterDesc1 = core::mem::zeroed();
            assert_eq!(((*a.vtbl).get_desc1)(adapter, &mut desc), S_OK);
            assert_eq!(desc.vendor_id, 0x1002);
            assert_eq!(desc.device_id, 0x13FE);
            assert!(desc.dedicated_video_memory >= 10 * 1024 * 1024 * 1024);

            let mut adapter2: *mut c_void = core::ptr::null_mut();
            assert_eq!(((*f.vtbl).enum_adapters1)(factory, 1, &mut adapter2), DXGI_ERROR_NOT_FOUND);

            ((*a.vtbl).base.release)(adapter);
            ((*f.vtbl).base.release)(factory);
        }
    }
}
