//! Process command-line & Universal CRT (`ucrtbase.dll` / `api-ms-win-crt-runtime-l1-1-0.dll`)
//! startup shims for real MSVC-compiled Xbox Series X executables.
//!
//! Keeps the guest title's own `argc`/`argv`/`wargv` and command line separate from `xsxemu`'s
//! host command line, and intercepts CRT startup queries (`GetCommandLineA/W`, `__p___argc`,
//! `__p___argv`, `__p___wargv`, `_configure_narrow_argv`, `_configure_wide_argv`,
//! `_get_narrow_winmain_command_line`, `_get_wide_winmain_command_line`).

use core::ffi::c_char;
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

struct GuestProcessArgs {
    argc: i32,
    _narrow_args: Vec<CString>,
    narrow_ptrs: Vec<*mut c_char>,
    narrow_argv_ptr: *mut *mut c_char,
    narrow_cmdline: Vec<u8>,
    _wide_args: Vec<Vec<u16>>,
    wide_ptrs: Vec<*mut u16>,
    wide_argv_ptr: *mut *mut u16,
    wide_cmdline: Vec<u16>,
    exit_code: Option<i32>,
}

// Raw pointers point into the pinned Vec/CString allocations inside the Mutex-protected struct.
unsafe impl Send for GuestProcessArgs {}

impl GuestProcessArgs {
    fn new(program: &str, args: &[String]) -> Self {
        let mut all = Vec::with_capacity(1 + args.len());
        all.push(program.to_string());
        all.extend(args.iter().cloned());

        let mut narrow_args = Vec::with_capacity(all.len());
        let mut wide_args = Vec::with_capacity(all.len());
        let mut cmd_str = String::new();

        for (idx, a) in all.iter().enumerate() {
            if idx > 0 {
                cmd_str.push(' ');
            }
            if a.contains([' ', '\t', '"']) || a.is_empty() {
                cmd_str.push('"');
                for ch in a.chars() {
                    if ch == '"' {
                        cmd_str.push('\\');
                    }
                    cmd_str.push(ch);
                }
                cmd_str.push('"');
            } else {
                cmd_str.push_str(a);
            }
            narrow_args.push(CString::new(a.replace('\0', "")).unwrap_or_default());
            let mut w: Vec<u16> = a.encode_utf16().collect();
            w.push(0);
            wide_args.push(w);
        }

        let mut narrow_ptrs: Vec<*mut c_char> = narrow_args
            .iter()
            .map(|c| c.as_ptr() as *mut c_char)
            .collect();
        narrow_ptrs.push(core::ptr::null_mut());

        let mut wide_ptrs: Vec<*mut u16> = wide_args
            .iter_mut()
            .map(|w| w.as_mut_ptr())
            .collect();
        wide_ptrs.push(core::ptr::null_mut());

        let mut narrow_cmdline = cmd_str.as_bytes().to_vec();
        narrow_cmdline.push(0);

        let mut wide_cmdline: Vec<u16> = cmd_str.encode_utf16().collect();
        wide_cmdline.push(0);

        let narrow_argv_ptr = narrow_ptrs.as_mut_ptr();
        let wide_argv_ptr = wide_ptrs.as_mut_ptr();

        Self {
            argc: all.len() as i32,
            _narrow_args: narrow_args,
            narrow_ptrs,
            narrow_argv_ptr,
            narrow_cmdline,
            _wide_args: wide_args,
            wide_ptrs,
            wide_argv_ptr,
            wide_cmdline,
            exit_code: None,
        }
    }
}

fn process_state() -> &'static Mutex<GuestProcessArgs> {
    static STATE: OnceLock<Mutex<GuestProcessArgs>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(GuestProcessArgs::new("game.exe", &[])))
}

/// Configure the guest executable's program name and arguments before invoking its entry point.
pub fn set_guest_command_line(program: &str, args: &[String]) {
    let mut guard = process_state().lock().unwrap();
    *guard = GuestProcessArgs::new(program, args);
}

/// Read the last exit code recorded by `ExitProcess` / `exit` / `abort`, if any.
pub fn last_guest_exit_code() -> Option<i32> {
    process_state().lock().unwrap().exit_code
}

pub fn register(reg: &mut ModuleRegistry) {
    // Command line (kernelx / kernel32 / kernelbase / api-ms-win-core-processenvironment)
    reg.register_export(
        "kernelx.dll",
        "GetCommandLineA",
        hle_get_command_line_a as *const () as usize,
    );
    reg.register_export(
        "kernelx.dll",
        "GetCommandLineW",
        hle_get_command_line_w as *const () as usize,
    );
    reg.register_export(
        "kernelx.dll",
        "ExitProcess",
        hle_exit_process as *const () as usize,
    );

    // Universal CRT runtime startup exports (ucrtbase.dll & api-ms-win-crt-runtime-l1-1-0.dll)
    for dll in ["ucrtbase.dll", "msvcrt.dll"] {
        reg.register_export(dll, "__p___argc", hle_p_argc as *const () as usize);
        reg.register_export(dll, "__p___argv", hle_p_argv as *const () as usize);
        reg.register_export(dll, "__p___wargv", hle_p_wargv as *const () as usize);
        reg.register_export(
            dll,
            "_configure_narrow_argv",
            hle_configure_argv as *const () as usize,
        );
        reg.register_export(
            dll,
            "_configure_wide_argv",
            hle_configure_argv as *const () as usize,
        );
        reg.register_export(
            dll,
            "_initialize_narrow_environment",
            hle_init_env as *const () as usize,
        );
        reg.register_export(
            dll,
            "_initialize_wide_environment",
            hle_init_env as *const () as usize,
        );
        reg.register_export(
            dll,
            "_get_narrow_winmain_command_line",
            hle_get_narrow_winmain_cmdline as *const () as usize,
        );
        reg.register_export(
            dll,
            "_get_wide_winmain_command_line",
            hle_get_wide_winmain_cmdline as *const () as usize,
        );
        reg.register_export(dll, "exit", hle_c_exit as *const () as usize);
        reg.register_export(dll, "_exit", hle_c_exit as *const () as usize);
        reg.register_export(dll, "quick_exit", hle_c_exit as *const () as usize);
        reg.register_export(dll, "abort", hle_abort as *const () as usize);
        reg.register_export(dll, "_c_exit", hle_noop as *const () as usize);
        reg.register_export(dll, "_cexit", hle_noop as *const () as usize);
        reg.register_export(dll, "_register_thread_local_exe_atexit_callback", hle_noop as *const () as usize);
    }
}

unsafe extern "win64" fn hle_get_command_line_a() -> *mut c_char {
    let mut guard = process_state().lock().unwrap();
    guard.narrow_cmdline.as_mut_ptr() as *mut c_char
}

unsafe extern "win64" fn hle_get_command_line_w() -> *mut u16 {
    let mut guard = process_state().lock().unwrap();
    guard.wide_cmdline.as_mut_ptr()
}

unsafe extern "win64" fn hle_p_argc() -> *mut i32 {
    let mut guard = process_state().lock().unwrap();
    &mut guard.argc as *mut i32
}

unsafe extern "win64" fn hle_p_argv() -> *mut *mut *mut c_char {
    let mut guard = process_state().lock().unwrap();
    guard.narrow_argv_ptr = guard.narrow_ptrs.as_mut_ptr();
    &mut guard.narrow_argv_ptr as *mut *mut *mut c_char
}

unsafe extern "win64" fn hle_p_wargv() -> *mut *mut *mut u16 {
    let mut guard = process_state().lock().unwrap();
    guard.wide_argv_ptr = guard.wide_ptrs.as_mut_ptr();
    &mut guard.wide_argv_ptr as *mut *mut *mut u16
}

unsafe extern "win64" fn hle_configure_argv(_mode: i32) -> i32 {
    0
}

unsafe extern "win64" fn hle_init_env() -> i32 {
    0
}

unsafe extern "win64" fn hle_get_narrow_winmain_cmdline() -> *mut c_char {
    let mut guard = process_state().lock().unwrap();
    let ptr = guard.narrow_cmdline.as_mut_ptr();
    let len = guard.narrow_cmdline.len();
    let mut i = 0usize;
    let mut quoted = false;
    while i < len && guard.narrow_cmdline[i] != 0 {
        let b = guard.narrow_cmdline[i];
        if b == b'"' {
            quoted = !quoted;
        } else if b <= b' ' && !quoted {
            break;
        }
        i += 1;
    }
    while i < len && guard.narrow_cmdline[i] != 0 && guard.narrow_cmdline[i] <= b' ' {
        i += 1;
    }
    ptr.add(i) as *mut c_char
}

unsafe extern "win64" fn hle_get_wide_winmain_cmdline() -> *mut u16 {
    let mut guard = process_state().lock().unwrap();
    let ptr = guard.wide_cmdline.as_mut_ptr();
    let len = guard.wide_cmdline.len();
    let mut i = 0usize;
    let mut quoted = false;
    while i < len && guard.wide_cmdline[i] != 0 {
        let ch = guard.wide_cmdline[i];
        if ch == b'"' as u16 {
            quoted = !quoted;
        } else if ch <= b' ' as u16 && !quoted {
            break;
        }
        i += 1;
    }
    while i < len && guard.wide_cmdline[i] != 0 && guard.wide_cmdline[i] <= b' ' as u16 {
        i += 1;
    }
    ptr.add(i)
}

unsafe extern "win64" fn hle_exit_process(code: u32) {
    process_state().lock().unwrap().exit_code = Some(code as i32);
    if let Some(rt) = current_runtime() {
        rt.trace(format!("kernelx!ExitProcess({code})"));
    }
}

unsafe extern "win64" fn hle_c_exit(code: i32) {
    process_state().lock().unwrap().exit_code = Some(code);
    if let Some(rt) = current_runtime() {
        rt.trace(format!("ucrtbase!exit({code})"));
    }
}

unsafe extern "win64" fn hle_abort() {
    process_state().lock().unwrap().exit_code = Some(3);
    if let Some(rt) = current_runtime() {
        rt.trace("ucrtbase!abort() -> exit_code=3");
    }
}

unsafe extern "win64" fn hle_noop() {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    #[test]
    fn guest_command_line_and_winmain_args_parse_correctly() {
        set_guest_command_line(
            r"GAME:\bin\title.exe",
            &["--fullscreen".into(), "save slot 1".into()],
        );
        unsafe {
            assert_eq!(*hle_p_argc(), 3);
            let cmd = CStr::from_ptr(hle_get_command_line_a()).to_str().unwrap();
            assert!(cmd.contains("--fullscreen"));
            assert!(cmd.contains("\"save slot 1\""));
            let winmain = CStr::from_ptr(hle_get_narrow_winmain_cmdline())
                .to_str()
                .unwrap();
            assert_eq!(winmain, "--fullscreen \"save slot 1\"");
        }
    }
}
