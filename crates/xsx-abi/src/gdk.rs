//! Public Microsoft GDK and Win32/Xbox OS ABI definitions.

use core::ffi::c_void;

/// Opaque Win32 / Xbox kernel handle.
pub type Handle = *mut c_void;

pub const INVALID_HANDLE_VALUE: Handle = (-1isize) as Handle;

/// Public `XSystemDeviceType` enum from Microsoft GDK documentation.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XSystemDeviceType {
    Unknown = 0x00,
    Pc = 0x01,
    XboxOne = 0x02,
    XboxOneS = 0x03,
    XboxOneX = 0x04,
    XboxOneXDevkit = 0x05,
    XboxScarlettLockhart = 0x06, // Xbox Series S
    XboxScarlettAnaconda = 0x07, // Xbox Series X
    XboxScarlettDevkit = 0x08,
}

/// Public `XTaskQueueDispatchMode` enum.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XTaskQueueDispatchMode {
    ThreadPool = 0,
    SerializedThreadPool = 1,
    Manual = 2,
    Immediate = 3,
}

/// Public `XTaskQueuePort` enum.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XTaskQueuePort {
    Work = 0,
    Completion = 1,
}

/// Public `XSystemAnalyticsInfo` version tuple.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XVersion {
    pub major: u16,
    pub minor: u16,
    pub build: u16,
    pub revision: u16,
}

/// Public `XSystemAnalyticsInfo` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XSystemAnalyticsInfo {
    pub os_version: XVersion,
    pub hosting_os_version: XVersion,
    pub family: [u8; 64],
    pub form: [u8; 64],
}

impl Default for XSystemAnalyticsInfo {
    fn default() -> Self {
        Self {
            os_version: XVersion::default(),
            hosting_os_version: XVersion::default(),
            family: [0; 64],
            form: [0; 64],
        }
    }
}

/// Title memory status reported by `TitleMemoryStatus` on Xbox OS.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TitleMemoryStatus {
    pub dw_length: u32,
    pub dw_reserved: u32,
    pub ull_total_mem: u64,
    pub ull_avail_mem: u64,
    pub ull_legacy_used: u64,
    pub ull_legacy_peak: u64,
    pub ull_legacy_avail: u64,
    pub ull_title_used: u64,
    pub ull_title_avail: u64,
}

/// Standard `MEM_*` allocation flags.
pub const MEM_COMMIT: u32 = 0x0000_1000;
pub const MEM_RESERVE: u32 = 0x0000_2000;
pub const MEM_RELEASE: u32 = 0x0000_8000;

/// Standard `PAGE_*` protection flags.
pub const PAGE_NOACCESS: u32 = 0x01;
pub const PAGE_READONLY: u32 = 0x02;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;

/// Standard `WAIT_*` return codes.
pub const WAIT_OBJECT_0: u32 = 0x0000_0000;
pub const WAIT_TIMEOUT: u32 = 0x0000_0102;
pub const WAIT_FAILED: u32 = 0xFFFF_FFFF;
