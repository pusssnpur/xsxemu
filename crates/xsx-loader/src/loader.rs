//! PE32+ image mapper, relocator, IAT patcher, and native Win64 ABI executor.

use std::fmt;
use std::io;

use crate::memory::{PageProtection, VirtualMemory};
use crate::pe::{
    ImportSymbol, PeError, PeImage, IMAGE_REL_BASED_ABSOLUTE, IMAGE_REL_BASED_DIR64,
    IMAGE_REL_BASED_HIGHLOW, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE,
};

#[derive(Debug)]
pub enum LoaderError {
    Pe(PeError),
    Memory(io::Error),
    UnresolvedImport { dll: String, symbol: ImportSymbol },
    SectionOutOfBounds { name: String, end: usize, image_size: usize },
    UnsupportedRelocation { kind: u16, rva: u32 },
    InvalidEntryPoint(u32),
}

impl fmt::Display for LoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoaderError::Pe(e) => write!(f, "PE parse error: {e}"),
            LoaderError::Memory(e) => write!(f, "virtual memory error: {e}"),
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

/// A PE32+ image mapped into executable host virtual memory, relocated, and IAT-patched.
pub struct LoadedImage {
    memory: VirtualMemory,
    preferred_base: u64,
    entry_point_rva: u32,
    relocations_applied: usize,
    resolved_imports: Vec<ResolvedImport>,
}

impl LoadedImage {
    /// Map, relocate, patch imports, and apply page protections for a parsed `PeImage`.
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
        let header_len = (pe.size_of_headers as usize).min(pe.raw_bytes.len()).min(image_size);
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

        // 5. Apply per-section OS page protections
        memory.protect(0, pe.size_of_headers as usize, PageProtection::Read)?;
        for sec in &pe.sections {
            let prot = section_protection(sec.characteristics);
            let span = (sec.virtual_size as usize).max(sec.size_of_raw_data as usize);
            memory.protect(sec.virtual_address as usize, span, prot)?;
        }

        if pe.entry_point_rva as usize >= memory.len() {
            return Err(LoaderError::InvalidEntryPoint(pe.entry_point_rva));
        }

        Ok(Self {
            memory,
            preferred_base: pe.image_base,
            entry_point_rva: pe.entry_point_rva,
            relocations_applied,
            resolved_imports,
        })
    }

    pub fn base_addr(&self) -> usize {
        self.memory.base_addr()
    }

    pub fn preferred_base(&self) -> u64 {
        self.preferred_base
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

    /// Execute the loaded guest PE32+ entry point natively using the Win64 calling convention.
    ///
    /// # Safety
    /// Executes machine code mapped from the loaded PE image inside the current process.
    pub unsafe fn execute_entry(&self) -> u32 {
        let entry: unsafe extern "win64" fn() -> u32 = std::mem::transmute(self.entry_point_va());
        entry()
    }
}

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
