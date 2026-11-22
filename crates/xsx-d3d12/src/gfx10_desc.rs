//! AMD GFX10 / RDNA 2 (`gfx103`) hardware resource descriptors (`T#`, `S#`, `V#`).
//!
//! Xbox Series X RDNA 2 shaders load 32-byte image descriptors (`T#`, 8 dwords),
//! 16-byte sampler descriptors (`S#`, 4 dwords), and 16-byte buffer descriptors (`V#`, 4 dwords)
//! directly from shader-visible descriptor heaps.
//!
//! Field positions match AMD's published GFX10.3 / Mesa `gfx10-rsrc.json` (`SQ_IMG_RSRC_WORD0..7`,
//! `SQ_BUF_RSRC_WORD0..3`, `SQ_IMG_SAMP_WORD0..3`).

pub type ImageWords = [u32; 8];
pub type SamplerWords = [u32; 4];
pub type BufferWords = [u32; 4];
pub type ChannelSwizzle = [u8; 4];

// SQ_SEL_XYZW01
pub const SELECT_ZERO: u8 = 0;
pub const SELECT_ONE: u8 = 1;
pub const SELECT_X: u8 = 4;
pub const SELECT_Y: u8 = 5;
pub const SELECT_Z: u8 = 6;
pub const SELECT_W: u8 = 7;

pub const IDENTITY_SWIZZLE: ChannelSwizzle = [SELECT_X, SELECT_Y, SELECT_Z, SELECT_W];

#[inline]
const fn field(value: u32, low: u32, width: u32) -> u32 {
    (value & ((1u32 << width) - 1)) << low
}

#[inline]
const fn get(word: u32, low: u32, width: u32) -> u32 {
    (word >> low) & ((1u32 << width) - 1)
}

#[inline]
fn unsigned_fixed_8(value: f32, low: f32, high: f32) -> u32 {
    let clamped = if value > low {
        if value < high { value } else { high }
    } else {
        low
    };
    (clamped * 256.0) as u32
}

#[inline]
pub fn is_valid_swizzle(swizzle: ChannelSwizzle) -> bool {
    swizzle.iter().all(|&s| s <= 7 && s != 2 && s != 3)
}

/// `SQ_RSRC_IMG_TYPE` (`WORD3[31:28]`). Values `0..=7` are reserved (distinguishing `T#` from `V#`).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gfx10ImageType {
    Image1D = 8,
    Image2D = 9,
    Image3D = 10,
    Cube = 11,
    Image1DArray = 12,
    Image2DArray = 13,
    Image2DMsaa = 14,
    Image2DMsaaArray = 15,
}

impl Gfx10ImageType {
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            8 => Some(Self::Image1D),
            9 => Some(Self::Image2D),
            10 => Some(Self::Image3D),
            11 => Some(Self::Cube),
            12 => Some(Self::Image1DArray),
            13 => Some(Self::Image2DArray),
            14 => Some(Self::Image2DMsaa),
            15 => Some(Self::Image2DMsaaArray),
            _ => None,
        }
    }

    pub const fn is_multisampled(self) -> bool {
        matches!(self, Self::Image2DMsaa | Self::Image2DMsaaArray)
    }
}

/// Decoded AMD GFX10/RDNA 2 `T#` image descriptor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gfx10ImageDesc {
    pub address: u64,       // 256-byte aligned console GPU VA
    pub format: u32,        // GFX10_FORMAT 9-bit code
    pub image_type: Gfx10ImageType,
    pub width: u32,         // 1..=16384
    pub height: u32,        // 1..=16384
    pub depth: u32,         // 1..=8192 for 3D
    pub first_layer: u32,
    pub layer_count: u32,
    pub first_mip: u32,
    pub mip_count: u32,
    pub resource_mips: u32,
    pub samples: u32,
    pub swizzle: ChannelSwizzle,
    pub min_lod: f32,
}

impl Default for Gfx10ImageDesc {
    fn default() -> Self {
        Self {
            address: 0,
            format: 0,
            image_type: Gfx10ImageType::Image2D,
            width: 1,
            height: 1,
            depth: 1,
            first_layer: 0,
            layer_count: 1,
            first_mip: 0,
            mip_count: 1,
            resource_mips: 1,
            samples: 1,
            swizzle: IDENTITY_SWIZZLE,
            min_lod: 0.0,
        }
    }
}

/// Returns `true` if `words` encodes a GFX10 `T#` image descriptor (`WORD3.TYPE >= 8`).
#[inline]
pub const fn is_image_descriptor(words: &ImageWords) -> bool {
    get(words[3], 28, 4) >= 8
}

/// Encode a `Gfx10ImageDesc` into 8 GFX10 `SQ_IMG_RSRC_WORD` dwords (`T#`).
pub fn encode_image_desc(img: &Gfx10ImageDesc) -> ImageWords {
    let msaa = img.image_type.is_multisampled();
    let sample_log2 = img.samples.max(1).ilog2();
    let base_level = if msaa { 0 } else { img.first_mip };
    let last_level = if msaa {
        sample_log2
    } else {
        img.first_mip + img.mip_count.saturating_sub(1)
    };
    let max_mip = if msaa {
        sample_log2
    } else {
        img.resource_mips.saturating_sub(1)
    };
    let last = if img.image_type == Gfx10ImageType::Image3D {
        img.depth.saturating_sub(1)
    } else {
        img.first_layer + img.layer_count.saturating_sub(1)
    };

    let units = img.address >> 8;
    let w = img.width.saturating_sub(1);
    let h = img.height.saturating_sub(1);

    let mut words = [0u32; 8];
    words[0] = units as u32;
    words[1] = field((units >> 32) as u32, 0, 8)
        | field(unsigned_fixed_8(img.min_lod, 0.0, 15.0), 8, 12)
        | field(img.format, 20, 9)
        | field(w, 30, 2);
    words[2] = field(w >> 2, 0, 12) | field(h, 14, 14);
    words[3] = field(img.swizzle[0] as u32, 0, 3)
        | field(img.swizzle[1] as u32, 3, 3)
        | field(img.swizzle[2] as u32, 6, 3)
        | field(img.swizzle[3] as u32, 9, 3)
        | field(base_level, 12, 4)
        | field(last_level, 16, 4)
        | field(img.image_type as u32, 28, 4);
    words[4] = field(last, 0, 13) | field(img.first_layer, 16, 13);
    words[5] = field(max_mip, 4, 4);
    words
}

/// Decode 8 GFX10 `SQ_IMG_RSRC_WORD` dwords (`T#`) into a validated `Gfx10ImageDesc`.
pub fn decode_image_desc(words: &ImageWords) -> Option<Gfx10ImageDesc> {
    let image_type = Gfx10ImageType::from_u32(get(words[3], 28, 4))?;
    let swizzle = [
        get(words[3], 0, 3) as u8,
        get(words[3], 3, 3) as u8,
        get(words[3], 6, 3) as u8,
        get(words[3], 9, 3) as u8,
    ];
    if !is_valid_swizzle(swizzle) {
        return None;
    }

    let address = (((get(words[1], 0, 8) as u64) << 32) | (words[0] as u64)) << 8;
    let min_lod = get(words[1], 8, 12) as f32 / 256.0;
    let format = get(words[1], 20, 9);
    let width = (get(words[1], 30, 2) | (get(words[2], 0, 12) << 2)) + 1;
    let height = get(words[2], 14, 14) + 1;

    let base_level = get(words[3], 12, 4);
    let last_level = get(words[3], 16, 4);

    let mut desc = Gfx10ImageDesc {
        address,
        format,
        image_type,
        width,
        height,
        swizzle,
        min_lod,
        ..Default::default()
    };

    if image_type.is_multisampled() {
        if base_level != 0 {
            return None;
        }
        desc.samples = 1u32 << last_level;
    } else {
        if last_level < base_level {
            return None;
        }
        desc.first_mip = base_level;
        desc.mip_count = last_level - base_level + 1;
        desc.resource_mips = get(words[5], 4, 4) + 1;
    }

    let last = get(words[4], 0, 13);
    desc.first_layer = get(words[4], 16, 13);
    if image_type == Gfx10ImageType::Image3D {
        desc.depth = last + 1;
    } else {
        if last < desc.first_layer {
            return None;
        }
        desc.layer_count = last - desc.first_layer + 1;
        if image_type == Gfx10ImageType::Cube && desc.layer_count % 6 != 0 {
            return None;
        }
    }
    Some(desc)
}

/// Decoded AMD GFX10 / RDNA 2 `V#` buffer descriptor (`SQ_BUF_RSRC_WORD0..3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gfx10BufferDesc {
    pub base_address: u64,
    pub stride: u32,
    pub num_records: u32,
    pub format: u32,
    pub swizzle: ChannelSwizzle,
}

pub fn encode_buffer_desc(buf: &Gfx10BufferDesc) -> BufferWords {
    let w0 = buf.base_address as u32;
    let w1 = field((buf.base_address >> 32) as u32, 0, 16) | field(buf.stride, 16, 14);
    let w2 = buf.num_records;
    let w3 = field(buf.swizzle[0] as u32, 0, 3)
        | field(buf.swizzle[1] as u32, 3, 3)
        | field(buf.swizzle[2] as u32, 6, 3)
        | field(buf.swizzle[3] as u32, 9, 3)
        | field(buf.format, 12, 7);
    [w0, w1, w2, w3]
}

pub fn decode_buffer_desc(words: &BufferWords) -> Option<Gfx10BufferDesc> {
    let swizzle = [
        get(words[3], 0, 3) as u8,
        get(words[3], 3, 3) as u8,
        get(words[3], 6, 3) as u8,
        get(words[3], 9, 3) as u8,
    ];
    if !is_valid_swizzle(swizzle) {
        return None;
    }
    let base_address = (words[0] as u64) | ((get(words[1], 0, 16) as u64) << 32);
    let stride = get(words[1], 16, 14);
    let num_records = words[2];
    let format = get(words[3], 12, 7);
    Some(Gfx10BufferDesc {
        base_address,
        stride,
        num_records,
        format,
        swizzle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_gfx10_image_and_buffer_descriptors() {
        let img = Gfx10ImageDesc {
            address: 0x0000_0042_1234_5600,
            format: 49, // R8G8B8A8_UNORM
            image_type: Gfx10ImageType::Image2DArray,
            width: 1920,
            height: 1080,
            depth: 1,
            first_layer: 2,
            layer_count: 6,
            first_mip: 1,
            mip_count: 4,
            resource_mips: 10,
            samples: 1,
            swizzle: [SELECT_Z, SELECT_Y, SELECT_X, SELECT_W], // BGRA
            min_lod: 0.5,
        };
        let words = encode_image_desc(&img);
        assert!(is_image_descriptor(&words));
        let decoded = decode_image_desc(&words).expect("valid T#");
        assert_eq!(decoded, img);

        let buf = Gfx10BufferDesc {
            base_address: 0x0000_1234_5678_9ABC,
            stride: 32,
            num_records: 4096,
            format: 21,
            swizzle: IDENTITY_SWIZZLE,
        };
        let bwords = encode_buffer_desc(&buf);
        let bdec = decode_buffer_desc(&bwords).expect("valid V#");
        assert_eq!(bdec, buf);
    }
}
