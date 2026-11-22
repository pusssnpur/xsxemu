//! PE32+ image mapper, relocator, IAT patcher, TLS callback runner, `.pdata` SEH registrar,
//! and native Win64 ABI executor.

use core::ffi::c_void;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use xsx_abi::gdk::{
    DllMainFn, RuntimeFunction, TlsCallbackFn, DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH,
};

use crate::memory::{PageProtection, VirtualMemory};
use crate::pe::{
    ImportSymbol, PeError, PeImage, IMAGE_DIRECTORY_ENTRY_TLS, IMAGE_REL_BASED_ABSOLUTE,
    IMAGE_REL_BASED_DIR64, IMAGE_REL_BASED_HIGHLOW, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ,
    IMAGE_SCN_MEM_WRITE,
};

static NEXT_TLS_INDEX: AtomicU32 = AtomicU32::new(1);

#[derive(Debug)]
pub enum LoaderError {
    Pe(PeError),
    Memory(io::Error),
    Io { path: String, err: io::Error },
    UnresolvedImport { dll: String, symbol: ImportSymbol },
    SectionOutOfBounds { name: String, end: usize, image_size: usize },
    UnsupportedRelocation { kind: u16, rva: u32 },
    InvalidEntryPoint(u32),
    DependencyCycle(String),
    DllMainFailed(String),
}

impl fmt::Display for LoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoaderError::Pe(e) => write!(f, "PE parse error: {e}"),
            LoaderError::Memory(e) => write!(f, "virtual memory error: {e}"),
            LoaderError::Io { path, err } => write!(f, "I/O error reading '{path}': {err}"),
            LoaderError::UnresolvedImport { dll, symbol } => {
                write!(f, "unresolved import: {dll}!{symbol}")
            }
            LoaderError::SectionOutOfBounds { name, end, image_size } => write!(
                f,
                "section '{name}' end (0x{end:X}) exceeds SizeOfImage (0x{image_size:X})"
            ),
            LoaderError::UnsupportedRelocation { kind, rva } => {
                write!(f, "unsupported relocation kind {kind} at RVA 0x{rva:08X}")
            }
            LoaderError::InvalidEntryPoint(rva) => {
                write!(f, "entry point RVA 0x{rva:08X} out of image bounds")
            }
            LoaderError::DependencyCycle(dll) => {
                write!(f, "circular guest DLL dependency detected: {dll}")
            }
            LoaderError::DllMainFailed(dll) => {
                write!(f, "guest DLL '{dll}' DllMain(DLL_PROCESS_ATTACH) returned FALSE")
            }
        }
    }
}

impl std::error::Error for LoaderError {}

impl From<PeError> for LoaderError {
    fn from(e: PeError) -> Self {
        LoaderError::Pe(e)
    }
}

impl From<io::Error> for LoaderError {
    fn from(e: io::Error) -> Self {
        LoaderError::Memory(e)
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedImport {
    pub dll: String,
    pub symbol: ImportSymbol,
    pub iat_rva: u32,
    pub target_addr: usize,
    pub is_stub: bool,
}

/// Callback interface used by the PE loader to resolve imported DLL functions.
pub trait ImportResolver {
    /// Return `(target_function_address, is_stub)` for a given `dll` and `symbol`.
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Option<(usize, bool)>;
}

/// A PE32+ image mapped into executable host virtual memory, relocated, IAT-patched,
/// with parsed exports, TLS callbacks, and `.pdata` SEH unwind tables.
pub struct LoadedImage {
    memory: VirtualMemory,
    preferred_base: u64,
    entry_point_rva: u32,
    is_dll: bool,
    relocations_applied: usize,
    resolved_imports: Vec<ResolvedImport>,
    exports_by_name: HashMap<String, usize>,
    exports_by_ordinal: HashMap<u16, usize>,
    tls_callbacks: Vec<usize>,
    tls_attached: AtomicBool,
    runtime_functions: Vec<RuntimeFunction>,
    pdata_registered_ptr: Option<usize>,
}

impl LoadedImage {
    /// Map, relocate, patch imports, register `.pdata` (on Windows), and apply page protections.
    pub fn load(
        pe: &PeImage,
        resolver: &dyn ImportResolver,
        force_relocate: bool,
    ) -> Result<Self, LoaderError> {
        let image_size = pe.size_of_image as usize;
        let preferred_hint = if force_relocate {
            None
        } else {
            Some(pe.image_base as usize)
        };
        let mut memory = VirtualMemory::allocate(preferred_hint, image_size)?;

        // 1. Copy PE headers
        let header_len = (pe.size_of_headers as usize)
            .min(pe.raw_bytes.len())
            .min(image_size);
        memory.as_mut_slice()[..header_len].copy_from_slice(&pe.raw_bytes[..header_len]);

        // 2. Map every section
        for sec in &pe.sections {
            let va = sec.virtual_address as usize;
            let raw_off = sec.pointer_to_raw_data as usize;
            let raw_len = sec.size_of_raw_data as usize;
            let virt_len = (sec.virtual_size as usize).max(raw_len);
            let end = va.saturating_add(virt_len);
            if end > memory.len() {
                return Err(LoaderError::SectionOutOfBounds {
                    name: sec.name.clone(),
                    end,
                    image_size: memory.len(),
                });
            }
            if raw_len > 0 && raw_off < pe.raw_bytes.len() {
                let copy_len = raw_len.min(pe.raw_bytes.len() - raw_off);
                memory.as_mut_slice()[va..va + copy_len]
                    .copy_from_slice(&pe.raw_bytes[raw_off..raw_off + copy_len]);
            }
        }

        // 3. Apply base relocations if mapped away from preferred ImageBase
        let loaded_base = memory.base_addr() as u64;
        let delta = loaded_base.wrapping_sub(pe.image_base) as i64;
        let mut relocations_applied = 0usize;
        if delta != 0 {
            let buf = memory.as_mut_slice();
            for block in &pe.relocations {
                for entry in &block.entries {
                    let rva = block.page_rva.saturating_add(entry.offset as u32);
                    let off = rva as usize;
                    match entry.kind {
                        IMAGE_REL_BASED_ABSOLUTE => {}
                        IMAGE_REL_BASED_DIR64 => {
                            if off + 8 > buf.len() {
                                return Err(LoaderError::UnsupportedRelocation {
                                    kind: entry.kind,
                                    rva,
                                });
                            }
                            let orig = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
                            let patched = orig.wrapping_add(delta as u64);
                            buf[off..off + 8].copy_from_slice(&patched.to_le_bytes());
                            relocations_applied += 1;
                        }
                        IMAGE_REL_BASED_HIGHLOW => {
                            if off + 4 > buf.len() {
                                return Err(LoaderError::UnsupportedRelocation {
                                    kind: entry.kind,
                                    rva,
                                });
                            }
                            let orig = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                            let patched = orig.wrapping_add(delta as u32);
                            buf[off..off + 4].copy_from_slice(&patched.to_le_bytes());
                            relocations_applied += 1;
                        }
                        other => {
                            return Err(LoaderError::UnsupportedRelocation { kind: other, rva });
                        }
                    }
                }
            }
        }

        // 4. Resolve & patch Import Address Table (IAT)
        let mut resolved_imports = Vec::new();
        {
            let buf = memory.as_mut_slice();
            for desc in &pe.imports {
                for entry in &desc.entries {
                    let Some((addr, is_stub)) = resolver.resolve(&desc.dll_name, &entry.symbol)
                    else {
                        return Err(LoaderError::UnresolvedImport {
                            dll: desc.dll_name.clone(),
                            symbol: entry.symbol.clone(),
                        });
                    };
                    let off = entry.iat_rva as usize;
                    if off + 8 > buf.len() {
                        return Err(LoaderError::Pe(PeError::InvalidRva(entry.iat_rva)));
                    }
                    buf[off..off + 8].copy_from_slice(&(addr as u64).to_le_bytes());
                    resolved_imports.push(ResolvedImport {
                        dll: desc.dll_name.clone(),
                        symbol: entry.symbol.clone(),
                        iat_rva: entry.iat_rva,
                        target_addr: addr,
                        is_stub,
                    });
                }
            }
        }

        // 5. Read relocated TLS Directory (write TLS index + collect callback VAs)
        let tls_callbacks = collect_tls_callbacks(pe, &mut memory);

        // 6. Build runtime export lookup tables (for DLLs & self-exporting EXEs)
        let mut exports_by_name = HashMap::new();
        let mut exports_by_ordinal = HashMap::new();
        if let Some(exp_dir) = &pe.exports {
            for e in &exp_dir.entries {
                if e.forwarder.is_none() {
                    let va = memory.base_addr() + e.rva as usize;
                    exports_by_ordinal.insert(e.ordinal, va);
                    if let Some(name) = &e.name {
                        exports_by_name.insert(name.clone(), va);
                    }
                }
            }
        }

        // 7. Apply per-section OS page protections
        memory.protect(0, pe.size_of_headers as usize, PageProtection::Read)?;
        for sec in &pe.sections {
            let prot = section_protection(sec.characteristics);
            let span = (sec.virtual_size as usize).max(sec.size_of_raw_data as usize);
            memory.protect(sec.virtual_address as usize, span, prot)?;
        }

        // 8. Register `.pdata` x64 SEH unwind table with Windows OS (no-op on Unix)
        let pdata_registered_ptr = register_pdata_table(pe, &memory);

        if pe.entry_point_rva != 0 && pe.entry_point_rva as usize >= memory.len() {
            return Err(LoaderError::InvalidEntryPoint(pe.entry_point_rva));
        }

        Ok(Self {
            memory,
            preferred_base: pe.image_base,
            entry_point_rva: pe.entry_point_rva,
            is_dll: pe.is_dll(),
            relocations_applied,
            resolved_imports,
            exports_by_name,
            exports_by_ordinal,
            tls_callbacks,
            tls_attached: AtomicBool::new(false),
            runtime_functions: pe.runtime_functions.clone(),
            pdata_registered_ptr,
        })
    }

    pub fn base_addr(&self) -> usize {
        self.memory.base_addr()
    }

    pub fn image_len(&self) -> usize {
        self.memory.len()
    }

    pub fn preferred_base(&self) -> u64 {
        self.preferred_base
    }

    pub fn is_dll(&self) -> bool {
        self.is_dll
    }

    pub fn entry_point_va(&self) -> usize {
        self.memory.base_addr() + self.entry_point_rva as usize
    }

    pub fn relocations_applied(&self) -> usize {
        self.relocations_applied
    }

    pub fn resolved_imports(&self) -> &[ResolvedImport] {
        &self.resolved_imports
    }

    pub fn tls_callbacks(&self) -> &[usize] {
        &self.tls_callbacks
    }

    pub fn runtime_functions(&self) -> &[RuntimeFunction] {
        &self.runtime_functions
    }

    pub fn is_pdata_registered(&self) -> bool {
        self.pdata_registered_ptr.is_some()
    }

    /// Look up the `.pdata` `RuntimeFunction` covering a guest virtual address.
    pub fn find_runtime_function(&self, va: usize) -> Option<RuntimeFunction> {
        let base = self.base_addr();
        if va < base || va >= base + self.memory.len() {
            return None;
        }
        let rva = (va - base) as u32;
        self.runtime_functions
            .iter()
            .copied()
            .find(|rf| rva >= rf.begin_address && rva < rf.end_address)
    }

    /// Look up an exported function virtual address by name or ordinal.
    pub fn resolve_export(&self, symbol: &ImportSymbol) -> Option<usize> {
        match symbol {
            ImportSymbol::Name { name, .. } => self.exports_by_name.get(name).copied(),
            ImportSymbol::Ordinal(ord) => self.exports_by_ordinal.get(ord).copied(),
        }
    }

    pub fn export_count(&self) -> usize {
        self.exports_by_ordinal.len()
    }

    /// Invoke all PE TLS callbacks (`PIMAGE_TLS_CALLBACK`) with `reason`.
    ///
    /// # Safety
    /// Executes mapped guest machine code callbacks in the current process.
    pub unsafe fn run_tls_callbacks(&self, reason: u32) -> usize {
        let handle = self.base_addr() as *mut c_void;
        for &cb_va in &self.tls_callbacks {
            let cb: TlsCallbackFn = std::mem::transmute(cb_va);
            cb(handle, reason, ptr::null_mut());
        }
        if reason == DLL_PROCESS_ATTACH {
            self.tls_attached.store(true, Ordering::SeqCst);
        } else if reason == DLL_PROCESS_DETACH {
            self.tls_attached.store(false, Ordering::SeqCst);
        }
        self.tls_callbacks.len()
    }

    /// Invoke `DllMain(hinstDLL, reason, NULL)` for a loaded guest DLL.
    ///
    /// # Safety
    /// Executes mapped guest machine code in the current process.
    pub unsafe fn run_dll_main(&self, reason: u32) -> bool {
        if self.entry_point_rva == 0 {
            return true;
        }
        let dll_main: DllMainFn = std::mem::transmute(self.entry_point_va());
        dll_main(self.base_addr() as *mut c_void, reason, ptr::null_mut()) != 0
    }

    /// Execute the loaded guest PE32+ executable entry point natively via Win64 ABI.
    /// Automatically invokes PE TLS callbacks (`DLL_PROCESS_ATTACH` before, `DLL_PROCESS_DETACH` after).
    ///
    /// # Safety
    /// Executes machine code mapped from the loaded PE image inside the current process.
    pub unsafe fn execute_entry(&self) -> u32 {
        if !self.tls_callbacks.is_empty() && !self.tls_attached.load(Ordering::SeqCst) {
            self.run_tls_callbacks(DLL_PROCESS_ATTACH);
        }
        let entry: unsafe extern "win64" fn() -> u32 = std::mem::transmute(self.entry_point_va());
        let rc = entry();
        if !self.tls_callbacks.is_empty() && self.tls_attached.load(Ordering::SeqCst) {
            self.run_tls_callbacks(DLL_PROCESS_DETACH);
        }
        rc
    }
}

impl Drop for LoadedImage {
    fn drop(&mut self) {
        if let Some(table_ptr) = self.pdata_registered_ptr.take() {
            unregister_pdata_table(table_ptr);
        }
    }
}

fn collect_tls_callbacks(pe: &PeImage, memory: &mut VirtualMemory) -> Vec<usize> {
    let Some(dir) = pe.data_directory(IMAGE_DIRECTORY_ENTRY_TLS) else {
        return Vec::new();
    };
    let tls_off = dir.virtual_address as usize;
    let base = memory.base_addr();
    let img_end = base + memory.len();
    let buf = memory.as_mut_slice();
    if tls_off + 40 > buf.len() {
        return Vec::new();
    }

    // Read relocated 64-bit VAs directly from the mapped IMAGE_TLS_DIRECTORY64
    let index_va = u64::from_le_bytes(buf[tls_off + 16..tls_off + 24].try_into().unwrap()) as usize;
    let callbacks_va =
        u64::from_le_bytes(buf[tls_off + 24..tls_off + 32].try_into().unwrap()) as usize;

    // Assign a non-zero TLS slot index into *AddressOfIndex if inside the mapped image
    if index_va >= base && index_va + 4 <= img_end {
        let idx = NEXT_TLS_INDEX.fetch_add(1, Ordering::Relaxed);
        let idx_off = index_va - base;
        buf[idx_off..idx_off + 4].copy_from_slice(&idx.to_le_bytes());
    }

    let mut callbacks = Vec::new();
    if callbacks_va >= base && callbacks_va + 8 <= img_end {
        let mut off = callbacks_va - base;
        while off + 8 <= buf.len() {
            let cb_va = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap()) as usize;
            if cb_va == 0 {
                break;
            }
            if cb_va >= base && cb_va < img_end {
                callbacks.push(cb_va);
            }
            off += 8;
        }
    }
    callbacks
}

#[cfg(target_os = "windows")]
fn register_pdata_table(pe: &PeImage, memory: &VirtualMemory) -> Option<usize> {
    let dir = pe.data_directory(crate::pe::IMAGE_DIRECTORY_ENTRY_EXCEPTION)?;
    let count = (dir.size as usize) / 12;
    if count == 0 {
        return None;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn RtlAddFunctionTable(
            function_table: *mut RuntimeFunction,
            entry_count: u32,
            base_address: u64,
        ) -> u8;
    }
    let ptr = unsafe { memory.base().add(dir.virtual_address as usize) as *mut RuntimeFunction };
    let ok = unsafe { RtlAddFunctionTable(ptr, count as u32, memory.base_addr() as u64) };
    if ok != 0 {
        Some(ptr as usize)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn unregister_pdata_table(table_ptr: usize) {
    #[link(name = "kernel32")]
    extern "system" {
        fn RtlDeleteFunctionTable(function_table: *mut RuntimeFunction) -> u8;
    }
    unsafe {
        let _ = RtlDeleteFunctionTable(table_ptr as *mut RuntimeFunction);
    }
}

#[cfg(not(target_os = "windows"))]
fn register_pdata_table(_pe: &PeImage, _memory: &VirtualMemory) -> Option<usize> {
    None
}

#[cfg(not(target_os = "windows"))]
fn unregister_pdata_table(_table_ptr: usize) {}

fn section_protection(characteristics: u32) -> PageProtection {
    let r = (characteristics & IMAGE_SCN_MEM_READ) != 0;
    let w = (characteristics & IMAGE_SCN_MEM_WRITE) != 0;
    let x = (characteristics & IMAGE_SCN_MEM_EXECUTE) != 0;
    match (r, w, x) {
        (_, true, true) => PageProtection::ReadWriteExecute,
        (_, false, true) => PageProtection::ReadExecute,
        (_, true, false) => PageProtection::ReadWrite,
        (true, false, false) => PageProtection::Read,
        (false, false, false) => PageProtection::NoAccess,
    }
}
