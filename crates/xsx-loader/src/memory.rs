//! Cross-platform OS executable virtual memory allocator (`VirtualAlloc` on Windows, `mmap` on Unix).

use std::ffi::c_void;
use std::io;
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageProtection {
    NoAccess,
    Read,
    ReadWrite,
    ReadExecute,
    ReadWriteExecute,
}

/// Owned page-aligned OS virtual memory allocation.
pub struct VirtualMemory {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for VirtualMemory {}
unsafe impl Sync for VirtualMemory {}

impl VirtualMemory {
    /// Allocate `len` bytes of read-write zeroed virtual memory, optionally hinting `preferred_base`.
    pub fn allocate(preferred_base: Option<usize>, len: usize) -> io::Result<Self> {
        let len = align_up(len.max(4096), 4096);
        let ptr = os_alloc(preferred_base, len)?;
        Ok(Self { ptr, len })
    }

    pub fn base(&self) -> *mut u8 {
        self.ptr
    }

    pub fn base_addr(&self) -> usize {
        self.ptr as usize
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// Change OS page protections for `[offset, offset + len)` (page-aligned internally).
    pub fn protect(&self, offset: usize, len: usize, prot: PageProtection) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let start = offset & !4095usize;
        let end = align_up(offset.saturating_add(len), 4096).min(self.len);
        if start >= end {
            return Ok(());
        }
        let region_ptr = unsafe { self.ptr.add(start) };
        os_protect(region_ptr, end - start, prot)
    }
}

impl Drop for VirtualMemory {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            let _ = os_free(self.ptr, self.len);
            self.ptr = ptr::null_mut();
            self.len = 0;
        }
    }
}

fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

#[cfg(unix)]
fn os_alloc(preferred_base: Option<usize>, len: usize) -> io::Result<*mut u8> {
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const MAP_PRIVATE: i32 = 0x02;
    #[cfg(target_os = "linux")]
    const MAP_ANONYMOUS: i32 = 0x20;
    #[cfg(target_os = "macos")]
    const MAP_ANONYMOUS: i32 = 0x1000;
    const MAP_FAILED: *mut c_void = (!0usize) as *mut c_void;

    extern "C" {
        fn mmap(
            addr: *mut c_void,
            length: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            offset: isize,
        ) -> *mut c_void;
    }

    let hint = preferred_base.unwrap_or(0) as *mut c_void;
    let p = unsafe { mmap(hint, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
    if p == MAP_FAILED || p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(p as *mut u8)
}

#[cfg(unix)]
fn os_protect(ptr: *mut u8, len: usize, prot: PageProtection) -> io::Result<()> {
    const PROT_NONE: i32 = 0x0;
    const PROT_READ: i32 = 0x1;
    const PROT_WRITE: i32 = 0x2;
    const PROT_EXEC: i32 = 0x4;

    extern "C" {
        fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
    }

    let flags = match prot {
        PageProtection::NoAccess => PROT_NONE,
        PageProtection::Read => PROT_READ,
        PageProtection::ReadWrite => PROT_READ | PROT_WRITE,
        PageProtection::ReadExecute => PROT_READ | PROT_EXEC,
        PageProtection::ReadWriteExecute => PROT_READ | PROT_WRITE | PROT_EXEC,
    };

    let rc = unsafe { mprotect(ptr as *mut c_void, len, flags) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn os_free(ptr: *mut u8, len: usize) -> io::Result<()> {
    extern "C" {
        fn munmap(addr: *mut c_void, len: usize) -> i32;
    }
    let rc = unsafe { munmap(ptr as *mut c_void, len) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn os_alloc(preferred_base: Option<usize>, len: usize) -> io::Result<*mut u8> {
    const MEM_COMMIT: u32 = 0x0000_1000;
    const MEM_RESERVE: u32 = 0x0000_2000;
    const PAGE_READWRITE: u32 = 0x04;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(
            lp_address: *mut c_void,
            dw_size: usize,
            fl_allocation_type: u32,
            fl_protect: u32,
        ) -> *mut c_void;
    }

    // Try preferred base first; fall back to OS-chosen address (which exercises relocations).
    if let Some(hint) = preferred_base {
        let p = unsafe {
            VirtualAlloc(hint as *mut c_void, len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
        };
        if !p.is_null() {
            return Ok(p as *mut u8);
        }
    }

    let p = unsafe { VirtualAlloc(ptr::null_mut(), len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
    if p.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(p as *mut u8)
}

#[cfg(windows)]
fn os_protect(ptr: *mut u8, len: usize, prot: PageProtection) -> io::Result<()> {
    const PAGE_NOACCESS: u32 = 0x01;
    const PAGE_READONLY: u32 = 0x02;
    const PAGE_READWRITE: u32 = 0x04;
    const PAGE_EXECUTE_READ: u32 = 0x20;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualProtect(
            lp_address: *mut c_void,
            dw_size: usize,
            fl_new_protect: u32,
            lpfl_old_protect: *mut u32,
        ) -> i32;
    }

    let flags = match prot {
        PageProtection::NoAccess => PAGE_NOACCESS,
        PageProtection::Read => PAGE_READONLY,
        PageProtection::ReadWrite => PAGE_READWRITE,
        PageProtection::ReadExecute => PAGE_EXECUTE_READ,
        PageProtection::ReadWriteExecute => PAGE_EXECUTE_READWRITE,
    };

    let mut old_prot = 0u32;
    let ok = unsafe { VirtualProtect(ptr as *mut c_void, len, flags, &mut old_prot) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn os_free(ptr: *mut u8, _len: usize) -> io::Result<()> {
    const MEM_RELEASE: u32 = 0x0000_8000;
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualFree(lp_address: *mut c_void, dw_size: usize, dw_free_type: u32) -> i32;
    }
    let ok = unsafe { VirtualFree(ptr as *mut c_void, 0, MEM_RELEASE) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
