//! PE32+ (AMD64) binary loader, base relocator, and IAT resolver for Xbox Series X|S titles.

pub mod loader;
pub mod memory;
pub mod pe;

pub use loader::{ImportResolver, LoadedImage, LoaderError, ResolvedImport};
pub use memory::{PageProtection, VirtualMemory};
pub use pe::{
    DataDirectory, ImportDescriptor, ImportEntry, ImportSymbol, PeError, PeImage, RelocBlock,
    RelocEntry, SectionHeader, IMAGE_FILE_MACHINE_AMD64, IMAGE_REL_BASED_ABSOLUTE,
    IMAGE_REL_BASED_DIR64, IMAGE_REL_BASED_HIGHLOW, IMAGE_SCN_MEM_EXECUTE, IMAGE_SCN_MEM_READ,
    IMAGE_SCN_MEM_WRITE, PE32_PLUS_MAGIC,
};
