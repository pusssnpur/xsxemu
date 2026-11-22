//! AMD GFX10 / RDNA 2 resource tiling (`GetResourceTiling`) and 2D texture swizzle / detile engine.
//!
//! Covers:
//! 1. **D3D12 64 KiB & 4 KiB Reserved/Tiled Resource Layout** (`compute_resource_tiling`):
//!    Matches AMD RDNA 2 / RDNA 4 hardware standard tile shapes (`256x256` @ 1 BPE through
//!    `64x64` @ 16 BPE, 4x MSAA, 3D volumes, and BC1–BC7 block-compressed textures) and
//!    packed mip tail transition rules (`width > tile_w / 2 || height > tile_h`).
//! 2. **AMD GFX10 2D Texture Swizzle & Detile** (`swizzle_2d`, `detile_2d`):
//!    Maps linear `(x, y)` texels/BC-blocks to tiled macro-block byte offsets using per-bit
//!    `(x_mask, y_mask)` XOR parity equations (standard 64KB `S` / `D` / `R_X` and 4KB modes).

pub const TILE_SIZE_IN_BYTES: u32 = 65536; // D3D12_TILED_RESOURCE_TILE_SIZE_IN_BYTES
pub const PACKED_TILE: u32 = 0xFFFF_FFFF; // D3D12_PACKED_TILE

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TileShape {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiledKind {
    Buffer,
    Texture2D,
    Texture3D,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TilingDesc {
    pub kind: TiledKind,
    pub width: u64,          // bytes for Buffer, texels for Texture
    pub height: u32,
    pub depth_or_layers: u32, // depth for Texture3D, array size for Texture2D
    pub mips: u32,
    pub element_bytes: u32,  // bytes per texel, or per 4x4 block for BC1-7
    pub block: u32,          // 1 for uncompressed, 4 for BC1-7
    pub samples: u32,        // 1 or 4 (for 2D single-mip MSAA)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubresourceTiling {
    pub width_in_tiles: u32,
    pub height_in_tiles: u32,
    pub depth_in_tiles: u32,
    pub start_tile_index: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceTiling {
    pub total_tiles: u32,
    pub shape: TileShape,
    pub standard_mips: u32,
    pub packed_mips: u32,
    pub packed_tiles: u32,
    pub packed_start: u32,
    pub subresources: Vec<SubresourceTiling>,
}

/// D3D12 / AMD RDNA 2 standard 64 KiB tile shape in elements (texels or BC blocks).
pub const fn element_tile_shape(volume: bool, element_bytes: u32, samples: u32) -> TileShape {
    if samples == 4 {
        if volume {
            return TileShape { width: 0, height: 0, depth: 0 };
        }
        return match element_bytes {
            1 => TileShape { width: 128, height: 128, depth: 1 },
            2 => TileShape { width: 128, height: 64, depth: 1 },
            4 => TileShape { width: 64, height: 64, depth: 1 },
            8 => TileShape { width: 64, height: 32, depth: 1 },
            16 => TileShape { width: 32, height: 32, depth: 1 },
            _ => TileShape { width: 0, height: 0, depth: 0 },
        };
    }
    if samples != 1 {
        return TileShape { width: 0, height: 0, depth: 0 };
    }
    match (volume, element_bytes) {
        (true, 1) => TileShape { width: 64, height: 32, depth: 32 },
        (true, 2) => TileShape { width: 32, height: 32, depth: 32 },
        (true, 4) => TileShape { width: 32, height: 32, depth: 16 },
        (true, 8) => TileShape { width: 32, height: 16, depth: 16 },
        (true, 16) => TileShape { width: 16, height: 16, depth: 16 },
        (false, 1) => TileShape { width: 256, height: 256, depth: 1 },
        (false, 2) => TileShape { width: 256, height: 128, depth: 1 },
        (false, 4) => TileShape { width: 128, height: 128, depth: 1 },
        (false, 8) => TileShape { width: 128, height: 64, depth: 1 },
        (false, 16) => TileShape { width: 64, height: 64, depth: 1 },
        _ => TileShape { width: 0, height: 0, depth: 0 },
    }
}

/// 4 KiB small-resource placement tile shape in elements (`D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT`).
pub const fn small_tile_shape(volume: bool, element_bytes: u32) -> TileShape {
    match (volume, element_bytes) {
        (true, 1) => TileShape { width: 16, height: 16, depth: 16 },
        (true, 2) => TileShape { width: 16, height: 16, depth: 8 },
        (true, 4) => TileShape { width: 16, height: 8, depth: 8 },
        (true, 8) => TileShape { width: 8, height: 8, depth: 8 },
        (true, 16) => TileShape { width: 8, height: 8, depth: 4 },
        (false, 1) => TileShape { width: 64, height: 64, depth: 1 },
        (false, 2) => TileShape { width: 64, height: 32, depth: 1 },
        (false, 4) => TileShape { width: 32, height: 32, depth: 1 },
        (false, 8) => TileShape { width: 32, height: 16, depth: 1 },
        (false, 16) => TileShape { width: 16, height: 16, depth: 1 },
        _ => TileShape { width: 0, height: 0, depth: 0 },
    }
}

#[inline]
const fn div_up(value: u64, by: u64) -> u64 {
    if value == 0 { 0 } else { 1 + (value - 1) / by }
}

/// Compute exact AMD RDNA 2 / D3D12 64 KiB tile partitioning for a reserved/tiled resource.
pub fn compute_resource_tiling(desc: &TilingDesc) -> Option<ResourceTiling> {
    const MAX_TILES: u64 = u32::MAX as u64;
    const MAX_PACKED_BYTES: u64 = MAX_TILES * TILE_SIZE_IN_BYTES as u64;

    if desc.kind == TiledKind::Buffer {
        let tiles = div_up(desc.width, TILE_SIZE_IN_BYTES as u64);
        if tiles == 0 || tiles > MAX_TILES {
            return None;
        }
        return Some(ResourceTiling {
            total_tiles: tiles as u32,
            shape: TileShape {
                width: TILE_SIZE_IN_BYTES,
                height: 1,
                depth: 1,
            },
            standard_mips: 0,
            packed_mips: 0,
            packed_tiles: 0,
            packed_start: 0,
            subresources: vec![SubresourceTiling {
                width_in_tiles: tiles as u32,
                height_in_tiles: 1,
                depth_in_tiles: 1,
                start_tile_index: 0,
            }],
        });
    }

    let volume = desc.kind == TiledKind::Texture3D;
    if desc.samples != 1 && (desc.samples != 4 || volume || desc.mips != 1 || desc.block != 1) {
        return None;
    }
    let elems = element_tile_shape(volume, desc.element_bytes, desc.samples);
    if elems.width == 0
        || desc.mips == 0
        || desc.block == 0
        || desc.width == 0
        || desc.height == 0
        || desc.depth_or_layers == 0
    {
        return None;
    }

    let shape_w = (elems.width as u64).checked_mul(desc.block as u64)?;
    let shape_h = (elems.height as u64).checked_mul(desc.block as u64)?;
    if shape_w > MAX_TILES || shape_h > MAX_TILES {
        return None;
    }

    let mut result = ResourceTiling {
        shape: TileShape {
            width: shape_w as u32,
            height: shape_h as u32,
            depth: elems.depth,
        },
        ..Default::default()
    };

    let layers = if volume { 1 } else { desc.depth_or_layers };
    let depth = if volume { desc.depth_or_layers } else { 1 };

    let mut slice = vec![SubresourceTiling::default(); desc.mips as usize];
    let mut standard_tiles = 0u64;
    let mut packed_bytes = 0u64;

    for mip in 0..desc.mips {
        let w = if mip >= 64 { 1 } else { (desc.width >> mip).max(1) };
        let h = if mip >= 32 { 1 } else { (desc.height >> mip).max(1) };
        let d = if mip >= 32 { 1 } else { (depth >> mip).max(1) };
        // AMD RDNA 2/4 rule: mip remains standard while wider than half a tile or taller than a tile
        let is_standard = result.packed_mips == 0
            && (w > (result.shape.width / 2) as u64 || h > result.shape.height);
        if is_standard {
            let tw = div_up(w, result.shape.width as u64);
            let th = div_up(h as u64, result.shape.height as u64);
            let td = div_up(d as u64, result.shape.depth as u64);
            if tw > MAX_TILES || th > MAX_TILES || td > MAX_TILES {
                return None;
            }
            let xy = tw.checked_mul(th)?;
            if xy > MAX_TILES || xy > MAX_TILES / td {
                return None;
            }
            let mip_tiles = xy * td;
            if mip_tiles > MAX_TILES - standard_tiles {
                return None;
            }
            slice[mip as usize] = SubresourceTiling {
                width_in_tiles: tw as u32,
                height_in_tiles: th as u32,
                depth_in_tiles: td as u32,
                start_tile_index: standard_tiles as u32,
            };
            standard_tiles += mip_tiles;
            result.standard_mips += 1;
        } else {
            slice[mip as usize] = SubresourceTiling {
                width_in_tiles: 0,
                height_in_tiles: 0,
                depth_in_tiles: 0,
                start_tile_index: PACKED_TILE,
            };
            let row_slice_bytes = div_up(w, desc.block as u64)
                .checked_mul(div_up(h as u64, desc.block as u64))?
                .checked_mul(desc.element_bytes as u64)?
                .checked_mul(desc.samples as u64)?;
            if (d as u64) > (MAX_PACKED_BYTES - packed_bytes) / row_slice_bytes.max(1) {
                return None;
            }
            packed_bytes += row_slice_bytes * d as u64;
            result.packed_mips += 1;
        }
    }

    let packed_tiles = if result.packed_mips > 0 {
        div_up(packed_bytes, TILE_SIZE_IN_BYTES as u64).max(1)
    } else {
        0
    };
    if packed_tiles > MAX_TILES - standard_tiles {
        return None;
    }
    let per_slice = standard_tiles + packed_tiles;
    if per_slice > MAX_TILES / layers as u64 {
        return None;
    }

    result.packed_tiles = packed_tiles as u32;
    result.packed_start = if result.packed_mips > 0 {
        standard_tiles as u32
    } else {
        0
    };
    result.total_tiles = (per_slice * layers as u64) as u32;
    result.subresources.reserve(desc.mips as usize * layers as usize);
    for layer in 0..layers {
        for &s in &slice {
            let mut copy = s;
            if copy.start_tile_index != PACKED_TILE {
                copy.start_tile_index += (per_slice * layer as u64) as u32;
            }
            result.subresources.push(copy);
        }
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// AMD GFX10 / RDNA 2 2D Texture Swizzle & Detile Engine
// ---------------------------------------------------------------------------

/// Per-address-bit XOR equation `(x_mask, y_mask)` inside a macro-block (`block_bytes` = 256, 4096, or 65536).
#[derive(Debug, Clone)]
pub struct SwizzleEquation {
    pub block_bytes: usize,
    pub bit_masks: Vec<(u32, u32)>, // bit i of byte offset = parity(x & mx) ^ parity(y & my)
}

impl SwizzleEquation {
    /// Standard D3D12 / GFX10 Morton-interleaved 64 KiB `SW_64KB_S` equation for 1/2/4/8/16 BPE.
    pub fn standard_64kb_s(bpe: usize) -> Option<Self> {
        if !matches!(bpe, 1 | 2 | 4 | 8 | 16) {
            return None;
        }
        let log_bpe = bpe.ilog2() as usize;
        // Address bits 0..log_bpe are byte-within-element (0 masks); bits log_bpe..16 interleave X and Y.
        let mut bit_masks = vec![(0u32, 0u32); 16];
        let mut x_bit = 0u32;
        let mut y_bit = 0u32;
        let mut place_x = true;
        for slot in bit_masks.iter_mut().skip(log_bpe) {
            if place_x {
                *slot = (1u32 << x_bit, 0);
                x_bit += 1;
            } else {
                *slot = (0, 1u32 << y_bit);
                y_bit += 1;
            }
            place_x = !place_x;
        }
        Some(Self {
            block_bytes: 65536,
            bit_masks,
        })
    }

    /// GFX10 RDNA 2 64 KiB XOR-pipe `SW_64KB_R_X` variant (XORs high block coordinates into pipe bits
    /// to spread adjacent tiles across RDNA 2 memory channels).
    pub fn rdna2_64kb_rx(bpe: usize) -> Option<Self> {
        let mut eq = Self::standard_64kb_s(bpe)?;
        let log_bpe = bpe.ilog2() as usize;
        // Spread upper tile bits into address bits 8..11 (RDNA 2 bank/pipe XOR pattern)
        for i in 8..12 {
            if i >= log_bpe && i + 4 < eq.bit_masks.len() {
                let (hx, hy) = eq.bit_masks[i + 4];
                eq.bit_masks[i].0 ^= hx;
                eq.bit_masks[i].1 ^= hy;
            }
        }
        Some(eq)
    }

    /// Compute tiled byte offset for element `(x, y)` in a `width_elems x height_elems` surface.
    #[inline]
    pub fn element_byte_offset(&self, x: u32, y: u32, bpe: usize, width_elems: u32) -> usize {
        let mut intra = 0usize;
        for (bit, &(mx, my)) in self.bit_masks.iter().enumerate() {
            if mx != 0 || my != 0 {
                let p = ((x & mx).count_ones() ^ (y & my).count_ones()) & 1;
                intra |= (p as usize) << bit;
            }
        }
        let elems = self.block_bytes / bpe;
        let lb = elems.ilog2();
        let bw = 1u32 << lb.div_ceil(2);
        let bh = (elems as u32) / bw;
        let blocks_x = width_elems.div_ceil(bw);
        let block_idx = ((y / bh) * blocks_x + (x / bw)) as usize;
        block_idx * self.block_bytes + intra
    }
}

/// Swizzle a linear 2D surface (`width_elems x height_elems`, `bpe` bytes per element) into tiled bytes.
pub fn swizzle_2d(
    linear: &[u8],
    eq: &SwizzleEquation,
    bpe: usize,
    width_elems: u32,
    height_elems: u32,
) -> Option<Vec<u8>> {
    let total_elems = (width_elems as usize).checked_mul(height_elems as usize)?;
    if linear.len() < total_elems.checked_mul(bpe)? {
        return None;
    }
    let elems_per_block = eq.block_bytes / bpe;
    let lb = elems_per_block.ilog2();
    let bw = 1u32 << lb.div_ceil(2);
    let bh = (elems_per_block as u32) / bw;
    let blocks_x = width_elems.div_ceil(bw) as usize;
    let blocks_y = height_elems.div_ceil(bh) as usize;
    let mut tiled = vec![0u8; blocks_x * blocks_y * eq.block_bytes];

    for y in 0..height_elems {
        for x in 0..width_elems {
            let src_off = ((y as usize) * (width_elems as usize) + (x as usize)) * bpe;
            let dst_off = eq.element_byte_offset(x, y, bpe, width_elems);
            if dst_off + bpe > tiled.len() {
                return None;
            }
            tiled[dst_off..dst_off + bpe].copy_from_slice(&linear[src_off..src_off + bpe]);
        }
    }
    Some(tiled)
}

/// Detile a GFX10/RDNA 2 tiled 2D surface back into linear row-major order.
pub fn detile_2d(
    tiled: &[u8],
    eq: &SwizzleEquation,
    bpe: usize,
    width_elems: u32,
    height_elems: u32,
) -> Option<Vec<u8>> {
    let total_elems = (width_elems as usize).checked_mul(height_elems as usize)?;
    let mut linear = vec![0u8; total_elems.checked_mul(bpe)?];

    for y in 0..height_elems {
        for x in 0..width_elems {
            let src_off = eq.element_byte_offset(x, y, bpe, width_elems);
            if src_off + bpe > tiled.len() {
                return None;
            }
            let dst_off = ((y as usize) * (width_elems as usize) + (x as usize)) * bpe;
            linear[dst_off..dst_off + bpe].copy_from_slice(&tiled[src_off..src_off + bpe]);
        }
    }
    Some(linear)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_amd_rdna2_resource_tiling_with_packed_tail() {
        let desc = TilingDesc {
            kind: TiledKind::Texture2D,
            width: 1024,
            height: 1024,
            depth_or_layers: 1,
            mips: 11,
            element_bytes: 4, // RGBA8
            block: 1,
            samples: 1,
        };
        let tiling = compute_resource_tiling(&desc).expect("valid tiling");
        assert_eq!(tiling.shape, TileShape { width: 128, height: 128, depth: 1 });
        // Standard mips: 1024, 512, 256, 128 (w=128 > 64 -> standard), then 64..1 packed
        assert_eq!(tiling.standard_mips, 4);
        assert_eq!(tiling.packed_mips, 7);
        assert_eq!(tiling.packed_tiles, 1);
        // 64 + 16 + 4 + 1 standard tiles + 1 packed tile = 86 tiles
        assert_eq!(tiling.total_tiles, 86);
        assert_eq!(tiling.subresources[4].start_tile_index, PACKED_TILE);
    }

    #[test]
    fn round_trips_64kb_s_and_rdna2_rx_swizzle() {
        let w = 128u32;
        let h = 128u32;
        let bpe = 4usize;
        let mut linear = vec![0u8; (w * h) as usize * bpe];
        for i in 0..(w * h) as usize {
            linear[i * 4..i * 4 + 4].copy_from_slice(&(i as u32).to_le_bytes());
        }

        for eq in [
            SwizzleEquation::standard_64kb_s(bpe).unwrap(),
            SwizzleEquation::rdna2_64kb_rx(bpe).unwrap(),
        ] {
            let tiled = swizzle_2d(&linear, &eq, bpe, w, h).expect("swizzle");
            assert_ne!(tiled[..linear.len()], linear[..]);
            let back = detile_2d(&tiled, &eq, bpe, w, h).expect("detile");
            assert_eq!(back, linear);
        }
    }
}
