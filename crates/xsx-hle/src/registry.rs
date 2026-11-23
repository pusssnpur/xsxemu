//! Xbox OS & Win32 DLL import registry with API-set forwarding, owned pseudo-handle lookup,
//! and Leonida-style return-class unimplemented import policy (`log` / `fail` / `stop`).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use xsx_abi::com::{E_NOTIMPL, S_OK};
use xsx_loader::{ImportResolver, ImportSymbol};

use crate::{d3d12_x, dstorage, input, kernelx, process, services, xaudio2, xgameruntime, xmem};

/// Behavior when a guest binary invokes an import not yet implemented by HLE or bundled DLLs.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnimplementedPolicy {
    /// Log once and return `0` (`S_OK`).
    Log = 0,
    /// Log once and return `E_NOTIMPL` (`0x80004001`) for known HRESULT functions, `0` otherwise.
    Fail = 1,
    /// Log and record a stop diagnostic (returns `E_NOTIMPL` / `0` cleanly without aborting tests).
    Stop = 2,
}

impl UnimplementedPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "log" => Some(Self::Log),
            "fail" => Some(Self::Fail),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// Verified return type classification for an unimplemented import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnClass {
    HResult,
    Other,
}

static GLOBAL_UNIMPLEMENTED_POLICY: AtomicU8 = AtomicU8::new(UnimplementedPolicy::Log as u8);

pub fn set_unimplemented_policy(policy: UnimplementedPolicy) {
    GLOBAL_UNIMPLEMENTED_POLICY.store(policy as u8, Ordering::SeqCst);
}

pub fn get_unimplemented_policy() -> UnimplementedPolicy {
    match GLOBAL_UNIMPLEMENTED_POLICY.load(Ordering::SeqCst) {
        1 => UnimplementedPolicy::Fail,
        2 => UnimplementedPolicy::Stop,
        _ => UnimplementedPolicy::Log,
    }
}

const NOT_HRESULT_GDK: &[&str] = &[
    "XAsyncCancel",
    "XAsyncComplete",
    "XTaskQueueCloseHandle",
    "XTaskQueueDispatch",
    "XTaskQueueGetCurrentProcessTaskQueue",
    "XTaskQueueSetCurrentProcessTaskQueue",
    "XTaskQueueUnregisterMonitor",
    "XTaskQueueUnregisterWaiter",
    "XUserCloseHandle",
    "XUserCloseSignOutDeferralHandle",
    "XUserCompare",
    "XUserIsStoreUser",
    "XUserUnregisterForChangeEvent",
    "XUserUnregisterForDefaultAudioEndpointUtf16Changed",
    "XUserUnregisterForDeviceAssociationChanged",
    "XStoreCloseContextHandle",
    "XStoreCloseLicenseHandle",
    "XStoreCloseProductsQueryHandle",
    "XStoreIsLicenseValid",
    "XStoreProductsQueryHasMorePages",
    "XStoreUnregisterGameLicenseChanged",
    "XStoreUnregisterPackageLicenseLost",
    "XGameSaveCloseContainer",
    "XGameSaveCloseProvider",
    "XGameSaveCloseUpdate",
    "XPackageCloseInstallationMonitorHandle",
    "XPackageCloseMountHandle",
    "XPackageGetInstallationProgress",
    "XPackageIsPackagedProcess",
    "XPackageUninstallPackage",
    "XPackageUnregisterInstallationProgressChanged",
    "XPackageUnregisterPackageInstalled",
    "XPackageUpdateInstallationMonitor",
];

const GDK_HRESULT_FAMILIES: &[&str] = &[
    "XAsync",
    "XTaskQueue",
    "XUser",
    "XStore",
    "XGameSave",
    "XPackage",
    "XGameUi",
];

/// Classify whether a Microsoft GDK / D3D12 / DXGI / XAudio2 / DirectStorage import returns an
/// `HRESULT` (verified against public Microsoft documentation).
pub fn classify_import_return_class(dll: &str, name: &str) -> ReturnClass {
    let lower = dll.to_ascii_lowercase();
    if matches!(lower.as_str(), "d3d12.dll" | "d3d12_xs.dll" | "d3d12_x.dll") {
        if name.starts_with("D3D12Create")
            || name.starts_with("D3D12Serialize")
            || matches!(
                name,
                "D3D12GetDebugInterface"
                    | "D3D12EnableExperimentalFeatures"
                    | "D3D12XboxCreateDevice"
                    | "D3D12XBOX_CreateDevice"
                    | "D3D12XboxCreateSyntheticDevice"
            )
        {
            return ReturnClass::HResult;
        }
    } else if lower == "dxgi.dll" {
        if name.starts_with("CreateDXGIFactory")
            || matches!(
                name,
                "DXGIGetDebugInterface1" | "DXGIDeclareAdapterRemovalSupport"
            )
        {
            return ReturnClass::HResult;
        }
    } else if matches!(lower.as_str(), "xaudio2_9.dll" | "xaudio2_9redist.dll") {
        if name == "XAudio2Create" {
            return ReturnClass::HResult;
        }
    } else if lower == "gameinput.dll" {
        if name == "GameInputCreate" {
            return ReturnClass::HResult;
        }
    } else if matches!(
        lower.as_str(),
        "dstorage.dll" | "dstorage_xs.dll" | "dstorage_x.dll"
    ) {
        if name == "DStorageGetFactory" {
            return ReturnClass::HResult;
        }
    } else if matches!(
        lower.as_str(),
        "xgameruntime.dll" | "xuser.dll" | "xstore.dll" | "xgamesave.dll"
    ) {
        if matches!(name, "XGameRuntimeInitialize" | "XGameEventWrite") {
            return ReturnClass::HResult;
        }
        if NOT_HRESULT_GDK.contains(&name) {
            return ReturnClass::Other;
        }
        if GDK_HRESULT_FAMILIES.iter().any(|f| name.starts_with(f)) {
            return ReturnClass::HResult;
        }
    }
    ReturnClass::Other
}

fn seen_unimplemented() -> &'static Mutex<HashSet<String>> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

fn log_unimplemented_once(tag: &str, return_class: ReturnClass) {
    let first = seen_unimplemented().lock().unwrap().insert(tag.to_string());
    if first {
        if let Some(rt) = crate::runtime::current_runtime() {
            rt.trace(format!("WARNING: unimplemented import {tag} ({return_class:?})"));
            if let Some(st) = rt.startup_tracer() {
                st.record_unimplemented(
                    tag,
                    match return_class {
                        ReturnClass::HResult => "hresult",
                        ReturnClass::Other => "other",
                    },
                );
            }
        }
    }
}

/// Fallback Win64 stub for non-HRESULT imports (returns 0).
unsafe extern "win64" fn unimplemented_stub_zero() -> i32 {
    log_unimplemented_once("unimplemented(other)", ReturnClass::Other);
    S_OK
}

/// Fallback Win64 stub for known HRESULT imports: returns `S_OK` under `Log`,
/// `E_NOTIMPL` under `Fail` or `Stop`.
unsafe extern "win64" fn unimplemented_stub_hresult() -> i32 {
    log_unimplemented_once("unimplemented(hresult)", ReturnClass::HResult);
    match get_unimplemented_policy() {
        UnimplementedPolicy::Log => S_OK,
        UnimplementedPolicy::Fail | UnimplementedPolicy::Stop => E_NOTIMPL,
    }
}

/// Maps normalized DLL names and export names (or ordinals) to Win64-callable Rust HLE thunks.
pub struct ModuleRegistry {
    exports: HashMap<(String, String), usize>,
    ordinals: HashMap<(String, u16), usize>,
    owned_dlls: HashSet<String>,
    allow_stubs: bool,
    policy: UnimplementedPolicy,
}

impl ModuleRegistry {
    /// Construct the default Xbox Series X HLE registry with all Phase 1–7 modules registered.
    pub fn new_default(allow_stubs: bool) -> Self {
        Self::with_policy(allow_stubs, get_unimplemented_policy())
    }

    pub fn with_policy(allow_stubs: bool, policy: UnimplementedPolicy) -> Self {
        let mut reg = Self {
            exports: HashMap::new(),
            ordinals: HashMap::new(),
            owned_dlls: HashSet::new(),
            allow_stubs,
            policy,
        };
        kernelx::register(&mut reg);
        process::register(&mut reg);
        xmem::register(&mut reg);
        xgameruntime::register(&mut reg);
        services::register(&mut reg);
        d3d12_x::register(&mut reg);
        crate::crt::register(&mut reg);
        crate::dxgi::register(&mut reg);
        crate::xg_xs::register(&mut reg);
        crate::acphal::register(&mut reg);
        crate::middleware::register(&mut reg);
        crate::net::register(&mut reg);
        input::register(&mut reg);
        xaudio2::register(&mut reg);
        dstorage::register(&mut reg);
        reg
    }

    pub fn register_export(&mut self, dll: &str, name: &str, addr: usize) {
        let norm = normalize_dll(dll);
        self.owned_dlls.insert(norm.clone());
        self.exports.insert((norm, name.to_string()), addr);
    }

    pub fn register_ordinal(&mut self, dll: &str, ordinal: u16, addr: usize) {
        let norm = normalize_dll(dll);
        self.owned_dlls.insert(norm.clone());
        self.ordinals.insert((norm, ordinal), addr);
    }

    pub fn export_count(&self) -> usize {
        self.exports.len()
    }

    /// Look up a named export from an owned HLE DLL (used by `GetProcAddress` on pseudo-handles).
    pub fn lookup_owned_export(&self, dll: &str, name: &str) -> Option<usize> {
        let norm = normalize_dll(dll);
        self.exports.get(&(norm, name.to_string())).copied()
    }

    /// Look up an ordinal export from an owned HLE DLL.
    pub fn lookup_owned_ordinal(&self, dll: &str, ordinal: u16) -> Option<usize> {
        let norm = normalize_dll(dll);
        self.ordinals.get(&(norm, ordinal)).copied()
    }

    /// Returns `true` if `dll` is served directly by our HLE shim registry (so `LoadLibraryA/W`
    /// and `GetModuleHandleA/W` return a pseudo-handle instead of falling through).
    pub fn owns_dll(&self, dll: &str) -> bool {
        let norm = normalize_dll(dll);
        self.owned_dlls.contains(&norm)
    }
}

impl ImportResolver for ModuleRegistry {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Option<(usize, bool)> {
        let norm_dll = normalize_dll(dll);
        match symbol {
            ImportSymbol::Name { name, .. } => {
                if let Some(&addr) = self.exports.get(&(norm_dll, name.clone())) {
                    return Some((addr, false));
                }
            }
            ImportSymbol::Ordinal(ord) => {
                if let Some(&addr) = self.ordinals.get(&(norm_dll, *ord)) {
                    return Some((addr, false));
                }
            }
        }
        if self.allow_stubs {
            let sym_name = match symbol {
                ImportSymbol::Name { name, .. } => name.as_str(),
                ImportSymbol::Ordinal(_) => "",
            };
            let cls = classify_import_return_class(dll, sym_name);
            let stub_addr = match (self.policy, cls) {
                (UnimplementedPolicy::Fail | UnimplementedPolicy::Stop, ReturnClass::HResult) => {
                    unimplemented_stub_hresult as *const () as usize
                }
                _ => unimplemented_stub_zero as *const () as usize,
            };
            Some((stub_addr, true))
        } else {
            None
        }
    }
}

/// Normalize DLL names and redirect Windows/Xbox API-set names to canonical HLE modules.
pub fn normalize_dll(dll: &str) -> String {
    let base = dll
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(dll)
        .to_ascii_lowercase();
    let lower = if let Some(stripped) = base.strip_suffix('.') {
        stripped.to_string()
    } else if !base.contains('.') && !base.is_empty() {
        format!("{base}.dll")
    } else {
        base
    };

    if lower.starts_with("api-ms-win-core-")
        || lower == "kernel32.dll"
        || lower == "kernelbase.dll"
        || lower == "kernelx.dll"
    {
        return "kernelx.dll".to_string();
    }
    if lower.starts_with("api-ms-win-crt-")
        || lower == "ucrtbase.dll"
        || lower == "ucrtbased.dll"
    {
        return "ucrtbase.dll".to_string();
    }
    if lower == "d3d12_xs.dll" || lower == "d3d12_x.dll" || lower == "d3d12.dll" {
        return "d3d12_x.dll".to_string();
    }
    if lower == "dstorage_xs.dll" || lower == "dstorage_x.dll" || lower == "dstorage.dll" {
        return "dstorage_x.dll".to_string();
    }
    if lower == "xaudio2_9.dll" || lower == "xaudio2_9redist.dll" {
        return "xaudio2_9.dll".to_string();
    }
    if lower == "xuser.dll" || lower == "xstore.dll" || lower == "xgamesave.dll" {
        return "xgameruntime.dll".to_string();
    }
    if lower == "xg.dll" || lower == "xg_x.dll" || lower == "xg_xs.dll" {
        return "xg_xs.dll".to_string();
    }
    if lower == "xapu.dll" || lower == "acphal.dll" {
        return "acphal.dll".to_string();
    }
    if lower == "bink2scarlett.dll" || lower == "bink2w64_scarlett.dll" {
        return "bink2w64_scarlett.dll".to_string();
    }
    if lower == "libhttpclient.dll" || lower == "libhttpclient.gdk.dll" {
        return "libhttpclient.gdk.dll".to_string();
    }
    if lower == "pixrt.dll" || lower == "pixeventruntime.dll" || lower == "pixevt.dll" {
        return "pixevt.dll".to_string();
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
    fn d3d12_xs_and_real_game_dlls_resolve() {
        let reg = ModuleRegistry::new_default(false);
        for (dll, name) in [
            ("d3d12_xs.dll", "D3D12XboxCreateDevice"),
            ("d3d12_xs.dll", "D3D12SerializeVersionedRootSignature"),
            ("d3d12_xs.dll", "D3D12CreateRootSignatureDeserializer"),
            ("xmem.dll", "XMemAlloc"),
            ("xmem.dll", "XMemGetWorkingSetStatistics"),
            ("xmem.dll", "XMemVirtualAlloc"),
            ("xgameruntime.dll", "InitializeApiImplEx2"),
            ("xgameruntime.dll", "QueryApiImpl"),
            ("xgameruntime.dll", "XPackageGetMountPath"),
            ("api-ms-win-crt-runtime-l1-1-0.dll", "_configure_narrow_argv"),
            ("dstorage_xs.dll", "DStorageGetFactory"),
            ("xaudio2_9redist.dll", "XAudio2Create"),
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

    #[test]
    fn return_class_policy_distinguishes_hresult_vs_other() {
        assert_eq!(
            classify_import_return_class("xgameruntime.dll", "XUserAddAsync"),
            ReturnClass::HResult
        );
        assert_eq!(
            classify_import_return_class("xgameruntime.dll", "XUserCloseHandle"),
            ReturnClass::Other
        );
        assert_eq!(
            classify_import_return_class("d3d12_xs.dll", "D3D12XboxCreateSyntheticDevice"),
            ReturnClass::HResult
        );
        assert_eq!(
            classify_import_return_class("kernel32.dll", "UnknownFunc"),
            ReturnClass::Other
        );

        set_unimplemented_policy(UnimplementedPolicy::Fail);
        let reg = ModuleRegistry::with_policy(true, UnimplementedPolicy::Fail);
        let (addr, is_stub) = reg
            .resolve(
                "xgameruntime.dll",
                &ImportSymbol::Name {
                    hint: 0,
                    name: "XGameUiShowMessageDialogAsync".into(),
                },
            )
            .unwrap();
        assert!(is_stub);
        let f: unsafe extern "win64" fn() -> i32 = unsafe { core::mem::transmute(addr) };
        assert_eq!(unsafe { f() }, E_NOTIMPL);
        set_unimplemented_policy(UnimplementedPolicy::Log);
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
