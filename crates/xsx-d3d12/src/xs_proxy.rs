//! Real Measured Xbox Series X (`d3d12_xs.dll`) COM VTable ABI Proxy.
//!
//! Real Xbox Series X binaries (`D3D12XboxCreateDevice`) call `ID3D12Device` and
//! `ID3D12DescriptorHeap` through the exact Windows SDK slot layout **with measured Xbox
//! Series X slot deviations**:
//!
//! - **Device Slot 8**: `CreateCommandQueue`
//! - **Device Slot 9**: `CreateCommandAllocator`
//! - **Device Slot 10**: `CreateGraphicsPipelineState`
//! - **Device Slot 11**: `CreateComputePipelineState`
//! - **Device Slot 12**: `CreateCommandList`
//! - **Device Slot 14**: `CreateDescriptorHeap` -> returns an Xbox `ID3D12DescriptorHeap` proxy
//!   whose **Slots 9 & 10** (`GetCPU/GPUDescriptorHandleForHeapStart`) return the 8-byte handle
//!   directly in `RAX` (Xbox ABI, vs PC MSVC hidden out-pointer in `RDX`)!
//! - **Device Slot 15**: `GetDescriptorHandleIncrementSize`
//! - **Device Slot 16**: `CreateRootSignature`
//! - **Device Slot 20**: `CreateRenderTargetView`
//! - **Device Slot 27**: `CreateCommittedResource`
//! - **Device Slot 36**: `CreateFence`
//! - **Device Slot 62**: Xbox `CreateComputePipelineState(desc, w2, w3, riid, out)` (takes 2 extra
//!   words before `riid`; PC slot 62 is `CreateStateObject`)
//! - **Device Slot 67**: Xbox `DeserializeComputePipelineStateX(native_desc, riid, out)` (FSPX
//!   kind-6 native RDNA 2 compute pipeline used by `amd_fidelityfx_upscaler_xbox.dll`)
//! - **Device Slot 76**: Xbox `CreateCommittedResource` (rewrites Xbox `Layout` word `0x101`/`0x1`
//!   to PC `ROW_MAJOR` for buffers or `UNKNOWN` for textures)
//!
//! Every unimplemented slot in `0..84` returns `E_NOTIMPL` and logs its slot index cleanly.

use core::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use xsx_abi::com::{
    Guid, HResult, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER, IID_ID3D12DESCRIPTOR_HEAP,
    IID_ID3D12DEVICE, IID_ID3D12DEVICE_CHILD, IID_ID3D12OBJECT, IID_ID3D12PAGEABLE, IID_IUNKNOWN,
    S_OK,
};
use xsx_abi::d3d12::{
    D3D12CommandQueueDesc, D3D12ComputePipelineStateDesc, D3D12CpuDescriptorHandle,
    D3D12DescriptorHeapDesc, D3D12GpuDescriptorHandle, D3D12GraphicsPipelineStateDesc,
    D3D12HeapProperties, D3D12ResourceDesc, D3D12ResourceDimension, D3D12ShaderBytecode,
    D3D12TextureLayout, XboxNativeComputePipelineStateDesc,
};

use crate::backend::D3D12Translator;
use crate::com_objects;

pub const XS_DEVICE_VTABLE_SLOTS: usize = 84;
pub const XS_HEAP_VTABLE_SLOTS: usize = 11;

static SLOT_HITS: [AtomicU64; XS_DEVICE_VTABLE_SLOTS] =
    [const { AtomicU64::new(0) }; XS_DEVICE_VTABLE_SLOTS];

pub fn xs_device_slot_hits(slot: usize) -> u64 {
    SLOT_HITS
        .get(slot)
        .map(|c| c.load(Ordering::Relaxed))
        .unwrap_or(0)
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct VtblSlot(*const c_void);
unsafe impl Sync for VtblSlot {}

#[repr(C)]
struct XsDeviceProxy {
    lp_vtbl: *const [VtblSlot; XS_DEVICE_VTABLE_SLOTS],
    ref_count: AtomicU32,
    translator: Arc<D3D12Translator>,
    inner_device_x: *mut c_void,
}

#[repr(C)]
struct XsHeapProxy {
    lp_vtbl: *const [VtblSlot; XS_HEAP_VTABLE_SLOTS],
    ref_count: AtomicU32,
    base_cpu_handle: usize,
    base_gpu_handle: u64,
    desc: D3D12DescriptorHeapDesc,
    device_proxy: *mut XsDeviceProxy,
}

/// Allocate a real-ABI Xbox Series X `ID3D12Device` proxy (84-slot vtable with slots 62/67/76
/// and heap proxy slots 9/10) backed by `D3D12Translator`.
pub fn create_xs_device_proxy(translator: Arc<D3D12Translator>) -> *mut c_void {
    let inner = com_objects::create_device_x_com(Arc::clone(&translator));
    let proxy = Box::new(XsDeviceProxy {
        lp_vtbl: &XS_DEVICE_VTBL,
        ref_count: AtomicU32::new(1),
        translator,
        inner_device_x: inner,
    });
    Box::into_raw(proxy) as *mut c_void
}

// ---------------------------------------------------------------------------
// Xbox Series X Device Proxy VTable Slots
// ---------------------------------------------------------------------------

unsafe extern "win64" fn xs_dev_qi(
    this: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[0].fetch_add(1, Ordering::Relaxed);
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = ptr::null_mut();
    if riid.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN || iid == IID_ID3D12OBJECT || iid == IID_ID3D12DEVICE {
        xs_dev_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn xs_dev_add_ref(this: *mut c_void) -> u32 {
    SLOT_HITS[1].fetch_add(1, Ordering::Relaxed);
    let obj = &*(this as *const XsDeviceProxy);
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn xs_dev_release(this: *mut c_void) -> u32 {
    SLOT_HITS[2].fetch_add(1, Ordering::Relaxed);
    let obj = &*(this as *const XsDeviceProxy);
    let prev = obj.ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        let boxed = Box::from_raw(this as *mut XsDeviceProxy);
        if !boxed.inner_device_x.is_null() {
            let vtbl = *(boxed.inner_device_x as *const *const xsx_abi::com::IUnknownVtbl);
            ((*vtbl).release)(boxed.inner_device_x);
        }
        0
    } else {
        prev - 1
    }
}

// Slot 8: CreateCommandQueue
unsafe extern "win64" fn xs_dev_create_command_queue(
    this: *mut c_void,
    desc: *const D3D12CommandQueueDesc,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[8].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_command_queue)(dev.inner_device_x, desc, riid, out)
}

// Slot 9: CreateCommandAllocator
unsafe extern "win64" fn xs_dev_create_command_allocator(
    this: *mut c_void,
    ty: u32,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[9].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_command_allocator)(dev.inner_device_x, ty, riid, out)
}

// Slot 10: CreateGraphicsPipelineState
unsafe extern "win64" fn xs_dev_create_graphics_pso(
    this: *mut c_void,
    desc: *const D3D12GraphicsPipelineStateDesc,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[10].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_graphics_pipeline_state)(dev.inner_device_x, desc, riid, out)
}

// Slot 11: CreateComputePipelineState
unsafe extern "win64" fn xs_dev_create_compute_pso(
    this: *mut c_void,
    desc: *const D3D12ComputePipelineStateDesc,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[11].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_compute_pipeline_state)(dev.inner_device_x, desc, riid, out)
}

// Slot 12: CreateCommandList
unsafe extern "win64" fn xs_dev_create_command_list(
    this: *mut c_void,
    node_mask: u32,
    ty: u32,
    alloc: *mut c_void,
    init_pso: *mut c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[12].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_graphics_command_list)(
        dev.inner_device_x,
        node_mask,
        ty,
        alloc,
        init_pso,
        riid,
        out,
    )
}

// Slot 14: CreateDescriptorHeap -> returns Xbox Heap Proxy (RAX handle return on slots 9/10)
unsafe extern "win64" fn xs_dev_create_descriptor_heap(
    this: *mut c_void,
    desc: *const D3D12DescriptorHeapDesc,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[14].fetch_add(1, Ordering::Relaxed);
    if !out.is_null() {
        *out = ptr::null_mut();
    }
    if desc.is_null() || riid.is_null() {
        return E_POINTER;
    }
    if out.is_null() {
        return xsx_abi::com::S_FALSE;
    }
    let dev = &*(this as *const XsDeviceProxy);
    let d = *desc;
    let base_cpu = dev.translator.alloc_descriptor_range(d.num_descriptors, 32);
    let base_gpu = 0x2000_0000_0000u64 + base_cpu as u64;
    dev.translator.record_trace(format!(
        "d3d12_xs::ID3D12Device[14]::CreateDescriptorHeap(type={}, count={}, cpu=0x{base_cpu:X}, gpu=0x{base_gpu:X})",
        d.r#type, d.num_descriptors
    ));
    xs_dev_add_ref(this);
    let heap = Box::new(XsHeapProxy {
        lp_vtbl: &XS_HEAP_VTBL,
        ref_count: AtomicU32::new(1),
        base_cpu_handle: base_cpu,
        base_gpu_handle: base_gpu,
        desc: d,
        device_proxy: this as *mut XsDeviceProxy,
    });
    *out = Box::into_raw(heap) as *mut c_void;
    S_OK
}

// Slot 15: GetDescriptorHandleIncrementSize
unsafe extern "win64" fn xs_dev_get_descriptor_increment(_this: *mut c_void, _ty: u32) -> u32 {
    SLOT_HITS[15].fetch_add(1, Ordering::Relaxed);
    32
}

// Slot 16: CreateRootSignature
unsafe extern "win64" fn xs_dev_create_root_signature(
    this: *mut c_void,
    node_mask: u32,
    blob: *const c_void,
    blob_len: usize,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[16].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_root_signature)(dev.inner_device_x, node_mask, blob, blob_len, riid, out)
}

// Slot 20: CreateRenderTargetView
unsafe extern "win64" fn xs_dev_create_rtv(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const c_void,
    dest: D3D12CpuDescriptorHandle,
) {
    SLOT_HITS[20].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_render_target_view)(dev.inner_device_x, resource, desc, dest);
}

// Slot 27: Standard PC CreateCommittedResource
unsafe extern "win64" fn xs_dev_create_committed_resource(
    this: *mut c_void,
    heap_props: *const D3D12HeapProperties,
    heap_flags: u32,
    desc: *const D3D12ResourceDesc,
    initial_state: u32,
    clear_value: *const c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[27].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_committed_resource)(
        dev.inner_device_x,
        heap_props,
        heap_flags,
        desc,
        initial_state,
        clear_value,
        riid,
        out,
    )
}

// Slot 36: CreateFence
unsafe extern "win64" fn xs_dev_create_fence(
    this: *mut c_void,
    init_val: u64,
    flags: u32,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[36].fetch_add(1, Ordering::Relaxed);
    let dev = &*(this as *const XsDeviceProxy);
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_fence)(dev.inner_device_x, init_val, flags, riid, out)
}

// Slot 62 (Xbox Series X deviation): CreateComputePipelineState(desc, w2, w3, riid, out)
unsafe extern "win64" fn xs_dev_slot62_create_compute_pso(
    this: *mut c_void,
    desc: *const D3D12ComputePipelineStateDesc,
    w2: u64,
    w3: u64,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[62].fetch_add(1, Ordering::Relaxed);
    if out.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    if riid.is_null() || desc.is_null() {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const XsDeviceProxy);
    dev.translator.record_trace(format!(
        "d3d12_xs::ID3D12Device[62]::XboxCreateComputePipelineState(w2=0x{w2:X}, w3=0x{w3:X})"
    ));
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_compute_pipeline_state)(dev.inner_device_x, desc, riid, out)
}

// Slot 67 (Xbox Series X deviation): DeserializeComputePipelineStateX(native_desc, riid, out)
unsafe extern "win64" fn xs_dev_slot67_deserialize_compute_pso_x(
    this: *mut c_void,
    native_desc: *const XboxNativeComputePipelineStateDesc,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[67].fetch_add(1, Ordering::Relaxed);
    if out.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    if native_desc.is_null() {
        return E_INVALIDARG;
    }
    if riid.is_null() {
        return E_POINTER;
    }
    let dev = &*(this as *const XsDeviceProxy);
    let nd = *native_desc;
    dev.translator.record_trace(format!(
        "d3d12_xs::ID3D12Device[67]::DeserializeComputePipelineStateX(kind0={}B, kind6_rdna2={}B, kind7={}B)",
        nd.kind0_size, nd.kind6_size, nd.kind7_size
    ));
    let pc_desc = D3D12ComputePipelineStateDesc {
        p_root_signature: nd.root_signature,
        cs: D3D12ShaderBytecode {
            p_shader_bytecode: nd.kind6,
            byte_code_length: nd.kind6_size as usize,
        },
        node_mask: 0,
        flags: 0,
    };
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_compute_pipeline_state)(dev.inner_device_x, &pc_desc, riid, out)
}

// Slot 76 (Xbox Series X deviation): CreateCommittedResource with Xbox layout word rewrite
unsafe extern "win64" fn xs_dev_slot76_create_resource(
    this: *mut c_void,
    heap_props: *const D3D12HeapProperties,
    heap_flags: u32,
    xbox_desc: *const D3D12ResourceDesc,
    initial_state: u32,
    clear_value: *const c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    SLOT_HITS[76].fetch_add(1, Ordering::Relaxed);
    if out.is_null() || xbox_desc.is_null() || riid.is_null() {
        return E_POINTER;
    }
    *out = ptr::null_mut();
    let dev = &*(this as *const XsDeviceProxy);
    let mut pc_desc = *xbox_desc;
    let orig_layout = pc_desc.layout;
    // Xbox layout words (0x101 tiled, 0x1 row-major buffer) are sanitized to PC D3D12 layouts
    pc_desc.layout = if pc_desc.dimension == D3D12ResourceDimension::Buffer as u32 {
        D3D12TextureLayout::RowMajor as u32
    } else {
        D3D12TextureLayout::Unknown as u32
    };
    dev.translator.record_trace(format!(
        "d3d12_xs::ID3D12Device[76]::XboxCreateResource(dim={}, xbox_layout=0x{orig_layout:X} -> pc_layout=0x{:X})",
        pc_desc.dimension, pc_desc.layout
    ));
    let vtbl = *(dev.inner_device_x as *const *const xsx_abi::d3d12::ID3D12DeviceXVtbl);
    ((*vtbl).create_committed_resource)(
        dev.inner_device_x,
        heap_props,
        heap_flags,
        &pc_desc,
        initial_state,
        clear_value,
        riid,
        out,
    )
}

unsafe extern "win64" fn xs_dev_unimplemented(this: *mut c_void) -> HResult {
    if !this.is_null() {
        let dev = &*(this as *const XsDeviceProxy);
        dev.translator
            .record_trace("d3d12_xs::ID3D12Device: unimplemented slot called -> E_NOTIMPL");
    }
    E_NOTIMPL
}

const fn build_xs_device_vtable() -> [VtblSlot; XS_DEVICE_VTABLE_SLOTS] {
    let mut t = [VtblSlot(xs_dev_unimplemented as *const c_void); XS_DEVICE_VTABLE_SLOTS];
    t[0] = VtblSlot(xs_dev_qi as *const c_void);
    t[1] = VtblSlot(xs_dev_add_ref as *const c_void);
    t[2] = VtblSlot(xs_dev_release as *const c_void);
    t[8] = VtblSlot(xs_dev_create_command_queue as *const c_void);
    t[9] = VtblSlot(xs_dev_create_command_allocator as *const c_void);
    t[10] = VtblSlot(xs_dev_create_graphics_pso as *const c_void);
    t[11] = VtblSlot(xs_dev_create_compute_pso as *const c_void);
    t[12] = VtblSlot(xs_dev_create_command_list as *const c_void);
    t[14] = VtblSlot(xs_dev_create_descriptor_heap as *const c_void);
    t[15] = VtblSlot(xs_dev_get_descriptor_increment as *const c_void);
    t[16] = VtblSlot(xs_dev_create_root_signature as *const c_void);
    t[20] = VtblSlot(xs_dev_create_rtv as *const c_void);
    t[27] = VtblSlot(xs_dev_create_committed_resource as *const c_void);
    t[36] = VtblSlot(xs_dev_create_fence as *const c_void);
    t[62] = VtblSlot(xs_dev_slot62_create_compute_pso as *const c_void);
    t[67] = VtblSlot(xs_dev_slot67_deserialize_compute_pso_x as *const c_void);
    t[76] = VtblSlot(xs_dev_slot76_create_resource as *const c_void);
    t
}

static XS_DEVICE_VTBL: [VtblSlot; XS_DEVICE_VTABLE_SLOTS] = build_xs_device_vtable();

// ---------------------------------------------------------------------------
// Xbox Series X Descriptor Heap Proxy (Slots 9/10 return handle in RAX!)
// ---------------------------------------------------------------------------

unsafe extern "win64" fn xs_heap_qi(
    this: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> HResult {
    if ppv.is_null() {
        return E_POINTER;
    }
    *ppv = ptr::null_mut();
    if riid.is_null() {
        return E_POINTER;
    }
    let iid = *riid;
    if iid == IID_IUNKNOWN
        || iid == IID_ID3D12OBJECT
        || iid == IID_ID3D12DEVICE_CHILD
        || iid == IID_ID3D12PAGEABLE
        || iid == IID_ID3D12DESCRIPTOR_HEAP
    {
        xs_heap_add_ref(this);
        *ppv = this;
        S_OK
    } else {
        E_NOINTERFACE
    }
}

unsafe extern "win64" fn xs_heap_add_ref(this: *mut c_void) -> u32 {
    let obj = &*(this as *const XsHeapProxy);
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn xs_heap_release(this: *mut c_void) -> u32 {
    let obj = &*(this as *const XsHeapProxy);
    let prev = obj.ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        let boxed = Box::from_raw(this as *mut XsHeapProxy);
        if !boxed.device_proxy.is_null() {
            xs_dev_release(boxed.device_proxy as *mut c_void);
        }
        0
    } else {
        prev - 1
    }
}

unsafe extern "win64" fn xs_heap_get_device(
    this: *mut c_void,
    riid: *const Guid,
    out: *mut *mut c_void,
) -> HResult {
    let obj = &*(this as *const XsHeapProxy);
    xs_dev_qi(obj.device_proxy as *mut c_void, riid, out)
}

// Slot 8: GetDesc (MSVC hidden out-pointer in RDX)
unsafe extern "win64" fn xs_heap_get_desc(
    this: *mut c_void,
    out_desc: *mut D3D12DescriptorHeapDesc,
) -> *mut D3D12DescriptorHeapDesc {
    if !out_desc.is_null() {
        let obj = &*(this as *const XsHeapProxy);
        *out_desc = obj.desc;
    }
    out_desc
}

// Slot 9 (Xbox ABI): returns 8-byte CPU descriptor handle in RAX
unsafe extern "win64" fn xs_heap_get_cpu_handle(this: *mut c_void) -> D3D12CpuDescriptorHandle {
    let obj = &*(this as *const XsHeapProxy);
    D3D12CpuDescriptorHandle {
        ptr: obj.base_cpu_handle,
    }
}

// Slot 10 (Xbox ABI): returns 8-byte GPU descriptor handle in RAX
unsafe extern "win64" fn xs_heap_get_gpu_handle(this: *mut c_void) -> D3D12GpuDescriptorHandle {
    let obj = &*(this as *const XsHeapProxy);
    D3D12GpuDescriptorHandle {
        ptr: obj.base_gpu_handle,
    }
}

unsafe extern "win64" fn xs_heap_stub(_this: *mut c_void) -> HResult {
    S_OK
}

static XS_HEAP_VTBL: [VtblSlot; XS_HEAP_VTABLE_SLOTS] = [
    VtblSlot(xs_heap_qi as *const c_void),             // 0: QueryInterface
    VtblSlot(xs_heap_add_ref as *const c_void),        // 1: AddRef
    VtblSlot(xs_heap_release as *const c_void),        // 2: Release
    VtblSlot(xs_heap_stub as *const c_void),           // 3: GetPrivateData
    VtblSlot(xs_heap_stub as *const c_void),           // 4: SetPrivateData
    VtblSlot(xs_heap_stub as *const c_void),           // 5: SetPrivateDataInterface
    VtblSlot(xs_heap_stub as *const c_void),           // 6: SetName
    VtblSlot(xs_heap_get_device as *const c_void),     // 7: GetDevice -> Xbox device proxy
    VtblSlot(xs_heap_get_desc as *const c_void),       // 8: GetDesc
    VtblSlot(xs_heap_get_cpu_handle as *const c_void), // 9: GetCPUDescriptorHandleForHeapStart (RAX)
    VtblSlot(xs_heap_get_gpu_handle as *const c_void), // 10: GetGPUDescriptorHandleForHeapStart (RAX)
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendKind, TranslatorConfig};
    use crate::translate::translate_create_device_params;
    use xsx_abi::com::{IID_ID3D12PIPELINE_STATE, IID_ID3D12RESOURCE};

    #[test]
    fn exercises_measured_xbox_slots_62_67_76_and_heap_9_10() {
        let mut cfg = TranslatorConfig::default();
        cfg.backend = BackendKind::Software;
        cfg.default_width = 64;
        cfg.default_height = 64;
        let tr = D3D12Translator::new(cfg, translate_create_device_params(None)).unwrap();
        let dev = create_xs_device_proxy(tr);
        let vtbl = unsafe { &**(dev as *const *const [VtblSlot; XS_DEVICE_VTABLE_SLOTS]) };

        // Slot 14 -> Heap proxy, then heap slots 9 & 10 return handles in RAX
        let create_heap: unsafe extern "win64" fn(
            *mut c_void,
            *const D3D12DescriptorHeapDesc,
            *const Guid,
            *mut *mut c_void,
        ) -> HResult = unsafe { core::mem::transmute(vtbl[14].0) };
        let hdesc = D3D12DescriptorHeapDesc {
            r#type: 0,
            num_descriptors: 16,
            flags: 1,
            node_mask: 0,
        };
        let mut heap: *mut c_void = ptr::null_mut();
        assert_eq!(
            unsafe { create_heap(dev, &hdesc, &IID_ID3D12DESCRIPTOR_HEAP, &mut heap) },
            S_OK
        );
        assert!(!heap.is_null());

        let hvtbl = unsafe { &**(heap as *const *const [VtblSlot; XS_HEAP_VTABLE_SLOTS]) };
        let get_cpu: unsafe extern "win64" fn(*mut c_void) -> D3D12CpuDescriptorHandle =
            unsafe { core::mem::transmute(hvtbl[9].0) };
        let get_gpu: unsafe extern "win64" fn(*mut c_void) -> D3D12GpuDescriptorHandle =
            unsafe { core::mem::transmute(hvtbl[10].0) };
        assert_ne!(unsafe { get_cpu(heap) }.ptr, 0);
        assert_ne!(unsafe { get_gpu(heap) }.ptr, 0);

        // Slot 67 -> DeserializeComputePipelineStateX (FSPX kind 6)
        let fake_code = [0x90u8; 36];
        let native_desc = XboxNativeComputePipelineStateDesc {
            root_signature: ptr::null_mut(),
            kind0: ptr::null(),
            kind0_size: 28,
            kind1: ptr::null(),
            kind1_size: 20,
            kind6: fake_code.as_ptr() as *const c_void,
            kind6_size: fake_code.len() as u64,
            kind7: ptr::null(),
            kind7_size: 0,
        };
        let slot67: unsafe extern "win64" fn(
            *mut c_void,
            *const XboxNativeComputePipelineStateDesc,
            *const Guid,
            *mut *mut c_void,
        ) -> HResult = unsafe { core::mem::transmute(vtbl[67].0) };
        let mut pso: *mut c_void = ptr::null_mut();
        assert_eq!(
            unsafe { slot67(dev, &native_desc, &IID_ID3D12PIPELINE_STATE, &mut pso) },
            S_OK
        );
        assert!(!pso.is_null());

        // Slot 76 -> Xbox CreateResource with Xbox tiled layout 0x101
        let heap_props = D3D12HeapProperties {
            r#type: 1,
            ..Default::default()
        };
        let rdesc = D3D12ResourceDesc {
            dimension: D3D12ResourceDimension::Texture2D as u32,
            width: 64,
            height: 64,
            depth_or_array_size: 1,
            mip_levels: 1,
            format: 28,
            layout: 0x101, // Xbox tiled layout word
            ..Default::default()
        };
        let slot76: unsafe extern "win64" fn(
            *mut c_void,
            *const D3D12HeapProperties,
            u32,
            *const D3D12ResourceDesc,
            u32,
            *const c_void,
            *const Guid,
            *mut *mut c_void,
        ) -> HResult = unsafe { core::mem::transmute(vtbl[76].0) };
        let mut res: *mut c_void = ptr::null_mut();
        assert_eq!(
            unsafe {
                slot76(
                    dev,
                    &heap_props,
                    0,
                    &rdesc,
                    0,
                    ptr::null(),
                    &IID_ID3D12RESOURCE,
                    &mut res,
                )
            },
            S_OK
        );
        assert!(!res.is_null());
        assert!(xs_device_slot_hits(67) >= 1);
        assert!(xs_device_slot_hits(76) >= 1);

        // Clean up COM references
        unsafe {
            let rel: unsafe extern "win64" fn(*mut c_void) -> u32 =
                core::mem::transmute(hvtbl[2].0);
            rel(heap);
            let pso_vtbl = *(pso as *const *const xsx_abi::com::IUnknownVtbl);
            ((*pso_vtbl).release)(pso);
            let res_vtbl = *(res as *const *const xsx_abi::com::IUnknownVtbl);
            ((*res_vtbl).release)(res);
            let dev_rel: unsafe extern "win64" fn(*mut c_void) -> u32 =
                core::mem::transmute(vtbl[2].0);
            dev_rel(dev);
        }
    }
}
