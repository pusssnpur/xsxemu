//! Xbox Virtual Disk (XVD / XVC) public container header parser & unencrypted reader.
//!
//! Clean-room scope: parses the publicly documented `msft-xvd` header (`0x200` magic,
//! 4 KiB header) and reads user data from unencrypted Dev Mode or synthetic XVD images.
//! Encrypted retail XVD/XVC images are cleanly rejected without attempting decryption.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const XVD_MAGIC: &[u8; 8] = b"msft-xvd";
pub const XVD_MAGIC_OFFSET: usize = 0x200;
pub const XVD_MIN_HEADER_SIZE: usize = 0x1000;

pub const XVD_FLAG_READ_ONLY: u32 = 1 << 0;
pub const XVD_FLAG_ENCRYPTION_DISABLED: u32 = 1 << 1;
pub const XVD_FLAG_DATA_INTEGRITY_DISABLED: u32 = 1 << 2;

const OFF_VOLUME_FLAGS: usize = 0x208;
const OFF_FORMAT_VERSION: usize = 0x20C;
const OFF_DRIVE_SIZE: usize = 0x218;
const OFF_CONTENT_ID: usize = 0x220;
const OFF_CONTENT_TYPE: usize = 0x240;
const OFF_EMBEDDED_XVD_LEN: usize = 0x244;
const OFF_USER_DATA_OFFSET: usize = 0x250;
const OFF_USER_DATA_LENGTH: usize = 0x258;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XvdError {
    Io(String),
    Truncated(usize),
    InvalidMagic,
    EncryptedImageNotSupported,
    OutOfBounds { offset: u64, size: u64, limit: u64 },
}

impl fmt::Display for XvdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "XVD I/O error: {e}"),
            Self::Truncated(n) => write!(f, "XVD image smaller than 4 KiB header ({n} bytes)"),
            Self::InvalidMagic => write!(f, "missing 'msft-xvd' magic at offset 0x200"),
            Self::EncryptedImageNotSupported => write!(
                f,
                "encrypted retail XVD/XVC images are not supported; supply an unencrypted Dev Mode or unpacked package"
            ),
            Self::OutOfBounds { offset, size, limit } => write!(
                f,
                "XVD user-data read [{offset}..+{size}] exceeds user_data_length ({limit})"
            ),
        }
    }
}

impl std::error::Error for XvdError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XvdHeaderInfo {
    pub magic: String,
    pub format_version: u32,
    pub content_type: u32,
    pub volume_flags: u32,
    pub drive_size: u64,
    pub user_data_offset: u64,
    pub user_data_length: u64,
    pub embedded_xvd_length: u32,
    pub is_encrypted: bool,
    pub has_data_integrity: bool,
    pub content_id: [u8; 16],
}

impl XvdHeaderInfo {
    pub fn content_id_hex(&self) -> String {
        let mut s = String::with_capacity(32);
        for b in self.content_id {
            s.push_str(&format!("{b:02X}"));
        }
        s
    }
}

enum XvdBacking {
    File(PathBuf),
    Memory(Vec<u8>),
}

/// Opened Xbox Virtual Disk (`msft-xvd`) container.
pub struct XvdContainer {
    header: XvdHeaderInfo,
    backing: XvdBacking,
}

impl XvdContainer {
    pub fn open_file(path: &Path) -> Result<Self, XvdError> {
        let mut f = File::open(path)
            .map_err(|e| XvdError::Io(format!("{}: {e}", path.display())))?;
        let mut hdr = vec![0u8; XVD_MIN_HEADER_SIZE];
        let mut read_total = 0usize;
        while read_total < XVD_MIN_HEADER_SIZE {
            match f.read(&mut hdr[read_total..]) {
                Ok(0) => return Err(XvdError::Truncated(read_total)),
                Ok(n) => read_total += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(XvdError::Io(format!("{}: {e}", path.display()))),
            }
        }
        let header = parse_xvd_header(&hdr)?;
        Ok(Self {
            header,
            backing: XvdBacking::File(path.to_path_buf()),
        })
    }

    pub fn open_bytes(bytes: Vec<u8>) -> Result<Self, XvdError> {
        let header = parse_xvd_header(&bytes)?;
        Ok(Self {
            header,
            backing: XvdBacking::Memory(bytes),
        })
    }

    pub fn header(&self) -> &XvdHeaderInfo {
        &self.header
    }

    /// Read bytes from the unencrypted user-data partition.
    pub fn read_user_data(&self, offset: u64, size: u64) -> Result<Vec<u8>, XvdError> {
        if self.header.is_encrypted {
            return Err(XvdError::EncryptedImageNotSupported);
        }
        if offset > self.header.user_data_length
            || size > self.header.user_data_length - offset
        {
            return Err(XvdError::OutOfBounds {
                offset,
                size,
                limit: self.header.user_data_length,
            });
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        let abs = self
            .header
            .user_data_offset
            .checked_add(offset)
            .ok_or(XvdError::OutOfBounds {
                offset,
                size,
                limit: self.header.user_data_length,
            })?;
        let size_usize = usize::try_from(size).map_err(|_| XvdError::OutOfBounds {
            offset,
            size,
            limit: self.header.user_data_length,
        })?;
        match &self.backing {
            XvdBacking::Memory(buf) => {
                let start = usize::try_from(abs).map_err(|_| XvdError::Truncated(buf.len()))?;
                let end = start
                    .checked_add(size_usize)
                    .ok_or(XvdError::Truncated(buf.len()))?;
                if end > buf.len() {
                    return Err(XvdError::Truncated(buf.len()));
                }
                Ok(buf[start..end].to_vec())
            }
            XvdBacking::File(path) => {
                let mut f = File::open(path)
                    .map_err(|e| XvdError::Io(format!("{}: {e}", path.display())))?;
                f.seek(SeekFrom::Start(abs))
                    .map_err(|e| XvdError::Io(format!("{}: {e}", path.display())))?;
                let mut out = vec![0u8; size_usize];
                f.read_exact(&mut out)
                    .map_err(|e| XvdError::Io(format!("{}: {e}", path.display())))?;
                Ok(out)
            }
        }
    }
}

pub fn parse_xvd_header(data: &[u8]) -> Result<XvdHeaderInfo, XvdError> {
    if data.len() < XVD_MIN_HEADER_SIZE {
        return Err(XvdError::Truncated(data.len()));
    }
    if &data[XVD_MAGIC_OFFSET..XVD_MAGIC_OFFSET + 8] != XVD_MAGIC {
        return Err(XvdError::InvalidMagic);
    }
    let rd32 = |off: usize| -> u32 {
        u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
    };
    let rd64 = |off: usize| -> u64 {
        u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
    };

    let volume_flags = rd32(OFF_VOLUME_FLAGS);
    let format_version = rd32(OFF_FORMAT_VERSION);
    let drive_size = rd64(OFF_DRIVE_SIZE);
    let mut content_id = [0u8; 16];
    content_id.copy_from_slice(&data[OFF_CONTENT_ID..OFF_CONTENT_ID + 16]);
    let content_type = rd32(OFF_CONTENT_TYPE);
    let embedded_xvd_length = rd32(OFF_EMBEDDED_XVD_LEN);
    let mut user_data_offset = rd64(OFF_USER_DATA_OFFSET);
    let mut user_data_length = rd64(OFF_USER_DATA_LENGTH);

    if user_data_offset < XVD_MIN_HEADER_SIZE as u64 {
        user_data_offset = XVD_MIN_HEADER_SIZE as u64;
    }
    if user_data_length == 0 && drive_size > user_data_offset {
        user_data_length = drive_size - user_data_offset;
    }

    Ok(XvdHeaderInfo {
        magic: String::from_utf8_lossy(XVD_MAGIC).into_owned(),
        format_version,
        content_type,
        volume_flags,
        drive_size,
        user_data_offset,
        user_data_length,
        embedded_xvd_length,
        is_encrypted: (volume_flags & XVD_FLAG_ENCRYPTION_DISABLED) == 0,
        has_data_integrity: (volume_flags & XVD_FLAG_DATA_INTEGRITY_DISABLED) == 0,
        content_id,
    })
}

/// Build a synthetic `msft-xvd` container image in memory for clean-room testing.
pub fn build_synthetic_xvd(user_data: &[u8], content_type: u32, mark_encrypted: bool) -> Vec<u8> {
    let total = XVD_MIN_HEADER_SIZE + user_data.len();
    let mut img = vec![0u8; total];
    img[XVD_MAGIC_OFFSET..XVD_MAGIC_OFFSET + 8].copy_from_slice(XVD_MAGIC);

    let mut flags = XVD_FLAG_READ_ONLY | XVD_FLAG_DATA_INTEGRITY_DISABLED;
    if !mark_encrypted {
        flags |= XVD_FLAG_ENCRYPTION_DISABLED;
    }
    img[OFF_VOLUME_FLAGS..OFF_VOLUME_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
    img[OFF_FORMAT_VERSION..OFF_FORMAT_VERSION + 4].copy_from_slice(&3u32.to_le_bytes());
    img[OFF_DRIVE_SIZE..OFF_DRIVE_SIZE + 8].copy_from_slice(&(total as u64).to_le_bytes());
    for (i, b) in img[OFF_CONTENT_ID..OFF_CONTENT_ID + 16].iter_mut().enumerate() {
        *b = 0xA0u8.wrapping_add(i as u8);
    }
    img[OFF_CONTENT_TYPE..OFF_CONTENT_TYPE + 4].copy_from_slice(&content_type.to_le_bytes());
    img[OFF_USER_DATA_OFFSET..OFF_USER_DATA_OFFSET + 8]
        .copy_from_slice(&(XVD_MIN_HEADER_SIZE as u64).to_le_bytes());
    img[OFF_USER_DATA_LENGTH..OFF_USER_DATA_LENGTH + 8]
        .copy_from_slice(&(user_data.len() as u64).to_le_bytes());
    img[XVD_MIN_HEADER_SIZE..].copy_from_slice(user_data);
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unencrypted_synthetic_xvd_and_reads_payload() {
        let payload = b"Xbox Series X unencrypted dev payload";
        let raw = build_synthetic_xvd(payload, 1, false);
        let xvd = XvdContainer::open_bytes(raw).unwrap();
        assert!(!xvd.header().is_encrypted);
        assert_eq!(xvd.header().format_version, 3);
        assert_eq!(xvd.header().user_data_length, payload.len() as u64);
        assert_eq!(xvd.read_user_data(0, payload.len() as u64).unwrap(), payload);
    }

    #[test]
    fn rejects_encrypted_retail_xvd_cleanly() {
        let raw = build_synthetic_xvd(b"secret", 1, true);
        let xvd = XvdContainer::open_bytes(raw).unwrap();
        assert!(xvd.header().is_encrypted);
        assert_eq!(
            xvd.read_user_data(0, 6),
            Err(XvdError::EncryptedImageNotSupported)
        );
    }
}
