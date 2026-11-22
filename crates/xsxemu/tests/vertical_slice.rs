//! End-to-end vertical slice integration test:
//! Verifies both default-enabled AMD FSR (Quality 1.5x EASU+RCAS) upscaling
//! AND explicit FSR-off native resolution passthrough.

use xsx_d3d12::{BackendKind, FsrConfig, FsrQualityMode, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RuntimeGuard};
use xsx_loader::{LoadedImage, PeImage};
use xsx_testgen::build_hello_xsx_pe;

#[test]
fn vertical_slice_executes_guest_pe64_with_default_fsr_upscaling() {
    let pe_bytes = build_hello_xsx_pe();
    let pe = PeImage::parse(&pe_bytes).expect("generated PE32+ should parse cleanly");

    let xbox_dlls = pe.xbox_dll_imports();
    assert!(xbox_dlls.contains(&"xgameruntime.dll"));
    assert!(xbox_dlls.contains(&"kernelx.dll"));
    assert!(xbox_dlls.contains(&"d3d12_x.dll"));
    assert!(!pe.relocations.is_empty());

    let registry = ModuleRegistry::new_default(false);
    let loaded = LoadedImage::load(&pe, &registry, true).expect("PE32+ should load & relocate");
    assert_ne!(loaded.base_addr() as u64, loaded.preferred_base());
    assert!(loaded.relocations_applied() >= 3);
    assert_eq!(loaded.resolved_imports().len(), 5);
    assert!(loaded.resolved_imports().iter().all(|i| !i.is_stub));

    // Default TranslatorConfig enables AMD FSR Quality (1.5x EASU + RCAS)!
    let cfg = TranslatorConfig {
        backend: BackendKind::Software,
        ..TranslatorConfig::default()
    };
    assert!(cfg.fsr.is_active());
    assert_eq!(cfg.fsr.mode, FsrQualityMode::Quality);

    let runtime = GuestRuntime::new(cfg, false);
    let _guard = RuntimeGuard::activate(runtime.clone());

    let exit_code = unsafe { loaded.execute_entry() };
    assert_eq!(exit_code, 0, "guest PE64 entry point should return 0");

    let debug_msgs = runtime.debug_messages();
    assert_eq!(debug_msgs.len(), 2);
    assert!(debug_msgs[0].contains("Xbox Series X (Anaconda)"));
    assert!(debug_msgs[1].contains("PresentX"));

    // Verify FSR upscaled 640x360 -> 960x540 by default!
    let frames = runtime.presented_frames();
    assert_eq!(frames.len(), 1);
    let frame = &frames[0];
    assert_eq!((frame.width, frame.height), (960, 540));

    let stats = runtime.fsr_stats();
    assert_eq!(stats.len(), 1);
    assert!(stats[0].easu_ran);
    assert!(stats[0].rcas_ran);
    assert_eq!((stats[0].input_width, stats[0].input_height), (640, 360));
    assert_eq!((stats[0].output_width, stats[0].output_height), (960, 540));

    // Corner pixel stays dark slate-green background
    let corner = frame.pixel_rgba(12, 12);
    assert_eq!(corner, [15, 36, 26, 255]);

    // Center pixel (480, 270) is shaded by the upscaled RGB triangle
    let center = frame.pixel_rgba(480, 270);
    assert_ne!(center, corner);
    assert!(center[0] > 70 && center[1] > 100 && center[2] > 90);

    // Near top vertex (480, 90) is strongly Xbox Green
    let top = frame.pixel_rgba(480, 90);
    assert!(top[1] > 180 && top[1] > top[0] && top[1] > top[2]);
}

#[test]
fn vertical_slice_supports_explicit_fsr_off() {
    let pe = PeImage::parse(&build_hello_xsx_pe()).unwrap();
    let registry = ModuleRegistry::new_default(false);
    let loaded = LoadedImage::load(&pe, &registry, true).unwrap();

    let cfg = TranslatorConfig {
        backend: BackendKind::Software,
        fsr: FsrConfig::disabled(),
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::new(cfg, false);
    let _guard = RuntimeGuard::activate(runtime.clone());

    assert_eq!(unsafe { loaded.execute_entry() }, 0);
    let frames = runtime.presented_frames();
    assert_eq!((frames[0].width, frames[0].height), (640, 360));
    assert!(!runtime.fsr_stats()[0].easu_ran);
    assert!(!runtime.fsr_stats()[0].rcas_ran);
}
