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
pub const INFINITE: u32 = 0xFFFF_FFFF;

/// `DllMain` / `PIMAGE_TLS_CALLBACK` reason codes.
pub const DLL_PROCESS_DETACH: u32 = 0;
pub const DLL_PROCESS_ATTACH: u32 = 1;
pub const DLL_THREAD_ATTACH: u32 = 2;
pub const DLL_THREAD_DETACH: u32 = 3;

/// File I/O access & creation disposition flags.
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;

pub const CREATE_NEW: u32 = 1;
pub const CREATE_ALWAYS: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const OPEN_ALWAYS: u32 = 4;
pub const TRUNCATE_EXISTING: u32 = 5;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
pub const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

pub const FILE_BEGIN: u32 = 0;
pub const FILE_CURRENT: u32 = 1;
pub const FILE_END: u32 = 2;

/// Win32 `SRWLOCK` (pointer-sized opaque lock slot).
#[repr(C)]
#[derive(Debug, Default)]
pub struct SrwLock {
    pub ptr: usize,
}

/// Win32 `CRITICAL_SECTION` (40 bytes on Win64).
#[repr(C)]
#[derive(Debug, Default)]
pub struct CriticalSection {
    pub debug_info: usize,
    pub lock_count: i32,
    pub recursion_count: i32,
    pub owning_thread: usize,
    pub lock_semaphore: usize,
    pub spin_count: usize,
}

/// Windows x64 `.pdata` `IMAGE_RUNTIME_FUNCTION_ENTRY` (12 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeFunction {
    pub begin_address: u32,
    pub end_address: u32,
    pub unwind_info_address: u32,
}

/// Win64 `PIMAGE_TLS_CALLBACK` signature.
pub type TlsCallbackFn =
    unsafe extern "win64" fn(dll_handle: *mut c_void, reason: u32, reserved: *mut c_void);

/// Win64 `DllMain` signature.
pub type DllMainFn =
    unsafe extern "win64" fn(hinst_dll: *mut c_void, fdw_reason: u32, lpv_reserved: *mut c_void) -> i32;

/// Win64 `LPTHREAD_START_ROUTINE` signature.
pub type ThreadStartRoutine = unsafe extern "win64" fn(lp_thread_parameter: *mut c_void) -> u32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win64_sync_and_pdata_sizes() {
        assert_eq!(core::mem::size_of::<SrwLock>(), 8);
        assert_eq!(core::mem::size_of::<CriticalSection>(), 40);
        assert_eq!(core::mem::size_of::<RuntimeFunction>(), 12);
    }
}
