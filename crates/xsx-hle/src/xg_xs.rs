//! `xg_xs.dll` / `xg.dll` — Xbox Series X|S Graphics Texture Layout Computer HLE.
//!
//! Real Xbox Series X titles (Fortnite, Concept Destruction, Unicorn Overlord, etc.)
//! call `XGComputeOptimalSwizzleMode` and `XGCreateTexture2DComputer` to compute
//! RDNA 2 tiled memory footprints (`XG_RESOURCE_LAYOUT`, 0x1B40 bytes) and swizzle/detile
//! texture uploads via the 9-slot non-IUnknown `IXGTexture2DComputer` COM interface:
//! - Slot 0: `AddRef`
//! - Slot 1: `Release`
//! - Slot 2: `GetResourceLayout`
//! - Slot 3: `GetResourceSizeBytes`
//! - Slot 4: `GetResourceBaseAlignmentBytes`
//! - Slot 5: `GetMipLevelOffsetBytes`
//! - Slot 6: `GetTexelElementOffsetBytes`
//! - Slot 7: `CopyIntoSubresource`
//! - Slot 8: `CopyFromSubresource`
//!
//! Backed by `xsx_d3d12::tiling` (`compute_resource_tiling`, `swizzle_2d`, `detile_2d`).

use core::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use xsx_abi::com::{HResult, E_INVALIDARG, S_OK};
use xsx_d3d12::tiling::{
    compute_resource_tiling, detile_2d, element_tile_shape, swizzle_2d, SwizzleEquation,
    TiledKind, TilingDesc, PACKED_TILE, TILE_SIZE_IN_BYTES,
};

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

// Verified Xbox Series X xg_xs.dll swizzle mode constants.
pub const XG_SWIZZLE_MODE_LINEAR: u32 = 0;
pub const XG_SWIZZLE_MODE_4KB_S: u32 = 5;
pub const XG_SWIZZLE_MODE_64KB_S: u32 = 9;
pub const XG_SWIZZLE_MODE_64KB_Z_X: u32 = 24;
pub const XG_SWIZZLE_MODE_64KB_S_X: u32 = 25;
pub const XG_SWIZZLE_MODE_64KB_R_X: u32 = 27;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XgSampleDesc {
    pub count: u32,
    pub quality: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XgTexture2DDesc {
    pub width: u32,
    pub height: u32,
    pub mip_levels: u32,
    pub array_size: u32,
    pub format: u32,
    pub sample_desc: XgSampleDesc,
    pub usage: u32,
    pub bind_flags: u32,
    pub cpu_access_flags: u32,
    pub misc_flags: u32,
    pub swizzle_mode: u32, // +0x2C
    pub pitch: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XgMipLevelLayout {
    pub size_bytes: u64,              // +0x00
    pub offset_bytes: u64,            // +0x08
    pub slice_2d_size_bytes: u64,     // +0x10
    pub pitch_pixels: u32,            // +0x18
    pub pitch_bytes: u32,             // +0x1C
    pub alignment_bytes: u32,         // +0x20
    pub padded_width_elements: u32,   // +0x24
    pub padded_height_elements: u32,  // +0x28
    pub padded_depth_or_array: u32,   // +0x2C
    pub width_elements: u32,          // +0x30
    pub height_elements: u32,         // +0x34
    pub reserved_width: u32,          // +0x38 (read by titles as mip width)
    pub reserved_height: u32,         // +0x3C (read by titles as mip height)
    pub depth_or_array_size: u32,     // +0x40
    pub sample_count: u32,            // +0x44
    pub swizzle_mode: u32,            // +0x48
    pub reserved_4c: [u32; 5],        // +0x4C
    pub reserved_60: u32,             // +0x60 (1 in observed Xbox layouts)
    pub reserved_64: u32,             // +0x64
    pub reserved_68: u64,             // +0x68
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XgPlaneLayout {
    pub usage: u32,                     // +0x00
    pub _pad04: u32,                    // +0x04
    pub size_bytes: u64,                // +0x08
    pub slice_stride_bytes: u64,        // +0x10 (Reserved10)
    pub base_offset_bytes: u64,         // +0x18
    pub base_alignment_bytes: u64,      // +0x20
    pub bytes_per_element: u32,         // +0x28
    pub tail_mip_index: u32,            // +0x2C
    pub reserved_30: u32,               // +0x30
    pub _pad34: u32,                    // +0x34
    pub mip_layout: [XgMipLevelLayout; 15], // +0x38
}

impl Default for XgPlaneLayout {
    fn default() -> Self {
        Self {
            usage: 0,
            _pad04: 0,
            size_bytes: 0,
            slice_stride_bytes: 0,
            base_offset_bytes: 0,
            base_alignment_bytes: 0,
            bytes_per_element: 0,
            tail_mip_index: 0,
            reserved_30: 0,
            _pad34: 0,
            mip_layout: [XgMipLevelLayout::default(); 15],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XgResourceLayout {
    pub size_bytes: u64,
    pub base_alignment_bytes: u64,
    pub mip_levels: u32,
    pub planes: u32,
    pub plane: [XgPlaneLayout; 4], // +0x18
    pub dimension: u32,
    pub _pad_end: u32,
}

impl Default for XgResourceLayout {
    fn default() -> Self {
        Self {
            size_bytes: 0,
            base_alignment_bytes: 65536,
            mip_levels: 0,
            planes: 0,
            plane: [XgPlaneLayout::default(); 4],
            dimension: 3, // Texture2D
            _pad_end: 0,
        }
    }
}

pub fn register(reg: &mut ModuleRegistry) {
    reg.register_export(
        "xg_xs.dll",
        "XGComputeOptimalSwizzleMode",
        hle_xg_compute_optimal_swizzle_mode as *const () as usize,
    );
    reg.register_export(
        "xg_xs.dll",
        "XGCreateTexture2DComputer",
        hle_xg_create_texture2d_computer as *const () as usize,
    );
    reg.register_export(
        "xg_xs.dll",
        "XGCreateTextureComputer",
        hle_xg_create_texture2d_computer as *const () as usize,
    );
}

/// Map common DXGI_FORMAT values to `(bytes_per_element, block_dim)`.
fn dxgi_format_metrics(format: u32) -> (u32, u32) {
    match format {
        // BC1 / BC4 (8 bytes per 4x4 block)
        70..=72 | 79..=81 => (8, 4),
        // BC2 / BC3 / BC5 / BC6H / BC7 (16 bytes per 4x4 block)
        73..=78 | 82..=84 | 94..=99 => (16, 4),
        // R32G32B32A32 (16 Bpp)
        1..=4 => (16, 1),
        // R16G16B16A16 / R32G32 (8 Bpp)
        9..=18 => (8, 1),
        // R8G8 / R16 (2 Bpp)
        48..=59 => (2, 1),
        // R8 (1 Bpp)
        60..=66 => (1, 1),
        // Default R8G8B8A8 / B8G8R8A8 / R32 / D32 (4 Bpp)
        _ => (4, 1),
    }
}

fn swizzle_equation_for(mode: u32, bpe: usize) -> Option<SwizzleEquation> {
    match mode {
        XG_SWIZZLE_MODE_64KB_S => SwizzleEquation::standard_64kb_s(bpe),
        XG_SWIZZLE_MODE_64KB_S_X | XG_SWIZZLE_MODE_64KB_R_X | XG_SWIZZLE_MODE_64KB_Z_X => {
            SwizzleEquation::rdna2_64kb_rx(bpe)
        }
        _ => None,
    }
}

/// Compute Xbox Series X `XG_RESOURCE_LAYOUT` using our RDNA 2 tiling engine.
pub fn compute_xg_texture2d_layout(desc: &XgTexture2DDesc) -> XgResourceLayout {
    let width = desc.width.max(1);
    let height = desc.height.max(1);
    let array_size = desc.array_size.max(1);
    let (bpe, block_dim) = dxgi_format_metrics(desc.format);
    let max_mips = (32 - width.max(height).leading_zeros()).clamp(1, 15);
    let mip_levels = if desc.mip_levels == 0 {
        max_mips
    } else {
        desc.mip_levels.min(15)
    };
    let sample_count = desc.sample_desc.count.max(1);
    let swizzle = desc.swizzle_mode;

    let is_64k = matches!(
        swizzle,
        XG_SWIZZLE_MODE_64KB_S
            | XG_SWIZZLE_MODE_64KB_S_X
            | XG_SWIZZLE_MODE_64KB_R_X
            | XG_SWIZZLE_MODE_64KB_Z_X
    );
    let base_alignment: u64 = if is_64k {
        TILE_SIZE_IN_BYTES as u64
    } else if swizzle == XG_SWIZZLE_MODE_LINEAR {
        256
    } else {
        4096
    };

    let tiling = compute_resource_tiling(&TilingDesc {
        kind: TiledKind::Texture2D,
        width: width as u64,
        height,
        depth_or_layers: array_size,
        mips: mip_levels,
        element_bytes: bpe,
        block: block_dim,
        samples: if sample_count == 4 { 4 } else { 1 },
    });
    let tile_shape = element_tile_shape(false, bpe, if sample_count == 4 { 4 } else { 1 });

    let mut layout = XgResourceLayout {
        size_bytes: 0,
        base_alignment_bytes: base_alignment,
        mip_levels,
        planes: 1,
        plane: [XgPlaneLayout::default(); 4],
        dimension: 3, // D3D12_RESOURCE_DIMENSION_TEXTURE2D
        _pad_end: 0,
    };

    let mut running_offset = 0u64;
    let pl = &mut layout.plane[0];
    pl.usage = 1;
    pl.base_offset_bytes = 0;
    pl.base_alignment_bytes = base_alignment;
    pl.bytes_per_element = bpe;
    pl.tail_mip_index = tiling.as_ref().map(|t| t.standard_mips).unwrap_or(mip_levels);

    for mip in 0..mip_levels as usize {
        let mw = (width >> mip).max(1);
        let mh = (height >> mip).max(1);
        let w_elem = mw.div_ceil(block_dim).max(1);
        let h_elem = mh.div_ceil(block_dim).max(1);

        let (pw_elem, ph_elem, mip_offset, mip_size) = if is_64k {
            let tw = tile_shape.width.max(1);
            let th = tile_shape.height.max(1);
            let pw = w_elem.div_ceil(tw) * tw;
            let ph = h_elem.div_ceil(th) * th;
            let tiles_x = w_elem.div_ceil(tw) as u64;
            let tiles_y = h_elem.div_ceil(th) as u64;
            let size = (tiles_x * tiles_y * TILE_SIZE_IN_BYTES as u64).max(TILE_SIZE_IN_BYTES as u64);
            let off = if let Some(ref t) = tiling {
                t.subresources
                    .get(mip)
                    .filter(|s| s.start_tile_index != PACKED_TILE)
                    .map(|s| s.start_tile_index as u64 * TILE_SIZE_IN_BYTES as u64)
                    .unwrap_or(running_offset)
            } else {
                running_offset
            };
            (pw, ph, off, size)
        } else {
            let pitch_aligned = ((w_elem * bpe) + 255) & !255;
            let pw = pitch_aligned / bpe.max(1);
            let size = (pitch_aligned as u64) * (h_elem as u64);
            (pw, h_elem, running_offset, size)
        };

        let pitch_bytes = (pw_elem * bpe).max(256);
        pl.mip_layout[mip] = XgMipLevelLayout {
            size_bytes: mip_size,
            offset_bytes: mip_offset,
            slice_2d_size_bytes: mip_size,
            pitch_pixels: pw_elem * block_dim,
            pitch_bytes,
            alignment_bytes: base_alignment.min(u32::MAX as u64) as u32,
            padded_width_elements: pw_elem,
            padded_height_elements: ph_elem,
            padded_depth_or_array: array_size,
            width_elements: w_elem,
            height_elements: h_elem,
            reserved_width: mw,
            reserved_height: mh,
            depth_or_array_size: array_size,
            sample_count,
            swizzle_mode: swizzle,
            reserved_4c: [0; 5],
            reserved_60: 1,
            reserved_64: 0,
            reserved_68: 0,
        };
        running_offset = (mip_offset + mip_size + base_alignment - 1) & !(base_alignment - 1);
    }

    let slice_stride = running_offset.max(base_alignment);
    let total_size = slice_stride * (array_size as u64);
    pl.size_bytes = total_size;
    pl.slice_stride_bytes = slice_stride;
    layout.size_bytes = total_size;
    layout
}

// ---------------------------------------------------------------------------
// IXGTexture2DComputer 9-slot non-IUnknown COM object
// ---------------------------------------------------------------------------

#[repr(C)]
struct IXGTexture2DComputerVtbl {
    add_ref: unsafe extern "win64" fn(this: *mut c_void) -> u32,
    release: unsafe extern "win64" fn(this: *mut c_void) -> u32,
    get_resource_layout:
        unsafe extern "win64" fn(this: *mut c_void, out: *mut XgResourceLayout) -> HResult,
    get_resource_size_bytes: unsafe extern "win64" fn(this: *mut c_void) -> u64,
    get_resource_base_alignment_bytes: unsafe extern "win64" fn(this: *mut c_void) -> u64,
    get_mip_level_offset_bytes:
        unsafe extern "win64" fn(this: *mut c_void, plane: u32, mip: u32) -> u64,
    get_texel_element_offset_bytes: unsafe extern "win64" fn(
        this: *mut c_void,
        plane: u32,
        mip: u32,
        x: u64,
        y: u32,
        z_or_slice: u32,
        sample: u32,
    ) -> u64,
    copy_into_subresource: unsafe extern "win64" fn(
        this: *mut c_void,
        dst: *mut u8,
        plane: u32,
        subresource: u32,
        src: *const u8,
        src_row_pitch: u32,
        src_depth_pitch: u32,
    ) -> HResult,
    copy_from_subresource: unsafe extern "win64" fn(
        this: *mut c_void,
        dst: *mut u8,
        dst_row_pitch: u32,
        dst_depth_pitch: u32,
        src: *const u8,
        plane: u32,
        subresource: u32,
    ) -> HResult,
}

#[repr(C)]
struct XgTexture2DComputerObj {
    lp_vtbl: *const IXGTexture2DComputerVtbl,
    ref_count: AtomicU32,
    desc: XgTexture2DDesc,
    layout: XgResourceLayout,
}

static XG_TEX2D_VTBL: IXGTexture2DComputerVtbl = IXGTexture2DComputerVtbl {
    add_ref: xg_add_ref,
    release: xg_release,
    get_resource_layout: xg_get_resource_layout,
    get_resource_size_bytes: xg_get_resource_size_bytes,
    get_resource_base_alignment_bytes: xg_get_resource_base_alignment_bytes,
    get_mip_level_offset_bytes: xg_get_mip_level_offset_bytes,
    get_texel_element_offset_bytes: xg_get_texel_element_offset_bytes,
    copy_into_subresource: xg_copy_into_subresource,
    copy_from_subresource: xg_copy_from_subresource,
};

unsafe extern "win64" fn xg_add_ref(this: *mut c_void) -> u32 {
    if this.is_null() {
        return 0;
    }
    let obj = &*(this as *const XgTexture2DComputerObj);
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "win64" fn xg_release(this: *mut c_void) -> u32 {
    if this.is_null() {
        return 0;
    }
    let obj = &*(this as *const XgTexture2DComputerObj);
    let rem = obj.ref_count.fetch_sub(1, Ordering::AcqRel) - 1;
    if rem == 0 {
        drop(Box::from_raw(this as *mut XgTexture2DComputerObj));
    }
    rem
}

unsafe extern "win64" fn xg_get_resource_layout(
    this: *mut c_void,
    out: *mut XgResourceLayout,
) -> HResult {
    if this.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    let obj = &*(this as *const XgTexture2DComputerObj);
    *out = obj.layout;
    S_OK
}

unsafe extern "win64" fn xg_get_resource_size_bytes(this: *mut c_void) -> u64 {
    if this.is_null() {
        return 0;
    }
    (*(this as *const XgTexture2DComputerObj)).layout.size_bytes
}

unsafe extern "win64" fn xg_get_resource_base_alignment_bytes(this: *mut c_void) -> u64 {
    if this.is_null() {
        return 65536;
    }
    (*(this as *const XgTexture2DComputerObj))
        .layout
        .base_alignment_bytes
}

unsafe extern "win64" fn xg_get_mip_level_offset_bytes(
    this: *mut c_void,
    plane: u32,
    mip: u32,
) -> u64 {
    if this.is_null() {
        return 0;
    }
    let obj = &*(this as *const XgTexture2DComputerObj);
    if plane >= obj.layout.planes || mip >= obj.layout.mip_levels {
        return 0;
    }
    obj.layout.plane[plane as usize].mip_layout[mip as usize].offset_bytes
}

unsafe extern "win64" fn xg_get_texel_element_offset_bytes(
    this: *mut c_void,
    plane: u32,
    mip: u32,
    x: u64,
    y: u32,
    z_or_slice: u32,
    _sample: u32,
) -> u64 {
    if this.is_null() {
        return 0;
    }
    let obj = &*(this as *const XgTexture2DComputerObj);
    if plane >= obj.layout.planes || mip >= obj.layout.mip_levels {
        return 0;
    }
    let pl = &obj.layout.plane[plane as usize];
    let ml = &pl.mip_layout[mip as usize];
    ml.offset_bytes
        + (z_or_slice as u64) * pl.slice_stride_bytes
        + (y as u64) * (ml.pitch_bytes as u64)
        + x * (pl.bytes_per_element as u64)
}

unsafe extern "win64" fn xg_copy_into_subresource(
    this: *mut c_void,
    dst: *mut u8,
    plane: u32,
    subresource: u32,
    src: *const u8,
    src_row_pitch: u32,
    _src_depth_pitch: u32,
) -> HResult {
    if this.is_null() || dst.is_null() || src.is_null() {
        return E_INVALIDARG;
    }
    let obj = &*(this as *const XgTexture2DComputerObj);
    if plane >= obj.layout.planes {
        return E_INVALIDARG;
    }
    let mips = obj.layout.mip_levels.max(1);
    let mip = (subresource % mips) as usize;
    let slice = subresource / mips;
    let pl = &obj.layout.plane[plane as usize];
    let ml = &pl.mip_layout[mip];
    let bpe = pl.bytes_per_element.max(1);

    let dst_base = dst.add(
        (ml.offset_bytes + (slice as u64) * pl.slice_stride_bytes) as usize,
    );

    if let Some(eq) = swizzle_equation_for(ml.swizzle_mode, bpe as usize) {
        let w = ml.width_elements;
        let h = ml.height_elements;
        let row_bytes = (w * bpe) as usize;
        let mut linear = vec![0u8; row_bytes * (h as usize)];
        for row in 0..h as usize {
            let copy_len = row_bytes.min(src_row_pitch as usize);
            core::ptr::copy_nonoverlapping(
                src.add(row * src_row_pitch as usize),
                linear.as_mut_ptr().add(row * row_bytes),
                copy_len,
            );
        }
        if let Some(tiled) = swizzle_2d(&linear, &eq, bpe as usize, w, h) {
            let copy_total = tiled.len().min(ml.size_bytes as usize);
            core::ptr::copy_nonoverlapping(tiled.as_ptr(), dst_base, copy_total);
            return S_OK;
        }
    }

    let copy_bytes = (src_row_pitch.min(ml.pitch_bytes)) as usize;
    for row in 0..ml.height_elements as usize {
        core::ptr::copy_nonoverlapping(
            src.add(row * src_row_pitch as usize),
            dst_base.add(row * ml.pitch_bytes as usize),
            copy_bytes,
        );
    }
    S_OK
}

unsafe extern "win64" fn xg_copy_from_subresource(
    this: *mut c_void,
    dst: *mut u8,
    dst_row_pitch: u32,
    _dst_depth_pitch: u32,
    src: *const u8,
    plane: u32,
    subresource: u32,
) -> HResult {
    if this.is_null() || dst.is_null() || src.is_null() {
        return E_INVALIDARG;
    }
    let obj = &*(this as *const XgTexture2DComputerObj);
    if plane >= obj.layout.planes {
        return E_INVALIDARG;
    }
    let mips = obj.layout.mip_levels.max(1);
    let mip = (subresource % mips) as usize;
    let slice = subresource / mips;
    let pl = &obj.layout.plane[plane as usize];
    let ml = &pl.mip_layout[mip];
    let bpe = pl.bytes_per_element.max(1);

    let src_base = src.add(
        (ml.offset_bytes + (slice as u64) * pl.slice_stride_bytes) as usize,
    );

    if let Some(eq) = swizzle_equation_for(ml.swizzle_mode, bpe as usize) {
        let w = ml.width_elements;
        let h = ml.height_elements;
        let tiled = core::slice::from_raw_parts(src_base, ml.size_bytes as usize);
        if let Some(linear) = detile_2d(tiled, &eq, bpe as usize, w, h) {
            let row_bytes = (w * bpe) as usize;
            for row in 0..h as usize {
                let copy_len = row_bytes.min(dst_row_pitch as usize);
                core::ptr::copy_nonoverlapping(
                    linear.as_ptr().add(row * row_bytes),
                    dst.add(row * dst_row_pitch as usize),
                    copy_len,
                );
            }
            return S_OK;
        }
    }

    let copy_bytes = (dst_row_pitch.min(ml.pitch_bytes)) as usize;
    for row in 0..ml.height_elements as usize {
        core::ptr::copy_nonoverlapping(
            src_base.add(row * ml.pitch_bytes as usize),
            dst.add(row * dst_row_pitch as usize),
            copy_bytes,
        );
    }
    S_OK
}

// ---------------------------------------------------------------------------
// Exported Win64 ABI thunks
// ---------------------------------------------------------------------------

pub unsafe extern "win64" fn hle_xg_compute_optimal_swizzle_mode(
    _resource_dimension: u32,
    _format: u32,
    _width: u32,
    _height: u32,
    _depth_or_array_size: u32,
    _sample_count: u32,
    bind_flags: u32,
    _reserved: u32,
) -> u32 {
    let mode = if (bind_flags & 0x40) != 0 {
        XG_SWIZZLE_MODE_64KB_Z_X
    } else if (bind_flags & 0x20) != 0 {
        XG_SWIZZLE_MODE_64KB_R_X
    } else {
        XG_SWIZZLE_MODE_4KB_S
    };
    if let Some(rt) = current_runtime() {
        rt.trace(format!("xg_xs!XGComputeOptimalSwizzleMode(bind=0x{bind_flags:X}) -> {mode}"));
    }
    mode
}

pub unsafe extern "win64" fn hle_xg_create_texture2d_computer(
    desc: *const XgTexture2DDesc,
    computer: *mut *mut c_void,
) -> HResult {
    if desc.is_null() || computer.is_null() {
        return E_INVALIDARG;
    }
    let d = *desc;
    let layout = compute_xg_texture2d_layout(&d);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "xg_xs!XGCreateTexture2DComputer({}x{}, mips={}, fmt={}, swizzle={}) -> {} bytes",
            d.width, d.height, layout.mip_levels, d.format, d.swizzle_mode, layout.size_bytes
        ));
    }
    let obj = Box::new(XgTexture2DComputerObj {
        lp_vtbl: &XG_TEX2D_VTBL,
        ref_count: AtomicU32::new(1),
        desc: d,
        layout,
    });
    *computer = Box::into_raw(obj) as *mut c_void;
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xg_struct_abi_sizes_match_xbox_series_x() {
        assert_eq!(core::mem::size_of::<XgMipLevelLayout>(), 0x70);
        assert_eq!(core::mem::offset_of!(XgMipLevelLayout, reserved_width), 0x38);
        assert_eq!(core::mem::offset_of!(XgMipLevelLayout, reserved_64), 0x64);
        assert_eq!(core::mem::offset_of!(XgPlaneLayout, mip_layout), 0x38);
        assert_eq!(core::mem::offset_of!(XgResourceLayout, plane), 0x18);
        assert_eq!(core::mem::offset_of!(XgTexture2DDesc, swizzle_mode), 0x2C);
    }

    #[test]
    fn xg_computer_roundtrips_64kb_swizzled_texture() {
        let swizzle = unsafe {
            hle_xg_compute_optimal_swizzle_mode(3, 28, 64, 64, 1, 1, 0x20, 0)
        };
        assert_eq!(swizzle, XG_SWIZZLE_MODE_64KB_R_X);

        let desc = XgTexture2DDesc {
            width: 64,
            height: 64,
            mip_levels: 1,
            array_size: 1,
            format: 28, // R8G8B8A8_UNORM
            sample_desc: XgSampleDesc { count: 1, quality: 0 },
            swizzle_mode: swizzle,
            ..Default::default()
        };
        let mut comp: *mut c_void = core::ptr::null_mut();
        assert_eq!(unsafe { hle_xg_create_texture2d_computer(&desc, &mut comp) }, S_OK);
        assert!(!comp.is_null());

        unsafe {
            let vtbl = &**(comp as *const *const IXGTexture2DComputerVtbl);
            let mut layout = XgResourceLayout::default();
            assert_eq!((vtbl.get_resource_layout)(comp, &mut layout), S_OK);
            assert!(layout.size_bytes >= 65536);
            assert_eq!((vtbl.get_resource_size_bytes)(comp), layout.size_bytes);

            let src: Vec<u8> = (0..(64 * 64 * 4)).map(|i| ((i * 7) & 0xFF) as u8).collect();
            let mut tiled = vec![0u8; layout.size_bytes as usize];
            let mut restored = vec![0u8; src.len()];
            assert_eq!(
                (vtbl.copy_into_subresource)(comp, tiled.as_mut_ptr(), 0, 0, src.as_ptr(), 64 * 4, 0),
                S_OK
            );
            assert_eq!(
                (vtbl.copy_from_subresource)(comp, restored.as_mut_ptr(), 64 * 4, 0, tiled.as_ptr(), 0, 0),
                S_OK
            );
            assert_eq!(restored, src);
            assert_eq!((vtbl.release)(comp), 0);
        }
    }
}
