//! Spec-compliant PE32+ (AMD64) parser for Xbox Series X|S & Windows x64 binaries.
//!
//! Supports:
//! - DOS, COFF, and PE32+ Optional Headers + Section Headers
//! - Import Directory Table (ILT/IAT, named & ordinal imports)
//! - Export Directory Table (named, ordinal, and forwarder exports)
//! - Base Relocation Table (`DIR64`, `HIGHLOW`, `ABSOLUTE`)
//! - TLS Directory (`IMAGE_TLS_DIRECTORY64`)
//! - Exception Directory (`.pdata` `RUNTIME_FUNCTION` entries)

use std::fmt;

use xsx_abi::gdk::RuntimeFunction;

pub const DOS_SIGNATURE: u16 = 0x5A4D; // "MZ"
pub const PE_SIGNATURE: u32 = 0x0000_4550; // "PE\0\0"
pub const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;
pub const PE32_PLUS_MAGIC: u16 = 0x020B;
pub const IMAGE_FILE_DLL: u16 = 0x2000;

pub const IMAGE_DIRECTORY_ENTRY_EXPORT: usize = 0;
pub const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
pub const IMAGE_DIRECTORY_ENTRY_EXCEPTION: usize = 3;
pub const IMAGE_DIRECTORY_ENTRY_BASERELOC: usize = 5;
pub const IMAGE_DIRECTORY_ENTRY_TLS: usize = 9;
pub const IMAGE_DIRECTORY_ENTRY_IAT: usize = 12;

pub const IMAGE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub const IMAGE_SCN_MEM_READ: u32 = 0x4000_0000;
pub const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;

pub const IMAGE_REL_BASED_ABSOLUTE: u16 = 0;
pub const IMAGE_REL_BASED_HIGHLOW: u16 = 3;
pub const IMAGE_REL_BASED_DIR64: u16 = 10;

#[derive(Debug, Clone)]
pub enum PeError {
    Truncated(&'static str),
    InvalidDosSignature(u16),
    InvalidPeSignature(u32),
    UnsupportedMachine(u16),
    UnsupportedOptionalMagic(u16),
    InvalidRva(u32),
    MalformedTable(&'static str),
}

impl fmt::Display for PeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PeError::Truncated(ctx) => write!(f, "PE file truncated while reading {ctx}"),
            PeError::InvalidDosSignature(sig) => write!(f, "invalid DOS signature: 0x{sig:04X}"),
            PeError::InvalidPeSignature(sig) => write!(f, "invalid PE signature: 0x{sig:08X}"),
            PeError::UnsupportedMachine(m) => {
                write!(f, "unsupported COFF machine 0x{m:04X} (expected AMD64 0x8664)")
            }
            PeError::UnsupportedOptionalMagic(m) => {
                write!(f, "unsupported Optional Header magic 0x{m:04X} (expected PE32+ 0x020B)")
            }
            PeError::InvalidRva(rva) => write!(f, "RVA 0x{rva:08X} does not map into any section"),
            PeError::MalformedTable(ctx) => write!(f, "malformed PE directory table: {ctx}"),
        }
    }
}

impl std::error::Error for PeError {}

#[derive(Debug, Clone, Copy, Default)]
pub struct DataDirectory {
    pub virtual_address: u32,
    pub size: u32,
}

#[derive(Debug, Clone)]
pub struct SectionHeader {
    pub name: String,
    pub virtual_size: u32,
    pub virtual_address: u32,
    pub size_of_raw_data: u32,
    pub pointer_to_raw_data: u32,
    pub characteristics: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSymbol {
    Name { hint: u16, name: String },
    Ordinal(u16),
}

impl fmt::Display for ImportSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportSymbol::Name { name, .. } => write!(f, "{name}"),
            ImportSymbol::Ordinal(ord) => write!(f, "#{ord}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub symbol: ImportSymbol,
    pub iat_rva: u32,
}

#[derive(Debug, Clone)]
pub struct ImportDescriptor {
    pub dll_name: String,
    pub original_first_thunk: u32,
    pub first_thunk: u32,
    pub entries: Vec<ImportEntry>,
}

#[derive(Debug, Clone)]
pub struct ExportEntry {
    pub ordinal: u16,
    pub name: Option<String>,
    pub rva: u32,
    pub forwarder: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExportDirectory {
    pub dll_name: String,
    pub ordinal_base: u32,
    pub entries: Vec<ExportEntry>,
}

/// Parsed `IMAGE_TLS_DIRECTORY64` (40 bytes).
#[derive(Debug, Clone, Copy)]
pub struct TlsDirectory {
    pub start_address_of_raw_data: u64,
    pub end_address_of_raw_data: u64,
    pub address_of_index: u64,
    pub address_of_callbacks: u64,
    pub size_of_zero_fill: u32,
    pub characteristics: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct RelocEntry {
    pub kind: u16,
    pub offset: u16,
}

#[derive(Debug, Clone)]
pub struct RelocBlock {
    pub page_rva: u32,
    pub entries: Vec<RelocEntry>,
}

#[derive(Debug, Clone)]
pub struct PeImage {
    pub machine: u16,
    pub characteristics: u16,
    pub entry_point_rva: u32,
    pub image_base: u64,
    pub section_alignment: u32,
    pub file_alignment: u32,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub subsystem: u16,
    pub dll_characteristics: u16,
    pub data_directories: Vec<DataDirectory>,
    pub sections: Vec<SectionHeader>,
    pub imports: Vec<ImportDescriptor>,
    pub exports: Option<ExportDirectory>,
    pub relocations: Vec<RelocBlock>,
    pub tls: Option<TlsDirectory>,
    pub runtime_functions: Vec<RuntimeFunction>,
    pub raw_bytes: Vec<u8>,
}

impl PeImage {
    /// Parse a PE32+ (AMD64) binary from raw bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, PeError> {
        let dos_sig = read_u16(bytes, 0, "DOS signature")?;
        if dos_sig != DOS_SIGNATURE {
            return Err(PeError::InvalidDosSignature(dos_sig));
        }

        let pe_offset = read_u32(bytes, 0x3C, "e_lfanew")? as usize;
        let pe_sig = read_u32(bytes, pe_offset, "PE signature")?;
        if pe_sig != PE_SIGNATURE {
            return Err(PeError::InvalidPeSignature(pe_sig));
        }

        // COFF File Header (20 bytes immediately after PE signature)
        let coff = pe_offset + 4;
        let machine = read_u16(bytes, coff, "COFF machine")?;
        if machine != IMAGE_FILE_MACHINE_AMD64 {
            return Err(PeError::UnsupportedMachine(machine));
        }
        let num_sections = read_u16(bytes, coff + 2, "NumberOfSections")? as usize;
        let opt_header_size = read_u16(bytes, coff + 16, "SizeOfOptionalHeader")? as usize;
        let characteristics = read_u16(bytes, coff + 18, "Characteristics")?;

        // Optional Header (PE32+)
        let opt = coff + 20;
        if opt + opt_header_size > bytes.len() || opt_header_size < 112 {
            return Err(PeError::Truncated("Optional Header"));
        }
        let magic = read_u16(bytes, opt, "Optional Header magic")?;
        if magic != PE32_PLUS_MAGIC {
            return Err(PeError::UnsupportedOptionalMagic(magic));
        }

        let entry_point_rva = read_u32(bytes, opt + 16, "AddressOfEntryPoint")?;
        let image_base = read_u64(bytes, opt + 24, "ImageBase")?;
        let section_alignment = read_u32(bytes, opt + 32, "SectionAlignment")?;
        let file_alignment = read_u32(bytes, opt + 36, "FileAlignment")?;
        let size_of_image = read_u32(bytes, opt + 56, "SizeOfImage")?;
        let size_of_headers = read_u32(bytes, opt + 60, "SizeOfHeaders")?;
        let subsystem = read_u16(bytes, opt + 68, "Subsystem")?;
        let dll_characteristics = read_u16(bytes, opt + 70, "DllCharacteristics")?;
        let num_rva_and_sizes = read_u32(bytes, opt + 108, "NumberOfRvaAndSizes")? as usize;
        let dir_count = num_rva_and_sizes.min(16);

        let mut data_directories = Vec::with_capacity(dir_count);
        for i in 0..dir_count {
            let off = opt + 112 + i * 8;
            if off + 8 > opt + opt_header_size {
                break;
            }
            data_directories.push(DataDirectory {
                virtual_address: read_u32(bytes, off, "DataDirectory.VirtualAddress")?,
                size: read_u32(bytes, off + 4, "DataDirectory.Size")?,
            });
        }

        // Section Headers (40 bytes each, immediately after Optional Header)
        let sec_table = opt + opt_header_size;
        let mut sections = Vec::with_capacity(num_sections);
        for i in 0..num_sections {
            let off = sec_table + i * 40;
            if off + 40 > bytes.len() {
                return Err(PeError::Truncated("SectionHeader"));
            }
            let raw_name = &bytes[off..off + 8];
            let end = raw_name.iter().position(|&b| b == 0).unwrap_or(8);
            let name = String::from_utf8_lossy(&raw_name[..end]).into_owned();
            sections.push(SectionHeader {
                name,
                virtual_size: read_u32(bytes, off + 8, "Section.VirtualSize")?,
                virtual_address: read_u32(bytes, off + 12, "Section.VirtualAddress")?,
                size_of_raw_data: read_u32(bytes, off + 16, "Section.SizeOfRawData")?,
                pointer_to_raw_data: read_u32(bytes, off + 20, "Section.PointerToRawData")?,
                characteristics: read_u32(bytes, off + 36, "Section.Characteristics")?,
            });
        }

        let mut image = Self {
            machine,
            characteristics,
            entry_point_rva,
            image_base,
            section_alignment,
            file_alignment,
            size_of_image,
            size_of_headers,
            subsystem,
            dll_characteristics,
            data_directories,
            sections,
            imports: Vec::new(),
            exports: None,
            relocations: Vec::new(),
            tls: None,
            runtime_functions: Vec::new(),
            raw_bytes: bytes.to_vec(),
        };

        image.imports = image.parse_imports()?;
        image.exports = image.parse_exports()?;
        image.relocations = image.parse_relocations()?;
        image.tls = image.parse_tls()?;
        image.runtime_functions = image.parse_exception_dir()?;
        Ok(image)
    }

    pub fn is_dll(&self) -> bool {
        (self.characteristics & IMAGE_FILE_DLL) != 0
    }

    pub fn data_directory(&self, idx: usize) -> Option<DataDirectory> {
        self.data_directories
            .get(idx)
            .copied()
            .filter(|d| d.virtual_address != 0 && d.size != 0)
    }

    /// Translate an RVA into a file offset within `self.raw_bytes`.
    pub fn rva_to_file_offset(&self, rva: u32) -> Result<usize, PeError> {
        if rva < self.size_of_headers {
            return Ok(rva as usize);
        }
        for sec in &self.sections {
            let span = sec.virtual_size.max(sec.size_of_raw_data);
            if rva >= sec.virtual_address && rva < sec.virtual_address.saturating_add(span) {
                let delta = rva - sec.virtual_address;
                let file_off = sec.pointer_to_raw_data.saturating_add(delta) as usize;
                if file_off <= self.raw_bytes.len() {
                    return Ok(file_off);
                }
            }
        }
        Err(PeError::InvalidRva(rva))
    }

    /// Detect whether this PE imports known Xbox Series X / GDK DLLs.
    pub fn xbox_dll_imports(&self) -> Vec<&str> {
        const KNOWN_XBOX_DLLS: &[&str] = &[
            "d3d12_x.dll",
            "d3d12_xs.dll",
            "xgameruntime.dll",
            "kernelx.dll",
            "xuser.dll",
            "dstorage_x.dll",
            "xcurl.dll",
            "xaudio2_9.dll",
            "pixeventruntime.dll",
        ];
        self.imports
            .iter()
            .map(|d| d.dll_name.as_str())
            .filter(|name| {
                let lower = name.to_ascii_lowercase();
                KNOWN_XBOX_DLLS.iter().any(|&k| k == lower)
            })
            .collect()
    }

    fn read_cstr_at_rva(&self, rva: u32) -> Result<String, PeError> {
        let start = self.rva_to_file_offset(rva)?;
        let slice = &self.raw_bytes[start..];
        let len = slice
            .iter()
            .position(|&b| b == 0)
            .ok_or(PeError::MalformedTable("unterminated C string"))?;
        Ok(String::from_utf8_lossy(&slice[..len]).into_owned())
    }

    fn parse_imports(&self) -> Result<Vec<ImportDescriptor>, PeError> {
        let Some(dir) = self.data_directory(IMAGE_DIRECTORY_ENTRY_IMPORT) else {
            return Ok(Vec::new());
        };
        let mut off = self.rva_to_file_offset(dir.virtual_address)?;
        let mut out = Vec::new();

        loop {
            if off + 20 > self.raw_bytes.len() {
                return Err(PeError::Truncated("ImportDescriptor"));
            }
            let orig_first_thunk = read_u32(&self.raw_bytes, off, "ILT")?;
            let _time_date_stamp = read_u32(&self.raw_bytes, off + 4, "TimeDateStamp")?;
            let _forwarder_chain = read_u32(&self.raw_bytes, off + 8, "ForwarderChain")?;
            let name_rva = read_u32(&self.raw_bytes, off + 12, "ImportNameRva")?;
            let first_thunk = read_u32(&self.raw_bytes, off + 16, "IAT")?;

            if orig_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
                break;
            }

            let dll_name = self.read_cstr_at_rva(name_rva)?;
            let lookup_rva = if orig_first_thunk != 0 {
                orig_first_thunk
            } else {
                first_thunk
            };

            let mut entries = Vec::new();
            let mut idx = 0u32;
            loop {
                let thunk_rva = lookup_rva + idx * 8;
                let thunk_off = self.rva_to_file_offset(thunk_rva)?;
                let thunk_val = read_u64(&self.raw_bytes, thunk_off, "ImportThunk64")?;
                if thunk_val == 0 {
                    break;
                }
                let iat_rva = first_thunk + idx * 8;
                let symbol = if (thunk_val & (1u64 << 63)) != 0 {
                    ImportSymbol::Ordinal((thunk_val & 0xFFFF) as u16)
                } else {
                    let hint_rva = (thunk_val & 0x7FFF_FFFF) as u32;
                    let hint_off = self.rva_to_file_offset(hint_rva)?;
                    let hint = read_u16(&self.raw_bytes, hint_off, "ImportHint")?;
                    let name = self.read_cstr_at_rva(hint_rva + 2)?;
                    ImportSymbol::Name { hint, name }
                };
                entries.push(ImportEntry { symbol, iat_rva });
                idx += 1;
            }

            out.push(ImportDescriptor {
                dll_name,
                original_first_thunk: orig_first_thunk,
                first_thunk,
                entries,
            });
            off += 20;
        }
        Ok(out)
    }

    fn parse_exports(&self) -> Result<Option<ExportDirectory>, PeError> {
        let Some(dir) = self.data_directory(IMAGE_DIRECTORY_ENTRY_EXPORT) else {
            return Ok(None);
        };
        let off = self.rva_to_file_offset(dir.virtual_address)?;
        if off + 40 > self.raw_bytes.len() {
            return Err(PeError::Truncated("ExportDirectory"));
        }
        let name_rva = read_u32(&self.raw_bytes, off + 12, "Export.Name")?;
        let ordinal_base = read_u32(&self.raw_bytes, off + 16, "Export.Base")?;
        let num_functions = read_u32(&self.raw_bytes, off + 20, "Export.NumberOfFunctions")? as usize;
        let num_names = read_u32(&self.raw_bytes, off + 24, "Export.NumberOfNames")? as usize;
        let addr_funcs_rva = read_u32(&self.raw_bytes, off + 28, "Export.AddressOfFunctions")?;
        let addr_names_rva = read_u32(&self.raw_bytes, off + 32, "Export.AddressOfNames")?;
        let addr_ords_rva = read_u32(&self.raw_bytes, off + 36, "Export.AddressOfNameOrdinals")?;

        let dll_name = if name_rva != 0 {
            self.read_cstr_at_rva(name_rva)?
        } else {
            String::new()
        };

        // Map function-table index -> export name
        let mut names_by_idx: Vec<Option<String>> = vec![None; num_functions];
        if num_names > 0 && addr_names_rva != 0 && addr_ords_rva != 0 {
            let names_off = self.rva_to_file_offset(addr_names_rva)?;
            let ords_off = self.rva_to_file_offset(addr_ords_rva)?;
            for i in 0..num_names {
                let fn_name_rva = read_u32(&self.raw_bytes, names_off + i * 4, "ExportNameRva")?;
                let ord_idx = read_u16(&self.raw_bytes, ords_off + i * 2, "ExportOrdinal")? as usize;
                if ord_idx < num_functions {
                    names_by_idx[ord_idx] = Some(self.read_cstr_at_rva(fn_name_rva)?);
                }
            }
        }

        let funcs_off = self.rva_to_file_offset(addr_funcs_rva)?;
        let dir_start = dir.virtual_address;
        let dir_end = dir.virtual_address.saturating_add(dir.size);
        let mut entries = Vec::with_capacity(num_functions);

        for idx in 0..num_functions {
            let fn_rva = read_u32(&self.raw_bytes, funcs_off + idx * 4, "ExportFunctionRva")?;
            if fn_rva == 0 {
                continue;
            }
            let forwarder = if fn_rva >= dir_start && fn_rva < dir_end {
                Some(self.read_cstr_at_rva(fn_rva)?)
            } else {
                None
            };
            entries.push(ExportEntry {
                ordinal: (ordinal_base + idx as u32) as u16,
                name: names_by_idx[idx].take(),
                rva: fn_rva,
                forwarder,
            });
        }

        Ok(Some(ExportDirectory {
            dll_name,
            ordinal_base,
            entries,
        }))
    }

    fn parse_relocations(&self) -> Result<Vec<RelocBlock>, PeError> {
        let Some(dir) = self.data_directory(IMAGE_DIRECTORY_ENTRY_BASERELOC) else {
            return Ok(Vec::new());
        };
        let start = self.rva_to_file_offset(dir.virtual_address)?;
        let end = start.saturating_add(dir.size as usize).min(self.raw_bytes.len());
        let mut cursor = start;
        let mut blocks = Vec::new();

        while cursor + 8 <= end {
            let page_rva = read_u32(&self.raw_bytes, cursor, "Reloc.PageRva")?;
            let block_size = read_u32(&self.raw_bytes, cursor + 4, "Reloc.BlockSize")? as usize;
            if block_size < 8 || cursor + block_size > end {
                break;
            }
            let count = (block_size - 8) / 2;
            let mut entries = Vec::with_capacity(count);
            for i in 0..count {
                let raw = read_u16(&self.raw_bytes, cursor + 8 + i * 2, "Reloc.Entry")?;
                entries.push(RelocEntry {
                    kind: raw >> 12,
                    offset: raw & 0x0FFF,
                });
            }
            blocks.push(RelocBlock { page_rva, entries });
            cursor += block_size;
        }

        Ok(blocks)
    }

    fn parse_tls(&self) -> Result<Option<TlsDirectory>, PeError> {
        let Some(dir) = self.data_directory(IMAGE_DIRECTORY_ENTRY_TLS) else {
            return Ok(None);
        };
        let off = self.rva_to_file_offset(dir.virtual_address)?;
        if off + 40 > self.raw_bytes.len() {
            return Err(PeError::Truncated("IMAGE_TLS_DIRECTORY64"));
        }
        Ok(Some(TlsDirectory {
            start_address_of_raw_data: read_u64(&self.raw_bytes, off, "Tls.StartRaw")?,
            end_address_of_raw_data: read_u64(&self.raw_bytes, off + 8, "Tls.EndRaw")?,
            address_of_index: read_u64(&self.raw_bytes, off + 16, "Tls.Index")?,
            address_of_callbacks: read_u64(&self.raw_bytes, off + 24, "Tls.Callbacks")?,
            size_of_zero_fill: read_u32(&self.raw_bytes, off + 32, "Tls.ZeroFill")?,
            characteristics: read_u32(&self.raw_bytes, off + 36, "Tls.Characteristics")?,
        }))
    }

    fn parse_exception_dir(&self) -> Result<Vec<RuntimeFunction>, PeError> {
        let Some(dir) = self.data_directory(IMAGE_DIRECTORY_ENTRY_EXCEPTION) else {
            return Ok(Vec::new());
        };
        let start = self.rva_to_file_offset(dir.virtual_address)?;
        let count = (dir.size as usize) / 12;
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let off = start + i * 12;
            if off + 12 > self.raw_bytes.len() {
                break;
            }
            let begin = read_u32(&self.raw_bytes, off, "Pdata.BeginAddress")?;
            let end = read_u32(&self.raw_bytes, off + 4, "Pdata.EndAddress")?;
            let unwind = read_u32(&self.raw_bytes, off + 8, "Pdata.UnwindInfoAddress")?;
            if begin == 0 && end == 0 && unwind == 0 {
                continue;
            }
            out.push(RuntimeFunction {
                begin_address: begin,
                end_address: end,
                unwind_info_address: unwind,
            });
        }
        Ok(out)
    }
}

fn read_u16(bytes: &[u8], off: usize, ctx: &'static str) -> Result<u16, PeError> {
    let slice = bytes.get(off..off + 2).ok_or(PeError::Truncated(ctx))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], off: usize, ctx: &'static str) -> Result<u32, PeError> {
    let slice = bytes.get(off..off + 4).ok_or(PeError::Truncated(ctx))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], off: usize, ctx: &'static str) -> Result<u64, PeError> {
    let slice = bytes.get(off..off + 8).ok_or(PeError::Truncated(ctx))?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}
