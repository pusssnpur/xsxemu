//! Phase 2 + FSR end-to-end integration test:
//! Exercises secondary guest DLL loading (`xsx_helper.dll` + `DllMain` + `.edata`),
//! PE TLS callbacks (`DLL_PROCESS_ATTACH`), `.pdata` exception directory parsing/lookup,
//! virtual Xbox `T:\` file I/O (`CreateFileA`/`WriteFile`), real OS multithreading
//! (`CreateThread`/`WaitForSingleObject`/`GetExitCodeThread`), and default-enabled
//! AMD FSR (EASU + RCAS) upscaling on `PresentX`.

use std::fs;
use std::sync::{Arc, Mutex};

use xsx_d3d12::{BackendKind, FsrConfig, FsrQualityMode, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RuntimeGuard, VirtualFs};
use xsx_loader::{LoadedModuleSet, PeImage};
use xsx_testgen::{build_helper_dll_pe, build_phase2_xsx_pe, PHASE2_FILE_PAYLOAD};

#[test]
fn phase2_runtime_completeness_and_fsr_end_to_end() {
    let tmp_dir = std::env::temp_dir().join(format!("xsxemu_phase2_test_{}", std::process::id()));
    let game_dir = tmp_dir.join("game");
    let scratch_dir = tmp_dir.join("scratch");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&game_dir).unwrap();
    fs::create_dir_all(&scratch_dir).unwrap();

    // Write secondary guest DLL (`xsx_helper.dll`) into game_dir so LoadedModuleSet discovers it
    let dll_bytes = build_helper_dll_pe();
    let dll_pe = PeImage::parse(&dll_bytes).expect("helper DLL should parse");
    assert!(dll_pe.is_dll());
    assert!(dll_pe.exports.is_some());
    assert_eq!(dll_pe.runtime_functions.len(), 2);
    fs::write(game_dir.join("xsx_helper.dll"), &dll_bytes).unwrap();

    // Parse main Phase 2 executable
    let exe_bytes = build_phase2_xsx_pe();
    let exe_pe = PeImage::parse(&exe_bytes).expect("phase2 EXE should parse");
    assert!(exe_pe.tls.is_some(), "phase2 EXE should have TLS directory");
    assert_eq!(exe_pe.runtime_functions.len(), 3, "expected 3 .pdata entries");

    // Configure VirtualFs + FSR Performance (2.0x -> 1280x720)
    let vfs = VirtualFs::new(game_dir.clone(), scratch_dir.clone());
    let cfg = TranslatorConfig {
        backend: BackendKind::Software,
        fsr: FsrConfig {
            mode: FsrQualityMode::Performance,
            sharpness: 0.85,
            enable_rcas: true,
            output_resolution: None,
        },
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::with_vfs(cfg, vfs, false);
    let _guard = RuntimeGuard::activate(runtime.clone());

    let registry = ModuleRegistry::new_default(false);
    let module_set = unsafe {
        LoadedModuleSet::load_with_dependencies(
            "phase2_xsx.exe",
            &exe_pe,
            &registry,
            vec![game_dir.clone()],
            true,
        )
        .expect("LoadedModuleSet should load main EXE and xsx_helper.dll")
    };

    assert_eq!(module_set.loaded_dll_names(), &["xsx_helper.dll"]);
    let main_img = Arc::clone(module_set.main_image());
    assert_eq!(main_img.tls_callbacks().len(), 1);
    assert!(
        main_img
            .find_runtime_function(main_img.entry_point_va())
            .is_some(),
        "entry point should be covered by .pdata RUNTIME_FUNCTION"
    );

    let modules_arc = Arc::new(Mutex::new(module_set));
    runtime.attach_modules(Arc::clone(&modules_arc));

    // Execute main (runs TLS callback -> calls xsx_helper.dll -> writes T:\phase2_check.txt ->
    // spawns real worker thread -> renders triangle -> FSR upscales 640x360 -> 1280x720!)
    let exit_code = unsafe { main_img.execute_entry() };
    assert_eq!(exit_code, 0, "phase2_xsx.exe should exit with 0");

    // 1. Verify DllMain, TLS callback, worker thread, and main completion debug strings
    let msgs = runtime.debug_messages();
    assert!(
        msgs.iter().any(|m| m.contains("xsx_helper.dll DllMain")),
        "expected secondary DLL DllMain log, got {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("TLS callback executed")),
        "expected TLS callback log, got {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("worker thread executed")),
        "expected worker thread log, got {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("FSR PresentX")),
        "expected main completion log, got {msgs:?}"
    );

    // 2. Verify virtual Xbox `T:\phase2_check.txt` was written onto host scratch_dir!
    let written_file = scratch_dir.join("phase2_check.txt");
    let contents = fs::read_to_string(&written_file).expect("T:\\phase2_check.txt should exist");
    assert_eq!(contents, PHASE2_FILE_PAYLOAD);

    // 3. Verify FSR Performance 2.0x upscaled 640x360 -> 1280x720 with EASU + RCAS
    let frames = runtime.presented_frames();
    assert_eq!(frames.len(), 1);
    assert_eq!((frames[0].width, frames[0].height), (1280, 720));
    let stats = runtime.fsr_stats();
    assert_eq!(stats[0].mode, FsrQualityMode::Performance);
    assert!(stats[0].easu_ran && stats[0].rcas_ran);

    let _ = fs::remove_dir_all(&tmp_dir);
}
