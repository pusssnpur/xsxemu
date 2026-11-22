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
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use xsx_abi::gdk::{
    CriticalSection, FileTime, Handle, SrwLock, SystemInfo, SystemTime, ThreadStartRoutine,
    TimeZoneInformation, TitleMemoryStatus, CREATE_ALWAYS, CREATE_NEW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_BEGIN, FILE_CURRENT, FILE_END, GENERIC_READ, GENERIC_WRITE,
    INVALID_FILE_ATTRIBUTES, INVALID_HANDLE_VALUE, OPEN_ALWAYS, OPEN_EXISTING,
    PROCESSOR_ARCHITECTURE_AMD64, TRUNCATE_EXISTING, WAIT_FAILED,
};
use xsx_loader::ImportSymbol;

use crate::registry::ModuleRegistry;
use crate::rtc::{filetime_to_systemtime, systemtime_to_filetime};
use crate::runtime::{current_runtime, EventState, GuestRuntime, KernelObject, ThreadState};

const PROCESS_HEAP_HANDLE: Handle = 0xDEAD_BEE0usize as Handle;

pub fn register(reg: &mut ModuleRegistry) {
    // Diagnostics, timing & Real-Time Clock (RTC)
    reg.register_export("kernelx.dll", "OutputDebugStringA", hle_output_debug_string_a as *const () as usize);
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

    // File I/O (Virtual Xbox G:\, T:\, D:\, S:\)
    reg.register_export("kernelx.dll", "CreateFileA", hle_create_file_a as *const () as usize);
    reg.register_export("kernelx.dll", "CreateFileW", hle_create_file_w as *const () as usize);
    reg.register_export("kernelx.dll", "ReadFile", hle_read_file as *const () as usize);
    reg.register_export("kernelx.dll", "WriteFile", hle_write_file as *const () as usize);
    reg.register_export("kernelx.dll", "SetFilePointerEx", hle_set_file_pointer_ex as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileSizeEx", hle_get_file_size_ex as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileAttributesA", hle_get_file_attributes_a as *const () as usize);
    reg.register_export("kernelx.dll", "GetFileAttributesW", hle_get_file_attributes_w as *const () as usize);

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

    // Runtime module queries
    reg.register_export("kernelx.dll", "GetModuleHandleA", hle_get_module_handle_a as *const () as usize);
    reg.register_export("kernelx.dll", "GetProcAddress", hle_get_proc_address as *const () as usize);
    reg.register_export("kernelx.dll", "LoadLibraryA", hle_load_library_a as *const () as usize);
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
    let Some(host_path) = rt.vfs.resolve_guest_path(guest_path) else {
        rt.set_last_error(3); // ERROR_PATH_NOT_FOUND
        return INVALID_HANDLE_VALUE;
    };

    if let Some(parent) = host_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let read = (access & GENERIC_READ) != 0 || access == 0;
    let write = (access & GENERIC_WRITE) != 0;
    let mut opts = OpenOptions::new();
    opts.read(read).write(write);

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
    let Some(host_path) = rt.vfs.resolve_guest_path(path) else {
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
// Dynamic Module Queries
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_get_module_handle_a(lp_module_name: *const c_char) -> Handle {
    let Some(rt) = current_runtime() else {
        return core::ptr::null_mut();
    };
    let Some(mods_arc) = rt.modules() else {
        return core::ptr::null_mut();
    };
    let name_opt = if lp_module_name.is_null() {
        None
    } else {
        Some(CStr::from_ptr(lp_module_name).to_string_lossy().into_owned())
    };
    let mods = mods_arc.lock().unwrap();
    let handle = mods
        .get_module(name_opt.as_deref())
        .map(|m| m.base_addr() as Handle)
        .unwrap_or(core::ptr::null_mut());
    rt.trace(format!("kernelx!GetModuleHandleA({name_opt:?}) -> {handle:?}"));
    handle
}

unsafe extern "win64" fn hle_get_proc_address(
    h_module: Handle,
    lp_proc_name: *const c_char,
) -> *mut c_void {
    if h_module.is_null() || lp_proc_name.is_null() {
        return core::ptr::null_mut();
    }
    let Some(rt) = current_runtime() else {
        return core::ptr::null_mut();
    };
    let Some(mods_arc) = rt.modules() else {
        return core::ptr::null_mut();
    };
    let raw = lp_proc_name as usize;
    let symbol = if raw <= 0xFFFF {
        ImportSymbol::Ordinal(raw as u16)
    } else {
        ImportSymbol::Name {
            hint: 0,
            name: CStr::from_ptr(lp_proc_name).to_string_lossy().into_owned(),
        }
    };
    let mods = mods_arc.lock().unwrap();
    let addr = mods
        .resolve_proc_address(h_module as usize, &symbol)
        .unwrap_or(0);
    rt.trace(format!(
        "kernelx!GetProcAddress({h_module:?}, {symbol}) -> 0x{addr:X}"
    ));
    addr as *mut c_void
}

unsafe extern "win64" fn hle_load_library_a(lp_lib_file_name: *const c_char) -> Handle {
    if lp_lib_file_name.is_null() {
        return core::ptr::null_mut();
    }
    let Some(rt) = current_runtime() else {
        return core::ptr::null_mut();
    };
    let Some(mods_arc) = rt.modules() else {
        return core::ptr::null_mut();
    };
    let name = CStr::from_ptr(lp_lib_file_name).to_string_lossy().into_owned();
    let reg = ModuleRegistry::new_default(false);
    let mut mods = mods_arc.lock().unwrap();
    match mods.load_library(&name, &reg) {
        Ok(img) => {
            let h = img.base_addr() as Handle;
            rt.trace(format!("kernelx!LoadLibraryA({name:?}) -> {h:?}"));
            h
        }
        Err(e) => {
            rt.trace(format!("kernelx!LoadLibraryA({name:?}) FAILED: {e}"));
            core::ptr::null_mut()
        }
    }
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
