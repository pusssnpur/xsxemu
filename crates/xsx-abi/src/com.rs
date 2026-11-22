//! COM base types (`GUID`, `HRESULT`, `IUnknownVtbl`) for Win64 ABI interop.

use core::ffi::c_void;

/// Standard Windows / Xbox `HRESULT` status code.
pub type HResult = i32;

pub const S_OK: HResult = 0x0000_0000_u32 as i32;
pub const S_FALSE: HResult = 0x0000_0001_u32 as i32;
pub const E_NOTIMPL: HResult = 0x8000_4001_u32 as i32;
pub const E_NOINTERFACE: HResult = 0x8000_4002_u32 as i32;
pub const E_POINTER: HResult = 0x8000_4003_u32 as i32;
pub const E_FAIL: HResult = 0x8000_4005_u32 as i32;
pub const E_OUTOFMEMORY: HResult = 0x8007_000E_u32 as i32;
pub const E_INVALIDARG: HResult = 0x8007_0057_u32 as i32;

#[inline]
pub const fn succeeded(hr: HResult) -> bool {
    hr >= 0
}

#[inline]
pub const fn failed(hr: HResult) -> bool {
    hr < 0
}

/// 128-bit COM interface/class identifier (`GUID` / `IID` / `REFIID`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn from_u128(v: u128) -> Self {
        let bytes = v.to_be_bytes();
        Self {
            data1: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            data2: u16::from_be_bytes([bytes[4], bytes[5]]),
            data3: u16::from_be_bytes([bytes[6], bytes[7]]),
            data4: [
                bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14],
                bytes[15],
            ],
        }
    }

    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

/// `IUnknown` COM vtable (slots 0..=2).
#[repr(C)]
pub struct IUnknownVtbl {
    pub query_interface:
        unsafe extern "win64" fn(this: *mut c_void, riid: *const Guid, ppv: *mut *mut c_void) -> HResult,
    pub add_ref: unsafe extern "win64" fn(this: *mut c_void) -> u32,
    pub release: unsafe extern "win64" fn(this: *mut c_void) -> u32,
}

// Well-known public COM IIDs (from public Win32 / Direct3D 12 documentation).
pub const IID_IUNKNOWN: Guid =
    Guid::new(0x0000_0000, 0x0000, 0x0000, [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46]);

pub const IID_ID3D12OBJECT: Guid =
    Guid::new(0xC4FE_C28F, 0x7966, 0x4E95, [0x9F, 0x94, 0xF4, 0x31, 0xCB, 0x56, 0xC3, 0xB8]);

pub const IID_ID3D12DEVICE: Guid =
    Guid::new(0x1898_19F1, 0x1DB6, 0x4B57, [0xBE, 0x54, 0x18, 0x21, 0x33, 0x9B, 0x85, 0xF7]);

pub const IID_ID3D12COMMAND_QUEUE: Guid =
    Guid::new(0x0EC8_70A6, 0x5D7E, 0x4C22, [0x8C, 0xFC, 0x5B, 0xAA, 0xE0, 0x76, 0x16, 0xED]);

pub const IID_ID3D12COMMAND_ALLOCATOR: Guid =
    Guid::new(0x6102_DEE4, 0xAF59, 0x4B09, [0xB9, 0x99, 0xB4, 0x4D, 0x73, 0xF0, 0x9B, 0x24]);

pub const IID_ID3D12GRAPHICS_COMMAND_LIST: Guid =
    Guid::new(0x5B16_0D0F, 0xAC1B, 0x4185, [0x8B, 0xA8, 0xB3, 0xAE, 0x42, 0xA5, 0xA4, 0x55]);

pub const IID_ID3D12DESCRIPTOR_HEAP: Guid =
    Guid::new(0x8EFB_471D, 0x616C, 0x4F49, [0x90, 0xF7, 0x12, 0x7B, 0xB7, 0x63, 0xFA, 0x51]);

pub const IID_ID3D12RESOURCE: Guid =
    Guid::new(0x6964_42BE, 0xA72E, 0x4059, [0xBC, 0x79, 0x5B, 0x5C, 0x98, 0x04, 0x0F, 0xAD]);

pub const IID_ID3D12FENCE: Guid =
    Guid::new(0x0A75_3DCF, 0xC4D8, 0x4B91, [0xAD, 0xF6, 0xBE, 0x5A, 0x60, 0xD9, 0x5A, 0x76]);

pub const IID_IDXGIFACTORY4: Guid =
    Guid::new(0x1BC6_EA02, 0xEF36, 0x464F, [0xBF, 0x0C, 0x21, 0xCA, 0x39, 0xE5, 0x16, 0x8A]);

pub const IID_IDXGISWAPCHAIN1: Guid =
    Guid::new(0x790A_45F7, 0x0D42, 0x4876, [0x98, 0x3A, 0x0A, 0x55, 0xCF, 0xE6, 0xF4, 0xAA]);

pub const IID_IDXGISWAPCHAIN3: Guid =
    Guid::new(0x94D9_9BDB, 0xF1F8, 0x4AB0, [0xB2, 0x36, 0x7D, 0xA0, 0x17, 0x0E, 0xDA, 0xB1]);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guid_layout_is_16_bytes() {
        assert_eq!(core::mem::size_of::<Guid>(), 16);
        assert_eq!(core::mem::align_of::<Guid>(), 4);
    }

    #[test]
    fn hresult_predicates() {
        assert!(succeeded(S_OK));
        assert!(succeeded(S_FALSE));
        assert!(failed(E_FAIL));
        assert!(failed(E_INVALIDARG));
    }
}
