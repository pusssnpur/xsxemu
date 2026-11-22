//! `kernelx.dll` / `kernelbase.dll` / `kernel32.dll` HLE thunks.

use core::ffi::{c_char, c_void};
use std::ffi::CStr;
use std::time::Duration;

use xsx_abi::gdk::{Handle, TitleMemoryStatus, WAIT_FAILED, WAIT_OBJECT_0};

use crate::registry::ModuleRegistry;
use crate::runtime::{current_runtime, KernelObject};

pub fn register(reg: &mut ModuleRegistry) {
    reg.register_export("kernelx.dll", "OutputDebugStringA", hle_output_debug_string_a as *const () as usize);
    reg.register_export("kernelx.dll", "QueryPerformanceCounter", hle_query_performance_counter as *const () as usize);
    reg.register_export("kernelx.dll", "QueryPerformanceFrequency", hle_query_performance_frequency as *const () as usize);
    reg.register_export("kernelx.dll", "GetTickCount64", hle_get_tick_count_64 as *const () as usize);
    reg.register_export("kernelx.dll", "Sleep", hle_sleep as *const () as usize);
    reg.register_export("kernelx.dll", "GetLastError", hle_get_last_error as *const () as usize);
    reg.register_export("kernelx.dll", "SetLastError", hle_set_last_error as *const () as usize);
    reg.register_export("kernelx.dll", "GetCurrentProcessId", hle_get_current_process_id as *const () as usize);
    reg.register_export("kernelx.dll", "GetCurrentThreadId", hle_get_current_thread_id as *const () as usize);
    reg.register_export("kernelx.dll", "CreateEventA", hle_create_event_a as *const () as usize);
    reg.register_export("kernelx.dll", "SetEvent", hle_set_event as *const () as usize);
    reg.register_export("kernelx.dll", "ResetEvent", hle_reset_event as *const () as usize);
    reg.register_export("kernelx.dll", "WaitForSingleObject", hle_wait_for_single_object as *const () as usize);
    reg.register_export("kernelx.dll", "CloseHandle", hle_close_handle as *const () as usize);
    reg.register_export("kernelx.dll", "VirtualAlloc", hle_virtual_alloc as *const () as usize);
    reg.register_export("kernelx.dll", "VirtualFree", hle_virtual_free as *const () as usize);
    reg.register_export("kernelx.dll", "TitleMemoryStatus", hle_title_memory_status as *const () as usize);
}

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
    *lp_freq = 1_000_000; // 1 MHz microsecond counter
    1
}

unsafe extern "win64" fn hle_get_tick_count_64() -> u64 {
    current_runtime().map(|rt| rt.elapsed_ms()).unwrap_or(0)
}

unsafe extern "win64" fn hle_sleep(dw_milliseconds: u32) {
    if dw_milliseconds > 0 {
        std::thread::sleep(Duration::from_millis(dw_milliseconds as u64));
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
    1
}

unsafe extern "win64" fn hle_create_event_a(
    _lp_attrs: *mut c_void,
    b_manual_reset: i32,
    b_initial_state: i32,
    _lp_name: *const c_char,
) -> Handle {
    if let Some(rt) = current_runtime() {
        let h = rt.insert_handle(KernelObject::Event {
            manual_reset: b_manual_reset != 0,
            signaled: b_initial_state != 0,
        });
        rt.trace(format!("kernelx!CreateEventA() -> {h:?}"));
        h
    } else {
        core::ptr::null_mut()
    }
}

unsafe extern "win64" fn hle_set_event(h_event: Handle) -> i32 {
    if let Some(rt) = current_runtime() {
        let ok = rt
            .with_handle_mut(h_event, |obj| {
                if let KernelObject::Event { signaled, .. } = obj {
                    *signaled = true;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        return i32::from(ok);
    }
    0
}

unsafe extern "win64" fn hle_reset_event(h_event: Handle) -> i32 {
    if let Some(rt) = current_runtime() {
        let ok = rt
            .with_handle_mut(h_event, |obj| {
                if let KernelObject::Event { signaled, .. } = obj {
                    *signaled = false;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        return i32::from(ok);
    }
    0
}

unsafe extern "win64" fn hle_wait_for_single_object(h_handle: Handle, _dw_ms: u32) -> u32 {
    if let Some(rt) = current_runtime() {
        let found = rt
            .with_handle_mut(h_handle, |obj| {
                if let KernelObject::Event {
                    manual_reset,
                    signaled,
                } = obj
                {
                    if !*manual_reset {
                        *signaled = false;
                    }
                }
            })
            .is_some();
        if found {
            return WAIT_OBJECT_0;
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

unsafe extern "win64" fn hle_title_memory_status(lp_buffer: *mut TitleMemoryStatus) -> i32 {
    if lp_buffer.is_null() {
        return 0;
    }
    // Xbox Series X exposes ~13.5 GiB title-accessible unified GDDR6 memory.
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
