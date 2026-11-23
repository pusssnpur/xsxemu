use std::fs;
use std::process::Command;

#[test]
fn boots_and_runs_actual_multi_subsystem_xbox_game_package() {
    let bin = env!("CARGO_BIN_EXE_xsxemu");
    let root = std::env::temp_dir().join(format!("xsxemu_actual_game_{}", std::process::id()));
    let pkg_dir = root.join("StarlightScarlett");
    let frame_path = root.join("starlight_fsr.bmp");
    let wav_path = root.join("starlight_audio.wav");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    // 1. Generate full MicrosoftGame.Config Xbox Series X game package
    let gen = Command::new(bin)
        .args(["gen-game", pkg_dir.to_str().unwrap()])
        .output()
        .expect("run xsxemu gen-game");
    assert!(
        gen.status.success(),
        "gen-game failed: {}",
        String::from_utf8_lossy(&gen.stderr)
    );

    // 2. Boot the package via MicrosoftGame.Config with forced relocation, software D3D12 + default FSR Quality
    let boot = Command::new(bin)
        .args([
            "boot",
            pkg_dir.to_str().unwrap(),
            "--backend",
            "software",
            "--force-relocate",
            "--trace",
            "--dump-frame",
            frame_path.to_str().unwrap(),
            "--dump-audio",
            wav_path.to_str().unwrap(),
        ])
        .output()
        .expect("run xsxemu boot");

    let stdout = String::from_utf8_lossy(&boot.stdout);
    let stderr = String::from_utf8_lossy(&boot.stderr);
    assert!(boot.status.success(), "boot failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("TitleId:           7A5F2026"));
    assert!(stdout.contains("Guest exited with code 0"));
    assert!(stdout.contains("Presented frames: 3"));
    assert!(stdout.contains("FSR (Quality): 640x360 -> 960x540"));
    assert!(stdout.contains("Audio mixed: 240 stereo 48kHz frames"));

    // 3. Verify 960x540 BMP output and Z-buffer occlusion (center pixel is green/cyan front triangle, not red background)
    let bmp = fs::read(&frame_path).expect("read dumped BMP");
    assert_eq!(&bmp[0..2], b"BM");
    let w = i32::from_le_bytes(bmp[18..22].try_into().unwrap());
    let h = i32::from_le_bytes(bmp[22..26].try_into().unwrap());
    assert_eq!((w, h), (960, 540));
    let row_stride = ((w * 3 + 3) & !3) as usize;
    let cx = (w / 2) as usize;
    let cy = (h / 2) as usize;
    let bmp_y = (h as usize) - 1 - cy;
    let off = 54 + bmp_y * row_stride + cx * 3;
    let (b, g, r) = (bmp[off], bmp[off + 1], bmp[off + 2]);
    assert!(
        g > r && g > 120,
        "expected Z-buffered green/cyan front starship at center, got RGB=({r},{g},{b})"
    );

    // 4. Verify 48 kHz stereo WAV output
    let wav = fs::read(&wav_path).expect("read dumped WAV");
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(wav.len(), 44 + 240 * 4);

    let _ = fs::remove_dir_all(&root);
}
