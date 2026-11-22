//! Binary-compatible Win64 COM objects implementing Xbox `d3d12_x` interfaces.
//!
//! Each COM object starts with `lp_vtbl: *const ...Vtbl` at offset 0 so guest
//! x86_64 code can perform standard COM double-indirection calls (`mov rax, [rcx]; call [rax + slot*8]`).

use core::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use xsx_abi::com::{
    Guid, HResult, IUnknownVtbl, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_POINTER,
    IID_ID3D12COMMAND_ALLOCATOR, IID_ID3D12COMMAND_QUEUE, IID_ID3D12DESCRIPTOR_HEAP,
    IID_ID3D12DEVICE, IID_ID3D12FENCE, IID_ID3D12GRAPHICS_COMMAND_LIST, IID_ID3D12OBJECT,
    IID_ID3D12RESOURCE, IID_IUNKNOWN, S_OK,
};
use xsx_abi::d3d12::{
    D3D12CommandQueueDesc, D3D12CpuDescriptorHandle, D3D12DescriptorHeapDesc, D3D12HeapProperties,
    D3D12Range, D3D12Rect, D3D12ResourceBarrier, D3D12ResourceDesc, D3D12VertexBufferView,
    D3D12Viewport, D3D12XboxPresentPlaneParameters, ID3D12CommandAllocatorXVtbl,
    ID3D12CommandQueueXVtbl, ID3D12DescriptorHeapXVtbl, ID3D12DeviceXVtbl, ID3D12FenceXVtbl,
    ID3D12GraphicsCommandListXVtbl, ID3D12ResourceXVtbl,
};

use crate::backend::{D3D12Translator, GpuCommand, ResourceKind};
use crate::translate::translate_present_x;

const DESCRIPTOR_INCREMENT_BYTES: usize = 32;

/// Allocate a heap-owned `ID3D12DeviceX` COM object with refcount = 1.
pub fn create_device_x_com(translator: Arc<D3D12Translator>) -> *mut c_void {
    let obj = Box::new(ComDeviceX {
        lp_vtbl: &DEVICE_VTBL,
        ref_count: AtomicU32::new(1),
        translator,
    });
    Box::into_raw(obj) as *mut c_void
}

// ===========================================================================
// Helper macros for IUnknown boilerplate
// ===========================================================================

macro_rules! impl_iunknown {
    ($struct_ty:ty, $iid:expr, $qi:ident, $addref:ident, $release:ident) => {
        unsafe extern "win64" fn $qi(
            this: *mut c_void,
            riid: *const Guid,
            ppv: *mut *mut c_void,
        ) -> HResult {
            if ppv.is_null() || riid.is_null() {
                return E_POINTER;
            }
            let iid = *riid;
            if iid == IID_IUNKNOWN || iid == IID_ID3D12OBJECT || iid == $iid {
                $addref(this);
                *ppv = this;
                S_OK
            } else {
                *ppv = ptr::null_mut();
                E_NOINTERFACE
            }
        }

        unsafe extern "win64" fn $addref(this: *mut c_void) -> u32 {
            let obj = &*(this as *const $struct_ty);
            obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
        }

        unsafe extern "win64" fn $release(this: *mut c_void) -> u32 {
            let obj = &*(this as *const $struct_ty);
            let prev = obj.ref_count.fetch_sub(1, Ordering::AcqRel);
            if prev == 1 {
                drop(Box::from_raw(this as *mut $struct_ty));
                0
            } else {
                prev - 1
            }
        }
    };
}

// ===========================================================================
// ID3D12DeviceX
// ===========================================================================

#[repr(C)]
struct ComDeviceX {
    lp_vtbl: *const ID3D12DeviceXVtbl,
    ref_count: AtomicU32,
    translator: Arc<D3D12Translator>,
}

impl_iunknown!(
    ComDeviceX,
    IID_ID3D12DEVICE,
    device_qi,
    device_add_ref,
    device_release
);

static DEVICE_VTBL: ID3D12DeviceXVtbl = ID3D12DeviceXVtbl {
    base: IUnknownVtbl {
        query_interface: device_qi,
        add_ref: device_add_ref,
        release: device_release,
    },
    create_command_queue: device_create_command_queue,
    create_command_allocator: device_create_command_allocator,
    create_graphics_command_list: device_create_graphics_command_list,
    create_descriptor_heap: device_create_descriptor_heap,
    get_descriptor_handle_increment_size: device_get_descriptor_handle_increment_size,
    create_committed_resource: device_create_committed_resource,
    create_render_target_view: device_create_render_target_view,
    create_fence: device_create_fence,
    wait_frame_event_x: device_wait_frame_event_x,
};

unsafe extern "win64" fn device_create_command_queue(
    this: *mut c_void,
    desc: *const D3D12CommandQueueDesc,
    _riid: *const Guid,
    pp_command_queue: *mut *mut c_void,
) -> HResult {
    if this.is_null() || desc.is_null() || pp_command_queue.is_null() {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const ComDeviceX);
    let d = *desc;
    dev.translator
        .record_trace(format!("ID3D12DeviceX::CreateCommandQueue(type={})", d.r#type));
    let obj = Box::new(ComCommandQueueX {
        lp_vtbl: &QUEUE_VTBL,
        ref_count: AtomicU32::new(1),
        translator: Arc::clone(&dev.translator),
    });
    *pp_command_queue = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn device_create_command_allocator(
    this: *mut c_void,
    r#type: u32,
    _riid: *const Guid,
    pp_command_allocator: *mut *mut c_void,
) -> HResult {
    if this.is_null() || pp_command_allocator.is_null() {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const ComDeviceX);
    dev.translator
        .record_trace(format!("ID3D12DeviceX::CreateCommandAllocator(type={type})"));
    let obj = Box::new(ComCommandAllocatorX {
        lp_vtbl: &ALLOCATOR_VTBL,
        ref_count: AtomicU32::new(1),
        translator: Arc::clone(&dev.translator),
    });
    *pp_command_allocator = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn device_create_graphics_command_list(
    this: *mut c_void,
    _node_mask: u32,
    r#type: u32,
    _p_command_allocator: *mut c_void,
    _p_initial_state: *mut c_void,
    _riid: *const Guid,
    pp_command_list: *mut *mut c_void,
) -> HResult {
    if this.is_null() || pp_command_list.is_null() {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const ComDeviceX);
    dev.translator.record_trace(format!(
        "ID3D12DeviceX::CreateGraphicsCommandList(type={type})"
    ));
    let obj = Box::new(ComGraphicsCommandListX {
        lp_vtbl: &CMDLIST_VTBL,
        ref_count: AtomicU32::new(1),
        translator: Arc::clone(&dev.translator),
        commands: Mutex::new(Vec::new()),
        closed: Mutex::new(false),
    });
    *pp_command_list = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn device_create_descriptor_heap(
    this: *mut c_void,
    desc: *const D3D12DescriptorHeapDesc,
    _riid: *const Guid,
    pp_heap: *mut *mut c_void,
) -> HResult {
    if this.is_null() || desc.is_null() || pp_heap.is_null() {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const ComDeviceX);
    let d = *desc;
    let base_handle = dev
        .translator
        .alloc_descriptor_range(d.num_descriptors, DESCRIPTOR_INCREMENT_BYTES);
    dev.translator.record_trace(format!(
        "ID3D12DeviceX::CreateDescriptorHeap(type={}, count={}, base=0x{base_handle:X})",
        d.r#type, d.num_descriptors
    ));
    let obj = Box::new(ComDescriptorHeapX {
        lp_vtbl: &DESCHEAP_VTBL,
        ref_count: AtomicU32::new(1),
        base_handle,
    });
    *pp_heap = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn device_get_descriptor_handle_increment_size(
    _this: *mut c_void,
    _heap_type: u32,
) -> u32 {
    DESCRIPTOR_INCREMENT_BYTES as u32
}

unsafe extern "win64" fn device_create_committed_resource(
    this: *mut c_void,
    p_heap_properties: *const D3D12HeapProperties,
    heap_flags: u32,
    p_desc: *const D3D12ResourceDesc,
    initial_resource_state: u32,
    _p_optimized_clear_value: *const c_void,
    _riid: *const Guid,
    ppv_resource: *mut *mut c_void,
) -> HResult {
    if this.is_null() || p_heap_properties.is_null() || p_desc.is_null() || ppv_resource.is_null()
    {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const ComDeviceX);
    match dev.translator.create_committed_resource(
        &*p_heap_properties,
        heap_flags,
        &*p_desc,
        initial_resource_state,
    ) {
        Ok((resource_id, gpu_va, kind)) => {
            let obj = Box::new(ComResourceX {
                lp_vtbl: &RESOURCE_VTBL,
                ref_count: AtomicU32::new(1),
                translator: Arc::clone(&dev.translator),
                resource_id,
                gpu_va,
                _kind: kind,
            });
            *ppv_resource = Box::into_raw(obj) as *mut c_void;
            S_OK
        }
        Err(_) => E_FAIL,
    }
}

unsafe extern "win64" fn device_create_render_target_view(
    this: *mut c_void,
    p_resource: *mut c_void,
    _p_desc: *const c_void,
    dest_descriptor: D3D12CpuDescriptorHandle,
) {
    if this.is_null() || p_resource.is_null() {
        return;
    }
    let dev = &*(this as *const ComDeviceX);
    let res = &*(p_resource as *const ComResourceX);
    dev.translator
        .create_rtv(dest_descriptor.ptr, res.resource_id);
}

unsafe extern "win64" fn device_create_fence(
    this: *mut c_void,
    initial_value: u64,
    _flags: u32,
    _riid: *const Guid,
    pp_fence: *mut *mut c_void,
) -> HResult {
    if this.is_null() || pp_fence.is_null() {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const ComDeviceX);
    let fence_id = dev.translator.create_fence(initial_value);
    let obj = Box::new(ComFenceX {
        lp_vtbl: &FENCE_VTBL,
        ref_count: AtomicU32::new(1),
        translator: Arc::clone(&dev.translator),
        fence_id,
    });
    *pp_fence = Box::into_raw(obj) as *mut c_void;
    S_OK
}

unsafe extern "win64" fn device_wait_frame_event_x(
    this: *mut c_void,
    event_type: u32,
    _timeout_ms: u32,
    _p_reserved: *mut c_void,
    p_frame_token: *mut u64,
) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let dev = &*(this as *const ComDeviceX);
    let token = dev.translator.next_frame_token();
    dev.translator.record_trace(format!(
        "ID3D12DeviceX::WaitFrameEventX(event={event_type}) -> token={token}"
    ));
    if !p_frame_token.is_null() {
        *p_frame_token = token;
    }
    S_OK
}

// ===========================================================================
// ID3D12CommandQueueX
// ===========================================================================

#[repr(C)]
struct ComCommandQueueX {
    lp_vtbl: *const ID3D12CommandQueueXVtbl,
    ref_count: AtomicU32,
    translator: Arc<D3D12Translator>,
}

impl_iunknown!(
    ComCommandQueueX,
    IID_ID3D12COMMAND_QUEUE,
    queue_qi,
    queue_add_ref,
    queue_release
);

static QUEUE_VTBL: ID3D12CommandQueueXVtbl = ID3D12CommandQueueXVtbl {
    base: IUnknownVtbl {
        query_interface: queue_qi,
        add_ref: queue_add_ref,
        release: queue_release,
    },
    execute_command_lists: queue_execute_command_lists,
    signal: queue_signal,
    present_x: queue_present_x,
};

unsafe extern "win64" fn queue_execute_command_lists(
    this: *mut c_void,
    num_command_lists: u32,
    pp_command_lists: *const *mut c_void,
) {
    if this.is_null() || pp_command_lists.is_null() || num_command_lists == 0 {
        return;
    }
    let queue = &*(this as *const ComCommandQueueX);
    for i in 0..num_command_lists as usize {
        let cl_ptr = *pp_command_lists.add(i);
        if cl_ptr.is_null() {
            continue;
        }
        let cl = &*(cl_ptr as *const ComGraphicsCommandListX);
        let cmds = cl.commands.lock().unwrap().clone();
        let _ = queue.translator.execute_command_list(&cmds);
    }
}

unsafe extern "win64" fn queue_signal(
    this: *mut c_void,
    p_fence: *mut c_void,
    value: u64,
) -> HResult {
    if this.is_null() || p_fence.is_null() {
        return E_INVALIDARG;
    }
    let queue = &*(this as *const ComCommandQueueX);
    let fence = &*(p_fence as *const ComFenceX);
    queue.translator.signal_fence(fence.fence_id, value);
    S_OK
}

unsafe extern "win64" fn queue_present_x(
    this: *mut c_void,
    plane_count: u32,
    p_plane_parameters: *const D3D12XboxPresentPlaneParameters,
    _p_present_parameters: *const c_void,
) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let queue = &*(this as *const ComCommandQueueX);
    let (resource_id, frame_token, sync_interval) =
        if plane_count > 0 && !p_plane_parameters.is_null() {
            let req = translate_present_x(&*p_plane_parameters);
            let res_id = if req.primary_resource_ptr != 0 {
                let res = &*(req.primary_resource_ptr as *const ComResourceX);
                Some(res.resource_id)
            } else {
                None
            };
            (res_id, req.frame_token, req.sync_interval)
        } else {
            (None, 0, 1)
        };

    match queue
        .translator
        .present_x(resource_id, frame_token, sync_interval)
    {
        Ok(()) => S_OK,
        Err(_) => E_FAIL,
    }
}

// ===========================================================================
// ID3D12CommandAllocatorX
// ===========================================================================

#[repr(C)]
struct ComCommandAllocatorX {
    lp_vtbl: *const ID3D12CommandAllocatorXVtbl,
    ref_count: AtomicU32,
    translator: Arc<D3D12Translator>,
}

impl_iunknown!(
    ComCommandAllocatorX,
    IID_ID3D12COMMAND_ALLOCATOR,
    alloc_qi,
    alloc_add_ref,
    alloc_release
);

static ALLOCATOR_VTBL: ID3D12CommandAllocatorXVtbl = ID3D12CommandAllocatorXVtbl {
    base: IUnknownVtbl {
        query_interface: alloc_qi,
        add_ref: alloc_add_ref,
        release: alloc_release,
    },
    reset: alloc_reset,
};

unsafe extern "win64" fn alloc_reset(this: *mut c_void) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let alloc = &*(this as *const ComCommandAllocatorX);
    alloc.translator.record_trace("ID3D12CommandAllocatorX::Reset()");
    S_OK
}

// ===========================================================================
// ID3D12GraphicsCommandListX
// ===========================================================================

#[repr(C)]
struct ComGraphicsCommandListX {
    lp_vtbl: *const ID3D12GraphicsCommandListXVtbl,
    ref_count: AtomicU32,
    translator: Arc<D3D12Translator>,
    commands: Mutex<Vec<GpuCommand>>,
    closed: Mutex<bool>,
}

impl_iunknown!(
    ComGraphicsCommandListX,
    IID_ID3D12GRAPHICS_COMMAND_LIST,
    cmdlist_qi,
    cmdlist_add_ref,
    cmdlist_release
);

static CMDLIST_VTBL: ID3D12GraphicsCommandListXVtbl = ID3D12GraphicsCommandListXVtbl {
    base: IUnknownVtbl {
        query_interface: cmdlist_qi,
        add_ref: cmdlist_add_ref,
        release: cmdlist_release,
    },
    close: cmdlist_close,
    reset: cmdlist_reset,
    resource_barrier: cmdlist_resource_barrier,
    rs_set_viewports: cmdlist_rs_set_viewports,
    rs_set_scissor_rects: cmdlist_rs_set_scissor_rects,
    om_set_render_targets: cmdlist_om_set_render_targets,
    clear_render_target_view: cmdlist_clear_render_target_view,
    ia_set_primitive_topology: cmdlist_ia_set_primitive_topology,
    ia_set_vertex_buffers: cmdlist_ia_set_vertex_buffers,
    draw_instanced: cmdlist_draw_instanced,
};

unsafe extern "win64" fn cmdlist_close(this: *mut c_void) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    *cl.closed.lock().unwrap() = true;
    cl.translator
        .record_trace("ID3D12GraphicsCommandListX::Close()");
    S_OK
}

unsafe extern "win64" fn cmdlist_reset(
    this: *mut c_void,
    _p_allocator: *mut c_void,
    _p_initial_state: *mut c_void,
) -> HResult {
    if this.is_null() {
        return E_INVALIDARG;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    cl.commands.lock().unwrap().clear();
    *cl.closed.lock().unwrap() = false;
    cl.translator
        .record_trace("ID3D12GraphicsCommandListX::Reset()");
    S_OK
}

unsafe extern "win64" fn cmdlist_resource_barrier(
    this: *mut c_void,
    num_barriers: u32,
    p_barriers: *const D3D12ResourceBarrier,
) {
    if this.is_null() || p_barriers.is_null() {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    let mut cmds = cl.commands.lock().unwrap();
    for i in 0..num_barriers as usize {
        let b = *p_barriers.add(i);
        let res_id = if !b.transition.p_resource.is_null() {
            (*(b.transition.p_resource as *const ComResourceX)).resource_id
        } else {
            0
        };
        cmds.push(GpuCommand::ResourceBarrier {
            resource_id: res_id,
            before: b.transition.state_before,
            after: b.transition.state_after,
        });
    }
}

unsafe extern "win64" fn cmdlist_rs_set_viewports(
    this: *mut c_void,
    num_viewports: u32,
    p_viewports: *const D3D12Viewport,
) {
    if this.is_null() || p_viewports.is_null() || num_viewports == 0 {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    let vp = *p_viewports;
    cl.commands.lock().unwrap().push(GpuCommand::SetViewport(vp));
}

unsafe extern "win64" fn cmdlist_rs_set_scissor_rects(
    this: *mut c_void,
    num_rects: u32,
    p_rects: *const D3D12Rect,
) {
    if this.is_null() || p_rects.is_null() || num_rects == 0 {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    let rect = *p_rects;
    cl.commands.lock().unwrap().push(GpuCommand::SetScissor(rect));
}

unsafe extern "win64" fn cmdlist_om_set_render_targets(
    this: *mut c_void,
    num_rt_descriptors: u32,
    p_rt_descriptors: *const D3D12CpuDescriptorHandle,
    _single_handle: i32,
    _p_dsv: *const D3D12CpuDescriptorHandle,
) {
    if this.is_null() || p_rt_descriptors.is_null() || num_rt_descriptors == 0 {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    let rtv = *p_rt_descriptors;
    cl.commands
        .lock()
        .unwrap()
        .push(GpuCommand::SetRenderTarget { rtv_handle: rtv.ptr });
}

unsafe extern "win64" fn cmdlist_clear_render_target_view(
    this: *mut c_void,
    rtv: D3D12CpuDescriptorHandle,
    color_rgba: *const [f32; 4],
    num_rects: u32,
    p_rects: *const D3D12Rect,
) {
    if this.is_null() || color_rgba.is_null() {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    let color = *color_rgba;
    let rects = if num_rects > 0 && !p_rects.is_null() {
        std::slice::from_raw_parts(p_rects, num_rects as usize).to_vec()
    } else {
        Vec::new()
    };
    cl.commands.lock().unwrap().push(GpuCommand::ClearRenderTarget {
        rtv_handle: rtv.ptr,
        color,
        rects,
    });
}

unsafe extern "win64" fn cmdlist_ia_set_primitive_topology(
    this: *mut c_void,
    primitive_topology: u32,
) {
    if this.is_null() {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    cl.commands
        .lock()
        .unwrap()
        .push(GpuCommand::SetPrimitiveTopology(primitive_topology));
}

unsafe extern "win64" fn cmdlist_ia_set_vertex_buffers(
    this: *mut c_void,
    start_slot: u32,
    num_views: u32,
    p_views: *const D3D12VertexBufferView,
) {
    if this.is_null() || p_views.is_null() || num_views == 0 {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    let mut cmds = cl.commands.lock().unwrap();
    for i in 0..num_views as usize {
        cmds.push(GpuCommand::SetVertexBuffer {
            slot: start_slot + i as u32,
            view: *p_views.add(i),
        });
    }
}

unsafe extern "win64" fn cmdlist_draw_instanced(
    this: *mut c_void,
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex_location: u32,
    start_instance_location: u32,
) {
    if this.is_null() {
        return;
    }
    let cl = &*(this as *const ComGraphicsCommandListX);
    cl.commands.lock().unwrap().push(GpuCommand::DrawInstanced {
        vertex_count: vertex_count_per_instance,
        instance_count,
        start_vertex: start_vertex_location,
        start_instance: start_instance_location,
    });
}

// ===========================================================================
// ID3D12DescriptorHeapX
// ===========================================================================

#[repr(C)]
struct ComDescriptorHeapX {
    lp_vtbl: *const ID3D12DescriptorHeapXVtbl,
    ref_count: AtomicU32,
    base_handle: usize,
}

impl_iunknown!(
    ComDescriptorHeapX,
    IID_ID3D12DESCRIPTOR_HEAP,
    heap_qi,
    heap_add_ref,
    heap_release
);

static DESCHEAP_VTBL: ID3D12DescriptorHeapXVtbl = ID3D12DescriptorHeapXVtbl {
    base: IUnknownVtbl {
        query_interface: heap_qi,
        add_ref: heap_add_ref,
        release: heap_release,
    },
    get_cpu_descriptor_handle_for_heap_start: heap_get_cpu_start,
};

unsafe extern "win64" fn heap_get_cpu_start(this: *mut c_void) -> D3D12CpuDescriptorHandle {
    let heap = &*(this as *const ComDescriptorHeapX);
    D3D12CpuDescriptorHandle {
        ptr: heap.base_handle,
    }
}

// ===========================================================================
// ID3D12ResourceX
// ===========================================================================

#[repr(C)]
struct ComResourceX {
    lp_vtbl: *const ID3D12ResourceXVtbl,
    ref_count: AtomicU32,
    translator: Arc<D3D12Translator>,
    resource_id: u64,
    gpu_va: u64,
    _kind: ResourceKind,
}

impl_iunknown!(
    ComResourceX,
    IID_ID3D12RESOURCE,
    res_qi,
    res_add_ref,
    res_release
);

static RESOURCE_VTBL: ID3D12ResourceXVtbl = ID3D12ResourceXVtbl {
    base: IUnknownVtbl {
        query_interface: res_qi,
        add_ref: res_add_ref,
        release: res_release,
    },
    map: res_map,
    unmap: res_unmap,
    get_gpu_virtual_address: res_get_gpu_va,
};

unsafe extern "win64" fn res_map(
    this: *mut c_void,
    _subresource: u32,
    _p_read_range: *const D3D12Range,
    pp_data: *mut *mut c_void,
) -> HResult {
    if this.is_null() || pp_data.is_null() {
        return E_INVALIDARG;
    }
    let res = &*(this as *const ComResourceX);
    match res.translator.map_resource(res.resource_id) {
        Ok(ptr) => {
            *pp_data = ptr as *mut c_void;
            S_OK
        }
        Err(_) => E_FAIL,
    }
}

unsafe extern "win64" fn res_unmap(
    this: *mut c_void,
    _subresource: u32,
    _p_written_range: *const D3D12Range,
) {
    if this.is_null() {
        return;
    }
    let res = &*(this as *const ComResourceX);
    res.translator.unmap_resource(res.resource_id);
}

unsafe extern "win64" fn res_get_gpu_va(this: *mut c_void) -> u64 {
    if this.is_null() {
        return 0;
    }
    let res = &*(this as *const ComResourceX);
    res.gpu_va
}

// ===========================================================================
// ID3D12FenceX
// ===========================================================================

#[repr(C)]
struct ComFenceX {
    lp_vtbl: *const ID3D12FenceXVtbl,
    ref_count: AtomicU32,
    translator: Arc<D3D12Translator>,
    fence_id: u64,
}

impl_iunknown!(
    ComFenceX,
    IID_ID3D12FENCE,
    fence_qi,
    fence_add_ref,
    fence_release
);

static FENCE_VTBL: ID3D12FenceXVtbl = ID3D12FenceXVtbl {
    base: IUnknownVtbl {
        query_interface: fence_qi,
        add_ref: fence_add_ref,
        release: fence_release,
    },
    get_completed_value: fence_get_completed_value,
    set_event_on_completion: fence_set_event_on_completion,
};

unsafe extern "win64" fn fence_get_completed_value(this: *mut c_void) -> u64 {
    if this.is_null() {
        return 0;
    }
    let fence = &*(this as *const ComFenceX);
    fence.translator.fence_value(fence.fence_id)
}

unsafe extern "win64" fn fence_set_event_on_completion(
    _this: *mut c_void,
    _value: u64,
    _h_event: *mut c_void,
) -> HResult {
    S_OK
}
