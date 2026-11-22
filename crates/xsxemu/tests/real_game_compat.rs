//! End-to-end Real-Game Xbox Series X ABI compatibility integration tests.
//!
//! Executes `realgame_xsx.exe` (native x86_64 Win64 PE32+ machine code) exercising:
//! - `api-ms-win-core-libraryloader-l1-2-0.dll!LoadLibraryW(L"xgameruntime.dll")` pseudo-handles
//! - `GetProcAddress("InitializeApiImplEx2")` & `GetProcAddress("QueryApiImpl")` -> `IXThreadingImpl`
//! - `xmem.dll!XMemAlloc` (64B aligned) + `XMemGetWorkingSetStatistics(0x20000001)` + `XMemFree`
//! - `d3d12_xs.dll!D3D12SerializeVersionedRootSignature` with Xbox `0x0300_0001` high-byte flags
//! - `d3d12_xs.dll!D3D12XboxCreateDevice` -> 84-slot Xbox ABI proxy (Slot 14 + Heap Slot 9 RAX,
//!   Slot 67 FSPX native RDNA 2 compute PSO, Slot 76 resource creation with Xbox layout `0x101`)
//! - `GAME:\` Copy-on-Write VFS overlay (original package file untouched on disk!)
//! - Crash-safe flushed JSONL `StartupTracer`

use std::fs;
use std::sync::{Arc, Mutex};

use xsx_d3d12::{xs_device_slot_hits, BackendKind, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RtcConfig, RuntimeGuard, StartupTracer, VirtualFs};
use xsx_loader::{LoadedModuleSet, PeImage};
use xsx_testgen::{
    build_real_game_xsx_pe, REALGAME_EXIT_CODE, REALGAME_OVERLAY_CONFIG, REALGAME_PKG_CONFIG,
};

#[test]
fn executes_real_xbox_series_x_game_abi_end_to_end() {
    let base = std::env::temp_dir().join(format!("xsxemu_realgame_e2e_{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let pkg_dir = base.join("package");
    let overlay_dir = base.join("overlay");
    let scratch_dir = base.join("scratch");
    let trace_path = base.join("startup.jsonl");

    fs::create_dir_all(pkg_dir.join("data")).unwrap();
    fs::write(pkg_dir.join("data/config.txt"), REALGAME_PKG_CONFIG).unwrap();

    let pe_bytes = build_real_game_xsx_pe();
    let pe = PeImage::parse(&pe_bytes).expect("valid realgame PE32+");

    let vfs = VirtualFs::with_overlay(pkg_dir.clone(), overlay_dir.clone(), scratch_dir);
    vfs.ensure_roots().unwrap();

    let cfg = TranslatorConfig {
        backend: BackendKind::Software,
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::with_options(cfg, vfs, RtcConfig::default(), false, false);
    let tracer = Arc::new(StartupTracer::create(&trace_path).unwrap());
    runtime.set_startup_tracer(Arc::clone(&tracer));
    let _guard = RuntimeGuard::activate(runtime.clone());

    let registry = ModuleRegistry::new_default(false);
    let module_set = unsafe {
        LoadedModuleSet::load_with_dependencies(
            "realgame_xsx.exe",
            &pe,
            &registry,
            vec![pkg_dir.clone()],
            false,
        )
    }
    .expect("all real-game imports must resolve with zero stubs");

    let main_img = Arc::clone(module_set.main_image());
    tracer.record_map("realgame_xsx.exe", main_img.base_addr(), pe.size_of_image as usize, false);
    runtime.attach_modules(Arc::new(Mutex::new(module_set)));

    let exit_code = unsafe { main_img.execute_entry() };
    tracer.record_exit(exit_code as i32);

    assert_eq!(exit_code, REALGAME_EXIT_CODE, "realgame_xsx.exe returned error code 0x{exit_code:X}");
    assert!(runtime.is_game_runtime_initialized());
    assert!(
        runtime.debug_messages().iter().any(|m| m == "RealGameCompat OK"),
        "expected OutputDebugStringW message, got {:?}",
        runtime.debug_messages()
    );

    // Verify measured Xbox d3d12_xs slots were hit by real guest machine code
    assert!(xs_device_slot_hits(14) >= 1, "device slot 14 (CreateDescriptorHeap) should be hit");
    assert!(xs_device_slot_hits(67) >= 1, "device slot 67 (DeserializeComputePipelineStateX) should be hit");
    assert!(xs_device_slot_hits(76) >= 1, "device slot 76 (XboxCreateResource) should be hit");

    // Verify Copy-on-Write VFS overlay protected the retail package
    assert_eq!(
        fs::read(pkg_dir.join("data/config.txt")).unwrap(),
        REALGAME_PKG_CONFIG,
        "retail package file must NEVER be modified"
    );
    assert_eq!(
        fs::read(overlay_dir.join("data/config.txt")).unwrap(),
        REALGAME_OVERLAY_CONFIG,
        "guest write to GAME:\\ must land in overlay"
    );

    // Verify crash-safe JSONL startup trace was flushed
    let trace_text = fs::read_to_string(&trace_path).unwrap();
    assert!(trace_text.contains("\"kind\":\"trace_open\""));
    assert!(trace_text.contains("\"kind\":\"map\""));
    assert!(trace_text.contains("RealGameCompat OK"));
    assert!(trace_text.contains("\"kind\":\"exit\""));

    let _ = fs::remove_dir_all(&base);
}
