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

    /// Serialize this container back into valid `DXBC` binary format.
    pub fn encode(&self) -> Vec<u8> {
        let header_size = 32 + self.chunks.len() * 4;
        let body_size: usize = self.chunks.iter().map(|c| 8 + c.data.len()).sum();
        let total_size = header_size + body_size;

        let mut out = Vec::with_capacity(total_size);
        out.extend_from_slice(&DXBC_MAGIC);
        out.extend_from_slice(&self.digest);
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

/// Inspect an Xbox Series X shader bytecode blob, strip Xbox-only metadata chunks,
/// and return a PC-compatible `DXBC`/`DXIL` container.
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
        digest: parsed.digest,
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
    fn strips_xbox_chunks_while_preserving_dxil_and_rootsig() {
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
    }
}
