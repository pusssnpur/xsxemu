//! Clean-room MSVC C/C++ Runtime HLE (`ucrtbase.dll`, `vcruntime140.dll`, `vcruntime140_1.dll`).
//!
//! Enables real MSVC-compiled Xbox Series X|S PE32+ executables to run CRT startup
//! (`_initterm`, `_configure_*_argv`, `atexit` tables), heap allocation (`malloc`/`_aligned_malloc`),
//! string/memory primitives, and C++/SEH runtime helpers cleanly inside `xsxemu`.

use core::ffi::{c_char, c_void};
use std::alloc::{alloc, alloc_zeroed, dealloc, realloc, Layout};
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::process;
use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

#[derive(Clone, Copy)]
struct AllocRecord {
    layout: Layout,
    /// Original user pointer returned by `_aligned_malloc` (same as base here).
    user_size: usize,
}

fn crt_allocs() -> &'static Mutex<HashMap<usize, AllocRecord>> {
    static MAP: OnceLock<Mutex<HashMap<usize, AllocRecord>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn onexit_funcs() -> &'static Mutex<Vec<unsafe extern "win64" fn() -> i32>> {
    static LIST: OnceLock<Mutex<Vec<unsafe extern "win64" fn() -> i32>>> = OnceLock::new();
    LIST.get_or_init(|| Mutex::new(Vec::new()))
}

static APP_TYPE: AtomicI32 = AtomicI32::new(2);
static NEW_MODE: AtomicI32 = AtomicI32::new(0);
static RAND_STATE: AtomicU32 = AtomicU32::new(1);

pub fn register(reg: &mut ModuleRegistry) {
    // ucrtbase.dll (and api-ms-win-crt-* forwarded here by normalize_dll)
    let u = "ucrtbase.dll";
    reg.register_export(u, "_initterm", hle_initterm as *const () as usize);
    reg.register_export(u, "_initterm_e", hle_initterm_e as *const () as usize);
    reg.register_export(u, "_initialize_onexit_table", hle_init_onexit_table as *const () as usize);
    reg.register_export(u, "_register_onexit_function", hle_register_onexit_function as *const () as usize);
    reg.register_export(u, "_execute_onexit_table", hle_execute_onexit_table as *const () as usize);
    reg.register_export(u, "_crt_atexit", hle_crt_atexit as *const () as usize);
    reg.register_export(u, "atexit", hle_crt_atexit as *const () as usize);
    reg.register_export(u, "exit", hle_exit as *const () as usize);
    reg.register_export(u, "_exit", hle_exit as *const () as usize);
    reg.register_export(u, "_cexit", hle_noop as *const () as usize);
    reg.register_export(u, "_c_exit", hle_noop as *const () as usize);
    reg.register_export(u, "abort", hle_abort as *const () as usize);
    reg.register_export(u, "terminate", hle_abort as *const () as usize);

    reg.register_export(u, "_configure_narrow_argv", hle_return_zero as *const () as usize);
    reg.register_export(u, "_configure_wide_argv", hle_return_zero as *const () as usize);
    reg.register_export(u, "_initialize_narrow_environment", hle_return_zero as *const () as usize);
    reg.register_export(u, "_initialize_wide_environment", hle_return_zero as *const () as usize);
    reg.register_export(u, "_get_initial_narrow_environment", hle_get_initial_narrow_env as *const () as usize);
    reg.register_export(u, "_get_initial_wide_environment", hle_get_initial_wide_env as *const () as usize);
    reg.register_export(u, "__p___argc", hle_p_argc as *const () as usize);
    reg.register_export(u, "__p___argv", hle_p_argv as *const () as usize);
    reg.register_export(u, "__p___wargv", hle_p_wargv as *const () as usize);
    reg.register_export(u, "_set_app_type", hle_set_app_type as *const () as usize);
    reg.register_export(u, "_set_new_mode", hle_set_new_mode as *const () as usize);
    reg.register_export(u, "__setusermatherr", hle_noop as *const () as usize);
    reg.register_export(u, "_controlfp_s", hle_return_zero as *const () as usize);
    reg.register_export(u, "_seh_filter_exe", hle_return_zero as *const () as usize);
    reg.register_export(u, "_set_fmode", hle_return_zero as *const () as usize);
    reg.register_export(u, "_errno", hle_errno as *const () as usize);

    // Heap / allocation
    reg.register_export(u, "malloc", hle_malloc as *const () as usize);
    reg.register_export(u, "calloc", hle_calloc as *const () as usize);
    reg.register_export(u, "realloc", hle_realloc as *const () as usize);
    reg.register_export(u, "free", hle_free as *const () as usize);
    reg.register_export(u, "_aligned_malloc", hle_aligned_malloc as *const () as usize);
    reg.register_export(u, "_aligned_free", hle_free as *const () as usize);
    reg.register_export(u, "_msize", hle_msize as *const () as usize);

    // Memory & string
    reg.register_export(u, "memcpy", hle_memcpy as *const () as usize);
    reg.register_export(u, "memmove", hle_memmove as *const () as usize);
    reg.register_export(u, "memset", hle_memset as *const () as usize);
    reg.register_export(u, "memcmp", hle_memcmp as *const () as usize);
    reg.register_export(u, "strlen", hle_strlen as *const () as usize);
    reg.register_export(u, "wcslen", hle_wcslen as *const () as usize);
    reg.register_export(u, "strcmp", hle_strcmp as *const () as usize);
    reg.register_export(u, "strncmp", hle_strncmp as *const () as usize);
    reg.register_export(u, "_stricmp", hle_stricmp as *const () as usize);
    reg.register_export(u, "wcscmp", hle_wcscmp as *const () as usize);
    reg.register_export(u, "_wcsicmp", hle_wcsicmp as *const () as usize);
    reg.register_export(u, "strcpy_s", hle_strcpy_s as *const () as usize);
    reg.register_export(u, "wcscpy_s", hle_wcscpy_s as *const () as usize);

    // Misc CRT
    reg.register_export(u, "srand", hle_srand as *const () as usize);
    reg.register_export(u, "rand", hle_rand as *const () as usize);
    reg.register_export(u, "_time64", hle_time64 as *const () as usize);
    reg.register_export(u, "__stdio_common_vsprintf", hle_stdio_common_vsprintf as *const () as usize);

    // vcruntime140.dll & vcruntime140_1.dll
    for v in ["vcruntime140.dll", "vcruntime140_1.dll"] {
        reg.register_export(v, "memcpy", hle_memcpy as *const () as usize);
        reg.register_export(v, "memmove", hle_memmove as *const () as usize);
        reg.register_export(v, "memset", hle_memset as *const () as usize);
        reg.register_export(v, "memcmp", hle_memcmp as *const () as usize);
        reg.register_export(v, "__C_specific_handler", hle_c_specific_handler as *const () as usize);
        reg.register_export(v, "__CxxFrameHandler4", hle_c_specific_handler as *const () as usize);
        reg.register_export(v, "__std_type_info_compare", hle_type_info_compare as *const () as usize);
        reg.register_export(v, "__std_type_info_destroy_list", hle_noop as *const () as usize);
        reg.register_export(v, "_CxxThrowException", hle_cxx_throw_exception as *const () as usize);
    }
}

// ---------------------------------------------------------------------------
// Startup / Initializers / Exit
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_initterm(pfbegin: *const usize, pfend: *const usize) {
    if pfbegin.is_null() || pfend.is_null() || pfend <= pfbegin {
        return;
    }
    let mut cur = pfbegin;
    while cur < pfend {
        let addr = *cur;
        if addr != 0 {
            let f: unsafe extern "win64" fn() = core::mem::transmute(addr);
            f();
        }
        cur = cur.add(1);
    }
}

unsafe extern "win64" fn hle_initterm_e(pfbegin: *const usize, pfend: *const usize) -> i32 {
    if pfbegin.is_null() || pfend.is_null() || pfend <= pfbegin {
        return 0;
    }
    let mut cur = pfbegin;
    while cur < pfend {
        let addr = *cur;
        if addr != 0 {
            let f: unsafe extern "win64" fn() -> i32 = core::mem::transmute(addr);
            let rc = f();
            if rc != 0 {
                return rc;
            }
        }
        cur = cur.add(1);
    }
    0
}

#[repr(C)]
struct OnExitTable {
    first: *mut usize,
    last: *mut usize,
    end: *mut usize,
}

unsafe extern "win64" fn hle_init_onexit_table(table: *mut OnExitTable) -> i32 {
    if !table.is_null() {
        (*table).first = core::ptr::null_mut();
        (*table).last = core::ptr::null_mut();
        (*table).end = core::ptr::null_mut();
    }
    0
}

unsafe extern "win64" fn hle_register_onexit_function(
    _table: *mut OnExitTable,
    func: *const c_void,
) -> i32 {
    if !func.is_null() {
        let f: unsafe extern "win64" fn() -> i32 = core::mem::transmute(func);
        onexit_funcs().lock().unwrap().push(f);
    }
    0
}

unsafe extern "win64" fn hle_execute_onexit_table(_table: *mut OnExitTable) -> i32 {
    let funcs: Vec<_> = {
        let mut g = onexit_funcs().lock().unwrap();
        g.drain(..).collect()
    };
    for f in funcs.into_iter().rev() {
        let _ = f();
    }
    0
}

unsafe extern "win64" fn hle_crt_atexit(func: *const c_void) -> i32 {
    hle_register_onexit_function(core::ptr::null_mut(), func)
}

unsafe extern "win64" fn hle_exit(code: i32) {
    let _ = hle_execute_onexit_table(core::ptr::null_mut());
    process::record_guest_exit(code);
}

unsafe extern "win64" fn hle_abort() {
    if let Some(rt) = current_runtime() {
        rt.trace("ucrtbase!abort() invoked");
    }
    process::record_guest_exit(3);
}

unsafe extern "win64" fn hle_noop() {}
unsafe extern "win64" fn hle_return_zero() -> i32 {
    0
}

unsafe extern "win64" fn hle_set_app_type(t: i32) {
    APP_TYPE.store(t, Ordering::Relaxed);
}

unsafe extern "win64" fn hle_set_new_mode(m: i32) -> i32 {
    NEW_MODE.swap(m, Ordering::Relaxed)
}

unsafe extern "win64" fn hle_errno() -> *mut i32 {
    static mut ERRNO_VAL: i32 = 0;
    core::ptr::addr_of_mut!(ERRNO_VAL)
}

unsafe extern "win64" fn hle_p_argc() -> *mut i32 {
    static mut ARGC: i32 = 1;
    core::ptr::addr_of_mut!(ARGC)
}

unsafe extern "win64" fn hle_p_argv() -> *mut *mut *mut c_char {
    static ARGV0: &[u8] = b"xsxemu_guest.exe\0";
    static mut ARGV_ARR: [*mut c_char; 2] = [ARGV0.as_ptr() as *mut c_char, core::ptr::null_mut()];
    static mut ARGV_PTR: *mut *mut c_char = core::ptr::addr_of_mut!(ARGV_ARR) as *mut *mut c_char;
    core::ptr::addr_of_mut!(ARGV_PTR)
}

unsafe extern "win64" fn hle_p_wargv() -> *mut *mut *mut u16 {
    static WARGV0: &[u16] = &[
        b'x' as u16, b's' as u16, b'x' as u16, b'.' as u16, b'e' as u16, b'x' as u16,
        b'e' as u16, 0,
    ];
    static mut WARGV_ARR: [*mut u16; 2] = [WARGV0.as_ptr() as *mut u16, core::ptr::null_mut()];
    static mut WARGV_PTR: *mut *mut u16 = core::ptr::addr_of_mut!(WARGV_ARR) as *mut *mut u16;
    core::ptr::addr_of_mut!(WARGV_PTR)
}

unsafe extern "win64" fn hle_get_initial_narrow_env() -> *mut *mut c_char {
    static mut EMPTY_ENV: [*mut c_char; 1] = [core::ptr::null_mut()];
    core::ptr::addr_of_mut!(EMPTY_ENV) as *mut *mut c_char
}

unsafe extern "win64" fn hle_get_initial_wide_env() -> *mut *mut u16 {
    static mut EMPTY_WENV: [*mut u16; 1] = [core::ptr::null_mut()];
    core::ptr::addr_of_mut!(EMPTY_WENV) as *mut *mut u16
}

// ---------------------------------------------------------------------------
// Heap Allocation
// ---------------------------------------------------------------------------

unsafe fn alloc_with_align(size: usize, align: usize, zeroed: bool) -> *mut c_void {
    let sz = size.max(1);
    let al = align.max(16).next_power_of_two();
    let Ok(layout) = Layout::from_size_align(sz, al) else {
        return core::ptr::null_mut();
    };
    let ptr = if zeroed { alloc_zeroed(layout) } else { alloc(layout) };
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    crt_allocs()
        .lock()
        .unwrap()
        .insert(ptr as usize, AllocRecord { layout, user_size: size });
    ptr as *mut c_void
}

unsafe extern "win64" fn hle_malloc(size: usize) -> *mut c_void {
    alloc_with_align(size, 16, false)
}

unsafe extern "win64" fn hle_calloc(num: usize, size: usize) -> *mut c_void {
    let Some(total) = num.checked_mul(size) else {
        return core::ptr::null_mut();
    };
    alloc_with_align(total, 16, true)
}

unsafe extern "win64" fn hle_aligned_malloc(size: usize, alignment: usize) -> *mut c_void {
    alloc_with_align(size, alignment, false)
}

unsafe extern "win64" fn hle_realloc(ptr: *mut c_void, new_size: usize) -> *mut c_void {
    if ptr.is_null() {
        return hle_malloc(new_size);
    }
    if new_size == 0 {
        hle_free(ptr);
        return core::ptr::null_mut();
    }
    let rec = crt_allocs().lock().unwrap().remove(&(ptr as usize));
    let Some(old) = rec else {
        return hle_malloc(new_size);
    };
    let sz = new_size.max(1);
    let Ok(new_layout) = Layout::from_size_align(sz, old.layout.align()) else {
        crt_allocs().lock().unwrap().insert(ptr as usize, old);
        return core::ptr::null_mut();
    };
    let new_ptr = realloc(ptr as *mut u8, old.layout, sz);
    if new_ptr.is_null() {
        crt_allocs().lock().unwrap().insert(ptr as usize, old);
        return core::ptr::null_mut();
    }
    crt_allocs().lock().unwrap().insert(
        new_ptr as usize,
        AllocRecord {
            layout: new_layout,
            user_size: new_size,
        },
    );
    new_ptr as *mut c_void
}

unsafe extern "win64" fn hle_free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    if let Some(rec) = crt_allocs().lock().unwrap().remove(&(ptr as usize)) {
        dealloc(ptr as *mut u8, rec.layout);
    }
}

unsafe extern "win64" fn hle_msize(ptr: *mut c_void) -> usize {
    if ptr.is_null() {
        return 0;
    }
    crt_allocs()
        .lock()
        .unwrap()
        .get(&(ptr as usize))
        .map(|r| r.user_size)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Memory & String Primitives
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_memcpy(dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    if !dst.is_null() && !src.is_null() && len > 0 {
        core::ptr::copy_nonoverlapping(src, dst, len);
    }
    dst
}

unsafe extern "win64" fn hle_memmove(dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    if !dst.is_null() && !src.is_null() && len > 0 {
        core::ptr::copy(src, dst, len);
    }
    dst
}

unsafe extern "win64" fn hle_memset(dst: *mut u8, val: i32, len: usize) -> *mut u8 {
    if !dst.is_null() && len > 0 {
        core::ptr::write_bytes(dst, val as u8, len);
    }
    dst
}

unsafe extern "win64" fn hle_memcmp(a: *const u8, b: *const u8, len: usize) -> i32 {
    if a.is_null() || b.is_null() || len == 0 {
        return 0;
    }
    let sa = core::slice::from_raw_parts(a, len);
    let sb = core::slice::from_raw_parts(b, len);
    match sa.cmp(sb) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

unsafe extern "win64" fn hle_strlen(s: *const u8) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

unsafe extern "win64" fn hle_wcslen(s: *const u16) -> usize {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

unsafe extern "win64" fn hle_strcmp(a: *const u8, b: *const u8) -> i32 {
    hle_strncmp(a, b, usize::MAX)
}

unsafe extern "win64" fn hle_strncmp(a: *const u8, b: *const u8, max: usize) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    for i in 0..max {
        let ca = *a.add(i);
        let cb = *b.add(i);
        if ca != cb {
            return (ca as i32) - (cb as i32);
        }
        if ca == 0 {
            return 0;
        }
    }
    0
}

unsafe extern "win64" fn hle_stricmp(a: *const u8, b: *const u8) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    let mut i = 0usize;
    loop {
        let ca = (*a.add(i)).to_ascii_lowercase();
        let cb = (*b.add(i)).to_ascii_lowercase();
        if ca != cb {
            return (ca as i32) - (cb as i32);
        }
        if ca == 0 {
            return 0;
        }
        i += 1;
    }
}

unsafe extern "win64" fn hle_wcscmp(a: *const u16, b: *const u16) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    let mut i = 0usize;
    loop {
        let ca = *a.add(i);
        let cb = *b.add(i);
        if ca != cb {
            return (ca as i32) - (cb as i32);
        }
        if ca == 0 {
            return 0;
        }
        i += 1;
    }
}

unsafe extern "win64" fn hle_wcsicmp(a: *const u16, b: *const u16) -> i32 {
    if a.is_null() || b.is_null() {
        return 0;
    }
    let mut i = 0usize;
    loop {
        let ca = char::from_u32(*a.add(i) as u32)
            .map(|c| c.to_ascii_lowercase() as u32)
            .unwrap_or(*a.add(i) as u32);
        let cb = char::from_u32(*b.add(i) as u32)
            .map(|c| c.to_ascii_lowercase() as u32)
            .unwrap_or(*b.add(i) as u32);
        if ca != cb {
            return (ca as i32) - (cb as i32);
        }
        if ca == 0 {
            return 0;
        }
        i += 1;
    }
}

unsafe extern "win64" fn hle_strcpy_s(dst: *mut u8, dst_cap: usize, src: *const u8) -> i32 {
    if dst.is_null() || src.is_null() || dst_cap == 0 {
        return 22; // EINVAL
    }
    let len = hle_strlen(src);
    if len + 1 > dst_cap {
        *dst = 0;
        return 34; // ERANGE
    }
    core::ptr::copy_nonoverlapping(src, dst, len + 1);
    0
}

unsafe extern "win64" fn hle_wcscpy_s(dst: *mut u16, dst_cap: usize, src: *const u16) -> i32 {
    if dst.is_null() || src.is_null() || dst_cap == 0 {
        return 22;
    }
    let len = hle_wcslen(src);
    if len + 1 > dst_cap {
        *dst = 0;
        return 34;
    }
    core::ptr::copy_nonoverlapping(src, dst, len + 1);
    0
}

// ---------------------------------------------------------------------------
// Misc & C++/SEH Runtime
// ---------------------------------------------------------------------------

unsafe extern "win64" fn hle_srand(seed: u32) {
    RAND_STATE.store(seed, Ordering::Relaxed);
}

unsafe extern "win64" fn hle_rand() -> i32 {
    let mut prev = RAND_STATE.load(Ordering::Relaxed);
    let next = loop {
        let n = prev.wrapping_mul(214_013).wrapping_add(2_531_011);
        match RAND_STATE.compare_exchange_weak(prev, n, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break n,
            Err(cur) => prev = cur,
        }
    };
    ((next >> 16) & 0x7FFF) as i32
}

unsafe extern "win64" fn hle_time64(out: *mut i64) -> i64 {
    let base = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(1_700_000_000);
    let offset = current_runtime().map(|rt| rt.rtc().offset_seconds).unwrap_or(0);
    let secs = base + offset;
    if !out.is_null() {
        *out = secs;
    }
    secs
}

unsafe extern "win64" fn hle_stdio_common_vsprintf(
    _options: u64,
    buffer: *mut u8,
    buffer_count: usize,
    format: *const u8,
    _locale: *const c_void,
    _arglist: *const c_void,
) -> i32 {
    if format.is_null() {
        return -1;
    }
    let len = hle_strlen(format);
    if !buffer.is_null() && buffer_count > 0 {
        let copy = len.min(buffer_count - 1);
        core::ptr::copy_nonoverlapping(format, buffer, copy);
        *buffer.add(copy) = 0;
    }
    len as i32
}

unsafe extern "win64" fn hle_c_specific_handler() -> i32 {
    1 // ExceptionContinueSearch
}

unsafe extern "win64" fn hle_type_info_compare(a: *const usize, b: *const usize) -> i32 {
    if a == b {
        0
    } else if a < b {
        -1
    } else {
        1
    }
}

unsafe extern "win64" fn hle_cxx_throw_exception(_obj: *const c_void, _info: *const c_void) {
    if let Some(rt) = current_runtime() {
        rt.trace("vcruntime140!_CxxThrowException invoked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crt_malloc_realloc_free_and_strings_work() {
        unsafe {
            let p = hle_malloc(32);
            assert!(!p.is_null());
            assert_eq!(hle_msize(p), 32);
            hle_memset(p as *mut u8, 0xAB, 32);
            let p2 = hle_realloc(p, 64);
            assert!(!p2.is_null());
            assert_eq!(hle_msize(p2), 64);
            assert_eq!(*(p2 as *const u8), 0xAB);
            hle_free(p2);

            let a = b"Hello\0";
            let b = b"hello\0";
            assert_eq!(hle_stricmp(a.as_ptr(), b.as_ptr()), 0);
            assert_ne!(hle_strcmp(a.as_ptr(), b.as_ptr()), 0);
            assert_eq!(hle_strlen(a.as_ptr()), 5);
        }
    }
}
