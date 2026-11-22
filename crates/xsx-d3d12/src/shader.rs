//! DirectX Shader Container (`DXBC` / `DXIL`) parser & Xbox-to-PC translator.
//!
//! Parses standard `DXBC` containers (used by both Shader Model 5 DXBC and Shader Model 6
//! DXIL), detects Xbox-specific console chunks (`XBOX`, `XGCS`, `XDBG`, `GCN0`, `RDNA`),
//! strips them cleanly, and emits a valid PC Direct3D 12 `DXBC` container with rebuilt
//! chunk offsets and total size.

use std::fmt;

pub const DXBC_MAGIC: [u8; 4] = *b"DXBC";

/// Console-specific FourCC chunk tags stripped during Xbox -> PC shader translation.
pub const XBOX_ONLY_CHUNKS: &[[u8; 4]] = &[
    *b"XBOX",
    *b"XGCS",
    *b"XDBG",
    *b"GCN0",
    *b"RDNA",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShaderError {
    TooSmall,
    InvalidMagic([u8; 4]),
    TruncatedChunkTable,
    ChunkOutOfBounds { index: usize, offset: usize },
}

impl fmt::Display for ShaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShaderError::TooSmall => write!(f, "shader container smaller than 32-byte DXBC header"),
            ShaderError::InvalidMagic(m) => {
                write!(f, "invalid shader container magic: {m:?} (expected b\"DXBC\")")
            }
            ShaderError::TruncatedChunkTable => write!(f, "DXBC chunk offset table truncated"),
            ShaderError::ChunkOutOfBounds { index, offset } => {
                write!(f, "DXBC chunk {index} at offset 0x{offset:X} out of bounds")
            }
        }
    }
}

impl std::error::Error for ShaderError {}

#[derive(Debug, Clone)]
pub struct ShaderChunk {
    pub fourcc: [u8; 4],
    pub data: Vec<u8>,
}

impl ShaderChunk {
    pub fn fourcc_str(&self) -> String {
        String::from_utf8_lossy(&self.fourcc).into_owned()
    }

    pub fn is_xbox_only(&self) -> bool {
        XBOX_ONLY_CHUNKS.contains(&self.fourcc)
    }
}

#[derive(Debug, Clone)]
pub struct ShaderContainer {
    pub digest: [u8; 16],
    pub version: u32,
    pub chunks: Vec<ShaderChunk>,
}

#[derive(Debug, Clone)]
pub struct TranslatedShader {
    pub pc_dxbc_bytes: Vec<u8>,
    pub has_dxil: bool,
    pub has_root_signature: bool,
    pub stripped_chunks: Vec<String>,
    pub retained_chunks: Vec<String>,
}

impl ShaderContainer {
    /// Parse a `DXBC` / `DXIL` binary container.
    pub fn parse(bytes: &[u8]) -> Result<Self, ShaderError> {
        if bytes.len() < 32 {
            return Err(ShaderError::TooSmall);
        }
        let magic: [u8; 4] = bytes[0..4].try_into().unwrap();
        if magic != DXBC_MAGIC {
            return Err(ShaderError::InvalidMagic(magic));
        }
        let digest: [u8; 16] = bytes[4..20].try_into().unwrap();
        let version = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let _total_size = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let chunk_count = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;

        let table_end = 32usize.saturating_add(chunk_count.saturating_mul(4));
        if table_end > bytes.len() {
            return Err(ShaderError::TruncatedChunkTable);
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        for i in 0..chunk_count {
            let off = u32::from_le_bytes(bytes[32 + i * 4..36 + i * 4].try_into().unwrap()) as usize;
            if off + 8 > bytes.len() {
                return Err(ShaderError::ChunkOutOfBounds { index: i, offset: off });
            }
            let fourcc: [u8; 4] = bytes[off..off + 4].try_into().unwrap();
            let size = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
            if off + 8 + size > bytes.len() {
                return Err(ShaderError::ChunkOutOfBounds { index: i, offset: off });
            }
            chunks.push(ShaderChunk {
                fourcc,
                data: bytes[off + 8..off + 8 + size].to_vec(),
            });
        }

        Ok(Self {
            digest,
            version,
            chunks,
        })
    }

    /// Serialize this container back into valid `DXBC` binary format and recompute the
    /// 16-byte Microsoft DXBC MD5-variant digest over `out[20..]`.
    pub fn encode(&self) -> Vec<u8> {
        let header_size = 32 + self.chunks.len() * 4;
        let body_size: usize = self.chunks.iter().map(|c| 8 + c.data.len()).sum();
        let total_size = header_size + body_size;

        let mut out = Vec::with_capacity(total_size);
        out.extend_from_slice(&DXBC_MAGIC);
        out.extend_from_slice(&[0u8; 16]); // placeholder for computed DXBC digest
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&(total_size as u32).to_le_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());

        let mut cursor = header_size as u32;
        for c in &self.chunks {
            out.extend_from_slice(&cursor.to_le_bytes());
            cursor += 8 + c.data.len() as u32;
        }
        for c in &self.chunks {
            out.extend_from_slice(&c.fourcc);
            out.extend_from_slice(&(c.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&c.data);
        }

        let digest = compute_dxbc_hash(&out[20..]);
        out[4..20].copy_from_slice(&digest);
        out
    }

    /// Build a synthetic `DXBC` container from `(fourcc, payload)` chunks (useful for tests & tooling).
    pub fn build(chunks: &[([u8; 4], &[u8])]) -> Vec<u8> {
        let c = Self {
            digest: [0; 16],
            version: 1,
            chunks: chunks
                .iter()
                .map(|&(fourcc, data)| ShaderChunk {
                    fourcc,
                    data: data.to_vec(),
                })
                .collect(),
        };
        c.encode()
    }
}

/// Compute Microsoft's 16-byte DXBC MD5-variant container hash over `payload` (`dxbc[20..]`).
pub fn compute_dxbc_hash(payload: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476];
    let full_blocks = payload.len() / 64;
    for i in 0..full_blocks {
        let block = &payload[i * 64..(i + 1) * 64];
        md5_compress(&mut state, block);
    }

    let rem = &payload[full_blocks * 64..];
    let bit_len = (payload.len() as u32).wrapping_mul(8);
    let mut tail = [0u8; 64];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;

    if rem.len() >= 56 {
        md5_compress(&mut state, &tail);
        tail = [0u8; 64];
    }
    // Microsoft DXBC MD5 padding places bit length in word 0 (if separate final block) or word 14,
    // and `(bit_len >> 1) | 1` in word 15 for DXBC verification.
    if rem.len() >= 56 {
        tail[0..4].copy_from_slice(&bit_len.to_le_bytes());
    } else {
        tail[56..60].copy_from_slice(&bit_len.to_le_bytes());
    }
    tail[60..64].copy_from_slice(&((bit_len >> 1) | 1).to_le_bytes());
    md5_compress(&mut state, &tail);

    let mut out = [0u8; 16];
    for (i, w) in state.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn md5_compress(state: &mut [u32; 4], block: &[u8]) {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20, 5,  9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];

    let mut m = [0u32; 16];
    for i in 0..16 {
        m[i] = u32::from_le_bytes(block[i * 4..(i + 1) * 4].try_into().unwrap());
    }

    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64 {
        let (f, g) = match i {
            0..=15 => ((b & c) | ((!b) & d), i),
            16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | (!d)), (7 * i) % 16),
        };
        let temp = d;
        d = c;
        c = b;
        b = b.wrapping_add(
            a.wrapping_add(f)
                .wrapping_add(K[i])
                .wrapping_add(m[g])
                .rotate_left(S[i]),
        );
        a = temp;
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// Inspect an Xbox Series X shader bytecode blob, strip Xbox-only metadata chunks,
/// and return a PC-compatible `DXBC`/`DXIL` container with recomputed DXBC checksum.
pub fn translate_xbox_shader_container(bytes: &[u8]) -> Result<TranslatedShader, ShaderError> {
    let parsed = ShaderContainer::parse(bytes)?;
    let mut stripped = Vec::new();
    let mut retained = Vec::new();
    let mut kept_chunks = Vec::new();
    let mut has_dxil = false;
    let mut has_root_signature = false;

    for chunk in parsed.chunks {
        if chunk.is_xbox_only() {
            stripped.push(chunk.fourcc_str());
        } else {
            if &chunk.fourcc == b"DXIL" {
                has_dxil = true;
            }
            if &chunk.fourcc == b"RTS0" {
                has_root_signature = true;
            }
            retained.push(chunk.fourcc_str());
            kept_chunks.push(chunk);
        }
    }

    let pc_container = ShaderContainer {
        digest: [0; 16],
        version: parsed.version,
        chunks: kept_chunks,
    };

    Ok(TranslatedShader {
        pc_dxbc_bytes: pc_container.encode(),
        has_dxil,
        has_root_signature,
        stripped_chunks: stripped,
        retained_chunks: retained,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_xbox_chunks_and_recomputes_dxbc_digest() {
        let raw = ShaderContainer::build(&[
            (*b"ISGN", &[1, 2, 3, 4]),
            (*b"XBOX", &[0xDE, 0xAD, 0xBE, 0xEF]),
            (*b"DXIL", b"BC\xC0\xDEdummy_dxil_payload"),
            (*b"XGCS", &[9, 8, 7, 6]),
            (*b"RTS0", &[0x02, 0x00, 0x00, 0x00]),
        ]);

        let translated = translate_xbox_shader_container(&raw).unwrap();
        assert!(translated.has_dxil);
        assert!(translated.has_root_signature);
        assert_eq!(translated.stripped_chunks, vec!["XBOX", "XGCS"]);
        assert_eq!(translated.retained_chunks, vec!["ISGN", "DXIL", "RTS0"]);

        let reparsed = ShaderContainer::parse(&translated.pc_dxbc_bytes).unwrap();
        assert_eq!(reparsed.chunks.len(), 3);
        assert!(reparsed.chunks.iter().all(|c| !c.is_xbox_only()));
        assert_eq!(reparsed.digest, compute_dxbc_hash(&translated.pc_dxbc_bytes[20..]));
        assert_ne!(reparsed.digest, [0u8; 16]);
    }
}
