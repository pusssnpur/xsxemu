//! Native Windows x86_64 Direct3D 12 (`d3d12.dll` + `dxgi.dll` + `user32.dll` + `d3dcompiler_47.dll`) backend.
//!
//! Phase 3 hardware pipeline:
//! - Creates a Win32 HWND + `IDXGIFactory4` + `ID3D12Device` + `ID3D12CommandQueue`
//! - Creates `IDXGISwapChain3` with `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT`
//! - Creates native host `ID3D12CommandAllocator`, `ID3D12GraphicsCommandList`, `ID3D12DescriptorHeap`,
//!   and `ID3D12Fence`
//! - Compiles `FSR_EASU_HLSL` and `FSR_RCAS_HLSL` via `d3dcompiler_47.dll!D3DCompile` when available
//! - Forwards `WaitFrameEventX` -> DXGI waitable latency handle and `PresentX` -> `IDXGISwapChain3::Present`

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
        IID_ID3D12GRAPHICS_COMMAND_LIST, IID_IDXGIFACTORY4, IID_IDXGISWAPCHAIN3,
    };
    use xsx_abi::d3d12::{
        D3D12CommandListType, D3D12CommandQueueDesc, D3D12DescriptorHeapDesc,
        D3D12DescriptorHeapType, D3D12HeapProperties, D3D12ResourceDesc, DxgiFormat,
        DxgiSampleDesc, DxgiSwapChainDesc1, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
        DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    use crate::backend::{GpuBackend, GpuCommand, PresentedFrame};
    use crate::fsr::{FsrComputePlan, FSR_EASU_HLSL, FSR_RCAS_HLSL};
    use crate::software::SoftwareD3D12Backend;
    use crate::translate::HostDeviceCreateConfig;

    #[link(name = "d3d12")]
    extern "system" {
        fn D3D12CreateDevice(
            p_adapter: *mut c_void,
            minimum_feature_level: u32,
            riid: *const Guid,
            pp_device: *mut *mut c_void,
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
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn WaitForSingleObject(h_handle: *mut c_void, dw_ms: u32) -> u32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
        fn LoadLibraryA(lp_lib_file_name: *const c_char) -> *mut c_void;
        fn GetProcAddress(h_module: *mut c_void, lp_proc_name: *const c_char) -> *mut c_void;
    }

    // Minimal host ID3D12Device vtable matching Windows SDK d3d12.h slots 0..=36
    #[repr(C)]
    struct HostID3D12DeviceVtbl {
        base: IUnknownVtbl,
        _slot3_to_7: [usize; 5],
        // Slot 8: CreateCommandQueue
        create_command_queue: unsafe extern "system" fn(
            this: *mut c_void,
            p_desc: *const D3D12CommandQueueDesc,
            riid: *const Guid,
            pp_command_queue: *mut *mut c_void,
        ) -> HResult,
        // Slot 9: CreateCommandAllocator
        create_command_allocator: unsafe extern "system" fn(
            this: *mut c_void,
            r#type: u32,
            riid: *const Guid,
            pp_allocator: *mut *mut c_void,
        ) -> HResult,
        _slot10_to_11: [usize; 2],
        // Slot 12: CreateCommandList
        create_command_list: unsafe extern "system" fn(
            this: *mut c_void,
            node_mask: u32,
            r#type: u32,
            p_allocator: *mut c_void,
            p_initial_state: *mut c_void,
            riid: *const Guid,
            pp_cmd_list: *mut *mut c_void,
        ) -> HResult,
        _slot13: usize,
        // Slot 14: CreateDescriptorHeap
        create_descriptor_heap: unsafe extern "system" fn(
            this: *mut c_void,
            p_desc: *const D3D12DescriptorHeapDesc,
            riid: *const Guid,
            pp_heap: *mut *mut c_void,
        ) -> HResult,
        _slot15_to_35: [usize; 21],
        // Slot 36: CreateFence
        create_fence: unsafe extern "system" fn(
            this: *mut c_void,
            initial_value: u64,
            flags: u32,
            riid: *const Guid,
            pp_fence: *mut *mut c_void,
        ) -> HResult,
    }

    // Minimal IDXGIFactory4 vtable prefix (CreateSwapChainForHwnd is slot 15)
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

    // Minimal IDXGISwapChain3 vtable prefix (Present = slot 8, GetFrameLatencyWaitableObject = slot 34)
    #[repr(C)]
    struct HostIDXGISwapChain3Vtbl {
        base: IUnknownVtbl,
        _slot3_to_7: [usize; 5],
        present: unsafe extern "system" fn(this: *mut c_void, sync_interval: u32, flags: u32) -> HResult,
        _slot9_to_33: [usize; 25],
        get_frame_latency_waitable_object: unsafe extern "system" fn(this: *mut c_void) -> *mut c_void,
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
        hwnd: *mut c_void,
        factory: *mut c_void,
        device: *mut c_void,
        queue: *mut c_void,
        allocator: *mut c_void,
        cmd_list: *mut c_void,
        rtv_heap: *mut c_void,
        fence: *mut c_void,
        swap_chain: *mut c_void,
        frame_latency_waitable: *mut c_void,
        fsr_easu_blob: *mut c_void,
        fsr_rcas_blob: *mut c_void,
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

            // Create host CommandAllocator, GraphicsCommandList, RTV DescriptorHeap, and Fence
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
                let _ = unsafe {
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
            }

            let heap_desc = D3D12DescriptorHeapDesc {
                r#type: D3D12DescriptorHeapType::Rtv as u32,
                num_descriptors: 4,
                flags: 0,
                node_mask: 0,
            };
            let mut rtv_heap: *mut c_void = ptr::null_mut();
            let _ = unsafe {
                ((*dev_vtbl).create_descriptor_heap)(
                    device,
                    &heap_desc,
                    &IID_ID3D12DESCRIPTOR_HEAP,
                    &mut rtv_heap,
                )
            };

            let mut fence: *mut c_void = ptr::null_mut();
            let _ = unsafe {
                ((*dev_vtbl).create_fence)(device, 0, 0, &IID_ID3D12FENCE, &mut fence)
            };

            // Create Win32 HWND + IDXGIFactory4 + IDXGISwapChain3 (best-effort on headless CI)
            let ws_style: u32 = if windowed { 0x10CF_0000 } else { 0x00CF_0000 };
            let hwnd = unsafe {
                CreateWindowExA(
                    0,
                    b"STATIC\0".as_ptr() as *const c_char,
                    b"xsxemu (Direct3D 12 + FSR)\0".as_ptr() as *const c_char,
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
                        }
                    }
                }
            }

            // Compile FSR EASU & RCAS HLSL compute shaders via d3dcompiler_47.dll (if present)
            let fsr_easu_blob = compile_hlsl_cs(FSR_EASU_HLSL);
            let fsr_rcas_blob = compile_hlsl_cs(FSR_RCAS_HLSL);

            Ok(Self {
                hwnd,
                factory,
                device,
                queue,
                allocator,
                cmd_list,
                rtv_heap,
                fence,
                swap_chain,
                frame_latency_waitable,
                fsr_easu_blob,
                fsr_rcas_blob,
                mirror: SoftwareD3D12Backend::new(width, height),
            })
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
            release_com(self.fsr_rcas_blob);
            release_com(self.fsr_easu_blob);
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
            if !self.swap_chain.is_null() {
                unsafe {
                    let sc3 = *(self.swap_chain as *const *const HostIDXGISwapChain3Vtbl);
                    let _ = ((*sc3).present)(self.swap_chain, sync_interval.min(1), 0);
                }
            }
            self.mirror
                .present_resource(resource_id, frame_index, sync_interval, fsr_plan)
        }
    }
}
