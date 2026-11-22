//! End-to-end vertical slice integration test:
//! Generates a real Xbox Series X PE32+ executable, maps & relocates it in OS
//! executable memory, patches Xbox DLL imports into the IAT, executes the guest
//! x86_64 machine code natively via Win64 ABI, and verifies the rendered RGB
//! triangle presented via `ID3D12CommandQueueX::PresentX`.

use xsx_d3d12::{BackendKind, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RuntimeGuard};
use xsx_loader::{LoadedImage, PeImage};
use xsx_testgen::build_hello_xsx_pe;

#[test]
fn vertical_slice_executes_guest_pe64_and_renders_triangle() {
    // 1. Generate real x86_64 PE32+ executable
    let pe_bytes = build_hello_xsx_pe();
    let pe = PeImage::parse(&pe_bytes).expect("generated PE32+ should parse cleanly");

    // Verify expected Xbox Series X imports are present
    let xbox_dlls = pe.xbox_dll_imports();
    assert!(xbox_dlls.contains(&"xgameruntime.dll"));
    assert!(xbox_dlls.contains(&"kernelx.dll"));
    assert!(xbox_dlls.contains(&"d3d12_x.dll"));
    assert!(!pe.relocations.is_empty(), "PE should include DIR64 base relocations");

    // 2. Load & force base relocation away from preferred ImageBase (0x1_4000_0000)
    let registry = ModuleRegistry::new_default(false);
    let loaded = LoadedImage::load(&pe, &registry, true).expect("PE32+ should load & relocate");
    assert_ne!(
        loaded.base_addr() as u64,
        loaded.preferred_base(),
        "force_relocate should map away from preferred base"
    );
    assert!(
        loaded.relocations_applied() >= 3,
        "expected >= 3 DIR64 relocations applied, got {}",
        loaded.relocations_applied()
    );
    assert_eq!(loaded.resolved_imports().len(), 5);
    assert!(loaded.resolved_imports().iter().all(|i| !i.is_stub));

    // 3. Activate HLE + Software D3D12 translator and execute guest x86_64 machine code!
    let cfg = TranslatorConfig {
        backend: BackendKind::Software,
        trace_calls: false,
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::new(cfg, false);
    let _guard = RuntimeGuard::activate(runtime.clone());

    let exit_code = unsafe { loaded.execute_entry() };
    assert_eq!(exit_code, 0, "guest PE64 entry point should return 0");

    // 4. Verify HLE debug strings (first string came through a relocated DIR64 pointer!)
    let debug_msgs = runtime.debug_messages();
    assert_eq!(debug_msgs.len(), 2);
    assert!(debug_msgs[0].contains("Xbox Series X (Anaconda)"));
    assert!(debug_msgs[1].contains("PresentX"));

    // 5. Verify D3D12.x frame presentation & rendered pixels!
    let frames = runtime.presented_frames();
    assert_eq!(frames.len(), 1, "expected exactly one presented frame");
    let frame = &frames[0];
    assert_eq!(frame.width, 640);
    assert_eq!(frame.height, 360);

    // Corner pixel (10, 10) should be the clear color [0.06, 0.14, 0.10, 1.0] -> (15, 36, 26, 255)
    let corner = frame.pixel_rgba(10, 10);
    assert_eq!(corner, [15, 36, 26, 255]);

    // Center pixel (320, 180) is inside the RGB triangle and must differ sharply from background!
    let center = frame.pixel_rgba(320, 180);
    assert_ne!(center, corner, "center pixel should be shaded by rasterized triangle");
    assert!(
        center[0] > 70 && center[1] > 100 && center[2] > 90,
        "expected bright interpolated RGB triangle pixel at center, got {center:?}"
    );

    // Near top vertex (320, 60) should be strongly Xbox Green!
    let top = frame.pixel_rgba(320, 60);
    assert!(
        top[1] > 180 && top[1] > top[0] && top[1] > top[2],
        "expected green-dominant pixel near top vertex, got {top:?}"
    );
}
