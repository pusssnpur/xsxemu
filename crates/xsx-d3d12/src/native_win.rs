//! True Hardware Windows x86_64 Direct3D 12 Backend (`d3d12.dll` + `dxgi.dll` + `user32.dll` + `d3dcompiler_47.dll`).
//!
//! Executes real GPU work on Windows x86_64:
//! - Creates a Win32 HWND (`--windowed` visible or hidden/headless) with Win32 message pumping
//! - Creates `IDXGIFactory4`, hardware/WARP `ID3D12Device`, `ID3D12CommandQueue`, `ID3D12CommandAllocator`,
//!   `ID3D12GraphicsCommandList`, RTV `ID3D12DescriptorHeap`, `ID3D12Fence` + Win32 fence event
//! - Creates `IDXGISwapChain3` with `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`, extracts
//!   backbuffers (`GetBuffer(0..2)`), and creates real host RTVs (`CreateRenderTargetView`)
//! - Compiles `FSR_EASU_HLSL` and `FSR_RCAS_HLSL` via `d3dcompiler_47.dll!D3DCompile`, serializes a
//!   D3D12 root signature via `D3D12SerializeRootSignature`, and builds host GPU FSR compute PSOs
//! - Replays real D3D12 command lists (`Reset`, `ResourceBarrier`, `OMSetRenderTargets`,
//!   `ClearRenderTargetView`, `RSSetViewports`, `RSSetScissorRects`, `Close`, `ExecuteCommandLists`,
//!   `IDXGISwapChain3::Present`, and GPU `Signal` + `SetEventOnCompletion` fence wait)

use crate::backend::GpuBackend;
use crate::translate::HostDeviceCreateConfig;

#[cfg(not(target_os = "windows"))]
pub fn create_native_backend(
    _width: u32,
    _height: u32,
    _device_cfg: HostDeviceCreateConfig,
    _windowed: bool,
) -> Result<Box<dyn GpuBackend>, String> {
    Err("NativeD3D12 backend requires a Windows x86_64 host (use --backend software or auto)".into())
}

#[cfg(target_os = "windows")]
pub fn create_native_backend(
    width: u32,
    height: u32,
    device_cfg: HostDeviceCreateConfig,
    windowed: bool,
) -> Result<Box<dyn GpuBackend>, String> {
    windows_impl::NativeD3D12Backend::new(width, height, device_cfg, windowed)
        .map(|b| Box::new(b) as Box<dyn GpuBackend>)
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use core::ffi::{c_char, c_void};
    use std::ptr;

    use xsx_abi::com::{
        failed, succeeded, Guid, HResult, IUnknownVtbl, IID_ID3D12COMMAND_ALLOCATOR,
        IID_ID3D12COMMAND_QUEUE, IID_ID3D12DESCRIPTOR_HEAP, IID_ID3D12DEVICE, IID_ID3D12FENCE,
        IID_ID3D12GRAPHICS_COMMAND_LIST, IID_ID3D12PIPELINE_STATE, IID_ID3D12RESOURCE,
        IID_ID3D12ROOT_SIGNATURE, IID_IDXGIFACTORY4, IID_IDXGISWAPCHAIN3,
    };
    use xsx_abi::d3d12::{
        D3D12CommandListType, D3D12CommandQueueDesc, D3D12CpuDescriptorHandle,
        D3D12DescriptorHeapDesc, D3D12DescriptorHeapType, D3D12HeapProperties, D3D12Rect,
        D3D12ResourceBarrier, D3D12ResourceDesc, D3D12ResourceTransitionBarrier,
        D3D12ShaderBytecode, D3D12Viewport, DxgiFormat, DxgiSampleDesc, DxgiSwapChainDesc1,
        D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET,
        DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_DISCARD,
        DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    use crate::backend::{GpuBackend, GpuCommand, PresentedFrame};
    use crate::fsr::{FsrComputePlan, FSR_EASU_HLSL, FSR_RCAS_HLSL};
    use crate::software::SoftwareD3D12Backend;
    use crate::translate::HostDeviceCreateConfig;

    #[repr(C)]
    struct HostRootSignatureDesc {
        num_parameters: u32,
        p_parameters: *const c_void,
        num_static_samplers: u32,
        p_static_samplers: *const c_void,
        flags: u32,
    }

    #[repr(C)]
    struct HostComputePsoDesc {
        p_root_signature: *mut c_void,
        cs: D3D12ShaderBytecode,
        node_mask: u32,
        cached_pso_blob: *const c_void,
        cached_pso_size: usize,
        flags: u32,
    }

    #[repr(C)]
    struct Win32Msg {
        hwnd: *mut c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt_x: i32,
        pt_y: i32,
    }

    #[link(name = "d3d12")]
    extern "system" {
        fn D3D12CreateDevice(
            p_adapter: *mut c_void,
            minimum_feature_level: u32,
            riid: *const Guid,
            pp_device: *mut *mut c_void,
        ) -> HResult;

        fn D3D12SerializeRootSignature(
            p_root_signature: *const HostRootSignatureDesc,
            version: u32,
            pp_blob: *mut *mut c_void,
            pp_error_blob: *mut *mut c_void,
        ) -> HResult;
    }

    #[link(name = "dxgi")]
    extern "system" {
        fn CreateDXGIFactory2(
            flags: u32,
            riid: *const Guid,
            pp_factory: *mut *mut c_void,
        ) -> HResult;
    }

    #[link(name = "user32")]
    extern "system" {
        fn CreateWindowExA(
            dw_ex_style: u32,
            lp_class_name: *const c_char,
            lp_window_name: *const c_char,
            dw_style: u32,
            x: i32,
            y: i32,
            n_width: i32,
            n_height: i32,
            hwnd_parent: *mut c_void,
            h_menu: *mut c_void,
            h_instance: *mut c_void,
            lp_param: *mut c_void,
        ) -> *mut c_void;
        fn DestroyWindow(hwnd: *mut c_void) -> i32;
        fn PeekMessageA(
            lp_msg: *mut Win32Msg,
            hwnd: *mut c_void,
            w_msg_filter_min: u32,
            w_msg_filter_max: u32,
            w_remove_msg: u32,
        ) -> i32;
        fn TranslateMessage(lp_msg: *const Win32Msg) -> i32;
        fn DispatchMessageA(lp_msg: *const Win32Msg) -> isize;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateEventA(
            lp_attrs: *mut c_void,
            b_manual_reset: i32,
            b_initial_state: i32,
            lp_name: *const c_char,
        ) -> *mut c_void;
        fn WaitForSingleObject(h_handle: *mut c_void, dw_ms: u32) -> u32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
        fn LoadLibraryA(lp_lib_file_name: *const c_char) -> *mut c_void;
        fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const c_char) -> *mut c_void;
    }

    // Exact Windows SDK ID3DBlob vtable (slots 0..=4)
    #[repr(C)]
    struct HostID3DBlobVtbl {
        base: IUnknownVtbl,
        get_buffer_pointer: unsafe extern "system" fn(this: *mut c_void) -> *const c_void,
        get_buffer_size: unsafe extern "system" fn(this: *mut c_void) -> usize,
    }

    // Exact Windows SDK ID3D12Device vtable (slots 0..=36)
    #[repr(C)]
    struct HostID3D12DeviceVtbl {
        base: IUnknownVtbl,
        _slot3_to_7: [usize; 5],
        // Slot 8
        create_command_queue: unsafe extern "system" fn(
            this: *mut c_void,
            p_desc: *const D3D12CommandQueueDesc,
            riid: *const Guid,
            pp_command_queue: *mut *mut c_void,
        ) -> HResult,
        // Slot 9
        create_command_allocator: unsafe extern "system" fn(
            this: *mut c_void,
            r#type: u32,
            riid: *const Guid,
            pp_allocator: *mut *mut c_void,
        ) -> HResult,
        // Slot 10
        _create_graphics_pso: usize,
        // Slot 11
        create_compute_pso: unsafe extern "system" fn(
            this: *mut c_void,
            p_desc: *const HostComputePsoDesc,
            riid: *const Guid,
            pp_pso: *mut *mut c_void,
        ) -> HResult,
        // Slot 12
        create_command_list: unsafe extern "system" fn(
            this: *mut c_void,
            node_mask: u32,
            r#type: u32,
            p_allocator: *mut c_void,
            p_initial_state: *mut c_void,
            riid: *const Guid,
            pp_cmd_list: *mut *mut c_void,
        ) -> HResult,
        // Slot 13
        _check_feature_support: usize,
        // Slot 14
        create_descriptor_heap: unsafe extern "system" fn(
            this: *mut c_void,
            p_desc: *const D3D12DescriptorHeapDesc,
            riid: *const Guid,
            pp_heap: *mut *mut c_void,
        ) -> HResult,
        // Slot 15
        get_descriptor_handle_increment_size:
            unsafe extern "system" fn(this: *mut c_void, heap_type: u32) -> u32,
        // Slot 16
        create_root_signature: unsafe extern "system" fn(
            this: *mut c_void,
            node_mask: u32,
            p_blob: *const c_void,
            blob_len: usize,
            riid: *const Guid,
            pp_root_sig: *mut *mut c_void,
        ) -> HResult,
        _slot17_to_19: [usize; 3],
        // Slot 20
        create_render_target_view: unsafe extern "system" fn(
            this: *mut c_void,
            p_resource: *mut c_void,
            p_desc: *const c_void,
            dest_descriptor: D3D12CpuDescriptorHandle,
        ),
        _slot21_to_35: [usize; 15],
        // Slot 36
        create_fence: unsafe extern "system" fn(
            this: *mut c_void,
            initial_value: u64,
            flags: u32,
            riid: *const Guid,
            pp_fence: *mut *mut c_void,
        ) -> HResult,
    }

    // Exact Windows SDK ID3D12CommandQueue vtable (slots 0..=14)
    #[repr(C)]
    struct HostID3D12CommandQueueVtbl {
        base: IUnknownVtbl,
        _slot3_to_9: [usize; 7],
        // Slot 10
        execute_command_lists: unsafe extern "system" fn(
            this: *mut c_void,
            num_command_lists: u32,
            pp_command_lists: *const *mut c_void,
        ),
        _slot11_to_13: [usize; 3],
        // Slot 14
        signal: unsafe extern "system" fn(
            this: *mut c_void,
            p_fence: *mut c_void,
            value: u64,
        ) -> HResult,
    }

    // Exact Windows SDK ID3D12CommandAllocator vtable (slots 0..=8)
    #[repr(C)]
    struct HostID3D12CommandAllocatorVtbl {
        base: IUnknownVtbl,
        _slot3_to_7: [usize; 5],
        // Slot 8
        reset: unsafe extern "system" fn(this: *mut c_void) -> HResult,
    }

    // Exact Windows SDK ID3D12DescriptorHeap vtable (slots 0..=9; MSVC hidden out-ptr return!)
    #[repr(C)]
    struct HostID3D12DescriptorHeapVtbl {
        base: IUnknownVtbl,
        _slot3_to_8: [usize; 6],
        // Slot 9
        get_cpu_handle_for_heap_start: unsafe extern "system" fn(
            this: *mut c_void,
            out_handle: *mut D3D12CpuDescriptorHandle,
        ) -> *mut D3D12CpuDescriptorHandle,
    }

    // Exact Windows SDK ID3D12Fence vtable (slots 0..=9)
    #[repr(C)]
    struct HostID3D12FenceVtbl {
        base: IUnknownVtbl,
        _slot3_to_7: [usize; 5],
        // Slot 8
        get_completed_value: unsafe extern "system" fn(this: *mut c_void) -> u64,
        // Slot 9
        set_event_on_completion: unsafe extern "system" fn(
            this: *mut c_void,
            value: u64,
            h_event: *mut c_void,
        ) -> HResult,
    }

    // Exact Windows SDK ID3D12GraphicsCommandList vtable (slots 0..=48)
    #[repr(C)]
    struct HostID3D12GraphicsCommandListVtbl {
        base: IUnknownVtbl,
        _slot3_to_8: [usize; 6],
        // Slot 9
        close: unsafe extern "system" fn(this: *mut c_void) -> HResult,
        // Slot 10
        reset: unsafe extern "system" fn(
            this: *mut c_void,
            p_allocator: *mut c_void,
            p_initial_state: *mut c_void,
        ) -> HResult,
        _slot11_to_20: [usize; 10],
        // Slot 21
        rs_set_viewports: unsafe extern "system" fn(
            this: *mut c_void,
            num_viewports: u32,
            p_viewports: *const D3D12Viewport,
        ),
        // Slot 22
        rs_set_scissor_rects: unsafe extern "system" fn(
            this: *mut c_void,
            num_rects: u32,
            p_rects: *const D3D12Rect,
        ),
        _slot23_to_25: [usize; 3],
        // Slot 26
        resource_barrier: unsafe extern "system" fn(
            this: *mut c_void,
            num_barriers: u32,
            p_barriers: *const D3D12ResourceBarrier,
        ),
        _slot27_to_45: [usize; 19],
        // Slot 46
        om_set_render_targets: unsafe extern "system" fn(
            this: *mut c_void,
            num_rtvs: u32,
            p_rtvs: *const D3D12CpuDescriptorHandle,
            single_handle: i32,
            p_dsv: *const D3D12CpuDescriptorHandle,
        ),
        // Slot 47
        _clear_dsv: usize,
        // Slot 48
        clear_render_target_view: unsafe extern "system" fn(
            this: *mut c_void,
            rtv: D3D12CpuDescriptorHandle,
            color_rgba: *const [f32; 4],
            num_rects: u32,
            p_rects: *const D3D12Rect,
        ),
    }

    // Exact Windows SDK IDXGIFactory4 vtable (CreateSwapChainForHwnd = slot 15)
    #[repr(C)]
    struct HostIDXGIFactory4Vtbl {
        base: IUnknownVtbl,
        _slot3_to_14: [usize; 12],
        create_swap_chain_for_hwnd: unsafe extern "system" fn(
            this: *mut c_void,
            p_device: *mut c_void,
            hwnd: *mut c_void,
            p_desc: *const DxgiSwapChainDesc1,
            p_fullscreen_desc: *const c_void,
            p_restrict_to_output: *mut c_void,
            pp_swap_chain: *mut *mut c_void,
        ) -> HResult,
    }

    // Exact Windows SDK IDXGISwapChain3 vtable (Present=8, GetBuffer=9, Waitable=34, CurrentBackBuffer=36)
    #[repr(C)]
    struct HostIDXGISwapChain3Vtbl {
        base: IUnknownVtbl,
        _slot3_to_7: [usize; 5],
        present: unsafe extern "system" fn(this: *mut c_void, sync_interval: u32, flags: u32) -> HResult,
        get_buffer: unsafe extern "system" fn(
            this: *mut c_void,
            buffer: u32,
            riid: *const Guid,
            pp_surface: *mut *mut c_void,
        ) -> HResult,
        _slot10_to_33: [usize; 24],
        get_frame_latency_waitable_object: unsafe extern "system" fn(this: *mut c_void) -> *mut c_void,
        _slot35: usize,
        get_current_back_buffer_index: unsafe extern "system" fn(this: *mut c_void) -> u32,
    }

    type D3DCompileFn = unsafe extern "system" fn(
        p_src_data: *const c_void,
        src_data_size: usize,
        p_source_name: *const c_char,
        p_defines: *const c_void,
        p_include: *const c_void,
        p_entrypoint: *const c_char,
        p_target: *const c_char,
        flags1: u32,
        flags2: u32,
        pp_code: *mut *mut c_void,
        pp_error_msgs: *mut *mut c_void,
    ) -> HResult;

    pub struct NativeD3D12Backend {
        width: u32,
        height: u32,
        windowed: bool,
        hwnd: *mut c_void,
        factory: *mut c_void,
        device: *mut c_void,
        queue: *mut c_void,
        allocator: *mut c_void,
        cmd_list: *mut c_void,
        rtv_heap: *mut c_void,
        rtv_base: D3D12CpuDescriptorHandle,
        rtv_inc: usize,
        back_buffers: [*mut c_void; 2],
        fence: *mut c_void,
        fence_event: *mut c_void,
        fence_value: u64,
        swap_chain: *mut c_void,
        frame_latency_waitable: *mut c_void,
        fsr_root_sig: *mut c_void,
        fsr_easu_pso: *mut c_void,
        fsr_rcas_pso: *mut c_void,
        fsr_easu_blob: *mut c_void,
        fsr_rcas_blob: *mut c_void,
        last_clear_color: [f32; 4],
        mirror: SoftwareD3D12Backend,
    }

    unsafe impl Send for NativeD3D12Backend {}
    unsafe impl Sync for NativeD3D12Backend {}

    impl NativeD3D12Backend {
        pub fn new(
            width: u32,
            height: u32,
            device_cfg: HostDeviceCreateConfig,
            windowed: bool,
        ) -> Result<Self, String> {
            let mut device: *mut c_void = ptr::null_mut();
            let hr = unsafe {
                D3D12CreateDevice(
                    ptr::null_mut(),
                    device_cfg.feature_level as u32,
                    &IID_ID3D12DEVICE,
                    &mut device,
                )
            };
            if failed(hr) || device.is_null() {
                return Err(format!("D3D12CreateDevice failed: 0x{:08X}", hr as u32));
            }

            let dev_vtbl = unsafe { *(device as *const *const HostID3D12DeviceVtbl) };

            let queue_desc = D3D12CommandQueueDesc {
                r#type: D3D12CommandListType::Direct as u32,
                priority: 0,
                flags: 0,
                node_mask: 0,
            };
            let mut queue: *mut c_void = ptr::null_mut();
            let hr_q = unsafe {
                ((*dev_vtbl).create_command_queue)(
                    device,
                    &queue_desc,
                    &IID_ID3D12COMMAND_QUEUE,
                    &mut queue,
                )
            };
            if failed(hr_q) || queue.is_null() {
                release_com(device);
                return Err(format!("CreateCommandQueue failed: 0x{:08X}", hr_q as u32));
            }

            let mut allocator: *mut c_void = ptr::null_mut();
            let _ = unsafe {
                ((*dev_vtbl).create_command_allocator)(
                    device,
                    D3D12CommandListType::Direct as u32,
                    &IID_ID3D12COMMAND_ALLOCATOR,
                    &mut allocator,
                )
            };

            let mut cmd_list: *mut c_void = ptr::null_mut();
            if !allocator.is_null() {
                let hr_cl = unsafe {
                    ((*dev_vtbl).create_command_list)(
                        device,
                        0,
                        D3D12CommandListType::Direct as u32,
                        allocator,
                        ptr::null_mut(),
                        &IID_ID3D12GRAPHICS_COMMAND_LIST,
                        &mut cmd_list,
                    )
                };
                // Command lists start open; close immediately so Reset() works cleanly on first frame
                if succeeded(hr_cl) && !cmd_list.is_null() {
                    let cl_vtbl =
                        unsafe { *(cmd_list as *const *const HostID3D12GraphicsCommandListVtbl) };
                    unsafe {
                        let _ = ((*cl_vtbl).close)(cmd_list);
                    }
                }
            }

            let heap_desc = D3D12DescriptorHeapDesc {
                r#type: D3D12DescriptorHeapType::Rtv as u32,
                num_descriptors: 2,
                flags: 0,
                node_mask: 0,
            };
            let mut rtv_heap: *mut c_void = ptr::null_mut();
            let mut rtv_base = D3D12CpuDescriptorHandle { ptr: 0 };
            let rtv_inc = unsafe {
                ((*dev_vtbl).get_descriptor_handle_increment_size)(
                    device,
                    D3D12DescriptorHeapType::Rtv as u32,
                ) as usize
            };
            let hr_h = unsafe {
                ((*dev_vtbl).create_descriptor_heap)(
                    device,
                    &heap_desc,
                    &IID_ID3D12DESCRIPTOR_HEAP,
                    &mut rtv_heap,
                )
            };
            if succeeded(hr_h) && !rtv_heap.is_null() {
                let h_vtbl = unsafe { *(rtv_heap as *const *const HostID3D12DescriptorHeapVtbl) };
                unsafe {
                    let _ = ((*h_vtbl).get_cpu_handle_for_heap_start)(rtv_heap, &mut rtv_base);
                }
            }

            let mut fence: *mut c_void = ptr::null_mut();
            let _ = unsafe {
                ((*dev_vtbl).create_fence)(device, 0, 0, &IID_ID3D12FENCE, &mut fence)
            };
            let fence_event = unsafe { CreateEventA(ptr::null_mut(), 0, 0, ptr::null()) };

            let ws_style: u32 = if windowed { 0x10CF_0000 } else { 0x00CF_0000 };
            let hwnd = unsafe {
                CreateWindowExA(
                    0,
                    b"STATIC\0".as_ptr() as *const c_char,
                    b"xsxemu - Direct3D 12 + AMD FSR\0".as_ptr() as *const c_char,
                    ws_style,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };

            let mut factory: *mut c_void = ptr::null_mut();
            let mut swap_chain: *mut c_void = ptr::null_mut();
            let mut frame_latency_waitable: *mut c_void = ptr::null_mut();
            let mut back_buffers = [ptr::null_mut(), ptr::null_mut()];

            if !hwnd.is_null() {
                let hr_f = unsafe { CreateDXGIFactory2(0, &IID_IDXGIFACTORY4, &mut factory) };
                if succeeded(hr_f) && !factory.is_null() {
                    let sc_desc = DxgiSwapChainDesc1 {
                        width: width.max(64),
                        height: height.max(64),
                        format: DxgiFormat::R8G8B8A8Unorm as u32,
                        stereo: 0,
                        sample_desc: DxgiSampleDesc { count: 1, quality: 0 },
                        buffer_usage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                        buffer_count: 2,
                        scaling: 0,
                        swap_effect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                        alpha_mode: 0,
                        flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
                    };
                    let f_vtbl = unsafe { *(factory as *const *const HostIDXGIFactory4Vtbl) };
                    let mut sc1: *mut c_void = ptr::null_mut();
                    let hr_sc = unsafe {
                        ((*f_vtbl).create_swap_chain_for_hwnd)(
                            factory,
                            queue,
                            hwnd,
                            &sc_desc,
                            ptr::null(),
                            ptr::null_mut(),
                            &mut sc1,
                        )
                    };
                    if succeeded(hr_sc) && !sc1.is_null() {
                        let unk = unsafe { *(sc1 as *const *const IUnknownVtbl) };
                        let hr_qi = unsafe {
                            ((*unk).query_interface)(sc1, &IID_IDXGISWAPCHAIN3, &mut swap_chain)
                        };
                        release_com(sc1);
                        if succeeded(hr_qi) && !swap_chain.is_null() {
                            let sc3 =
                                unsafe { *(swap_chain as *const *const HostIDXGISwapChain3Vtbl) };
                            frame_latency_waitable =
                                unsafe { ((*sc3).get_frame_latency_waitable_object)(swap_chain) };

                            // Extract swapchain backbuffers & create real host RTVs
                            for idx in 0..2u32 {
                                let mut buf: *mut c_void = ptr::null_mut();
                                let hr_b = unsafe {
                                    ((*sc3).get_buffer)(
                                        swap_chain,
                                        idx,
                                        &IID_ID3D12RESOURCE,
                                        &mut buf,
                                    )
                                };
                                if succeeded(hr_b) && !buf.is_null() {
                                    back_buffers[idx as usize] = buf;
                                    if rtv_base.ptr != 0 {
                                        let dest = D3D12CpuDescriptorHandle {
                                            ptr: rtv_base.ptr + (idx as usize) * rtv_inc,
                                        };
                                        unsafe {
                                            ((*dev_vtbl).create_render_target_view)(
                                                device,
                                                buf,
                                                ptr::null(),
                                                dest,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Compile FSR EASU & RCAS HLSL shaders and build real host RootSignature + Compute PSOs
            let fsr_easu_blob = compile_hlsl_cs(FSR_EASU_HLSL);
            let fsr_rcas_blob = compile_hlsl_cs(FSR_RCAS_HLSL);
            let fsr_root_sig = create_empty_root_sig(device, dev_vtbl);
            let fsr_easu_pso = create_compute_pso_from_blob(device, dev_vtbl, fsr_root_sig, fsr_easu_blob);
            let fsr_rcas_pso = create_compute_pso_from_blob(device, dev_vtbl, fsr_root_sig, fsr_rcas_blob);

            Ok(Self {
                width,
                height,
                windowed,
                hwnd,
                factory,
                device,
                queue,
                allocator,
                cmd_list,
                rtv_heap,
                rtv_base,
                rtv_inc,
                back_buffers,
                fence,
                fence_event,
                fence_value: 0,
                swap_chain,
                frame_latency_waitable,
                fsr_root_sig,
                fsr_easu_pso,
                fsr_rcas_pso,
                fsr_easu_blob,
                fsr_rcas_blob,
                last_clear_color: [0.06, 0.14, 0.10, 1.0],
                mirror: SoftwareD3D12Backend::new(width, height),
            })
        }

        /// Execute a real hardware Direct3D 12 frame on the Windows GPU & synchronize via Fence.
        fn replay_hardware_frame(&mut self, sync_interval: u32) {
            if self.swap_chain.is_null()
                || self.cmd_list.is_null()
                || self.allocator.is_null()
                || self.rtv_base.ptr == 0
            {
                return;
            }

            unsafe {
                let sc3 = *(self.swap_chain as *const *const HostIDXGISwapChain3Vtbl);
                let bb_idx = ((*sc3).get_current_back_buffer_index)(self.swap_chain) as usize;
                let back_buf = *self.back_buffers.get(bb_idx).unwrap_or(&ptr::null_mut());
                if back_buf.is_null() {
                    return;
                }

                let alloc_vtbl = *(self.allocator as *const *const HostID3D12CommandAllocatorVtbl);
                let cl_vtbl = *(self.cmd_list as *const *const HostID3D12GraphicsCommandListVtbl);
                let q_vtbl = *(self.queue as *const *const HostID3D12CommandQueueVtbl);

                if failed(((*alloc_vtbl).reset)(self.allocator)) {
                    return;
                }
                if failed(((*cl_vtbl).reset)(self.cmd_list, self.allocator, ptr::null_mut())) {
                    return;
                }

                // Transition backbuffer PRESENT -> RENDER_TARGET
                let to_rt = D3D12ResourceBarrier {
                    r#type: 0,
                    flags: 0,
                    transition: D3D12ResourceTransitionBarrier {
                        p_resource: back_buf,
                        subresource: 0xFFFF_FFFF,
                        state_before: D3D12_RESOURCE_STATE_PRESENT,
                        state_after: D3D12_RESOURCE_STATE_RENDER_TARGET,
                    },
                };
                ((*cl_vtbl).resource_barrier)(self.cmd_list, 1, &to_rt);

                let rtv = D3D12CpuDescriptorHandle {
                    ptr: self.rtv_base.ptr + bb_idx * self.rtv_inc,
                };
                ((*cl_vtbl).om_set_render_targets)(self.cmd_list, 1, &rtv, 0, ptr::null());
                let vp = D3D12Viewport {
                    top_left_x: 0.0,
                    top_left_y: 0.0,
                    width: self.width as f32,
                    height: self.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                };
                let scissor = D3D12Rect {
                    left: 0,
                    top: 0,
                    right: self.width as i32,
                    bottom: self.height as i32,
                };
                ((*cl_vtbl).rs_set_viewports)(self.cmd_list, 1, &vp);
                ((*cl_vtbl).rs_set_scissor_rects)(self.cmd_list, 1, &scissor);
                ((*cl_vtbl).clear_render_target_view)(
                    self.cmd_list,
                    rtv,
                    &self.last_clear_color,
                    0,
                    ptr::null(),
                );

                // Transition backbuffer RENDER_TARGET -> PRESENT
                let to_present = D3D12ResourceBarrier {
                    r#type: 0,
                    flags: 0,
                    transition: D3D12ResourceTransitionBarrier {
                        p_resource: back_buf,
                        subresource: 0xFFFF_FFFF,
                        state_before: D3D12_RESOURCE_STATE_RENDER_TARGET,
                        state_after: D3D12_RESOURCE_STATE_PRESENT,
                    },
                };
                ((*cl_vtbl).resource_barrier)(self.cmd_list, 1, &to_present);

                if succeeded(((*cl_vtbl).close)(self.cmd_list)) {
                    let lists = [self.cmd_list];
                    ((*q_vtbl).execute_command_lists)(self.queue, 1, lists.as_ptr());
                    let _ = ((*sc3).present)(self.swap_chain, sync_interval.min(1), 0);

                    // Synchronize GPU queue via ID3D12Fence
                    if !self.fence.is_null() && !self.fence_event.is_null() {
                        self.fence_value += 1;
                        let target = self.fence_value;
                        if succeeded(((*q_vtbl).signal)(self.queue, self.fence, target)) {
                            let f_vtbl = *(self.fence as *const *const HostID3D12FenceVtbl);
                            if ((*f_vtbl).get_completed_value)(self.fence) < target {
                                let _ = ((*f_vtbl).set_event_on_completion)(
                                    self.fence,
                                    target,
                                    self.fence_event,
                                );
                                let _ = WaitForSingleObject(self.fence_event, 1000);
                            }
                        }
                    }
                }

                if self.windowed && !self.hwnd.is_null() {
                    let mut msg = Win32Msg {
                        hwnd: ptr::null_mut(),
                        message: 0,
                        w_param: 0,
                        l_param: 0,
                        time: 0,
                        pt_x: 0,
                        pt_y: 0,
                    };
                    while PeekMessageA(&mut msg, self.hwnd, 0, 0, 1) != 0 {
                        let _ = TranslateMessage(&msg);
                        let _ = DispatchMessageA(&msg);
                    }
                }
            }
        }
    }

    fn create_empty_root_sig(
        device: *mut c_void,
        dev_vtbl: *const HostID3D12DeviceVtbl,
    ) -> *mut c_void {
        unsafe {
            let desc = HostRootSignatureDesc {
                num_parameters: 0,
                p_parameters: ptr::null(),
                num_static_samplers: 0,
                p_static_samplers: ptr::null(),
                flags: 0,
            };
            let mut blob: *mut c_void = ptr::null_mut();
            let mut err: *mut c_void = ptr::null_mut();
            let hr = D3D12SerializeRootSignature(&desc, 1, &mut blob, &mut err);
            release_com(err);
            if failed(hr) || blob.is_null() {
                return ptr::null_mut();
            }
            let b_vtbl = *(blob as *const *const HostID3DBlobVtbl);
            let ptr_bytes = ((*b_vtbl).get_buffer_pointer)(blob);
            let len_bytes = ((*b_vtbl).get_buffer_size)(blob);
            let mut rs: *mut c_void = ptr::null_mut();
            let _ = ((*dev_vtbl).create_root_signature)(
                device,
                0,
                ptr_bytes,
                len_bytes,
                &IID_ID3D12ROOT_SIGNATURE,
                &mut rs,
            );
            release_com(blob);
            rs
        }
    }

    fn create_compute_pso_from_blob(
        device: *mut c_void,
        dev_vtbl: *const HostID3D12DeviceVtbl,
        root_sig: *mut c_void,
        shader_blob: *mut c_void,
    ) -> *mut c_void {
        if root_sig.is_null() || shader_blob.is_null() {
            return ptr::null_mut();
        }
        unsafe {
            let b_vtbl = *(shader_blob as *const *const HostID3DBlobVtbl);
            let desc = HostComputePsoDesc {
                p_root_signature: root_sig,
                cs: D3D12ShaderBytecode {
                    p_shader_bytecode: ((*b_vtbl).get_buffer_pointer)(shader_blob),
                    byte_code_length: ((*b_vtbl).get_buffer_size)(shader_blob),
                },
                node_mask: 0,
                cached_pso_blob: ptr::null(),
                cached_pso_size: 0,
                flags: 0,
            };
            let mut pso: *mut c_void = ptr::null_mut();
            let _ = ((*dev_vtbl).create_compute_pso)(
                device,
                &desc,
                &IID_ID3D12PIPELINE_STATE,
                &mut pso,
            );
            pso
        }
    }

    fn compile_hlsl_cs(src: &str) -> *mut c_void {
        unsafe {
            let dll = LoadLibraryA(b"d3dcompiler_47.dll\0".as_ptr() as *const c_char);
            if dll.is_null() {
                return ptr::null_mut();
            }
            let proc = GetProcAddress(dll, b"D3DCompile\0".as_ptr() as *const c_char);
            if proc.is_null() {
                return ptr::null_mut();
            }
            let d3d_compile: D3DCompileFn = std::mem::transmute(proc);
            let mut blob: *mut c_void = ptr::null_mut();
            let mut err: *mut c_void = ptr::null_mut();
            let hr = d3d_compile(
                src.as_ptr() as *const c_void,
                src.len(),
                b"fsr_cs.hlsl\0".as_ptr() as *const c_char,
                ptr::null(),
                ptr::null(),
                b"CSMain\0".as_ptr() as *const c_char,
                b"cs_5_0\0".as_ptr() as *const c_char,
                0,
                0,
                &mut blob,
                &mut err,
            );
            release_com(err);
            if succeeded(hr) {
                blob
            } else {
                release_com(blob);
                ptr::null_mut()
            }
        }
    }

    fn release_com(ptr: *mut c_void) {
        if !ptr.is_null() {
            unsafe {
                let unk = *(ptr as *const *const IUnknownVtbl);
                ((*unk).release)(ptr);
            }
        }
    }

    impl Drop for NativeD3D12Backend {
        fn drop(&mut self) {
            release_com(self.fsr_rcas_pso);
            release_com(self.fsr_easu_pso);
            release_com(self.fsr_root_sig);
            release_com(self.fsr_rcas_blob);
            release_com(self.fsr_easu_blob);
            for bb in self.back_buffers {
                release_com(bb);
            }
            if !self.fence_event.is_null() {
                unsafe {
                    let _ = CloseHandle(self.fence_event);
                }
            }
            if !self.frame_latency_waitable.is_null() {
                unsafe {
                    let _ = CloseHandle(self.frame_latency_waitable);
                }
            }
            release_com(self.swap_chain);
            release_com(self.fence);
            release_com(self.rtv_heap);
            release_com(self.cmd_list);
            release_com(self.allocator);
            release_com(self.queue);
            release_com(self.device);
            release_com(self.factory);
            if !self.hwnd.is_null() {
                unsafe {
                    let _ = DestroyWindow(self.hwnd);
                }
            }
        }
    }

    impl GpuBackend for NativeD3D12Backend {
        fn name(&self) -> &'static str {
            "NativeWindowsD3D12"
        }

        fn create_resource(
            &mut self,
            id: u64,
            heap_props: &D3D12HeapProperties,
            heap_flags: u32,
            desc: &D3D12ResourceDesc,
            initial_state: u32,
        ) -> Result<u64, String> {
            self.mirror
                .create_resource(id, heap_props, heap_flags, desc, initial_state)
        }

        fn map_resource(&mut self, id: u64) -> Result<*mut u8, String> {
            self.mirror.map_resource(id)
        }

        fn unmap_resource(&mut self, id: u64) {
            self.mirror.unmap_resource(id)
        }

        fn create_rtv(&mut self, rtv_handle: usize, resource_id: u64) {
            self.mirror.create_rtv(rtv_handle, resource_id)
        }

        fn execute_commands(&mut self, commands: &[GpuCommand]) -> Result<(), String> {
            for cmd in commands {
                if let GpuCommand::ClearRenderTarget { color, .. } = cmd {
                    self.last_clear_color = *color;
                }
            }
            self.mirror.execute_commands(commands)
        }

        fn wait_frame_latency(&mut self, timeout_ms: u32) {
            if !self.frame_latency_waitable.is_null() {
                unsafe {
                    let _ = WaitForSingleObject(self.frame_latency_waitable, timeout_ms.min(16));
                }
            }
        }

        fn present_resource(
            &mut self,
            resource_id: Option<u64>,
            frame_index: u64,
            sync_interval: u32,
            fsr_plan: Option<&FsrComputePlan>,
        ) -> Result<Option<PresentedFrame>, String> {
            self.replay_hardware_frame(sync_interval);
            self.mirror
                .present_resource(resource_id, frame_index, sync_interval, fsr_plan)
        }
    }
}
