//! `kernelx.dll` / `kernelbase.dll` / `kernel32.dll` HLE thunks.
//!
//! Phase 2 coverage:
//! - Virtual Xbox filesystem (`CreateFileA/W`, `ReadFile`, `WriteFile`, `SetFilePointerEx`,
//!   `GetFileSizeEx`, `GetFileAttributesA/W`) mapped onto `G:\`, `T:\`, `D:\`, `S:\`
//! - Real OS multithreading & synchronization (`CreateThread`, `GetExitCodeThread`,
//!   `CreateEventA`, `SetEvent`, `ResetEvent`, `WaitForSingleObject`, `SRWLOCK`, `CRITICAL_SECTION`)
//! - Heap, virtual memory, and Win32 TLS slots (`VirtualAlloc/Free`, `HeapAlloc/ReAlloc/Free`, `Tls*`)
//! - Runtime dynamic module lookups (`GetModuleHandleA`, `GetProcAddress`, `LoadLibraryA`)

use core::ffi::{c_char, c_void};
use std::ffi::CStr;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use xsx_abi::gdk::{
    CriticalSection, FileTime, Handle, SrwLock, SystemInfo, SystemTime, ThreadStartRoutine,
    TimeZoneInformation, TitleMemoryStatus, Win32FileAttributeData, Win32FindDataA,
    Win32FindDataW, CREATE_ALWAYS, CREATE_NEW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_BEGIN, FILE_CURRENT, FILE_END, GENERIC_READ, GENERIC_WRITE, INVALID_FILE_ATTRIBUTES,
    INVALID_HANDLE_VALUE, OPEN_ALWAYS, OPEN_EXISTING, PROCESSOR_ARCHITECTURE_AMD64,
    TRUNCATE_EXISTING, WAIT_FAILED,
};
use xsx_loader::ImportSymbol;

use crate::registry::{normalize_dll, ModuleRegistry};
use crate::rtc::{filetime_to_systemtime, systemtime_to_filetime};
use crate::runtime::{
    current_runtime, EventState, FindSearchState, GuestRuntime, KernelObject, ThreadState,
};
use crate::vfs::VfsDirEntry;

const PROCESS_HEAP_HANDLE: Handle = 0xDEAD_BEE0usize as Handle;

pub fn register(reg: &mut ModuleRegistry) {
    // Diagnostics, timing & Real-Time Clock (RTC)
    reg.register_export("kernelx.dll", "OutputDebugStringA", hle_output_debug_string_a as *const () as usize);
    reg.register_export("kernelx.dll", "OutputDebugStringW", hle_output_debug_string_w as *const () as usize);
    reg.register_export("kernelx.dll", "QueryPerformanceCounter", hle_query_performance_counter as *const () as usize);
    reg.register_export("kernelx.dll", "QueryPerformanceFrequency", hle_query_performance_frequency as *const () as usize);
    reg.register_export("kernelx.dll", "GetTickCount64", hle_get_tick_count_64 as *const () as usize);
    reg.register_export("kernelx.dll", "Sleep", hle_sleep as *const () as usize);
    reg.register_export("kernelx.dll", "GetSystemTime", hle_get_system_time as *const () as usize);
    reg.register_export("kernelx.dll", "GetLocalTime", hle_get_local_time as *const () as usize);
    reg.register_export("kernelx.dll", "GetSystemTimeAsFileTime", hle_get_system_time_as_file_time as *const () as usize);
    reg.register_export("kernelx.dll", "GetSystemTimePreciseAsFileTime", hle_get_system_time_as_file_time as *const () as usize);
    reg.register_export("kernelx.dll", "SystemTimeToFileTime", hle_system_time_to_file_time as *const () as usize);
    reg.register_export("kernelx.dll", "FileTimeToSystemTime", hle_file_time_to_system_time as *const () as usize);
    reg.register_export("kernelx.dll", "GetTimeZoneInformation", hle_get_time_zone_information as *const () as usize);
    reg.register_export("kernelx.dll", "GetSystemInfo", hle_get_system_info as *const () as usize);
    reg.register_export("kernelx.dll", "IsProcessorFeaturePresent", hle_is_processor_feature_present as *const () as usize);
    reg.register_export("kernelx.dll", "FlushInstructionCache", hle_flush_instruction_cache as *const () as usize);
    reg.register_export("kernelx.dll", "GetLastError", hle_get_last_error as *const () as usize);
    reg.register_export("kernelx.dll", "SetLastError", hle_set_last_error as *const () as usize);
    reg.register_export("kernelx.dll", "GetCurrentProcessId", hle_get_current_process_id as *const () as usize);
    reg.register_export("kernelx.dll", "GetCurrentThreadId", hle_get_current_thread_id as *const () as usize);

    // Events, threads & waits
    reg.register_export("kernelx.dll", "CreateEventA", hle_create_event_a as *const () as usize);
    reg.register_export("kernelx.dll", "SetEvent", hle_set_event as *const () as usize);
    reg.register_export("kernelx.dll", "ResetEvent", hle_reset_event as *const () as usize);
    reg.register_export("kernelx.dll", "CreateThread", hle_create_thread as *const () as usize);
    reg.register_export("kernelx.dll", "GetExitCodeThread", hle_get_exit_code_thread as *const () as usize);
    reg.register_export("kernelx.dll", "WaitForSingleObject", hle_wait_for_single_object as *const () as usize);
    reg.register_export("kernelx.dll", "CloseHandle", hle_close_handle as *const () as usize);

    // SRWLock & CriticalSection
    reg.register_export("kernelx.dll", "InitializeSRWLock", hle_initialize_srw_lock as *const () as usize);
    reg.register_export("kernelx.dll", "AcquireSRWLockExclusive", hle_acquire_srw_lock_exclusive as *const () as usize);
    reg.register_export("kernelx.dll", "ReleaseSRWLockExclusive", hle_release_srw_lock_exclusive as *const () as usize);
    reg.register_export("kernelx.dll", "AcquireSRWLockShared", hle_acquire_srw_lock_shared as *const () as usize);
    reg.register_export("kernelx.dll", "ReleaseSRWLockShared", hle_release_srw_lock_shared as *const () as usize);
    reg.register_export("kernelx.dll", "InitializeCriticalSection", hle_initialize_critical_section as *const () as usize);
    reg.register_export("kernelx.dll", "EnterCriticalSection", hle_enter_critical_section as *const () as usize);
    reg.register_export("kernelx.dll", "LeaveCriticalSection", hle_leave_critical_section as *const () as usize);
    reg.register_export("kernelx.dll", "DeleteCriticalSection", hle_delete_critical_section as *const () as usize);

    // File I/O (Virtual Xbox GAME:\, G:\, T:\, D:\, S:\ with CoW overlay)
    reg.register_export("kernelx.dll", "CreateFileA", hle_create_file_a as *const () as usize);
    reg.register_export("kernelx.dll", "CreateFileW", hle_create_file_w as *const () as usize);
    reg.register_export("kernelx.dll", "ReadFile", hle_read_file as *const () as usize);
    reg.register_export("kernelx.dll", "WriteFile", hle_write_file as *const () as usize);
    reg.register_export("kernelx.dll", "SetFilePointerEx", hle_set_file_pointer_ex as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileSizeEx", hle_get_file_size_ex as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileAttributesA", hle_get_file_attributes_a as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileAttributesW", hle_get_file_attributes_w as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileAttributesExA", hle_get_file_attributes_ex_a as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileAttributesExW", hle_get_file_attributes_ex_w as *const () as usize);
    reg.register_export("kernelx.dll", "DeleteFileA", hle_delete_file_a as *const () as usize);
    reg.register_export("kernelx.dll", "DeleteFileW", hle_delete_file_w as *const () as usize);
    reg.register_export("kernelx.dll", "CreateDirectoryA", hle_create_directory_a as *const () as usize);
    reg.register_export("kernelx.dll", "CreateDirectoryW", hle_create_directory_w as *const () as usize);
    reg.register_export("kernelx.dll", "RemoveDirectoryA", hle_remove_directory_a as *const () as usize);
    reg.register_export("kernelx.dll", "RemoveDirectoryW", hle_remove_directory_w as *const () as usize);
    reg.register_export("kernelx.dll", "FindFirstFileA", hle_find_first_file_a as *const () as usize);
    reg.register_export("kernelx.dll", "FindFirstFileW", hle_find_first_file_w as *const () as usize);
    reg.register_export("kernelx.dll", "FindNextFileA", hle_find_next_file_a as *const () as usize);
    reg.register_export("kernelx.dll", "FindNextFileW", hle_find_next_file_w as *const () as usize);
    reg.register_export("kernelx.dll", "FindClose", hle_find_close as *const () as usize);

    // Memory, Heap & Win32 TLS slots
    reg.register_export("kernelx.dll", "VirtualAlloc", hle_virtual_alloc as *const () as usize);
    reg.register_export("kernelx.dll", "VirtualFree", hle_virtual_free as *const () as usize);
    reg.register_export("kernelx.dll", "GetProcessHeap", hle_get_process_heap as *const () as usize);
    reg.register_export("kernelx.dll", "HeapAlloc", hle_heap_alloc as *const () as usize);
    reg.register_export("kernelx.dll", "HeapReAlloc", hle_heap_realloc as *const () as usize);
    reg.register_export("kernelx.dll", "HeapFree", hle_heap_free as *const () as usize);
    reg.register_export("kernelx.dll", "HeapSize", hle_heap_size as *const () as usize);
    reg.register_export("kernelx.dll", "TitleMemoryStatus", hle_title_memory_status as *const () as usize);
    reg.register_export("kernelx.dll", "TlsAlloc", hle_tls_alloc as *const () as usize);
    reg.register_export("kernelx.dll", "TlsGetValue", hle_tls_get_value as *const () as usize);
    reg.register_export("kernelx.dll", "TlsSetValue", hle_tls_set_value as *const () as usize);
    reg.register_export("kernelx.dll", "TlsFree", hle_tls_free as *const () as usize);

    // Runtime module queries & pseudo-handle owned-DLL resolution
    reg.register_export("kernelx.dll", "GetModuleHandleA", hle_get_module_handle_a as *const () as usize);
    reg.register_export("kernelx.dll", "GetModuleHandleW", hle_get_module_handle_w as *const () as usize);
    reg.register_export("kernelx.dll", "GetProcAddress", hle_get_proc_address as *const () as usize);
    reg.register_export("kernelx.dll", "LoadLibraryA", hle_load_library_a as *const () as usize);
    reg.register_export("kernelx.dll", "LoadLibraryW", hle_load_library_w as *const () as usize);
    reg.register_export("kernelx.dll", "LoadLibraryExA", hle_load_library_ex_a as *const () as usize);
    reg.register_export("kernelx.dll", "LoadLibraryExW", hle_load_library_ex_w as *const () as usize);
    reg.register_export("kernelx.dll", "FreeLibrary", hle_free_library as *const () as usize);

    // Process, command line, console, pointer encoding, FLS, condvar, UTF-8/16, memory status
    reg.register_export("kernelx.dll", "GetCurrentProcess", hle_get_current_process as *const () as usize);
    reg.register_export("kernelx.dll", "GetCurrentThread", hle_get_current_thread as *const () as usize);
    reg.register_export("kernelx.dll", "GetModuleFileNameA", hle_get_module_file_name_a as *const () as usize);
    reg.register_export("kernelx.dll", "GetModuleFileNameW", hle_get_module_file_name_w as *const () as usize);
    reg.register_export("kernelx.dll", "GetStartupInfoW", hle_get_startup_info_w as *const () as usize);
    reg.register_export("kernelx.dll", "GetStdHandle", hle_get_std_handle as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileType", hle_get_file_type as *const () as usize);
    reg.register_export("kernelx.dll", "GetConsoleMode", hle_get_console_mode as *const () as usize);
    reg.register_export("kernelx.dll", "GetEnvironmentStringsW", hle_get_environment_strings_w as *const () as usize);
    reg.register_export("kernelx.dll", "FreeEnvironmentStringsW", hle_free_environment_strings_w as *const () as usize);
    reg.register_export("kernelx.dll", "InitializeCriticalSectionAndSpinCount", hle_init_cs_spin as *const () as usize);
    reg.register_export("kernelx.dll", "InitializeCriticalSectionEx", hle_init_cs_ex as *const () as usize);
    reg.register_export("kernelx.dll", "TryEnterCriticalSection", hle_try_enter_cs as *const () as usize);
    reg.register_export("kernelx.dll", "CreateEventW", hle_create_event_w as *const () as usize);
    reg.register_export("kernelx.dll", "WaitForMultipleObjects", hle_wait_for_multiple_objects as *const () as usize);
    reg.register_export("kernelx.dll", "InitializeConditionVariable", hle_init_condvar as *const () as usize);
    reg.register_export("kernelx.dll", "WakeConditionVariable", hle_wake_condvar as *const () as usize);
    reg.register_export("kernelx.dll", "WakeAllConditionVariable", hle_wake_all_condvar as *const () as usize);
    reg.register_export("kernelx.dll", "SleepConditionVariableCS", hle_sleep_condvar_cs as *const () as usize);
    reg.register_export("kernelx.dll", "SleepConditionVariableSRW", hle_sleep_condvar_srw as *const () as usize);
    reg.register_export("kernelx.dll", "InitOnceExecuteOnce", hle_init_once_execute_once as *const () as usize);
    reg.register_export("kernelx.dll", "SwitchToThread", hle_switch_to_thread as *const () as usize);
    reg.register_export("kernelx.dll", "GetCurrentProcessorNumber", hle_get_current_processor_number as *const () as usize);
    reg.register_export("kernelx.dll", "SetThreadPriority", hle_return_one_i32 as *const () as usize);
    reg.register_export("kernelx.dll", "SetThreadAffinityMask", hle_set_thread_affinity_mask as *const () as usize);
    reg.register_export("kernelx.dll", "SetThreadDescription", hle_return_zero_i32 as *const () as usize);
    reg.register_export("kernelx.dll", "MultiByteToWideChar", hle_multi_byte_to_wide_char as *const () as usize);
    reg.register_export("kernelx.dll", "WideCharToMultiByte", hle_wide_char_to_multi_byte as *const () as usize);
    reg.register_export("kernelx.dll", "GetACP", hle_get_acp as *const () as usize);
    reg.register_export("kernelx.dll", "IsValidCodePage", hle_return_one_i32 as *const () as usize);
    reg.register_export("kernelx.dll", "EncodePointer", hle_encode_decode_pointer as *const () as usize);
    reg.register_export("kernelx.dll", "DecodePointer", hle_encode_decode_pointer as *const () as usize);
    reg.register_export("kernelx.dll", "IsDebuggerPresent", hle_return_zero_i32 as *const () as usize);
    reg.register_export("kernelx.dll", "SetUnhandledExceptionFilter", hle_set_unhandled_exception_filter as *const () as usize);
    reg.register_export("kernelx.dll", "UnhandledExceptionFilter", hle_return_zero_i32 as *const () as usize);
    reg.register_export("kernelx.dll", "FlsAlloc", hle_fls_alloc as *const () as usize);
    reg.register_export("kernelx.dll", "FlsGetValue", hle_tls_get_value as *const () as usize);
    reg.register_export("kernelx.dll", "FlsSetValue", hle_tls_set_value as *const () as usize);
    reg.register_export("kernelx.dll", "FlsFree", hle_tls_free as *const () as usize);
    reg.register_export("kernelx.dll", "VirtualProtect", hle_virtual_protect as *const () as usize);
    reg.register_export("kernelx.dll", "GlobalMemoryStatusEx", hle_global_memory_status_ex as *const () as usize);
}

// ---------------------------------------------------------------------------
// Diagnostics & Timing
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_output_debug_string_a(lp_output_string: *const c_char) {
    if lp_output_string.is_null() {
        return;
    }
    let msg = CStr::from_ptr(lp_output_string)
        .to_string_lossy()
        .into_owned();
    if let Some(rt) = current_runtime() {
        rt.trace(format!("kernelx!OutputDebugStringA({msg:?})"));
        rt.log_debug_string(msg);
    }
}

unsafe extern "win64" fn hle_output_debug_string_w(lp_output_string: *const u16) {
    if lp_output_string.is_null() {
        return;
    }
    let msg = read_utf16_cstr(lp_output_string);
    if let Some(rt) = current_runtime() {
        rt.trace(format!("kernelx!OutputDebugStringW({msg:?})"));
        rt.log_debug_string(msg);
    }
}

unsafe extern "win64" fn hle_query_performance_counter(lp_count: *mut i64) -> i32 {
    if lp_count.is_null() {
        return 0;
    }
    let ticks = current_runtime().map(|rt| rt.elapsed_us()).unwrap_or(0);
    *lp_count = ticks as i64;
    1
}

unsafe extern "win64" fn hle_query_performance_frequency(lp_freq: *mut i64) -> i32 {
    if lp_freq.is_null() {
        return 0;
    }
    *lp_freq = 1_000_000;
    1
}

unsafe extern "win64" fn hle_get_tick_count_64() -> u64 {
    current_runtime().map(|rt| rt.elapsed_ms()).unwrap_or(0)
}

unsafe extern "win64" fn hle_sleep(dw_milliseconds: u32) {
    if dw_milliseconds > 0 {
        thread::sleep(Duration::from_millis(dw_milliseconds as u64));
    }
}

unsafe extern "win64" fn hle_get_last_error() -> u32 {
    current_runtime().map(|rt| rt.get_last_error()).unwrap_or(0)
}

unsafe extern "win64" fn hle_set_last_error(dw_err_code: u32) {
    if let Some(rt) = current_runtime() {
        rt.set_last_error(dw_err_code);
    }
}

unsafe extern "win64" fn hle_get_current_process_id() -> u32 {
    std::process::id()
}

unsafe extern "win64" fn hle_get_current_thread_id() -> u32 {
    GuestRuntime::current_thread_u32()
}

// ---------------------------------------------------------------------------
// Events, Real Threads & Waits
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_create_event_a(
    _lp_attrs: *mut c_void,
    b_manual_reset: i32,
    b_initial_state: i32,
    _lp_name: *const c_char,
) -> Handle {
    if let Some(rt) = current_runtime() {
        let ev = Arc::new((
            Mutex::new(EventState {
                manual_reset: b_manual_reset != 0,
                signaled: b_initial_state != 0,
            }),
            Condvar::new(),
        ));
        let h = rt.insert_handle(KernelObject::Event(ev));
        rt.trace(format!("kernelx!CreateEventA() -> {h:?}"));
        h
    } else {
        core::ptr::null_mut()
    }
}

unsafe extern "win64" fn hle_set_event(h_event: Handle) -> i32 {
    if let Some(rt) = current_runtime() {
        if let Some(ev) = rt.get_event(h_event) {
            let (lock, cvar) = &*ev;
            let mut st = lock.lock().unwrap();
            st.signaled = true;
            if st.manual_reset {
                cvar.notify_all();
            } else {
                cvar.notify_one();
            }
            return 1;
        }
    }
    0
}

unsafe extern "win64" fn hle_reset_event(h_event: Handle) -> i32 {
    if let Some(rt) = current_runtime() {
        if let Some(ev) = rt.get_event(h_event) {
            let (lock, _) = &*ev;
            lock.lock().unwrap().signaled = false;
            return 1;
        }
    }
    0
}

unsafe extern "win64" fn hle_create_thread(
    _lp_thread_attributes: *mut c_void,
    _dw_stack_size: usize,
    lp_start_address: *const c_void,
    lp_parameter: *mut c_void,
    _dw_creation_flags: u32,
    lp_thread_id: *mut u32,
) -> Handle {
    if lp_start_address.is_null() {
        return core::ptr::null_mut();
    }
    let Some(rt) = current_runtime() else {
        return core::ptr::null_mut();
    };

    let state = Arc::new(ThreadState {
        done: Mutex::new(false),
        condvar: Condvar::new(),
        exit_code: AtomicU32::new(259), // STILL_ACTIVE
    });
    let state_bg = Arc::clone(&state);
    let start_addr = lp_start_address as usize;
    let param_addr = lp_parameter as usize;
    let rt_bg = Arc::clone(&rt);

    let tid_slot = Arc::new(AtomicU32::new(0));
    let tid_bg = Arc::clone(&tid_slot);

    let _ = thread::spawn(move || {
        tid_bg.store(GuestRuntime::current_thread_u32(), Ordering::Relaxed);
        let entry: ThreadStartRoutine = std::mem::transmute(start_addr);
        let rc = entry(param_addr as *mut c_void);
        state_bg.exit_code.store(rc, Ordering::SeqCst);
        let mut done = state_bg.done.lock().unwrap();
        *done = true;
        state_bg.condvar.notify_all();
        rt_bg.trace(format!("kernelx!Thread exited with code {rc}"));
    });

    let h = rt.insert_handle(KernelObject::Thread(state));
    if !lp_thread_id.is_null() {
        *lp_thread_id = tid_slot.load(Ordering::Relaxed).max(2);
    }
    rt.trace(format!(
        "kernelx!CreateThread(start=0x{start_addr:X}, param=0x{param_addr:X}) -> {h:?}"
    ));
    h
}

unsafe extern "win64" fn hle_get_exit_code_thread(h_thread: Handle, lp_exit_code: *mut u32) -> i32 {
    if lp_exit_code.is_null() {
        return 0;
    }
    if let Some(rt) = current_runtime() {
        if let Some(th) = rt.get_thread(h_thread) {
            *lp_exit_code = th.exit_code.load(Ordering::SeqCst);
            return 1;
        }
    }
    0
}

unsafe extern "win64" fn hle_wait_for_single_object(h_handle: Handle, dw_ms: u32) -> u32 {
    if let Some(rt) = current_runtime() {
        if let Some(res) = rt.wait_handle(h_handle, dw_ms) {
            return res;
        }
    }
    WAIT_FAILED
}

unsafe extern "win64" fn hle_close_handle(h_object: Handle) -> i32 {
    if let Some(rt) = current_runtime() {
        return i32::from(rt.close_handle(h_object));
    }
    0
}

// ---------------------------------------------------------------------------
// SRWLock & CriticalSection
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_initialize_srw_lock(srw: *mut SrwLock) {
    if srw.is_null() {
        return;
    }
    (*srw).ptr = 0;
    if let Some(rt) = current_runtime() {
        let _ = rt.get_or_init_srw(srw as usize);
    }
}

unsafe extern "win64" fn hle_acquire_srw_lock_exclusive(srw: *mut SrwLock) {
    if srw.is_null() {
        return;
    }
    if let Some(rt) = current_runtime() {
        let pair = rt.get_or_init_srw(srw as usize);
        let (lock, cvar) = &*pair;
        let mut st = lock.lock().unwrap();
        while st.writer || st.readers > 0 {
            st = cvar.wait(st).unwrap();
        }
        st.writer = true;
    }
}

unsafe extern "win64" fn hle_release_srw_lock_exclusive(srw: *mut SrwLock) {
    if srw.is_null() {
        return;
    }
    if let Some(rt) = current_runtime() {
        let pair = rt.get_or_init_srw(srw as usize);
        let (lock, cvar) = &*pair;
        let mut st = lock.lock().unwrap();
        st.writer = false;
        cvar.notify_all();
    }
}

unsafe extern "win64" fn hle_acquire_srw_lock_shared(srw: *mut SrwLock) {
    if srw.is_null() {
        return;
    }
    if let Some(rt) = current_runtime() {
        let pair = rt.get_or_init_srw(srw as usize);
        let (lock, cvar) = &*pair;
        let mut st = lock.lock().unwrap();
        while st.writer {
            st = cvar.wait(st).unwrap();
        }
        st.readers += 1;
    }
}

unsafe extern "win64" fn hle_release_srw_lock_shared(srw: *mut SrwLock) {
    if srw.is_null() {
        return;
    }
    if let Some(rt) = current_runtime() {
        let pair = rt.get_or_init_srw(srw as usize);
        let (lock, cvar) = &*pair;
        let mut st = lock.lock().unwrap();
        st.readers = st.readers.saturating_sub(1);
        if st.readers == 0 {
            cvar.notify_all();
        }
    }
}

unsafe extern "win64" fn hle_initialize_critical_section(cs: *mut CriticalSection) {
    if cs.is_null() {
        return;
    }
    *cs = CriticalSection::default();
    if let Some(rt) = current_runtime() {
        let _ = rt.get_or_init_crit(cs as usize);
    }
}

unsafe extern "win64" fn hle_enter_critical_section(cs: *mut CriticalSection) {
    if cs.is_null() {
        return;
    }
    if let Some(rt) = current_runtime() {
        let pair = rt.get_or_init_crit(cs as usize);
        let (lock, cvar) = &*pair;
        let me = thread::current().id();
        let mut st = lock.lock().unwrap();
        while st.owner.is_some() && st.owner != Some(me) {
            st = cvar.wait(st).unwrap();
        }
        st.owner = Some(me);
        st.recursion += 1;
    }
}

unsafe extern "win64" fn hle_leave_critical_section(cs: *mut CriticalSection) {
    if cs.is_null() {
        return;
    }
    if let Some(rt) = current_runtime() {
        let pair = rt.get_or_init_crit(cs as usize);
        let (lock, cvar) = &*pair;
        let mut st = lock.lock().unwrap();
        st.recursion = st.recursion.saturating_sub(1);
        if st.recursion == 0 {
            st.owner = None;
            cvar.notify_one();
        }
    }
}

unsafe extern "win64" fn hle_delete_critical_section(cs: *mut CriticalSection) {
    if cs.is_null() {
        return;
    }
    if let Some(rt) = current_runtime() {
        rt.remove_crit(cs as usize);
    }
}

// ---------------------------------------------------------------------------
// File I/O (Virtual Xbox G:\, T:\, D:\, S:\)
// ---------------------------------------------------------------------------

fn open_guest_file(guest_path: &str, access: u32, disposition: u32) -> Handle {
    let Some(rt) = current_runtime() else {
        return INVALID_HANDLE_VALUE;
    };
    let _ = rt.vfs.ensure_roots();

    let read = (access & GENERIC_READ) != 0 || access == 0;
    let write = (access & GENERIC_WRITE) != 0
        || matches!(
            disposition,
            CREATE_NEW | CREATE_ALWAYS | OPEN_ALWAYS | TRUNCATE_EXISTING
        );

    let host_path = if write {
        let copy_up = matches!(disposition, OPEN_EXISTING | OPEN_ALWAYS);
        match rt.vfs.resolve_for_write(guest_path, copy_up) {
            Some(p) => p,
            None => {
                rt.set_last_error(3); // ERROR_PATH_NOT_FOUND
                return INVALID_HANDLE_VALUE;
            }
        }
    } else {
        match rt.vfs.resolve_for_read(guest_path) {
            Some(p) => p,
            None => {
                rt.set_last_error(2); // ERROR_FILE_NOT_FOUND
                return INVALID_HANDLE_VALUE;
            }
        }
    };

    if write {
        if let Some(parent) = host_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
    }

    let mut opts = OpenOptions::new();
    opts.read(read).write((access & GENERIC_WRITE) != 0);

    match disposition {
        CREATE_NEW => {
            opts.create_new(true).write(true);
        }
        CREATE_ALWAYS => {
            opts.create(true).truncate(true).write(true);
        }
        OPEN_EXISTING => {}
        OPEN_ALWAYS => {
            opts.create(true);
        }
        TRUNCATE_EXISTING => {
            opts.truncate(true).write(true);
        }
        _ => {
            rt.set_last_error(87); // ERROR_INVALID_PARAMETER
            return INVALID_HANDLE_VALUE;
        }
    }

    match opts.open(&host_path) {
        Ok(file) => {
            let h = rt.insert_handle(KernelObject::File(Arc::new(Mutex::new(file))));
            rt.trace(format!(
                "kernelx!CreateFile({guest_path:?} -> {}) -> {h:?}",
                host_path.display()
            ));
            h
        }
        Err(_) => {
            rt.set_last_error(2); // ERROR_FILE_NOT_FOUND
            INVALID_HANDLE_VALUE
        }
    }
}

unsafe extern "win64" fn hle_create_file_a(
    lp_file_name: *const c_char,
    dw_desired_access: u32,
    _dw_share_mode: u32,
    _lp_security_attributes: *mut c_void,
    dw_creation_disposition: u32,
    _dw_flags_and_attributes: u32,
    _h_template_file: Handle,
) -> Handle {
    if lp_file_name.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    let path = CStr::from_ptr(lp_file_name).to_string_lossy();
    open_guest_file(&path, dw_desired_access, dw_creation_disposition)
}

unsafe extern "win64" fn hle_create_file_w(
    lp_file_name: *const u16,
    dw_desired_access: u32,
    _dw_share_mode: u32,
    _lp_security_attributes: *mut c_void,
    dw_creation_disposition: u32,
    _dw_flags_and_attributes: u32,
    _h_template_file: Handle,
) -> Handle {
    if lp_file_name.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    let path = read_utf16_cstr(lp_file_name);
    open_guest_file(&path, dw_desired_access, dw_creation_disposition)
}

unsafe extern "win64" fn hle_read_file(
    h_file: Handle,
    lp_buffer: *mut u8,
    n_bytes_to_read: u32,
    lp_bytes_read: *mut u32,
    _lp_overlapped: *mut c_void,
) -> i32 {
    if lp_buffer.is_null() {
        return 0;
    }
    let Some(rt) = current_runtime() else {
        return 0;
    };
    let Some(file_arc) = rt.get_file(h_file) else {
        return 0;
    };
    let slice = std::slice::from_raw_parts_mut(lp_buffer, n_bytes_to_read as usize);
    let mut f = file_arc.lock().unwrap();
    match f.read(slice) {
        Ok(n) => {
            if !lp_bytes_read.is_null() {
                *lp_bytes_read = n as u32;
            }
            1
        }
        Err(_) => 0,
    }
}

unsafe extern "win64" fn hle_write_file(
    h_file: Handle,
    lp_buffer: *const u8,
    n_bytes_to_write: u32,
    lp_bytes_written: *mut u32,
    _lp_overlapped: *mut c_void,
) -> i32 {
    if lp_buffer.is_null() {
        return 0;
    }
    let Some(rt) = current_runtime() else {
        return 0;
    };
    let Some(file_arc) = rt.get_file(h_file) else {
        return 0;
    };
    let slice = std::slice::from_raw_parts(lp_buffer, n_bytes_to_write as usize);
    let mut f = file_arc.lock().unwrap();
    match f.write(slice) {
        Ok(n) => {
            if !lp_bytes_written.is_null() {
                *lp_bytes_written = n as u32;
            }
            1
        }
        Err(_) => 0,
    }
}

unsafe extern "win64" fn hle_set_file_pointer_ex(
    h_file: Handle,
    li_distance_to_move: i64,
    lp_new_file_pointer: *mut i64,
    dw_move_method: u32,
) -> i32 {
    let Some(rt) = current_runtime() else {
        return 0;
    };
    let Some(file_arc) = rt.get_file(h_file) else {
        return 0;
    };
    let pos = match dw_move_method {
        FILE_BEGIN => SeekFrom::Start(li_distance_to_move.max(0) as u64),
        FILE_CURRENT => SeekFrom::Current(li_distance_to_move),
        FILE_END => SeekFrom::End(li_distance_to_move),
        _ => return 0,
    };
    let mut f = file_arc.lock().unwrap();
    match f.seek(pos) {
        Ok(new_pos) => {
            if !lp_new_file_pointer.is_null() {
                *lp_new_file_pointer = new_pos as i64;
            }
            1
        }
        Err(_) => 0,
    }
}

unsafe extern "win64" fn hle_get_file_size_ex(h_file: Handle, lp_file_size: *mut i64) -> i32 {
    if lp_file_size.is_null() {
        return 0;
    }
    let Some(rt) = current_runtime() else {
        return 0;
    };
    let Some(file_arc) = rt.get_file(h_file) else {
        return 0;
    };
    let f = file_arc.lock().unwrap();
    match f.metadata() {
        Ok(meta) => {
            *lp_file_size = meta.len() as i64;
            1
        }
        Err(_) => 0,
    }
}

fn file_attributes_for_guest_path(path: &str) -> u32 {
    let Some(rt) = current_runtime() else {
        return INVALID_FILE_ATTRIBUTES;
    };
    let Some(host_path) = rt.vfs.resolve_for_read(path) else {
        return INVALID_FILE_ATTRIBUTES;
    };
    match fs::metadata(host_path) {
        Ok(m) if m.is_dir() => FILE_ATTRIBUTE_DIRECTORY,
        Ok(_) => FILE_ATTRIBUTE_NORMAL,
        Err(_) => INVALID_FILE_ATTRIBUTES,
    }
}

unsafe extern "win64" fn hle_get_file_attributes_a(lp_file_name: *const c_char) -> u32 {
    if lp_file_name.is_null() {
        return INVALID_FILE_ATTRIBUTES;
    }
    let path = CStr::from_ptr(lp_file_name).to_string_lossy();
    file_attributes_for_guest_path(&path)
}

unsafe extern "win64" fn hle_get_file_attributes_w(lp_file_name: *const u16) -> u32 {
    if lp_file_name.is_null() {
        return INVALID_FILE_ATTRIBUTES;
    }
    let path = read_utf16_cstr(lp_file_name);
    file_attributes_for_guest_path(&path)
}

unsafe fn fill_file_attr_ex(path: &str, out: *mut Win32FileAttributeData) -> i32 {
    if out.is_null() {
        return 0;
    }
    let Some(rt) = current_runtime() else {
        return 0;
    };
    let Some(host_path) = rt.vfs.resolve_for_read(path) else {
        rt.set_last_error(2);
        return 0;
    };
    let Ok(meta) = fs::metadata(host_path) else {
        rt.set_last_error(2);
        return 0;
    };
    let sz = meta.len();
    let now = rt.rtc().now_filetime_utc();
    *out = Win32FileAttributeData {
        dw_file_attributes: if meta.is_dir() {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        },
        ft_creation_time: now,
        ft_last_access_time: now,
        ft_last_write_time: now,
        n_file_size_high: (sz >> 32) as u32,
        n_file_size_low: sz as u32,
    };
    1
}

unsafe extern "win64" fn hle_get_file_attributes_ex_a(
    lp_file_name: *const c_char,
    _info_level: u32,
    lp_file_info: *mut Win32FileAttributeData,
) -> i32 {
    if lp_file_name.is_null() {
        return 0;
    }
    let path = CStr::from_ptr(lp_file_name).to_string_lossy();
    fill_file_attr_ex(&path, lp_file_info)
}

unsafe extern "win64" fn hle_get_file_attributes_ex_w(
    lp_file_name: *const u16,
    _info_level: u32,
    lp_file_info: *mut Win32FileAttributeData,
) -> i32 {
    if lp_file_name.is_null() {
        return 0;
    }
    let path = read_utf16_cstr(lp_file_name);
    fill_file_attr_ex(&path, lp_file_info)
}

unsafe extern "win64" fn hle_delete_file_a(lp_file_name: *const c_char) -> i32 {
    if lp_file_name.is_null() {
        return 0;
    }
    let path = CStr::from_ptr(lp_file_name).to_string_lossy();
    let ok = current_runtime()
        .map(|rt| rt.vfs.delete_guest_file(&path))
        .unwrap_or(false);
    i32::from(ok)
}

unsafe extern "win64" fn hle_delete_file_w(lp_file_name: *const u16) -> i32 {
    if lp_file_name.is_null() {
        return 0;
    }
    let path = read_utf16_cstr(lp_file_name);
    let ok = current_runtime()
        .map(|rt| rt.vfs.delete_guest_file(&path))
        .unwrap_or(false);
    i32::from(ok)
}

unsafe extern "win64" fn hle_create_directory_a(
    lp_path_name: *const c_char,
    _lp_security: *mut c_void,
) -> i32 {
    if lp_path_name.is_null() {
        return 0;
    }
    let path = CStr::from_ptr(lp_path_name).to_string_lossy();
    let ok = current_runtime()
        .map(|rt| rt.vfs.create_guest_dir(&path))
        .unwrap_or(false);
    i32::from(ok)
}

unsafe extern "win64" fn hle_create_directory_w(
    lp_path_name: *const u16,
    _lp_security: *mut c_void,
) -> i32 {
    if lp_path_name.is_null() {
        return 0;
    }
    let path = read_utf16_cstr(lp_path_name);
    let ok = current_runtime()
        .map(|rt| rt.vfs.create_guest_dir(&path))
        .unwrap_or(false);
    i32::from(ok)
}

unsafe extern "win64" fn hle_remove_directory_a(lp_path_name: *const c_char) -> i32 {
    if lp_path_name.is_null() {
        return 0;
    }
    let path = CStr::from_ptr(lp_path_name).to_string_lossy();
    let ok = current_runtime()
        .map(|rt| rt.vfs.remove_guest_dir(&path))
        .unwrap_or(false);
    i32::from(ok)
}

unsafe extern "win64" fn hle_remove_directory_w(lp_path_name: *const u16) -> i32 {
    if lp_path_name.is_null() {
        return 0;
    }
    let path = read_utf16_cstr(lp_path_name);
    let ok = current_runtime()
        .map(|rt| rt.vfs.remove_guest_dir(&path))
        .unwrap_or(false);
    i32::from(ok)
}

fn fill_find_data_a(entry: &VfsDirEntry, out: &mut Win32FindDataA) {
    *out = Win32FindDataA::default();
    out.dw_file_attributes = if entry.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    out.n_file_size_high = (entry.size >> 32) as u32;
    out.n_file_size_low = entry.size as u32;
    let bytes = entry.name.as_bytes();
    let n = bytes.len().min(out.c_file_name.len() - 1);
    out.c_file_name[..n].copy_from_slice(&bytes[..n]);
    out.c_file_name[n] = 0;
}

fn fill_find_data_w(entry: &VfsDirEntry, out: &mut Win32FindDataW) {
    *out = Win32FindDataW::default();
    out.dw_file_attributes = if entry.is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    out.n_file_size_high = (entry.size >> 32) as u32;
    out.n_file_size_low = entry.size as u32;
    let units: Vec<u16> = entry.name.encode_utf16().collect();
    let n = units.len().min(out.c_file_name.len() - 1);
    out.c_file_name[..n].copy_from_slice(&units[..n]);
    out.c_file_name[n] = 0;
}

unsafe extern "win64" fn hle_find_first_file_a(
    lp_pattern: *const c_char,
    lp_find_data: *mut Win32FindDataA,
) -> Handle {
    if lp_pattern.is_null() || lp_find_data.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    let Some(rt) = current_runtime() else {
        return INVALID_HANDLE_VALUE;
    };
    let pat = CStr::from_ptr(lp_pattern).to_string_lossy();
    let entries = rt.vfs.find_guest_files(&pat);
    if entries.is_empty() {
        rt.set_last_error(2);
        return INVALID_HANDLE_VALUE;
    }
    fill_find_data_a(&entries[0], &mut *lp_find_data);
    rt.insert_handle(KernelObject::FindSearch(Arc::new(Mutex::new(
        FindSearchState { entries, cursor: 1 },
    ))))
}

unsafe extern "win64" fn hle_find_first_file_w(
    lp_pattern: *const u16,
    lp_find_data: *mut Win32FindDataW,
) -> Handle {
    if lp_pattern.is_null() || lp_find_data.is_null() {
        return INVALID_HANDLE_VALUE;
    }
    let Some(rt) = current_runtime() else {
        return INVALID_HANDLE_VALUE;
    };
    let pat = read_utf16_cstr(lp_pattern);
    let entries = rt.vfs.find_guest_files(&pat);
    if entries.is_empty() {
        rt.set_last_error(2);
        return INVALID_HANDLE_VALUE;
    }
    fill_find_data_w(&entries[0], &mut *lp_find_data);
    rt.insert_handle(KernelObject::FindSearch(Arc::new(Mutex::new(
        FindSearchState { entries, cursor: 1 },
    ))))
}

unsafe extern "win64" fn hle_find_next_file_a(
    h_find: Handle,
    lp_find_data: *mut Win32FindDataA,
) -> i32 {
    if lp_find_data.is_null() {
        return 0;
    }
    let Some(rt) = current_runtime() else {
        return 0;
    };
    let Some(search) = rt.get_find_search(h_find) else {
        return 0;
    };
    let mut st = search.lock().unwrap();
    if st.cursor >= st.entries.len() {
        rt.set_last_error(18); // ERROR_NO_MORE_FILES
        return 0;
    }
    fill_find_data_a(&st.entries[st.cursor], &mut *lp_find_data);
    st.cursor += 1;
    1
}

unsafe extern "win64" fn hle_find_next_file_w(
    h_find: Handle,
    lp_find_data: *mut Win32FindDataW,
) -> i32 {
    if lp_find_data.is_null() {
        return 0;
    }
    let Some(rt) = current_runtime() else {
        return 0;
    };
    let Some(search) = rt.get_find_search(h_find) else {
        return 0;
    };
    let mut st = search.lock().unwrap();
    if st.cursor >= st.entries.len() {
        rt.set_last_error(18); // ERROR_NO_MORE_FILES
        return 0;
    }
    fill_find_data_w(&st.entries[st.cursor], &mut *lp_find_data);
    st.cursor += 1;
    1
}

unsafe extern "win64" fn hle_find_close(h_find: Handle) -> i32 {
    if let Some(rt) = current_runtime() {
        i32::from(rt.close_handle(h_find))
    } else {
        0
    }
}

unsafe fn read_utf16_cstr(ptr: *const u16) -> String {
    let mut units = Vec::new();
    let mut cur = ptr;
    loop {
        let ch = *cur;
        if ch == 0 {
            break;
        }
        units.push(ch);
        cur = cur.add(1);
    }
    String::from_utf16_lossy(&units)
}

// ---------------------------------------------------------------------------
// Memory, Heap & TLS Slots
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_virtual_alloc(
    _lp_address: *mut c_void,
    dw_size: usize,
    _fl_allocation_type: u32,
    _fl_protect: u32,
) -> *mut c_void {
    if let Some(rt) = current_runtime() {
        let ptr = rt.alloc_virtual(dw_size) as *mut c_void;
        rt.trace(format!("kernelx!VirtualAlloc(size={dw_size}) -> {ptr:?}"));
        ptr
    } else {
        core::ptr::null_mut()
    }
}

unsafe extern "win64" fn hle_virtual_free(
    lp_address: *mut c_void,
    _dw_size: usize,
    _dw_free_type: u32,
) -> i32 {
    if let Some(rt) = current_runtime() {
        return i32::from(rt.free_virtual(lp_address as *mut u8));
    }
    0
}

unsafe extern "win64" fn hle_get_process_heap() -> Handle {
    PROCESS_HEAP_HANDLE
}

unsafe extern "win64" fn hle_heap_alloc(_h_heap: Handle, _dw_flags: u32, dw_bytes: usize) -> *mut c_void {
    current_runtime()
        .map(|rt| rt.alloc_virtual(dw_bytes) as *mut c_void)
        .unwrap_or(core::ptr::null_mut())
}

unsafe extern "win64" fn hle_heap_realloc(
    _h_heap: Handle,
    _dw_flags: u32,
    lp_mem: *mut c_void,
    dw_bytes: usize,
) -> *mut c_void {
    current_runtime()
        .map(|rt| rt.realloc_virtual(lp_mem as *mut u8, dw_bytes) as *mut c_void)
        .unwrap_or(core::ptr::null_mut())
}

unsafe extern "win64" fn hle_heap_free(_h_heap: Handle, _dw_flags: u32, lp_mem: *mut c_void) -> i32 {
    current_runtime()
        .map(|rt| i32::from(rt.free_virtual(lp_mem as *mut u8)))
        .unwrap_or(0)
}

unsafe extern "win64" fn hle_heap_size(_h_heap: Handle, _dw_flags: u32, lp_mem: *mut c_void) -> usize {
    current_runtime()
        .map(|rt| rt.virtual_size(lp_mem as *mut u8))
        .unwrap_or(0)
}

unsafe extern "win64" fn hle_title_memory_status(lp_buffer: *mut TitleMemoryStatus) -> i32 {
    if lp_buffer.is_null() {
        return 0;
    }
    const SERIES_X_TITLE_MEM: u64 = 13_500 * 1024 * 1024;
    let status = &mut *lp_buffer;
    status.dw_length = core::mem::size_of::<TitleMemoryStatus>() as u32;
    status.ull_total_mem = SERIES_X_TITLE_MEM;
    status.ull_avail_mem = SERIES_X_TITLE_MEM - (256 * 1024 * 1024);
    status.ull_title_used = 256 * 1024 * 1024;
    status.ull_title_avail = SERIES_X_TITLE_MEM - (256 * 1024 * 1024);
    if let Some(rt) = current_runtime() {
        rt.trace("kernelx!TitleMemoryStatus() -> Series X 13.5 GiB budget");
    }
    1
}

unsafe extern "win64" fn hle_tls_alloc() -> u32 {
    current_runtime()
        .map(|rt| rt.alloc_tls_slot())
        .unwrap_or(0xFFFF_FFFF)
}

unsafe extern "win64" fn hle_tls_get_value(dw_tls_index: u32) -> *mut c_void {
    GuestRuntime::get_tls_value(dw_tls_index) as *mut c_void
}

unsafe extern "win64" fn hle_tls_set_value(dw_tls_index: u32, lp_tls_value: *mut c_void) -> i32 {
    GuestRuntime::set_tls_value(dw_tls_index, lp_tls_value as usize);
    1
}

unsafe extern "win64" fn hle_tls_free(dw_tls_index: u32) -> i32 {
    GuestRuntime::free_tls_slot(dw_tls_index);
    1
}

// ---------------------------------------------------------------------------
// Dynamic Module Queries (with pseudo-handle owned-DLL resolution for GDK thunks)
// ---------------------------------------------------------------------------

const PSEUDO_HANDLE_BASE: usize = 0x7E00_0000;

fn pseudo_modules() -> &'static Mutex<Vec<String>> {
    static PSEUDOS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    PSEUDOS.get_or_init(|| Mutex::new(Vec::new()))
}

fn owned_pseudo_handle(raw_name: &str) -> Option<Handle> {
    let reg = ModuleRegistry::new_default(false);
    if !reg.owns_dll(raw_name) {
        return None;
    }
    let norm = normalize_dll(raw_name);
    let mut list = pseudo_modules().lock().unwrap();
    if let Some(idx) = list.iter().position(|d| d == &norm) {
        return Some((PSEUDO_HANDLE_BASE + idx * 16) as Handle);
    }
    let idx = list.len();
    list.push(norm);
    Some((PSEUDO_HANDLE_BASE + idx * 16) as Handle)
}

fn lookup_pseudo_dll(h_module: Handle) -> Option<String> {
    let raw = h_module as usize;
    if raw < PSEUDO_HANDLE_BASE || !(raw - PSEUDO_HANDLE_BASE).is_multiple_of(16) {
        return None;
    }
    let idx = (raw - PSEUDO_HANDLE_BASE) / 16;
    pseudo_modules().lock().unwrap().get(idx).cloned()
}

fn get_module_handle_impl(name_opt: Option<&str>) -> Handle {
    if let Some(name) = name_opt {
        if let Some(ph) = owned_pseudo_handle(name) {
            if let Some(rt) = current_runtime() {
                rt.trace(format!("kernelx!GetModuleHandle({name:?}) -> pseudo {ph:?}"));
            }
            return ph;
        }
    }
    let Some(rt) = current_runtime() else {
        return core::ptr::null_mut();
    };
    let Some(mods_arc) = rt.modules() else {
        return core::ptr::null_mut();
    };
    let mods = mods_arc.lock().unwrap();
    let handle = mods
        .get_module(name_opt)
        .map(|m| m.base_addr() as Handle)
        .unwrap_or(core::ptr::null_mut());
    rt.trace(format!("kernelx!GetModuleHandle({name_opt:?}) -> {handle:?}"));
    handle
}

fn load_library_impl(name: &str) -> Handle {
    if let Some(ph) = owned_pseudo_handle(name) {
        if let Some(rt) = current_runtime() {
            rt.trace(format!("kernelx!LoadLibrary({name:?}) -> pseudo {ph:?}"));
        }
        return ph;
    }
    let Some(rt) = current_runtime() else {
        return core::ptr::null_mut();
    };
    let Some(mods_arc) = rt.modules() else {
        return core::ptr::null_mut();
    };
    let reg = ModuleRegistry::new_default(false);
    let mut mods = mods_arc.lock().unwrap();
    match unsafe { mods.load_library(name, &reg) } {
        Ok(img) => {
            let h = img.base_addr() as Handle;
            rt.trace(format!("kernelx!LoadLibrary({name:?}) -> {h:?}"));
            h
        }
        Err(e) => {
            rt.trace(format!("kernelx!LoadLibrary({name:?}) FAILED: {e}"));
            core::ptr::null_mut()
        }
    }
}

unsafe extern "win64" fn hle_get_module_handle_a(lp_module_name: *const c_char) -> Handle {
    let name_opt = if lp_module_name.is_null() {
        None
    } else {
        Some(CStr::from_ptr(lp_module_name).to_string_lossy().into_owned())
    };
    get_module_handle_impl(name_opt.as_deref())
}

unsafe extern "win64" fn hle_get_module_handle_w(lp_module_name: *const u16) -> Handle {
    let name_opt = if lp_module_name.is_null() {
        None
    } else {
        Some(read_utf16_cstr(lp_module_name))
    };
    get_module_handle_impl(name_opt.as_deref())
}

unsafe extern "win64" fn hle_get_proc_address(
    h_module: Handle,
    lp_proc_name: *const c_char,
) -> *mut c_void {
    if h_module.is_null() || lp_proc_name.is_null() {
        return core::ptr::null_mut();
    }
    let raw = lp_proc_name as usize;
    let symbol = if raw <= 0xFFFF {
        ImportSymbol::Ordinal(raw as u16)
    } else {
        ImportSymbol::Name {
            hint: 0,
            name: CStr::from_ptr(lp_proc_name).to_string_lossy().into_owned(),
        }
    };

    // 1. Check HLE-owned pseudo-handles first (e.g. xgameruntime.dll, d3d12_xs.dll, xmem.dll)
    if let Some(dll_name) = lookup_pseudo_dll(h_module) {
        let reg = ModuleRegistry::new_default(false);
        let addr = match &symbol {
            ImportSymbol::Name { name, .. } => reg.lookup_owned_export(&dll_name, name).unwrap_or(0),
            ImportSymbol::Ordinal(_) => 0,
        };
        if let Some(rt) = current_runtime() {
            if addr == 0 {
                rt.set_last_error(127); // ERROR_PROC_NOT_FOUND
            }
            rt.trace(format!(
                "kernelx!GetProcAddress(pseudo:{dll_name}, {symbol}) -> 0x{addr:X}"
            ));
        }
        return addr as *mut c_void;
    }

    // 2. Otherwise search loaded guest PE modules
    let Some(rt) = current_runtime() else {
        return core::ptr::null_mut();
    };
    let Some(mods_arc) = rt.modules() else {
        return core::ptr::null_mut();
    };
    let mods = mods_arc.lock().unwrap();
    let addr = mods
        .resolve_proc_address(h_module as usize, &symbol)
        .unwrap_or(0);
    if addr == 0 {
        rt.set_last_error(127); // ERROR_PROC_NOT_FOUND
    }
    rt.trace(format!(
        "kernelx!GetProcAddress({h_module:?}, {symbol}) -> 0x{addr:X}"
    ));
    addr as *mut c_void
}

unsafe extern "win64" fn hle_load_library_a(lp_lib_file_name: *const c_char) -> Handle {
    if lp_lib_file_name.is_null() {
        return core::ptr::null_mut();
    }
    let name = CStr::from_ptr(lp_lib_file_name).to_string_lossy().into_owned();
    load_library_impl(&name)
}

unsafe extern "win64" fn hle_load_library_w(lp_lib_file_name: *const u16) -> Handle {
    if lp_lib_file_name.is_null() {
        return core::ptr::null_mut();
    }
    let name = read_utf16_cstr(lp_lib_file_name);
    load_library_impl(&name)
}

unsafe extern "win64" fn hle_load_library_ex_a(
    lp_lib_file_name: *const c_char,
    _h_file: Handle,
    _dw_flags: u32,
) -> Handle {
    hle_load_library_a(lp_lib_file_name)
}

unsafe extern "win64" fn hle_load_library_ex_w(
    lp_lib_file_name: *const u16,
    _h_file: Handle,
    _dw_flags: u32,
) -> Handle {
    hle_load_library_w(lp_lib_file_name)
}

unsafe extern "win64" fn hle_free_library(h_module: Handle) -> i32 {
    if h_module.is_null() {
        return 0;
    }
    // Owned pseudo-handles and loaded guest modules remain live until session teardown
    1
}

// ---------------------------------------------------------------------------
// Real-Time Clock (RTC) & System Info
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_get_system_time(lp_system_time: *mut SystemTime) {
    if lp_system_time.is_null() {
        return;
    }
    let st = current_runtime()
        .map(|rt| {
            let st = rt.rtc().now_systemtime_utc();
            rt.trace(format!(
                "kernelx!GetSystemTime() -> {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                st.w_year, st.w_month, st.w_day, st.w_hour, st.w_minute, st.w_second
            ));
            st
        })
        .unwrap_or_default();
    *lp_system_time = st;
}

unsafe extern "win64" fn hle_get_local_time(lp_system_time: *mut SystemTime) {
    if lp_system_time.is_null() {
        return;
    }
    let st = current_runtime()
        .map(|rt| rt.rtc().now_systemtime_local())
        .unwrap_or_default();
    *lp_system_time = st;
}

unsafe extern "win64" fn hle_get_system_time_as_file_time(lp_file_time: *mut FileTime) {
    if lp_file_time.is_null() {
        return;
    }
    let ft = current_runtime()
        .map(|rt| rt.rtc().now_filetime_utc())
        .unwrap_or_default();
    *lp_file_time = ft;
}

unsafe extern "win64" fn hle_system_time_to_file_time(
    lp_system_time: *const SystemTime,
    lp_file_time: *mut FileTime,
) -> i32 {
    if lp_system_time.is_null() || lp_file_time.is_null() {
        return 0;
    }
    match systemtime_to_filetime(*lp_system_time) {
        Some(ft) => {
            *lp_file_time = ft;
            1
        }
        None => 0,
    }
}

unsafe extern "win64" fn hle_file_time_to_system_time(
    lp_file_time: *const FileTime,
    lp_system_time: *mut SystemTime,
) -> i32 {
    if lp_file_time.is_null() || lp_system_time.is_null() {
        return 0;
    }
    *lp_system_time = filetime_to_systemtime(*lp_file_time);
    1
}

unsafe extern "win64" fn hle_get_time_zone_information(lp_tzi: *mut TimeZoneInformation) -> u32 {
    if lp_tzi.is_null() {
        return 0xFFFF_FFFF;
    }
    *lp_tzi = current_runtime()
        .map(|rt| rt.rtc().timezone_info())
        .unwrap_or_default();
    1 // TIME_ZONE_ID_STANDARD
}

unsafe extern "win64" fn hle_get_system_info(lp_system_info: *mut SystemInfo) {
    if lp_system_info.is_null() {
        return;
    }
    // Report Xbox Series X AMD Zen 2 8-core / 16-thread topology
    *lp_system_info = SystemInfo {
        w_processor_architecture: PROCESSOR_ARCHITECTURE_AMD64,
        w_reserved: 0,
        dw_page_size: 4096,
        lp_minimum_application_address: 0x0001_0000,
        lp_maximum_application_address: 0x0000_7FFF_FFFE_FFFF,
        dw_active_processor_mask: 0x00FF,
        dw_number_of_processors: 8,
        dw_processor_type: 8664,
        dw_allocation_granularity: 65536,
        w_processor_level: 23, // AMD Zen 2 Family 17h
        w_processor_revision: 0x4700,
    };
}

unsafe extern "win64" fn hle_is_processor_feature_present(feature: u32) -> i32 {
    // Report standard x86_64 / Zen 2 features (SSE2, SSE3, SSE4.1, SSE4.2, AVX, AVX2, XMMI64, RDTSC)
    match feature {
        6 | 10 | 12 | 13 | 14 | 17 | 36 | 37 | 38 | 39 | 40 => 1,
        _ => 0,
    }
}

unsafe extern "win64" fn hle_flush_instruction_cache(
    _h_process: Handle,
    _lp_base_address: *const c_void,
    _dw_size: usize,
) -> i32 {
    1
}

// ---------------------------------------------------------------------------
// Extended Win32/Xbox Startup, Synchronization, Condvar, FLS & Encoding
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_get_current_process() -> Handle {
    (!0usize) as Handle
}

unsafe extern "win64" fn hle_get_current_thread() -> Handle {
    (!1usize) as Handle
}

unsafe extern "win64" fn hle_get_module_file_name_a(
    _h_mod: Handle,
    buf: *mut u8,
    size: u32,
) -> u32 {
    if buf.is_null() || size == 0 {
        return 0;
    }
    let name = b"G:\\xsxemu_guest.exe";
    let n = name.len().min((size as usize).saturating_sub(1));
    core::ptr::copy_nonoverlapping(name.as_ptr(), buf, n);
    *buf.add(n) = 0;
    n as u32
}

unsafe extern "win64" fn hle_get_module_file_name_w(
    _h_mod: Handle,
    buf: *mut u16,
    size: u32,
) -> u32 {
    if buf.is_null() || size == 0 {
        return 0;
    }
    let units: Vec<u16> = "G:\\xsxemu_guest.exe".encode_utf16().collect();
    let n = units.len().min((size as usize).saturating_sub(1));
    core::ptr::copy_nonoverlapping(units.as_ptr(), buf, n);
    *buf.add(n) = 0;
    n as u32
}

unsafe extern "win64" fn hle_get_startup_info_w(info: *mut u8) {
    if !info.is_null() {
        core::ptr::write_bytes(info, 0, 104); // sizeof(STARTUPINFOW) on Win64
        *(info as *mut u32) = 104;
    }
}

unsafe extern "win64" fn hle_get_std_handle(std_id: u32) -> Handle {
    match std_id {
        0xFFFF_FFF6 => 0x1001usize as Handle, // STD_INPUT_HANDLE
        0xFFFF_FFF5 => 0x1002usize as Handle, // STD_OUTPUT_HANDLE
        0xFFFF_FFF4 => 0x1003usize as Handle, // STD_ERROR_HANDLE
        _ => INVALID_HANDLE_VALUE,
    }
}

unsafe extern "win64" fn hle_get_file_type(_h: Handle) -> u32 {
    2 // FILE_TYPE_CHAR
}

unsafe extern "win64" fn hle_get_console_mode(_h: Handle, mode: *mut u32) -> i32 {
    if !mode.is_null() {
        *mode = 0x0007;
    }
    1
}

unsafe extern "win64" fn hle_get_environment_strings_w() -> *mut u16 {
    static mut ENV_BLOCK: [u16; 2] = [0, 0];
    core::ptr::addr_of_mut!(ENV_BLOCK) as *mut u16
}

unsafe extern "win64" fn hle_free_environment_strings_w(_env: *mut u16) -> i32 {
    1
}

unsafe extern "win64" fn hle_init_cs_spin(cs: *mut CriticalSection, _spin: u32) -> i32 {
    hle_initialize_critical_section(cs);
    1
}

unsafe extern "win64" fn hle_init_cs_ex(cs: *mut CriticalSection, _spin: u32, _flags: u32) -> i32 {
    hle_initialize_critical_section(cs);
    1
}

unsafe extern "win64" fn hle_try_enter_cs(cs: *mut CriticalSection) -> i32 {
    if cs.is_null() {
        return 0;
    }
    hle_enter_critical_section(cs);
    1
}

unsafe extern "win64" fn hle_create_event_w(
    sec: *mut c_void,
    manual_reset: i32,
    initial_state: i32,
    _name: *const u16,
) -> Handle {
    hle_create_event_a(sec, manual_reset, initial_state, core::ptr::null())
}

unsafe extern "win64" fn hle_wait_for_multiple_objects(
    count: u32,
    handles: *const Handle,
    wait_all: i32,
    timeout_ms: u32,
) -> u32 {
    if count == 0 || handles.is_null() {
        return WAIT_FAILED;
    }
    let slice = core::slice::from_raw_parts(handles, count as usize);
    if wait_all != 0 {
        for &h in slice {
            let rc = hle_wait_for_single_object(h, timeout_ms);
            if rc != 0 {
                return rc;
            }
        }
        0
    } else {
        hle_wait_for_single_object(slice[0], timeout_ms)
    }
}

fn condvar_mutex() -> &'static (Mutex<u64>, Condvar) {
    static CV: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();
    CV.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

unsafe extern "win64" fn hle_init_condvar(cv: *mut usize) {
    if !cv.is_null() {
        *cv = 0;
    }
}

unsafe extern "win64" fn hle_wake_condvar(_cv: *mut usize) {
    let (m, c) = condvar_mutex();
    if let Ok(mut g) = m.lock() {
        *g = g.wrapping_add(1);
        c.notify_one();
    }
}

unsafe extern "win64" fn hle_wake_all_condvar(_cv: *mut usize) {
    let (m, c) = condvar_mutex();
    if let Ok(mut g) = m.lock() {
        *g = g.wrapping_add(1);
        c.notify_all();
    }
}

unsafe extern "win64" fn hle_sleep_condvar_cs(
    _cv: *mut usize,
    cs: *mut CriticalSection,
    timeout_ms: u32,
) -> i32 {
    hle_leave_critical_section(cs);
    let (m, c) = condvar_mutex();
    if let Ok(g) = m.lock() {
        let ms = if timeout_ms == 0xFFFF_FFFF { 1 } else { timeout_ms.min(50) };
        let _ = c.wait_timeout(g, Duration::from_millis(ms as u64));
    }
    hle_enter_critical_section(cs);
    1
}

unsafe extern "win64" fn hle_sleep_condvar_srw(
    _cv: *mut usize,
    srw: *mut SrwLock,
    timeout_ms: u32,
    flags: u32,
) -> i32 {
    let shared = (flags & 1) != 0;
    if shared {
        hle_release_srw_lock_shared(srw);
    } else {
        hle_release_srw_lock_exclusive(srw);
    }
    let (m, c) = condvar_mutex();
    if let Ok(g) = m.lock() {
        let ms = if timeout_ms == 0xFFFF_FFFF { 1 } else { timeout_ms.min(50) };
        let _ = c.wait_timeout(g, Duration::from_millis(ms as u64));
    }
    if shared {
        hle_acquire_srw_lock_shared(srw);
    } else {
        hle_acquire_srw_lock_exclusive(srw);
    }
    1
}

unsafe extern "win64" fn hle_init_once_execute_once(
    init_once: *mut usize,
    init_fn: unsafe extern "win64" fn(*mut usize, *mut c_void, *mut *mut c_void) -> i32,
    param: *mut c_void,
    ctx: *mut *mut c_void,
) -> i32 {
    if init_once.is_null() {
        return 0;
    }
    if *init_once == 0 {
        let ok = init_fn(init_once, param, ctx);
        if ok != 0 {
            *init_once = 2; // RTL_RUN_ONCE_CHECK_ONLY / completed
        }
        ok
    } else {
        1
    }
}

unsafe extern "win64" fn hle_switch_to_thread() -> i32 {
    thread::yield_now();
    1
}

unsafe extern "win64" fn hle_get_current_processor_number() -> u32 {
    0
}

unsafe extern "win64" fn hle_return_one_i32() -> i32 {
    1
}

unsafe extern "win64" fn hle_return_zero_i32() -> i32 {
    0
}

unsafe extern "win64" fn hle_set_thread_affinity_mask(_t: Handle, mask: usize) -> usize {
    mask.max(1)
}

unsafe extern "win64" fn hle_multi_byte_to_wide_char(
    _cp: u32,
    _flags: u32,
    src: *const u8,
    src_len: i32,
    dst: *mut u16,
    dst_len: i32,
) -> i32 {
    if src.is_null() {
        return 0;
    }
    let bytes = if src_len < 0 {
        let mut n = 0usize;
        while *src.add(n) != 0 {
            n += 1;
        }
        core::slice::from_raw_parts(src, n + 1)
    } else {
        core::slice::from_raw_parts(src, src_len as usize)
    };
    let s = String::from_utf8_lossy(bytes);
    let units: Vec<u16> = s.encode_utf16().collect();
    if dst.is_null() || dst_len == 0 {
        return units.len() as i32;
    }
    let copy = units.len().min(dst_len as usize);
    core::ptr::copy_nonoverlapping(units.as_ptr(), dst, copy);
    copy as i32
}

unsafe extern "win64" fn hle_wide_char_to_multi_byte(
    _cp: u32,
    _flags: u32,
    src: *const u16,
    src_len: i32,
    dst: *mut u8,
    dst_len: i32,
    _def: *const u8,
    _used_def: *mut i32,
) -> i32 {
    if src.is_null() {
        return 0;
    }
    let units = if src_len < 0 {
        let mut n = 0usize;
        while *src.add(n) != 0 {
            n += 1;
        }
        core::slice::from_raw_parts(src, n + 1)
    } else {
        core::slice::from_raw_parts(src, src_len as usize)
    };
    let s = String::from_utf16_lossy(units);
    let bytes = s.as_bytes();
    if dst.is_null() || dst_len == 0 {
        return bytes.len() as i32;
    }
    let copy = bytes.len().min(dst_len as usize);
    core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, copy);
    copy as i32
}

unsafe extern "win64" fn hle_get_acp() -> u32 {
    65001 // UTF-8
}

unsafe extern "win64" fn hle_encode_decode_pointer(ptr: usize) -> usize {
    ptr
}

unsafe extern "win64" fn hle_set_unhandled_exception_filter(_f: usize) -> usize {
    0
}

unsafe extern "win64" fn hle_fls_alloc(_cb: usize) -> u32 {
    hle_tls_alloc()
}

unsafe extern "win64" fn hle_virtual_protect(
    _addr: *mut c_void,
    _size: usize,
    _new_prot: u32,
    old_prot: *mut u32,
) -> i32 {
    if !old_prot.is_null() {
        *old_prot = 0x40; // PAGE_EXECUTE_READWRITE
    }
    1
}

#[repr(C)]
struct MemoryStatusEx {
    dw_length: u32,
    dw_memory_load: u32,
    ull_total_phys: u64,
    ull_avail_phys: u64,
    ull_total_page_file: u64,
    ull_avail_page_file: u64,
    ull_total_virtual: u64,
    ull_avail_virtual: u64,
    ull_avail_extended_virtual: u64,
}

unsafe extern "win64" fn hle_global_memory_status_ex(ms: *mut MemoryStatusEx) -> i32 {
    if ms.is_null() {
        return 0;
    }
    let m = &mut *ms;
    m.dw_memory_load = 25;
    m.ull_total_phys = 16 * 1024 * 1024 * 1024;
    m.ull_avail_phys = 12 * 1024 * 1024 * 1024;
    m.ull_total_page_file = 16 * 1024 * 1024 * 1024;
    m.ull_avail_page_file = 12 * 1024 * 1024 * 1024;
    m.ull_total_virtual = 0x0000_7FFF_FFFF_0000;
    m.ull_avail_virtual = 0x0000_7FF0_0000_0000;
    m.ull_avail_extended_virtual = 0;
    1
}
