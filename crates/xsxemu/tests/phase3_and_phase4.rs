//! End-to-end Phase 3 + Phase 4 full-stack integration test:
//! Executes a real x86_64 PE32+ Xbox Series X binary natively via Win64 ABI exercising:
//! - Phase 4: XUser, XStore, persistent XGameSave, DirectStorage (`dstorage_x.dll`),
//!   XInput (`xinput1_4.dll`), and XAudio2 (`xaudio2_9.dll`)
//! - Phase 3: D3D12.x RootSignature, Graphics PSO with Xbox DXBC/DXIL chunk stripping,
//!   Compute Dispatch, UMA staging ring, and default AMD FSR Quality (1.5x EASU + RCAS) upscaling.

use std::fs;

use xsx_abi::services::{XInputGamepad, XINPUT_GAMEPAD_A};
use xsx_d3d12::{BackendKind, FsrQualityMode, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RuntimeGuard, VirtualFs};
use xsx_loader::{LoadedImage, PeImage};
use xsx_testgen::{build_full_stack_xsx_pe, FULLSTACK_ASSET_PAYLOAD, FULLSTACK_SAVE_PAYLOAD};

#[test]
fn phase3_and_phase4_full_stack_end_to_end() {
    let tmp_dir = std::env::temp_dir().join(format!("xsxemu_fullstack_test_{}", std::process::id()));
    let game_dir = tmp_dir.join("G");
    let scratch_dir = tmp_dir.join("T");
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(game_dir.join("assets")).unwrap();
    fs::create_dir_all(&scratch_dir).unwrap();

    // Provision `G:\assets\level.bin` for DirectStorage streaming!
    fs::write(game_dir.join("assets").join("level.bin"), FULLSTACK_ASSET_PAYLOAD).unwrap();

    let pe_bytes = build_full_stack_xsx_pe();
    let pe = PeImage::parse(&pe_bytes).expect("full_stack_xsx.exe should parse cleanly");

    let registry = ModuleRegistry::new_default(false);
    let loaded = LoadedImage::load(&pe, &registry, true).expect("should load & relocate");
    assert!(loaded.resolved_imports().iter().all(|i| !i.is_stub));

    let vfs = VirtualFs::new(game_dir, scratch_dir.clone());
    let cfg = TranslatorConfig {
        backend: BackendKind::Software,
        ..TranslatorConfig::default() // FSR Quality 1.5x enabled by default!
    };
    let runtime = GuestRuntime::with_vfs(cfg, vfs, false);

    // Inject virtual gamepad A button press before running guest
    runtime.set_virtual_gamepad(
        0,
        XInputGamepad {
            w_buttons: XINPUT_GAMEPAD_A,
            b_right_trigger: 200,
            ..XInputGamepad::default()
        },
    );

    let _guard = RuntimeGuard::activate(runtime.clone());
    let exit_code = unsafe { loaded.execute_entry() };
    assert_eq!(exit_code, 0, "full_stack_xsx.exe should exit with 0");

    // 1. Verify XUser, XGameSave readback, DirectStorage streamed payload, and completion logs
    let msgs = runtime.debug_messages();
    assert!(
        msgs.iter().any(|m| m == "XsxPlayer1"),
        "expected XUser gamertag 'XsxPlayer1', got {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m == FULLSTACK_SAVE_PAYLOAD),
        "expected XGameSave readback payload, got {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m == FULLSTACK_ASSET_PAYLOAD),
        "expected DirectStorage streamed asset payload, got {msgs:?}"
    );
    assert!(
        msgs.iter().any(|m| m.contains("Phase 3 + Phase 4")),
        "expected completion message, got {msgs:?}"
    );

    // 2. Verify XGameSave persisted the save blob on disk under S:\slot0\player.sav
    let saved_file = scratch_dir.join("saves").join("slot0").join("player.sav");
    assert_eq!(
        fs::read_to_string(&saved_file).expect("save blob should exist on disk"),
        FULLSTACK_SAVE_PAYLOAD
    );

    // 3. Verify DirectStorage telemetry
    assert_eq!(
        runtime.dstorage_bytes_read(),
        FULLSTACK_ASSET_PAYLOAD.len() as u64
    );

    // 4. Verify XInput rumble feedback was received
    let pad = runtime.get_gamepad_state(0);
    assert_eq!(pad.last_vibration.w_left_motor_speed, 16_000);
    assert_eq!(pad.last_vibration.w_right_motor_speed, 32_000);

    // 5. Verify XAudio2 PCM samples were submitted (1024 bytes = 256 stereo 16-bit frames)
    assert_eq!(runtime.audio_samples_submitted(), 256);
    assert_eq!(runtime.audio_pcm_len(), 1024);

    // 6. Verify Phase 3 D3D12.x shader stripping, compute dispatch, and default FSR Quality upscaling!
    let translator = runtime
        .d3d12_translator()
        .expect("D3D12 translator should be initialized");
    assert_eq!(translator.dispatch_count(), 1);

    let shaders = translator.translated_shaders();
    assert_eq!(shaders.len(), 2, "expected VS and PS Xbox DXBC containers translated");
    assert!(shaders[0].has_dxil);
    assert!(shaders[0].has_root_signature);
    assert_eq!(shaders[0].stripped_chunks, vec!["XBOX", "XGCS"]);

    let frames = runtime.presented_frames();
    assert_eq!(frames.len(), 1);
    assert_eq!((frames[0].width, frames[0].height), (960, 540));
    assert_eq!(runtime.fsr_stats()[0].mode, FsrQualityMode::Quality);

    let _ = fs::remove_dir_all(&tmp_dir);
}
