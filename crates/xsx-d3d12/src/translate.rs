//! Xbox Series X `d3d12_x` -> PC Direct3D 12 parameter & semantic translation rules.

use xsx_abi::d3d12::{
    D3D12HeapProperties, D3D12HeapType, D3D12XboxCreateDeviceParameters,
    D3D12XboxPresentPlaneParameters, D3DFeatureLevel, D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG,
    D3D12XBOX_HEAP_FLAG_CPU_CACHED, D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE,
};

/// Translated host D3D12 device creation parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostDeviceCreateConfig {
    pub feature_level: D3DFeatureLevel,
    pub enable_debug_layer: bool,
    pub ring_buffer_hint_bytes: u32,
}

/// Translated host DXGI swapchain presentation request derived from `PresentX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPresentRequest {
    pub frame_token: u64,
    pub sync_interval: u32,
    pub present_flags: u32,
    pub primary_resource_ptr: usize,
    pub color_space: u32,
}

/// Translate `D3D12XBOX_CREATE_DEVICE_PARAMETERS` into standard PC D3D12 device settings.
pub fn translate_create_device_params(
    params: Option<&D3D12XboxCreateDeviceParameters>,
) -> HostDeviceCreateConfig {
    match params {
        Some(p) => HostDeviceCreateConfig {
            feature_level: D3DFeatureLevel::Level12_0,
            enable_debug_layer: (p.process_debug_flags & D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG) != 0,
            ring_buffer_hint_bytes: p.graphics_command_queue_ring_size_bytes,
        },
        None => HostDeviceCreateConfig {
            feature_level: D3DFeatureLevel::Level12_0,
            enable_debug_layer: false,
            ring_buffer_hint_bytes: 0,
        },
    }
}

/// Sanitize Xbox Series X unified-memory heap properties and Xbox-only heap flags
/// into valid PC Direct3D 12 `D3D12_HEAP_PROPERTIES` and `D3D12_HEAP_FLAGS`.
pub fn translate_heap_properties(
    props: &D3D12HeapProperties,
    xbox_heap_flags: u32,
) -> (D3D12HeapProperties, u32) {
    let mut host_props = *props;

    // Strip Xbox-only unified-memory heap flags that PC D3D12 rejects with E_INVALIDARG.
    let xbox_only_mask =
        D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE | D3D12XBOX_HEAP_FLAG_CPU_CACHED;
    let host_flags = xbox_heap_flags & !xbox_only_mask;

    // On Xbox UMA, titles frequently request CPU_CACHED on DEFAULT heaps. On discrete PC GPUs,
    // keep DEFAULT heaps non-CPU-accessible unless explicitly UPLOAD/READBACK.
    if host_props.r#type == D3D12HeapType::Default as u32
        || host_props.r#type == D3D12HeapType::Upload as u32
        || host_props.r#type == D3D12HeapType::Readback as u32
    {
        host_props.cpu_page_property = 0; // D3D12_CPU_PAGE_PROPERTY_UNKNOWN
        host_props.memory_pool_preference = 0; // D3D12_MEMORY_POOL_UNKNOWN
    }

    // Normalize single-GPU node masks for PC.
    if host_props.creation_node_mask == 0 {
        host_props.creation_node_mask = 1;
    }
    if host_props.visible_node_mask == 0 {
        host_props.visible_node_mask = 1;
    }

    (host_props, host_flags)
}

/// Translate Xbox Series X `PresentX` plane parameters to a host swapchain present request.
///
/// # Safety
/// `plane.pp_resources` must either be null or point to at least `plane.resource_count` pointers.
pub unsafe fn translate_present_x(plane: &D3D12XboxPresentPlaneParameters) -> HostPresentRequest {
    let primary_resource_ptr = if plane.resource_count > 0 && !plane.pp_resources.is_null() {
        (*plane.pp_resources) as usize
    } else {
        0
    };

    // Map Xbox immediate/vsync options to DXGI Present sync interval.
    let immediate = (plane.options & 0x1) != 0;
    HostPresentRequest {
        frame_token: plane.token,
        sync_interval: if immediate { 0 } else { 1 },
        present_flags: 0,
        primary_resource_ptr,
        color_space: plane.color_space,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_xbox_only_heap_flags() {
        let props = D3D12HeapProperties {
            r#type: D3D12HeapType::Default as u32,
            cpu_page_property: 2,
            memory_pool_preference: 1,
            creation_node_mask: 0,
            visible_node_mask: 0,
        };
        let xbox_flags =
            D3D12XBOX_HEAP_FLAG_GRAPHICS_WRITABLE | D3D12XBOX_HEAP_FLAG_CPU_CACHED | 0x4;
        let (host_props, host_flags) = translate_heap_properties(&props, xbox_flags);
        assert_eq!(host_flags, 0x4);
        assert_eq!(host_props.cpu_page_property, 0);
        assert_eq!(host_props.memory_pool_preference, 0);
        assert_eq!(host_props.creation_node_mask, 1);
        assert_eq!(host_props.visible_node_mask, 1);
    }

    #[test]
    fn translates_create_device_debug_flag() {
        let params = D3D12XboxCreateDeviceParameters {
            version: 1,
            process_debug_flags: D3D12XBOX_CREATE_DEVICE_FLAG_DEBUG,
            graphics_command_queue_ring_size_bytes: 64 * 1024,
            ..Default::default()
        };
        let cfg = translate_create_device_params(Some(&params));
        assert!(cfg.enable_debug_layer);
        assert_eq!(cfg.feature_level, D3DFeatureLevel::Level12_0);
        assert_eq!(cfg.ring_buffer_hint_bytes, 64 * 1024);
    }
}
