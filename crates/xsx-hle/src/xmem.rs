//! `xmem.dll` — Xbox Series X GDK tracked memory allocator & virtual memory shims.
//!
//! Covers the real measured exports imported by Xbox Series X titles and middleware
//! (including `amd_fidelityfx_upscaler_xbox.dll`):
//! - `XMemAlloc(size, attributes)` — 64-byte aligned (cache line & SIMD safe)
//! - `XMemFree(ptr, attributes)`
//! - `XMemVirtualAlloc(address, size, allocation_type, attributes, protect)` — handles Xbox
//!   `0x2040_0000` (64 KiB pages) and rejects unsupported `0x2000_0000` (2 MiB large pages)
//! - `XMemVirtualQuery(process, address, out64)` — fills 48-byte `MemoryBasicInformation`
//!   prefix and zero-fills trailing 16 bytes
//! - `XMemGetWorkingSetStatistics(kind, stats)` — measured 2-arg ABI (kind `0x2000_0001` at
//!   all 7 FSR call sites; writes `0` at `+0` and 64-bit working set bytes at `+8`)
//! - `XMemGetWriteWatch` / `XMemResetWriteWatch`

use core::ffi::c_void;
use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use xsx_abi::gdk::{Handle, MemoryBasicInformation, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};

use crate::registry::ModuleRegistry;
use crate::runtime::current_runtime;

const XMEM_ALIGN: usize = 64;
const XMEM_LARGE_PAGES: u32 = 0x2000_0000;
const XMEM_64K_PAGES: u32 = 0x2040_0000;
const XMEM_PAGE_MODE_MASK: u32 = XMEM_64K_PAGES;
const XMEM_PAGE_SIZE_64K: usize = 0x1_0000;

const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_NOT_SUPPORTED: u32 = 50;

#[derive(Debug, Clone, Copy)]
struct XMemBlock {
    size: usize,
    align: usize,
}

fn xmem_table() -> &'static Mutex<HashMap<usize, XMemBlock>> {
    static TABLE: OnceLock<Mutex<HashMap<usize, XMemBlock>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register(reg: &mut ModuleRegistry) {
    reg.register_export("xmem.dll", "XMemAlloc", hle_xmem_alloc as *const () as usize);
    reg.register_export("xmem.dll", "XMemFree", hle_xmem_free as *const () as usize);
    reg.register_export(
        "xmem.dll",
        "XMemVirtualAlloc",
        hle_xmem_virtual_alloc as *const () as usize,
    );
    reg.register_export(
        "xmem.dll",
        "XMemVirtualQuery",
        hle_xmem_virtual_query as *const () as usize,
    );
    reg.register_export(
        "xmem.dll",
        "XMemGetWorkingSetStatistics",
        hle_xmem_get_working_set_statistics as *const () as usize,
    );
    reg.register_export(
        "xmem.dll",
        "XMemGetWriteWatch",
        hle_xmem_get_write_watch as *const () as usize,
    );
    reg.register_export(
        "xmem.dll",
        "XMemResetWriteWatch",
        hle_xmem_reset_write_watch as *const () as usize,
    );
}

fn alloc_aligned_block(size: usize, align: usize) -> *mut c_void {
    let eff_size = size.max(1);
    let rounded = (eff_size + align - 1) & !(align - 1);
    let Ok(layout) = Layout::from_size_align(rounded, align) else {
        return core::ptr::null_mut();
    };
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        return core::ptr::null_mut();
    }
    xmem_table()
        .lock()
        .unwrap()
        .insert(ptr as usize, XMemBlock { size: rounded, align });
    ptr as *mut c_void
}

/// `void* XMemAlloc(size_t size, uint64_t attributes)` — 64-byte aligned.
pub unsafe extern "win64" fn hle_xmem_alloc(size: usize, attributes: u64) -> *mut c_void {
    let ptr = alloc_aligned_block(size, XMEM_ALIGN);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "xmem!XMemAlloc(size={size}, attrs=0x{attributes:X}) -> {ptr:?}"
        ));
    }
    ptr
}

/// `void XMemFree(void* ptr, uint64_t attributes)`
pub unsafe extern "win64" fn hle_xmem_free(ptr: *mut c_void, attributes: u64) {
    if ptr.is_null() {
        return;
    }
    if let Some(block) = xmem_table().lock().unwrap().remove(&(ptr as usize)) {
        if let Ok(layout) = Layout::from_size_align(block.size, block.align) {
            dealloc(ptr as *mut u8, layout);
        }
    }
    if let Some(rt) = current_runtime() {
        rt.trace(format!("xmem!XMemFree({ptr:?}, attrs=0x{attributes:X})"));
    }
}

/// `void* XMemVirtualAlloc(void* addr, size_t size, DWORD alloc_type, uint64_t attrs, DWORD protect)`
pub unsafe extern "win64" fn hle_xmem_virtual_alloc(
    address: *mut c_void,
    size: usize,
    allocation_type: u32,
    attributes: u64,
    protect: u32,
) -> *mut c_void {
    if size == 0 {
        if let Some(rt) = current_runtime() {
            rt.set_last_error(ERROR_INVALID_PARAMETER);
        }
        return core::ptr::null_mut();
    }
    if attributes != 0 {
        if let Some(rt) = current_runtime() {
            rt.set_last_error(ERROR_NOT_SUPPORTED);
        }
        return core::ptr::null_mut();
    }

    let page_mode = allocation_type & XMEM_PAGE_MODE_MASK;
    if page_mode == XMEM_LARGE_PAGES {
        if let Some(rt) = current_runtime() {
            rt.set_last_error(ERROR_NOT_SUPPORTED);
        }
        return core::ptr::null_mut();
    }
    if page_mode != 0 && page_mode != XMEM_64K_PAGES {
        if let Some(rt) = current_runtime() {
            rt.set_last_error(ERROR_INVALID_PARAMETER);
        }
        return core::ptr::null_mut();
    }

    let align = if page_mode == XMEM_64K_PAGES {
        XMEM_PAGE_SIZE_64K
    } else {
        4096
    };
    let Some(rounded_size) = size
        .checked_add(align - 1)
        .map(|v| v & !(align - 1))
    else {
        if let Some(rt) = current_runtime() {
            rt.set_last_error(ERROR_INVALID_PARAMETER);
        }
        return core::ptr::null_mut();
    };

    // If caller passed an address inside an already-allocated XMem block (re-commit), return it
    if !address.is_null() {
        let addr_u = address as usize;
        let table = xmem_table().lock().unwrap();
        for (&base, blk) in table.iter() {
            if addr_u >= base && addr_u < base + blk.size {
                return (addr_u & !(align - 1)) as *mut c_void;
            }
        }
    }

    let ptr = alloc_aligned_block(rounded_size, align);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "xmem!XMemVirtualAlloc(size=0x{size:X}, type=0x{allocation_type:X}, prot=0x{protect:X}) -> {ptr:?}"
        ));
    }
    ptr
}

/// `SIZE_T XMemVirtualQuery(HANDLE process, const void* address, void* out64)`
pub unsafe extern "win64" fn hle_xmem_virtual_query(
    _process: Handle,
    address: *const c_void,
    out: *mut c_void,
) -> usize {
    if out.is_null() {
        return 0;
    }
    // Always zero the full 64-byte console output struct first
    core::ptr::write_bytes(out as *mut u8, 0, 64);
    if address.is_null() {
        return 0;
    }

    let addr_u = address as usize;
    let table = xmem_table().lock().unwrap();
    let mut found: Option<(usize, usize)> = None;
    for (&base, blk) in table.iter() {
        if addr_u >= base && addr_u < base + blk.size {
            found = Some((base, blk.size));
            break;
        }
    }

    let (base, size) = found.unwrap_or((addr_u & !0xFFF, 0x1000));
    let mbi = MemoryBasicInformation {
        base_address: base,
        allocation_base: base,
        allocation_protect: PAGE_READWRITE,
        partition_id: 0,
        _pad0: 0,
        region_size: size,
        state: MEM_COMMIT | MEM_RESERVE,
        protect: PAGE_READWRITE,
        r#type: 0x20000, // MEM_PRIVATE
        _pad1: 0,
    };
    core::ptr::copy_nonoverlapping(
        &mbi as *const MemoryBasicInformation as *const u8,
        out as *mut u8,
        core::mem::size_of::<MemoryBasicInformation>(),
    );
    core::mem::size_of::<MemoryBasicInformation>()
}

/// `BOOL XMemGetWorkingSetStatistics(UINT32 kind, void* stats)`
///
/// Measured 2-arg ABI (used at all 7 call sites in `amd_fidelityfx_upscaler_xbox.dll` with
/// `kind = 0x2000_0001`): writes 0 at `+0` and 64-bit working set bytes at `+8`.
pub unsafe extern "win64" fn hle_xmem_get_working_set_statistics(
    kind: u32,
    stats: *mut c_void,
) -> i32 {
    if stats.is_null() {
        return 0;
    }
    let bytes = stats as *mut u8;
    core::ptr::write_bytes(bytes, 0, 16);
    let allocated: u64 = xmem_table()
        .lock()
        .unwrap()
        .values()
        .map(|b| b.size as u64)
        .sum();
    // Report at least 64 MiB baseline process working set + tracked XMem allocations
    let working_set = allocated.saturating_add(64 * 1024 * 1024);
    core::ptr::copy_nonoverlapping(working_set.to_le_bytes().as_ptr(), bytes.add(8), 8);
    if let Some(rt) = current_runtime() {
        rt.trace(format!(
            "xmem!XMemGetWorkingSetStatistics(kind=0x{kind:X}) -> working_set={working_set}"
        ));
    }
    1
}

/// `UINT XMemGetWriteWatch(...)` — returns 0 (success) with 0 dirty pages reported.
pub unsafe extern "win64" fn hle_xmem_get_write_watch(
    _flags: u32,
    _base: *mut c_void,
    _region_size: usize,
    _addresses: *mut *mut c_void,
    count: *mut usize,
    granularity: *mut u32,
) -> u32 {
    if !count.is_null() {
        *count = 0;
    }
    if !granularity.is_null() {
        *granularity = 4096;
    }
    0
}

/// `UINT XMemResetWriteWatch(...)` — returns 0 (success).
pub unsafe extern "win64" fn hle_xmem_reset_write_watch(
    _base: *mut c_void,
    _region_size: usize,
) -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xmem_alloc_aligns_to_64_bytes_and_queries_working_set() {
        unsafe {
            let p = hle_xmem_alloc(100, 0x5E5_0000);
            assert!(!p.is_null());
            assert_eq!((p as usize) % 64, 0);

            let mut out = [0xAAu8; 64];
            let written = hle_xmem_virtual_query(core::ptr::null_mut(), p, out.as_mut_ptr() as *mut c_void);
            assert_eq!(written, 48);
            // Trailing 16 bytes zero-filled
            assert_eq!(&out[48..64], &[0u8; 16]);

            let mut stats = [0u8; 16];
            assert_eq!(
                hle_xmem_get_working_set_statistics(0x2000_0001, stats.as_mut_ptr() as *mut c_void),
                1
            );
            let ws = u64::from_le_bytes(stats[8..16].try_into().unwrap());
            assert!(ws >= 64 * 1024 * 1024);

            hle_xmem_free(p, 0);

            // 64 KiB page mode succeeds and aligns to 64 KiB
            let p64k = hle_xmem_virtual_alloc(
                core::ptr::null_mut(),
                12_000,
                XMEM_64K_PAGES | MEM_COMMIT | MEM_RESERVE,
                0,
                PAGE_READWRITE,
            );
            assert!(!p64k.is_null());
            assert_eq!((p64k as usize) % XMEM_PAGE_SIZE_64K, 0);
            hle_xmem_free(p64k, 0);

            // 2 MiB large page mode fails explicitly
            assert!(hle_xmem_virtual_alloc(
                core::ptr::null_mut(),
                4096,
                XMEM_LARGE_PAGES | MEM_COMMIT | MEM_RESERVE,
                0,
                PAGE_READWRITE,
            )
            .is_null());
        }
    }
}
