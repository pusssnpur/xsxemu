//! PE32+ (AMD64) binary loader, secondary DLL loader, TLS runner, `.pdata` registrar,
//! base relocator, and IAT resolver for Xbox Series X|S titles.

pub mod loader;
pub mod manifest;
pub mod memory;
pub mod modules;
pub mod pe;
pub mod xvd;

pub use loader::{ImportResolver, LoadedImage, LoaderError, ResolvedImport};
pub use manifest::{GameConfig, GameExecutableEntry, ManifestError, TitleMemoryMode};
pub use memory::{PageProtection, VirtualMemory};
pub use modules::{default_search_dirs, LoadedModuleSet};
pub use pe::{
    DataDirectory, ExportDirectory, ExportEntry, ImportDescriptor, ImportEntry, ImportSymbol,
    PeError, PeImage, RelocBlock, RelocEntry, SectionHeader, TlsDirectory,
    IMAGE_DIRECTORY_ENTRY_BASERELOC, IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT,
    IMAGE_DIRECTORY_ENTRY_EXCEPTION, IMAGE_DIRECTORY_ENTRY_EXPORT, IMAGE_DIRECTORY_ENTRY_IMPORT,
    IMAGE_DIRECTORY_ENTRY_TLS, IMAGE_FILE_DLL, IMAGE_FILE_MACHINE_AMD64,
    IMAGE_REL_BASED_ABSOLUTE, IMAGE_REL_BASED_DIR64, IMAGE_REL_BASED_HIGHLOW,
    IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ, IMAGE_SCN_MEM_WRITE, PE32_PLUS_MAGIC,
};
pub use xvd::{
    build_synthetic_xvd, parse_xvd_header, XvdContainer, XvdError, XvdHeaderInfo,
    XVD_FLAG_DATA_INTEGRITY_DISABLED, XVD_FLAG_ENCRYPTION_DISABLED, XVD_FLAG_READ_ONLY,
    XVD_MAGIC, XVD_MAGIC_OFFSET, XVD_MIN_HEADER_SIZE,
};
