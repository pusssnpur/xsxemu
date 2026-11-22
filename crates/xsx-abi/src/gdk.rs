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

/// Win32 `SYSTEMTIME` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemTime {
    pub w_year: u16,
    pub w_month: u16,
    pub w_day_of_week: u16,
    pub w_day: u16,
    pub w_hour: u16,
    pub w_minute: u16,
    pub w_second: u16,
    pub w_milliseconds: u16,
}

/// Win32 `FILETIME` (8 bytes — 100-nanosecond intervals since 1601-01-01 00:00:00 UTC).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileTime {
    pub dw_low_date_time: u32,
    pub dw_high_date_time: u32,
}

impl FileTime {
    pub const fn from_u64(ticks: u64) -> Self {
        Self {
            dw_low_date_time: ticks as u32,
            dw_high_date_time: (ticks >> 32) as u32,
        }
    }

    pub const fn to_u64(self) -> u64 {
        (self.dw_low_date_time as u64) | ((self.dw_high_date_time as u64) << 32)
    }
}

/// Win32 `TIME_ZONE_INFORMATION` (172 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TimeZoneInformation {
    pub bias: i32,
    pub standard_name: [u16; 32],
    pub standard_date: SystemTime,
    pub standard_bias: i32,
    pub daylight_name: [u16; 32],
    pub daylight_date: SystemTime,
    pub daylight_bias: i32,
}

impl Default for TimeZoneInformation {
    fn default() -> Self {
        Self {
            bias: 0,
            standard_name: [0; 32],
            standard_date: SystemTime::default(),
            standard_bias: 0,
            daylight_name: [0; 32],
            daylight_date: SystemTime::default(),
            daylight_bias: 0,
        }
    }
}

/// Win64 `SYSTEM_INFO` (48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemInfo {
    pub w_processor_architecture: u16,
    pub w_reserved: u16,
    pub dw_page_size: u32,
    pub lp_minimum_application_address: usize,
    pub lp_maximum_application_address: usize,
    pub dw_active_processor_mask: usize,
    pub dw_number_of_processors: u32,
    pub dw_processor_type: u32,
    pub dw_allocation_granularity: u32,
    pub w_processor_level: u16,
    pub w_processor_revision: u16,
}

pub const PROCESSOR_ARCHITECTURE_AMD64: u16 = 9;

/// `XAsyncBlock` from Microsoft GDK `XAsync.h` (56 bytes on Win64).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct XAsyncBlock {
    pub queue: Handle,
    pub context: *mut c_void,
    pub callback: Option<unsafe extern "win64" fn(async_block: *mut XAsyncBlock)>,
    pub internal: [u8; 32],
}

/// `XTaskQueueCallback` signature.
pub type XTaskQueueCallback =
    unsafe extern "win64" fn(context: *mut c_void, canceled: bool);

/// `XTaskQueueRegistrationToken` (8 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct XTaskQueueRegistrationToken {
    pub token: u64,
}

/// `InitializeApiImplEx2` 4th argument options struct.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ApiImplOptions {
    pub unknown: u32,
    pub is_inline: i32,
    pub game_config: *const u8,
}

/// `XNetworkingConnectivityHint` from GDK `XNetworking.h` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct XNetworkingConnectivityHint {
    pub connectivity_level_hint: u32,
    pub connectivity_cost_hint: u32,
    pub iana_interface_type: u32,
    pub network_initialized: u8,
    pub approaching_data_limit: u8,
    pub over_data_limit: u8,
    pub roaming: u8,
}

/// Win64 `MEMORY_BASIC_INFORMATION` (48 bytes), prefix of 64-byte `XMemVirtualQuery` output.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MemoryBasicInformation {
    pub base_address: usize,
    pub allocation_base: usize,
    pub allocation_protect: u32,
    pub partition_id: u16,
    pub _pad0: u16,
    pub region_size: usize,
    pub state: u32,
    pub protect: u32,
    pub r#type: u32,
    pub _pad1: u32,
}

/// Win32 `WIN32_FILE_ATTRIBUTE_DATA` (36 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Win32FileAttributeData {
    pub dw_file_attributes: u32,
    pub ft_creation_time: FileTime,
    pub ft_last_access_time: FileTime,
    pub ft_last_write_time: FileTime,
    pub n_file_size_high: u32,
    pub n_file_size_low: u32,
}

/// Win32 `WIN32_FIND_DATAA` (320 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Win32FindDataA {
    pub dw_file_attributes: u32,
    pub ft_creation_time: FileTime,
    pub ft_last_access_time: FileTime,
    pub ft_last_write_time: FileTime,
    pub n_file_size_high: u32,
    pub n_file_size_low: u32,
    pub dw_reserved0: u32,
    pub dw_reserved1: u32,
    pub c_file_name: [u8; 260],
    pub c_alternate_file_name: [u8; 14],
    pub _pad: [u8; 2],
}

impl Default for Win32FindDataA {
    fn default() -> Self {
        Self {
            dw_file_attributes: 0,
            ft_creation_time: FileTime::default(),
            ft_last_access_time: FileTime::default(),
            ft_last_write_time: FileTime::default(),
            n_file_size_high: 0,
            n_file_size_low: 0,
            dw_reserved0: 0,
            dw_reserved1: 0,
            c_file_name: [0; 260],
            c_alternate_file_name: [0; 14],
            _pad: [0; 2],
        }
    }
}

/// Win32 `WIN32_FIND_DATAW` (592 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Win32FindDataW {
    pub dw_file_attributes: u32,
    pub ft_creation_time: FileTime,
    pub ft_last_access_time: FileTime,
    pub ft_last_write_time: FileTime,
    pub n_file_size_high: u32,
    pub n_file_size_low: u32,
    pub dw_reserved0: u32,
    pub dw_reserved1: u32,
    pub c_file_name: [u16; 260],
    pub c_alternate_file_name: [u16; 14],
}

impl Default for Win32FindDataW {
    fn default() -> Self {
        Self {
            dw_file_attributes: 0,
            ft_creation_time: FileTime::default(),
            ft_last_access_time: FileTime::default(),
            ft_last_write_time: FileTime::default(),
            n_file_size_high: 0,
            n_file_size_low: 0,
            dw_reserved0: 0,
            dw_reserved1: 0,
            c_file_name: [0; 260],
            c_alternate_file_name: [0; 14],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win64_sync_and_pdata_sizes() {
        assert_eq!(core::mem::size_of::<SrwLock>(), 8);
        assert_eq!(core::mem::size_of::<CriticalSection>(), 40);
        assert_eq!(core::mem::size_of::<RuntimeFunction>(), 12);
        assert_eq!(core::mem::size_of::<SystemTime>(), 16);
        assert_eq!(core::mem::size_of::<FileTime>(), 8);
        assert_eq!(core::mem::size_of::<TimeZoneInformation>(), 172);
        assert_eq!(core::mem::size_of::<SystemInfo>(), 48);
        assert_eq!(core::mem::size_of::<XAsyncBlock>(), 56);
        assert_eq!(core::mem::size_of::<XNetworkingConnectivityHint>(), 16);
        assert_eq!(core::mem::size_of::<MemoryBasicInformation>(), 48);
        assert_eq!(core::mem::size_of::<Win32FileAttributeData>(), 36);
        assert_eq!(core::mem::size_of::<Win32FindDataA>(), 320);
        assert_eq!(core::mem::size_of::<Win32FindDataW>(), 592);
    }
}
