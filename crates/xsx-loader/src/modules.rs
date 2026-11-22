//! Multi-module PE32+ loader supporting secondary guest DLLs, recursive dependency
//! resolution, `DllMain` initialization, and runtime `GetModuleHandle`/`GetProcAddress`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xsx_abi::gdk::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};

use crate::loader::{ImportResolver, LoadedImage, LoaderError};
use crate::pe::{ImportSymbol, PeImage};

/// A primary guest PE32+ executable together with all recursively loaded secondary guest DLLs.
pub struct LoadedModuleSet {
    main_name: String,
    main_image: Arc<LoadedImage>,
    /// Loaded guest DLLs keyed by lowercase DLL filename (e.g. `"xsx_helper.dll"`).
    dlls: HashMap<String, Arc<LoadedImage>>,
    /// Initialization order (dependencies first) for clean reverse-order teardown.
    load_order: Vec<String>,
    search_dirs: Vec<PathBuf>,
    force_relocate: bool,
}

impl LoadedModuleSet {
    /// Load a primary PE32+ executable and recursively load any secondary guest DLLs
    /// found in `search_dirs` that are not handled by `hle_resolver`.
    ///
    /// # Safety
    /// Invokes TLS callbacks and `DllMain(DLL_PROCESS_ATTACH)` of loaded secondary guest DLLs.
    pub unsafe fn load_with_dependencies(
        main_name: impl Into<String>,
        main_pe: &PeImage,
        hle_resolver: &dyn ImportResolver,
        search_dirs: Vec<PathBuf>,
        force_relocate: bool,
    ) -> Result<Self, LoaderError> {
        let mut dlls: HashMap<String, Arc<LoadedImage>> = HashMap::new();
        let mut load_order: Vec<String> = Vec::new();
        let mut visiting: HashSet<String> = HashSet::new();

        // Recursively load every non-HLE DLL imported by main_pe (eager & delay-load)
        for desc in main_pe.imports.iter().chain(main_pe.delay_imports.iter()) {
            let key = desc.dll_name.to_ascii_lowercase();
            if should_load_guest_dll(&desc.dll_name, &desc.entries, hle_resolver) {
                load_guest_dll_recursive(
                    &key,
                    hle_resolver,
                    &search_dirs,
                    force_relocate,
                    &mut dlls,
                    &mut load_order,
                    &mut visiting,
                )?;
            }
        }

        let composite = CompositeResolver {
            hle: hle_resolver,
            dlls: &dlls,
        };
        let main_image = Arc::new(LoadedImage::load(main_pe, &composite, force_relocate)?);

        Ok(Self {
            main_name: main_name.into(),
            main_image,
            dlls,
            load_order,
            search_dirs,
            force_relocate,
        })
    }

    pub fn main_image(&self) -> &Arc<LoadedImage> {
        &self.main_image
    }

    pub fn main_name(&self) -> &str {
        &self.main_name
    }

    pub fn loaded_dll_names(&self) -> &[String] {
        &self.load_order
    }

    pub fn get_module(&self, name: Option<&str>) -> Option<Arc<LoadedImage>> {
        match name {
            None => Some(Arc::clone(&self.main_image)),
            Some(n) => {
                let lower = n.to_ascii_lowercase();
                if lower == self.main_name.to_ascii_lowercase() {
                    return Some(Arc::clone(&self.main_image));
                }
                self.dlls
                    .get(&lower)
                    .or_else(|| self.dlls.get(&format!("{lower}.dll")))
                    .cloned()
            }
        }
    }

    pub fn get_module_by_base(&self, base_addr: usize) -> Option<Arc<LoadedImage>> {
        if self.main_image.base_addr() == base_addr {
            return Some(Arc::clone(&self.main_image));
        }
        self.dlls
            .values()
            .find(|img| img.base_addr() == base_addr)
            .cloned()
    }

    pub fn resolve_proc_address(&self, module_base: usize, symbol: &ImportSymbol) -> Option<usize> {
        self.get_module_by_base(module_base)
            .and_then(|img| img.resolve_export(symbol))
    }

    /// Dynamically load an additional guest DLL at runtime (backing `LoadLibraryA`).
    ///
    /// # Safety
    /// Executes TLS callbacks and `DllMain(DLL_PROCESS_ATTACH)` of the loaded guest DLL.
    pub unsafe fn load_library(
        &mut self,
        dll_name: &str,
        hle_resolver: &dyn ImportResolver,
    ) -> Result<Arc<LoadedImage>, LoaderError> {
        let key = normalize_dll_filename(dll_name);
        if let Some(existing) = self.dlls.get(&key) {
            return Ok(Arc::clone(existing));
        }
        let mut visiting = HashSet::new();
        load_guest_dll_recursive(
            &key,
            hle_resolver,
            &self.search_dirs,
            self.force_relocate,
            &mut self.dlls,
            &mut self.load_order,
            &mut visiting,
        )?;
        Ok(Arc::clone(&self.dlls[&key]))
    }

    /// Execute the primary PE32+ image entry point (including TLS callbacks).
    ///
    /// # Safety
    /// Executes guest x86_64 machine code in the current process.
    pub unsafe fn execute_main(&self) -> u32 {
        self.main_image.execute_entry()
    }
}

impl Drop for LoadedModuleSet {
    fn drop(&mut self) {
        // Detach secondary guest DLLs in reverse load order
        for key in self.load_order.iter().rev() {
            if let Some(img) = self.dlls.get(key) {
                unsafe {
                    let _ = img.run_dll_main(DLL_PROCESS_DETACH);
                    if !img.tls_callbacks().is_empty() {
                        img.run_tls_callbacks(DLL_PROCESS_DETACH);
                    }
                }
            }
        }
    }
}

struct CompositeResolver<'a> {
    hle: &'a dyn ImportResolver,
    dlls: &'a HashMap<String, Arc<LoadedImage>>,
}

impl<'a> ImportResolver for CompositeResolver<'a> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Option<(usize, bool)> {
        let key = dll.to_ascii_lowercase();
        if let Some(guest_dll) = self.dlls.get(&key) {
            if let Some(va) = guest_dll.resolve_export(symbol) {
                return Some((va, false));
            }
        }
        self.hle.resolve(dll, symbol)
    }
}

fn should_load_guest_dll(
    dll_name: &str,
    entries: &[crate::pe::ImportEntry],
    hle_resolver: &dyn ImportResolver,
) -> bool {
    // If HLE resolves the first imported symbol as a real (non-stub) export, use HLE.
    if let Some(first) = entries.first() {
        if let Some((_, is_stub)) = hle_resolver.resolve(dll_name, &first.symbol) {
            if !is_stub {
                return false;
            }
        }
    }
    true
}

fn normalize_dll_filename(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".dll") {
        lower
    } else {
        format!("{lower}.dll")
    }
}

fn find_dll_in_dirs(dll_lower: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        let direct = dir.join(dll_lower);
        if direct.is_file() {
            return Some(direct);
        }
        // Case-insensitive directory scan (important on Linux hosts for Windows DLL names)
        if let Ok(read_dir) = fs::read_dir(dir) {
            for entry in read_dir.flatten() {
                if let Some(fname) = entry.file_name().to_str() {
                    if fname.to_ascii_lowercase() == dll_lower && entry.path().is_file() {
                        return Some(entry.path());
                    }
                }
            }
        }
    }
    None
}

unsafe fn load_guest_dll_recursive(
    dll_key: &str,
    hle_resolver: &dyn ImportResolver,
    search_dirs: &[PathBuf],
    force_relocate: bool,
    dlls: &mut HashMap<String, Arc<LoadedImage>>,
    load_order: &mut Vec<String>,
    visiting: &mut HashSet<String>,
) -> Result<(), LoaderError> {
    if dlls.contains_key(dll_key) {
        return Ok(());
    }
    if !visiting.insert(dll_key.to_string()) {
        return Err(LoaderError::DependencyCycle(dll_key.to_string()));
    }

    let Some(path) = find_dll_in_dirs(dll_key, search_dirs) else {
        visiting.remove(dll_key);
        // Not present on disk: leave it for the HLE resolver (which may stub or report unresolved).
        return Ok(());
    };

    let bytes = fs::read(&path).map_err(|err| LoaderError::Io {
        path: path.display().to_string(),
        err,
    })?;
    let pe = PeImage::parse(&bytes)?;

    for desc in &pe.imports {
        let dep_key = desc.dll_name.to_ascii_lowercase();
        if should_load_guest_dll(&desc.dll_name, &desc.entries, hle_resolver) {
            load_guest_dll_recursive(
                &dep_key,
                hle_resolver,
                search_dirs,
                force_relocate,
                dlls,
                load_order,
                visiting,
            )?;
        }
    }

    let composite = CompositeResolver {
        hle: hle_resolver,
        dlls,
    };
    let loaded = Arc::new(LoadedImage::load(&pe, &composite, force_relocate)?);

    // Run TLS callbacks & DllMain(DLL_PROCESS_ATTACH)
    if !loaded.tls_callbacks().is_empty() {
        loaded.run_tls_callbacks(DLL_PROCESS_ATTACH);
    }
    if !loaded.run_dll_main(DLL_PROCESS_ATTACH) {
        return Err(LoaderError::DllMainFailed(dll_key.to_string()));
    }

    dlls.insert(dll_key.to_string(), loaded);
    load_order.push(dll_key.to_string());
    visiting.remove(dll_key);
    Ok(())
}

/// Extract parent directory of an executable path for default DLL search.
pub fn default_search_dirs(exe_path: &Path, extra_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(parent) = exe_path.parent() {
        if !parent.as_os_str().is_empty() {
            dirs.push(parent.to_path_buf());
        }
    }
    for d in extra_dirs {
        if !dirs.contains(d) {
            dirs.push(d.clone());
        }
    }
    dirs
}
