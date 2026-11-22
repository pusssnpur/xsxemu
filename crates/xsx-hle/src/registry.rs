//! Xbox OS & Win32 DLL import registry with API-set forwarding and optional stubbing.

use std::collections::HashMap;

use xsx_abi::com::S_OK;
use xsx_loader::{ImportResolver, ImportSymbol};

use crate::{d3d12_x, dstorage, input, kernelx, services, xaudio2, xgameruntime};

/// Generic fallback Win64 stub returning `S_OK` (0).
unsafe extern "win64" fn unimplemented_stub() -> i32 {
    if let Some(rt) = crate::runtime::current_runtime() {
        rt.trace("WARNING: called unimplemented HLE stub -> returning S_OK");
    }
    S_OK
}

/// Maps normalized DLL names and export names to Win64-callable Rust HLE thunks.
pub struct ModuleRegistry {
    exports: HashMap<(String, String), usize>,
    allow_stubs: bool,
}

impl ModuleRegistry {
    /// Construct the default Xbox Series X HLE registry with all Phase 1–4 modules registered.
    pub fn new_default(allow_stubs: bool) -> Self {
        let mut reg = Self {
            exports: HashMap::new(),
            allow_stubs,
        };
        kernelx::register(&mut reg);
        xgameruntime::register(&mut reg);
        services::register(&mut reg);
        d3d12_x::register(&mut reg);
        input::register(&mut reg);
        xaudio2::register(&mut reg);
        dstorage::register(&mut reg);
        reg
    }

    pub fn register_export(&mut self, dll: &str, name: &str, addr: usize) {
        self.exports
            .insert((normalize_dll(dll), name.to_string()), addr);
    }

    pub fn export_count(&self) -> usize {
        self.exports.len()
    }
}

impl ImportResolver for ModuleRegistry {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Option<(usize, bool)> {
        let norm_dll = normalize_dll(dll);
        if let ImportSymbol::Name { name, .. } = symbol {
            if let Some(&addr) = self.exports.get(&(norm_dll, name.clone())) {
                return Some((addr, false));
            }
        }
        if self.allow_stubs {
            Some((unimplemented_stub as *const () as usize, true))
        } else {
            None
        }
    }
}

/// Normalize DLL names and redirect Windows/Xbox API-set names to canonical HLE modules.
pub fn normalize_dll(dll: &str) -> String {
    let lower = dll.to_ascii_lowercase();
    if lower.starts_with("api-ms-win-core-")
        || lower == "kernel32.dll"
        || lower == "kernelbase.dll"
        || lower == "kernelx.dll"
    {
        return "kernelx.dll".to_string();
    }
    if lower == "d3d12_xs.dll" || lower == "d3d12_x.dll" || lower == "d3d12.dll" {
        return "d3d12_x.dll".to_string();
    }
    if lower == "xuser.dll" || lower == "xstore.dll" || lower == "xgamesave.dll" {
        return "xgameruntime.dll".to_string();
    }
    lower
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_set_forwarding_resolves_kernelx() {
        let reg = ModuleRegistry::new_default(false);
        let sym = ImportSymbol::Name {
            hint: 0,
            name: "QueryPerformanceCounter".into(),
        };
        let resolved = reg.resolve("api-ms-win-core-profile-l1-1-0.dll", &sym);
        assert!(resolved.is_some());
        assert!(!resolved.unwrap().1);
    }

    #[test]
    fn d3d12_xs_aliases_d3d12_x() {
        let reg = ModuleRegistry::new_default(false);
        let sym = ImportSymbol::Name {
            hint: 0,
            name: "D3D12XBOX_CreateDevice".into(),
        };
        assert!(reg.resolve("d3d12_xs.dll", &sym).is_some());
    }

    #[test]
    fn phase4_modules_resolve_cleanly() {
        let reg = ModuleRegistry::new_default(false);
        for (dll, name) in [
            ("xinput1_4.dll", "XInputGetState"),
            ("gameinput.dll", "GameInputCreate"),
            ("xaudio2_9.dll", "XAudio2Create"),
            ("dstorage_x.dll", "DStorageGetFactory"),
            ("xuser.dll", "XUserGetGamertag"),
            ("xgamesave.dll", "XGameSaveInitializeProvider"),
        ] {
            let sym = ImportSymbol::Name {
                hint: 0,
                name: name.into(),
            };
            let res = reg.resolve(dll, &sym);
            assert!(res.is_some(), "expected {dll}!{name} to resolve");
            assert!(!res.unwrap().1);
        }
    }
}
