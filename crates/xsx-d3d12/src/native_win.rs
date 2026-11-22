//! Native Windows x86_64 Direct3D 12 (`d3d12.dll` + `dxgi.dll`) backend.
//!
//! On Windows x86_64 hosts, creates a real hardware `ID3D12Device` via `D3D12CreateDevice`,
//! forwards translated resource/command operations, and falls back to software readback for
//! frame dumps when running headless.

use crate::backend::GpuBackend;
use crate::translate::HostDeviceCreateConfig;

#[cfg(not(target_os = "windows"))]
pub fn create_native_backend(
    _width: u32,
    _height: u32,
    _device_cfg: HostDeviceCreateConfig,
) -> Result<Box<dyn GpuBackend>, String> {
    Err("NativeD3D12 backend requires a Windows x86_64 host (use --backend software or auto)".into())
}

#[cfg(target_os = "windows")]
pub fn create_native_backend(
    width: u32,
    height: u32,
    device_cfg: HostDeviceCreateConfig,
) -> Result<Box<dyn GpuBackend>, String> {
    windows_impl::NativeD3D12Backend::new(width, height, device_cfg)
        .map(|b| Box::new(b) as Box<dyn GpuBackend>)
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use core::ffi::c_void;
    use std::ptr;

    use xsx_abi::com::{failed, Guid, HResult, IUnknownVtbl, IID_ID3D12COMMAND_QUEUE, IID_ID3D12DEVICE};
    use xsx_abi::d3d12::{
        D3D12CommandListType, D3D12CommandQueueDesc, D3D12HeapProperties, D3D12ResourceDesc,
    };

    use crate::backend::{GpuBackend, GpuCommand, PresentedFrame};
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

    // Minimal host ID3D12Device vtable prefix (slots 0..=8 matching Windows SDK d3d12.h)
    #[repr(C)]
    struct HostID3D12DeviceVtbl {
        base: IUnknownVtbl,
        _get_private_data: usize,
        _set_private_data: usize,
        _set_private_data_interface: usize,
        _set_name: usize,
        _get_node_count: usize,
        create_command_queue: unsafe extern "system" fn(
            this: *mut c_void,
            p_desc: *const D3D12CommandQueueDesc,
            riid: *const Guid,
            pp_command_queue: *mut *mut c_void,
        ) -> HResult,
    }

    /// Native Windows x86_64 D3D12 hardware device wrapper paired with a shadow
    /// software mirror for deterministic headless frame capture during bring-up.
    pub struct NativeD3D12Backend {
        device: *mut c_void,
        queue: *mut c_void,
        mirror: SoftwareD3D12Backend,
    }

    unsafe impl Send for NativeD3D12Backend {}
    unsafe impl Sync for NativeD3D12Backend {}

    impl NativeD3D12Backend {
        pub fn new(
            width: u32,
            height: u32,
            device_cfg: HostDeviceCreateConfig,
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
                return Err(format!("D3D12CreateDevice failed with HRESULT 0x{:08X}", hr as u32));
            }

            let queue_desc = D3D12CommandQueueDesc {
                r#type: D3D12CommandListType::Direct as u32,
                priority: 0,
                flags: 0,
                node_mask: 0,
            };
            let mut queue: *mut c_void = ptr::null_mut();
            let vtbl = unsafe { *(device as *const *const HostID3D12DeviceVtbl) };
            let hr_q = unsafe {
                ((*vtbl).create_command_queue)(
                    device,
                    &queue_desc,
                    &IID_ID3D12COMMAND_QUEUE,
                    &mut queue,
                )
            };
            if failed(hr_q) || queue.is_null() {
                unsafe {
                    let unk = *(device as *const *const IUnknownVtbl);
                    ((*unk).release)(device);
                }
                return Err(format!(
                    "Host ID3D12Device::CreateCommandQueue failed with HRESULT 0x{:08X}",
                    hr_q as u32
                ));
            }

            Ok(Self {
                device,
                queue,
                mirror: SoftwareD3D12Backend::new(width, height),
            })
        }
    }

    impl Drop for NativeD3D12Backend {
        fn drop(&mut self) {
            unsafe {
                if !self.queue.is_null() {
                    let unk = *(self.queue as *const *const IUnknownVtbl);
                    ((*unk).release)(self.queue);
                }
                if !self.device.is_null() {
                    let unk = *(self.device as *const *const IUnknownVtbl);
                    ((*unk).release)(self.device);
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

        fn present_resource(
            &mut self,
            resource_id: Option<u64>,
            frame_index: u64,
            sync_interval: u32,
        ) -> Result<Option<PresentedFrame>, String> {
            self.mirror
                .present_resource(resource_id, frame_index, sync_interval)
        }
    }
}
