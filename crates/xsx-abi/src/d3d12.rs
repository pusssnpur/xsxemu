//! Direct3D 12 and Xbox Series X `d3d12_x` ABI structures and COM vtable layouts.
//!
//! Struct layouts match public Direct3D 12 documentation so they can be passed
//! across the Win64 ABI boundary to both our `d3d12_x` translation layer and
//! native Windows `d3d12.dll` / `dxgi.dll`.

use core::ffi::c_void;

use crate::com::{Guid, HResult, IUnknownVtbl};

// ---------------------------------------------------------------------------
// Core D3D12 Enums & Constants
// ---------------------------------------------------------------------------

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3DFeatureLevel {
    Level11_0 = 0xb000,
    Level11_1 = 0xb100,
    Level12_0 = 0xc000,
    Level12_1 = 0xc100,
    Level12_2 = 0xc200,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3D12CommandListType {
    Direct = 0,
    Bundle = 1,
    Compute = 2,
    Copy = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3D12DescriptorHeapType {
    CbvSrvUav = 0,
    Sampler = 1,
    Rtv = 2,
    Dsv = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3D12HeapType {
    Default = 1,
    Upload = 2,
    Readback = 3,
    Custom = 4,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3D12ResourceDimension {
    Unknown = 0,
    Buffer = 1,
    Texture1D = 2,
    Texture2D = 3,
    Texture3D = 4,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3D12TextureLayout {
    Unknown = 0,
    RowMajor = 1,
    UndefinedSwizzle64KB = 2,
    StandardSwizzle64KB = 3,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DxgiFormat {
    Unknown = 0,
    R32G32B32A32Float = 2,
    R32G32B32Float = 6,
    R8G8B8A8Unorm = 28,
    R8G8B8A8UnormSrgb = 29,
    B8G8R8A8Unorm = 87,
    D32Float = 40,
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum D3DPrimitiveTopology {
    Undefined = 0,
    PointList = 1,
    LineList = 2,
    LineStrip = 3,
    TriangleList = 4,
    TriangleStrip = 5,
}

// Common D3D12_RESOURCE_STATES bitflags
pub const D3D12_RESOURCE_STATE_COMMON: u32 = 0;
pub const D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER: u32 = 0x1;
pub const D3D12_RESOURCE_STATE_RENDER_TARGET: u32 = 0x4;
pub const D3D12_RESOURCE_STATE_GENERIC_READ: u32 = 0x1 | 0x2 | 0x40 | 0x80 | 0x200 | 0x800;
pub const D3D12_RESOURCE_STATE_PRESENT: u32 = 0;

// Common D3D12_RESOURCE_FLAGS
pub const D3D12_RESOURCE_FLAG_NONE: u32 = 0;
pub const D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET: u32 = 0x1;
pub const D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL: u32 = 0x2;
pub const D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS: u32 = 0x4;

// Xbox-specific heap / creation flags that must be sanitized for PC D3D12
pub const D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE: u32 = 0x1000_0000;
pub const D3D12XBOX_HEAP_FLAG_CPU_CACHED: u32 = 0x2000_0000;
pub const D3D12XBOX_CREATE_DEVICE_FLAG_NONE: u32 = 0;
pub const D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG: u32 = 0x1;

// ---------------------------------------------------------------------------
// Core D3D12 Structs
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12CommandQueueDesc {
    pub r#type: u32,
    pub priority: i32,
    pub flags: u32,
    pub node_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12DescriptorHeapDesc {
    pub r#type: u32,
    pub num_descriptors: u32,
    pub flags: u32,
    pub node_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12CpuDescriptorHandle {
    pub ptr: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12GpuDescriptorHandle {
    pub ptr: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12HeapProperties {
    pub r#type: u32,
    pub cpu_page_property: u32,
    pub memory_pool_preference: u32,
    pub creation_node_mask: u32,
    pub visible_node_mask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DxgiSampleDesc {
    pub count: u32,
    pub quality: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12ResourceDesc {
    pub dimension: u32,
    pub alignment: u64,
    pub width: u64,
    pub height: u32,
    pub depth_or_array_size: u16,
    pub mip_levels: u16,
    pub format: u32,
    pub sample_desc: DxgiSampleDesc,
    pub layout: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12Viewport {
    pub top_left_x: f32,
    pub top_left_y: f32,
    pub width: f32,
    pub height: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12VertexBufferView {
    pub buffer_location: u64,
    pub size_in_bytes: u32,
    pub stride_in_bytes: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12Range {
    pub begin: usize,
    pub end: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12ResourceTransitionBarrier {
    pub p_resource: *mut c_void,
    pub subresource: u32,
    pub state_before: u32,
    pub state_after: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12ResourceBarrier {
    pub r#type: u32,
    pub flags: u32,
    pub transition: D3D12ResourceTransitionBarrier,
}

/// Xbox Series X `D3D12XBOX_CREATE_DEVICE_PARAMETERS` (clean-room ABI subset).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct D3D12XboxCreateDeviceParameters {
    pub version: u32,
    pub process_debug_flags: u32,
    pub graphics_command_queue_ring_size_bytes: u32,
    pub graphics_scratch_memory_size_bytes: u32,
    pub compute_scratch_memory_size_bytes: u32,
}

/// Xbox Series X `D3D12XBOX_PRESENT_PLANE_PARAMETERS` (clean-room ABI subset).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct D3D12XboxPresentPlaneParameters {
    pub token: u64,
    pub resource_count: u32,
    pub _pad: u32,
    pub pp_resources: *const *mut c_void,
    pub color_space: u32,
    pub options: u32,
}

/// Vertex format used by our clean-room vertical-slice test pipeline:
/// clip-space `(x, y, z)` + linear RGBA `(r, g, b, a)`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SimpleVertex {
    pub pos: [f32; 3],
    pub color: [f32; 4],
}

// ---------------------------------------------------------------------------
// Clean-room Xbox `d3d12_x` COM VTable Layouts
//
// Each interface starts with `IUnknownVtbl` (slots 0..=2) followed by our
// documented method table for the vertical slice. Every method uses
// `extern "win64"` so guest PE64 binaries invoke them natively.
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct ID3D12DeviceXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub create_command_queue: unsafe extern "win64" fn(
        this: *mut c_void,
        desc: *const D3D12CommandQueueDesc,
        riid: *const Guid,
        pp_command_queue: *mut *mut c_void,
    ) -> HResult,
    // Slot 4
    pub create_command_allocator: unsafe extern "win64" fn(
        this: *mut c_void,
        r#type: u32,
        riid: *const Guid,
        pp_command_allocator: *mut *mut c_void,
    ) -> HResult,
    // Slot 5
    pub create_graphics_command_list: unsafe extern "win64" fn(
        this: *mut c_void,
        node_mask: u32,
        r#type: u32,
        p_command_allocator: *mut c_void,
        p_initial_state: *mut c_void,
        riid: *const Guid,
        pp_command_list: *mut *mut c_void,
    ) -> HResult,
    // Slot 6
    pub create_descriptor_heap: unsafe extern "win64" fn(
        this: *mut c_void,
        desc: *const D3D12DescriptorHeapDesc,
        riid: *const Guid,
        pp_heap: *mut *mut c_void,
    ) -> HResult,
    // Slot 7
    pub get_descriptor_handle_increment_size:
        unsafe extern "win64" fn(this: *mut c_void, heap_type: u32) -> u32,
    // Slot 8
    pub create_committed_resource: unsafe extern "win64" fn(
        this: *mut c_void,
        p_heap_properties: *const D3D12HeapProperties,
        heap_flags: u32,
        p_desc: *const D3D12ResourceDesc,
        initial_resource_state: u32,
        p_optimized_clear_value: *const c_void,
        riid: *const Guid,
        ppv_resource: *mut *mut c_void,
    ) -> HResult,
    // Slot 9
    pub create_render_target_view: unsafe extern "win64" fn(
        this: *mut c_void,
        p_resource: *mut c_void,
        p_desc: *const c_void,
        dest_descriptor: D3D12CpuDescriptorHandle,
    ),
    // Slot 10
    pub create_fence: unsafe extern "win64" fn(
        this: *mut c_void,
        initial_value: u64,
        flags: u32,
        riid: *const Guid,
        pp_fence: *mut *mut c_void,
    ) -> HResult,
    // Slot 11 (Xbox extension: WaitFrameEventX)
    pub wait_frame_event_x: unsafe extern "win64" fn(
        this: *mut c_void,
        event_type: u32,
        timeout_ms: u32,
        p_reserved: *mut c_void,
        p_frame_token: *mut u64,
    ) -> HResult,
}

#[repr(C)]
pub struct ID3D12CommandQueueXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub execute_command_lists: unsafe extern "win64" fn(
        this: *mut c_void,
        num_command_lists: u32,
        pp_command_lists: *const *mut c_void,
    ),
    // Slot 4
    pub signal: unsafe extern "win64" fn(
        this: *mut c_void,
        p_fence: *mut c_void,
        value: u64,
    ) -> HResult,
    // Slot 5 (Xbox Series X direct-flip present)
    pub present_x: unsafe extern "win64" fn(
        this: *mut c_void,
        plane_count: u32,
        p_plane_parameters: *const D3D12XboxPresentPlaneParameters,
        p_present_parameters: *const c_void,
    ) -> HResult,
}

#[repr(C)]
pub struct ID3D12CommandAllocatorXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub reset: unsafe extern "win64" fn(this: *mut c_void) -> HResult,
}

#[repr(C)]
pub struct ID3D12GraphicsCommandListXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub close: unsafe extern "win64" fn(this: *mut c_void) -> HResult,
    // Slot 4
    pub reset: unsafe extern "win64" fn(
        this: *mut c_void,
        p_allocator: *mut c_void,
        p_initial_state: *mut c_void,
    ) -> HResult,
    // Slot 5
    pub resource_barrier: unsafe extern "win64" fn(
        this: *mut c_void,
        num_barriers: u32,
        p_barriers: *const D3D12ResourceBarrier,
    ),
    // Slot 6
    pub rs_set_viewports: unsafe extern "win64" fn(
        this: *mut c_void,
        num_viewports: u32,
        p_viewports: *const D3D12Viewport,
    ),
    // Slot 7
    pub rs_set_scissor_rects: unsafe extern "win64" fn(
        this: *mut c_void,
        num_rects: u32,
        p_rects: *const D3D12Rect,
    ),
    // Slot 8
    pub om_set_render_targets: unsafe extern "win64" fn(
        this: *mut c_void,
        num_rt_descriptors: u32,
        p_rt_descriptors: *const D3D12CpuDescriptorHandle,
        rts_single_handle_to_descriptor_range: i32,
        p_depth_stencil_descriptor: *const D3D12CpuDescriptorHandle,
    ),
    // Slot 9
    pub clear_render_target_view: unsafe extern "win64" fn(
        this: *mut c_void,
        rtv: D3D12CpuDescriptorHandle,
        color_rgba: *const [f32; 4],
        num_rects: u32,
        p_rects: *const D3D12Rect,
    ),
    // Slot 10
    pub ia_set_primitive_topology:
        unsafe extern "win64" fn(this: *mut c_void, primitive_topology: u32),
    // Slot 11
    pub ia_set_vertex_buffers: unsafe extern "win64" fn(
        this: *mut c_void,
        start_slot: u32,
        num_views: u32,
        p_views: *const D3D12VertexBufferView,
    ),
    // Slot 12
    pub draw_instanced: unsafe extern "win64" fn(
        this: *mut c_void,
        vertex_count_per_instance: u32,
        instance_count: u32,
        start_vertex_location: u32,
        start_instance_location: u32,
    ),
}

#[repr(C)]
pub struct ID3D12DescriptorHeapXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3: Note in Win64 C++ ABI, small 8-byte structs returned by value fit in RAX.
    pub get_cpu_descriptor_handle_for_heap_start:
        unsafe extern "win64" fn(this: *mut c_void) -> D3D12CpuDescriptorHandle,
}

#[repr(C)]
pub struct ID3D12ResourceXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub map: unsafe extern "win64" fn(
        this: *mut c_void,
        subresource: u32,
        p_read_range: *const D3D12Range,
        pp_data: *mut *mut c_void,
    ) -> HResult,
    // Slot 4
    pub unmap: unsafe extern "win64" fn(
        this: *mut c_void,
        subresource: u32,
        p_written_range: *const D3D12Range,
    ),
    // Slot 5
    pub get_gpu_virtual_address: unsafe extern "win64" fn(this: *mut c_void) -> u64,
}

#[repr(C)]
pub struct ID3D12FenceXVtbl {
    pub base: IUnknownVtbl,
    // Slot 3
    pub get_completed_value: unsafe extern "win64" fn(this: *mut c_void) -> u64,
    // Slot 4
    pub set_event_on_completion: unsafe extern "win64" fn(
        this: *mut c_void,
        value: u64,
        h_event: *mut c_void,
    ) -> HResult,
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn d3d12_struct_sizes_match_win64_abi() {
        assert_eq!(size_of::<D3D12CommandQueueDesc>(), 16);
        assert_eq!(size_of::<D3D12DescriptorHeapDesc>(), 16);
        assert_eq!(size_of::<D3D12HeapProperties>(), 20);
        assert_eq!(size_of::<D3D12ResourceDesc>(), 56);
        assert_eq!(size_of::<D3D12Viewport>(), 24);
        assert_eq!(size_of::<D3D12Rect>(), 16);
        assert_eq!(size_of::<D3D12VertexBufferView>(), 16);
        assert_eq!(size_of::<D3D12CpuDescriptorHandle>(), 8);
        assert_eq!(size_of::<SimpleVertex>(), 28);
    }
}
