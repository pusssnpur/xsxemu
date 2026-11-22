//! End-to-end Phase 3 + Phase 4 + Phase 5 Improvements integration test:
//! Executes `full_stack_xsx.exe` natively via Win64 ABI exercising:
//! - Phase 5: Real-Time Clock (`GetSystemTime`, deterministic RTC), `GetSystemInfo` (8-core Zen 2),
//!   persistent `ShaderCache` (hit/miss + DXBC MD5 checksum), multithreaded AMD FSR, and 48 kHz
//!   stereo `AudioMixer` + RIFF/WAVE `.wav` export of a 440 Hz sine tone
//! - Phase 4: `XUser`, `XStore`, persistent `XGameSave`, `dstorage_x.dll`, `xinput1_4.dll`, `xaudio2_9.dll`
//! - Phase 3: `d3d12_x.dll` RootSignature, Graphics PSO with Xbox DXBC/DXIL chunk stripping,
//!   Compute Dispatch, and default AMD FSR Quality (1.5x EASU + RCAS) upscaling.

use std::fs;

use xsx_abi::services::{XInputGamepad, XINPUT_GAMEPAD_A};
use xsx_d3d12::{compute_dxbc_hash, BackendKind, FsrQualityMode, ShaderContainer, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RtcConfig, RuntimeGuard, VirtualFs};
use xsx_loader::{LoadedImage, PeImage};
use xsx_testgen::{build_full_stack_xsx_pe, FULLSTACK_ASSET_PAYLOAD, FULLSTACK_SAVE_PAYLOAD};

#[test]
fn phase3_and_phase4_full_stack_end_to_end() {
    let tmp_dir = std::env::temp_dir().join(format!("xsxemu_fullstack_test_{}", std::process::id()));
    let game_dir = tmp_dir.join("G");
    let scratch_dir = tmp_dir.join("T");
    let cache_dir = tmp_dir.join("shader_cache");
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
        shader_cache_dir: Some(cache_dir.clone()),
        ..TranslatorConfig::default()
    };
    let rtc = RtcConfig {
        offset_seconds: 0,
        fixed_unix_ms: Some(1_763_830_800_000), // Deterministic RTC timestamp
        tz_bias_minutes: 0,
    };
    let runtime = GuestRuntime::with_options(cfg, vfs, rtc, false, false);

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
    assert!(msgs.iter().any(|m| m == "XsxPlayer1"));
    assert!(msgs.iter().any(|m| m == FULLSTACK_SAVE_PAYLOAD));
    assert!(msgs.iter().any(|m| m == FULLSTACK_ASSET_PAYLOAD));
    assert!(msgs.iter().any(|m| m.contains("Phase 3 + Phase 4")));

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

    // 5. Verify AudioMixer 48 kHz stereo mixing + RIFF/WAVE export of 440 Hz sine tone
    assert_eq!(runtime.audio_samples_submitted(), 256);
    assert_eq!(runtime.audio_pcm_len(), 1024);
    let wav_path = tmp_dir.join("audio_out.wav");
    runtime.write_audio_wav(&wav_path).unwrap();
    let wav = fs::read(&wav_path).unwrap();
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(wav.len(), 44 + 1024);

    // 6. Verify Phase 3 + Phase 5 shader stripping, DXBC MD5 digest, ShaderCache hit, and FSR
    let translator = runtime
        .d3d12_translator()
        .expect("D3D12 translator should be initialized");
    assert_eq!(translator.dispatch_count(), 1);

    let (cache_hits, cache_misses) = translator.shader_cache_stats();
    assert_eq!(cache_misses, 1, "VS should miss shader cache");
    assert_eq!(cache_hits, 1, "PS reusing identical shader blob should hit shader cache");

    let shaders = translator.translated_shaders();
    assert_eq!(shaders.len(), 2);
    assert!(shaders[0].has_dxil);
    assert!(shaders[0].has_root_signature);
    assert_eq!(shaders[0].stripped_chunks, vec!["XBOX", "XGCS"]);
    let reparsed = ShaderContainer::parse(&shaders[0].pc_dxbc_bytes).unwrap();
    assert_eq!(
        reparsed.digest,
        compute_dxbc_hash(&shaders[0].pc_dxbc_bytes[20..])
    );

    let frames = runtime.presented_frames();
    assert_eq!(frames.len(), 1);
    assert_eq!((frames[0].width, frames[0].height), (960, 540));
    assert_eq!(runtime.fsr_stats()[0].mode, FsrQualityMode::Quality);

    let _ = fs::remove_dir_all(&tmp_dir);
}
