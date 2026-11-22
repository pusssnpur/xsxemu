//! `xsxemu` CLI — Xbox Series X|S clean-room game translation layer runner.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use xsx_d3d12::{
    disassemble_rdna2, extract_fssm_program, extract_fspx_parts, format_rdna2_disassembly,
    inspect_xbox_shader_blob, lift_rdna2_to_hlsl, BackendKind, FsrConfig, FsrQualityMode,
    ShaderStage, TranslatorConfig,
};
use xsx_hle::{
    classify_import_return_class, set_active_title_id, set_guest_command_line,
    set_unimplemented_policy, GuestRuntime, ModuleRegistry, ReturnClass, RtcConfig, RuntimeGuard,
    StartupTracer, UnimplementedPolicy, VirtualFs,
};
use xsx_loader::{
    default_search_dirs, GameConfig, ImportSymbol, LoadedModuleSet, PeImage, XvdContainer,
};
use xsx_testgen::{
    build_full_stack_xsx_pe, build_hello_xsx_pe, build_helper_dll_pe, build_phase2_xsx_pe,
    build_real_game_xsx_pe, FULLSTACK_ASSET_PAYLOAD, REALGAME_PKG_CONFIG,
};
use xsxemu::gui::{launch_gui, GuiOptions};

fn print_usage() {
    eprintln!(
        "xsxemu — Clean-room Xbox Series X|S Game Translation Layer (v{})\n\
         \n\
         USAGE:\n\
           xsxemu gui [--web] [--port PORT] [--no-browser]\n\
           xsxemu gen-sample <output.exe>\n\
           xsxemu gen-phase2 <output_dir>\n\
           xsxemu gen-fullstack <output_dir>\n\
           xsxemu gen-realgame <output_dir>\n\
           xsxemu inspect <binary.exe|dll>\n\
           xsxemu inspect-shader <shader.bin>\n\
           xsxemu inspect-xvd <container.xvd>\n\
           xsxemu disasm <shader.bin>\n\
           xsxemu lift-hlsl <shader.bin> [--stage vs|ps|cs]\n\
           xsxemu boot <package_dir> [RUN OPTIONS] [-- guest-args...]\n\
           xsxemu run <binary.exe> [RUN OPTIONS] [-- guest-args...]\n\
           xsxemu bench-fsr [--width W] [--height H] [--fsr MODE] [--iters N]\n\
         \n\
         RUN OPTIONS:\n\
           --dry-run                             Map image & print import/HRESULT coverage without executing code\n\
           --backend <auto|d3d12|software>       Select GPU backend (default: auto)\n\
           --windowed                            Show visible Win32 swapchain window on Windows\n\
           --fsr <quality|balanced|performance|  AMD FSR upscaling preset (default: quality)\n\
                  ultra-quality|ultra-performance|native-aa|off>\n\
           --fsr-sharpness <0.0..=1.0>           RCAS sharpening strength (default: 0.8)\n\
           --output-res <WxH>                    Explicit FSR output resolution (e.g. 1920x1080)\n\
           --shader-cache <dir>                  Persistent on-disk DXBC/DXIL shader cache\n\
           --rtc-offset <seconds>                Signed Real-Time Clock offset in seconds\n\
           --live-audio                          Enable live Windows winmm.dll sound output\n\
           --dump-audio <path.wav>               Export mixed 48 kHz stereo XAudio2 output to WAV\n\
           --dll-dir <path>                      Extra search directory for secondary guest DLLs\n\
           --game-dir <path>                     Host directory mapped to Xbox GAME:\\, G:\\, D:\\\n\
           --overlay <path>                      Copy-on-Write overlay dir for GAME:\\ writes (package stays read-only)\n\
           --scratch-dir <path>                  Host directory mapped to Xbox T:\\ (saves in T:\\saves)\n\
           --dump-frame <path.bmp|path.ppm>      Write presented (FSR-upscaled) frame to disk\n\
           --unimplemented <log|fail|stop>       Unimplemented import policy (default: log; fail returns E_NOTIMPL for HRESULTs)\n\
           --trace                               Enable HLE, D3D12.x, RTC, and FSR call tracing\n\
           --trace-jsonl <path.jsonl>            Write crash-safe flushed JSONL startup & import trace\n\
           --allow-stubs                         Stub unresolved imports instead of failing\n\
           --force-relocate                      Force base relocation away from ImageBase",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_usage();
        return ExitCode::from(2);
    }

    match args[0].as_str() {
        "gui" => cmd_gui(&args[1..]),
        "gen-sample" => cmd_gen_sample(&args[1..]),
        "gen-phase2" => cmd_gen_phase2(&args[1..]),
        "gen-fullstack" => cmd_gen_fullstack(&args[1..]),
        "gen-realgame" => cmd_gen_realgame(&args[1..]),
        "inspect" => cmd_inspect(&args[1..]),
        "inspect-shader" => cmd_inspect_shader(&args[1..]),
        "inspect-xvd" => cmd_inspect_xvd(&args[1..]),
        "disasm" => cmd_disasm(&args[1..]),
        "lift-hlsl" => cmd_lift_hlsl(&args[1..]),
        "boot" => cmd_boot(&args[1..]),
        "run" => cmd_run(&args[1..]),
        "bench-fsr" => cmd_bench_fsr(&args[1..]),
        "-h" | "--help" | "help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn cmd_gui(args: &[String]) -> ExitCode {
    let mut opts = GuiOptions::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--web" => opts.web_only = true,
            "--no-browser" => opts.open_browser = false,
            "--port" => {
                i += 1;
                if let Some(p) = args.get(i).and_then(|s| s.parse::<u16>().ok()) {
                    opts.port = p;
                } else {
                    eprintln!("--port requires a valid TCP port (0..=65535)");
                    return ExitCode::from(2);
                }
            }
            other => {
                eprintln!("unknown gui option: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    if let Err(e) = launch_gui(opts) {
        eprintln!("GUI error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn cmd_gen_sample(args: &[String]) -> ExitCode {
    let Some(out_path) = args.first() else {
        eprintln!("usage: xsxemu gen-sample <output.exe>");
        return ExitCode::from(2);
    };
    let bytes = build_hello_xsx_pe();
    if let Err(e) = fs::write(out_path, &bytes) {
        eprintln!("failed to write {out_path}: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "Generated synthetic Xbox Series X PE32+ executable: {out_path} ({} bytes)",
        bytes.len()
    );
    ExitCode::SUCCESS
}

fn cmd_gen_phase2(args: &[String]) -> ExitCode {
    let Some(out_dir) = args.first() else {
        eprintln!("usage: xsxemu gen-phase2 <output_dir>");
        return ExitCode::from(2);
    };
    let dir = Path::new(out_dir);
    if let Err(e) = fs::create_dir_all(dir) {
        eprintln!("failed to create directory {out_dir}: {e}");
        return ExitCode::FAILURE;
    }
    let dll_path = dir.join("xsx_helper.dll");
    let exe_path = dir.join("phase2_xsx.exe");
    let dll_bytes = build_helper_dll_pe();
    let exe_bytes = build_phase2_xsx_pe();

    if let Err(e) = fs::write(&dll_path, &dll_bytes) {
        eprintln!("failed to write {}: {e}", dll_path.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(&exe_path, &exe_bytes) {
        eprintln!("failed to write {}: {e}", exe_path.display());
        return ExitCode::FAILURE;
    }

    println!(
        "Generated Phase 2 suite:\n  - {} ({} bytes)\n  - {} ({} bytes)",
        exe_path.display(),
        exe_bytes.len(),
        dll_path.display(),
        dll_bytes.len()
    );
    ExitCode::SUCCESS
}

fn cmd_gen_fullstack(args: &[String]) -> ExitCode {
    let Some(out_dir) = args.first() else {
        eprintln!("usage: xsxemu gen-fullstack <output_dir>");
        return ExitCode::from(2);
    };
    let dir = Path::new(out_dir);
    let assets_dir = dir.join("assets");
    if let Err(e) = fs::create_dir_all(&assets_dir) {
        eprintln!("failed to create directory {}: {e}", assets_dir.display());
        return ExitCode::FAILURE;
    }
    let exe_path = dir.join("full_stack_xsx.exe");
    let asset_path = assets_dir.join("level.bin");
    let exe_bytes = build_full_stack_xsx_pe();

    if let Err(e) = fs::write(&exe_path, &exe_bytes) {
        eprintln!("failed to write {}: {e}", exe_path.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(&asset_path, FULLSTACK_ASSET_PAYLOAD) {
        eprintln!("failed to write {}: {e}", asset_path.display());
        return ExitCode::FAILURE;
    }

    println!(
        "Generated Phase 3 + Phase 4 full-stack suite:\n  - {} ({} bytes)\n  - {} ({} bytes)",
        exe_path.display(),
        exe_bytes.len(),
        asset_path.display(),
        FULLSTACK_ASSET_PAYLOAD.len()
    );
    ExitCode::SUCCESS
}

fn cmd_gen_realgame(args: &[String]) -> ExitCode {
    let Some(out_dir) = args.first() else {
        eprintln!("usage: xsxemu gen-realgame <output_dir>");
        return ExitCode::from(2);
    };
    let dir = Path::new(out_dir);
    let data_dir = dir.join("data");
    if let Err(e) = fs::create_dir_all(&data_dir) {
        eprintln!("failed to create directory {}: {e}", data_dir.display());
        return ExitCode::FAILURE;
    }
    let exe_path = dir.join("realgame_xsx.exe");
    let cfg_path = data_dir.join("config.txt");
    let exe_bytes = build_real_game_xsx_pe();
    if let Err(e) = fs::write(&exe_path, &exe_bytes) {
        eprintln!("failed to write {}: {e}", exe_path.display());
        return ExitCode::FAILURE;
    }
    if let Err(e) = fs::write(&cfg_path, REALGAME_PKG_CONFIG) {
        eprintln!("failed to write {}: {e}", cfg_path.display());
        return ExitCode::FAILURE;
    }
    println!(
        "Generated Real-Game Xbox Series X ABI suite:\n  - {} ({} bytes)\n  - {} ({} bytes)",
        exe_path.display(),
        exe_bytes.len(),
        cfg_path.display(),
        REALGAME_PKG_CONFIG.len()
    );
    ExitCode::SUCCESS
}

fn cmd_inspect_shader(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu inspect-shader <shader.bin>");
        return ExitCode::from(2);
    };
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let info = inspect_xbox_shader_blob(&bytes);
    println!("=== Xbox Series X Shader Inspection: {path} ===");
    println!("Format:            {}", info.kind);
    println!("Total Size:        {} bytes", info.total_bytes);
    println!("Extracted RDNA 2:  {} bytes", info.extracted_rdna2_bytes);
    println!("Payload SHA-256:   {}", info.payload_sha256);
    println!(
        "Chunks:            {}",
        if info.chunks.is_empty() {
            "none".to_string()
        } else {
            info.chunks.join(", ")
        }
    );
    ExitCode::SUCCESS
}

fn extract_shader_isa(bytes: &[u8]) -> Vec<u8> {
    if let Some(fssm) = extract_fssm_program(bytes) {
        return fssm;
    }
    if let Some(fspx) = extract_fspx_parts(bytes) {
        if !fspx.code.is_empty() {
            return fspx.code.to_vec();
        }
    }
    bytes.to_vec()
}

fn cmd_inspect_xvd(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu inspect-xvd <container.xvd>");
        return ExitCode::from(2);
    };
    let xvd = match XvdContainer::open_file(Path::new(path)) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("XVD parse error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let h = xvd.header();
    println!("=== Xbox Virtual Disk (XVD) Inspection: {path} ===");
    println!("Magic:             {}", h.magic);
    println!("Format Version:    {}", h.format_version);
    println!("Content Type:      {}", h.content_type);
    println!("Content ID:        {}", h.content_id_hex());
    println!("Volume Flags:      0x{:08X}", h.volume_flags);
    println!("Encrypted:         {}", h.is_encrypted);
    println!("Data Integrity:    {}", h.has_data_integrity);
    println!("Drive Size:        {} bytes", h.drive_size);
    println!("User Data Offset:  0x{:X}", h.user_data_offset);
    println!("User Data Length:  {} bytes", h.user_data_length);
    ExitCode::SUCCESS
}

fn cmd_disasm(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu disasm <shader.bin>");
        return ExitCode::from(2);
    };
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let isa = extract_shader_isa(&bytes);
    let insns = disassemble_rdna2(&isa);
    println!(
        "; RDNA 2 Disassembly: {path} ({} bytes -> {} instructions)",
        isa.len(),
        insns.len()
    );
    print!("{}", format_rdna2_disassembly(&insns));
    ExitCode::SUCCESS
}

fn cmd_lift_hlsl(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu lift-hlsl <shader.bin> [--stage vs|ps|cs]");
        return ExitCode::from(2);
    };
    let mut stage = ShaderStage::Pixel;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--stage" {
            i += 1;
            if let Some(s) = args.get(i).and_then(|v| ShaderStage::parse(v)) {
                stage = s;
            } else {
                eprintln!("invalid --stage (expected vs, ps, or cs)");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let isa = extract_shader_isa(&bytes);
    match lift_rdna2_to_hlsl(&isa, stage) {
        Ok(lifted) => {
            println!(
                "// Lifted {} / {} instructions ({} comments) -> {} ({})",
                lifted.lifted_count,
                lifted.instruction_count,
                lifted.fallback_comments,
                lifted.entry_point,
                lifted.profile
            );
            print!("{}", lifted.hlsl);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("HLSL lift error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_boot(args: &[String]) -> ExitCode {
    let Some(pkg_str) = args.first() else {
        eprintln!("usage: xsxemu boot <package_dir> [RUN OPTIONS] [-- guest-args...]");
        return ExitCode::from(2);
    };
    let pkg_dir = Path::new(pkg_str);
    let (cfg, manifest_path) = match GameConfig::load_from_package_dir(pkg_dir) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to load MicrosoftGame.Config from {}: {e}", pkg_dir.display());
            return ExitCode::FAILURE;
        }
    };
    if let Some(tid) = cfg.title_id_u32 {
        set_active_title_id(tid);
    }
    let Some(exe_path) = cfg.resolve_primary_executable_path(pkg_dir) else {
        eprintln!("MicrosoftGame.Config lists no executable");
        return ExitCode::FAILURE;
    };
    println!("=== Xbox Series X Package Boot ({}) ===", manifest_path.display());
    println!(
        "Display Name:      {}",
        if cfg.display_name.is_empty() {
            &cfg.identity_name
        } else {
            &cfg.display_name
        }
    );
    println!("TitleId:           {}", cfg.title_id);
    if !cfg.store_id.is_empty() {
        println!("StoreId:           {}", cfg.store_id);
    }
    println!(
        "Anaconda Memory:   {:?} ({:.1} GiB budget)",
        cfg.anaconda_memory_mode,
        cfg.anaconda_memory_mode.anaconda_budget_bytes() as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    if let Some(mb) = cfg.persistent_local_storage_mb {
        println!("PLS Quota (S:\\):   {} MiB", mb);
    }
    println!("Primary Executable: {}", exe_path.display());

    // Forward into cmd_run with --game-dir <package_dir> prepended unless already given
    let mut forwarded = vec![exe_path.to_string_lossy().into_owned()];
    let has_game_dir = args[1..].iter().any(|a| a == "--game-dir");
    if !has_game_dir {
        forwarded.push("--game-dir".into());
        forwarded.push(pkg_str.clone());
    }
    forwarded.extend_from_slice(&args[1..]);
    cmd_run(&forwarded)
}

fn cmd_bench_fsr(args: &[String]) -> ExitCode {
    let mut width: u32 = 1280;
    let mut height: u32 = 720;
    let mut fsr = FsrConfig::default();
    let mut iters: u32 = 5;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--iters" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|s| s.parse().ok()) {
                    iters = v;
                }
            }
            "--width" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|s| s.parse().ok()) {
                    width = v;
                }
            }
            "--height" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|s| s.parse().ok()) {
                    height = v;
                }
            }
            "--fsr" => {
                i += 1;
                if let Some(m) = args.get(i).and_then(|s| FsrQualityMode::parse(s)) {
                    fsr.mode = m;
                }
            }
            _ => {}
        }
        i += 1;
    }

    let frame = xsx_d3d12::fsr::test_pattern(width.max(1), height.max(1));

    let r = xsx_d3d12::fsr::bench_frame(&frame, &fsr, iters);
    println!(
        "FSR Benchmark ({}): {}x{} -> {}x{} | EASU={} RCAS={} | avg {:.2} ms, min {:.2} ms ({:.1} FPS avg, {} iters, {} threads)",
        r.mode.name(),
        width,
        height,
        r.output_width,
        r.output_height,
        r.easu_ran,
        r.rcas_ran,
        r.avg_ms,
        r.min_ms,
        r.fps(),
        r.iterations,
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    );
    ExitCode::SUCCESS
}

fn cmd_inspect(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu inspect <binary.exe|dll>");
        return ExitCode::from(2);
    };
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let pe = match PeImage::parse(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("PE32+ parse error: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!("=== PE32+ Image Inspection: {path} ===");
    println!(
        "Type:              {}",
        if pe.is_dll() { "DLL" } else { "EXE" }
    );
    println!("Machine:           0x{:04X} (AMD64)", pe.machine);
    println!("Preferred Base:    0x{:016X}", pe.image_base);
    println!("Entry Point RVA:   0x{:08X}", pe.entry_point_rva);
    println!(
        "Size Of Image:     0x{:08X} ({} bytes)",
        pe.size_of_image, pe.size_of_image
    );
    println!("Sections ({}):", pe.sections.len());
    for s in &pe.sections {
        println!(
            "  {:<8} RVA=0x{:08X} VSize=0x{:06X} RawOff=0x{:06X} RawSize=0x{:06X} Flags=0x{:08X}",
            s.name,
            s.virtual_address,
            s.virtual_size,
            s.pointer_to_raw_data,
            s.size_of_raw_data,
            s.characteristics
        );
    }

    let xbox_dlls = pe.xbox_dll_imports();
    println!(
        "Xbox GDK/D3D12.x DLLs detected: {}",
        if xbox_dlls.is_empty() {
            "none".to_string()
        } else {
            xbox_dlls.join(", ")
        }
    );

    println!("Import Table ({} DLLs):", pe.imports.len());
    for desc in &pe.imports {
        println!("  {} ({} symbols):", desc.dll_name, desc.entries.len());
        for entry in &desc.entries {
            println!("    IAT RVA 0x{:08X} -> {}", entry.iat_rva, entry.symbol);
        }
    }

    if !pe.delay_imports.is_empty() {
        println!("Delay-Load Import Table ({} DLLs):", pe.delay_imports.len());
        for desc in &pe.delay_imports {
            println!("  {} ({} symbols):", desc.dll_name, desc.entries.len());
            for entry in &desc.entries {
                println!("    Delay IAT RVA 0x{:08X} -> {}", entry.iat_rva, entry.symbol);
            }
        }
    }

    if let Some(exp) = &pe.exports {
        println!(
            "Export Table ({}, {} entries):",
            exp.dll_name,
            exp.entries.len()
        );
        for e in &exp.entries {
            println!(
                "  #{} {} -> RVA 0x{:08X}",
                e.ordinal,
                e.name.as_deref().unwrap_or("<ordinal>"),
                e.rva
            );
        }
    }

    if let Some(tls) = &pe.tls {
        println!(
            "TLS Directory:     IndexVA=0x{:016X} CallbacksVA=0x{:016X}",
            tls.address_of_index, tls.address_of_callbacks
        );
    }

    if !pe.runtime_functions.is_empty() {
        println!(
            "Exception (.pdata): {} RUNTIME_FUNCTION entries",
            pe.runtime_functions.len()
        );
    }

    let total_relocs: usize = pe.relocations.iter().map(|b| b.entries.len()).sum();
    println!(
        "Base Relocations:  {} blocks ({} entries)",
        pe.relocations.len(),
        total_relocs
    );
    ExitCode::SUCCESS
}

fn parse_resolution(spec: &str) -> Option<(u32, u32)> {
    let lower = spec.to_ascii_lowercase();
    let (w_str, h_str) = lower.split_once('x')?;
    let w: u32 = w_str.parse().ok()?;
    let h: u32 = h_str.parse().ok()?;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

fn cmd_run(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu run <binary.exe> [OPTIONS]");
        return ExitCode::from(2);
    };

    let mut dry_run = false;
    let mut backend = BackendKind::Auto;
    let mut windowed = false;
    let mut fsr = FsrConfig::default();
    let mut shader_cache_dir: Option<PathBuf> = None;
    let mut rtc = RtcConfig::default();
    let mut live_audio = false;
    let mut dump_audio: Option<PathBuf> = None;
    let mut dump_frame: Option<PathBuf> = None;
    let mut extra_dll_dirs: Vec<PathBuf> = Vec::new();
    let mut game_dir: Option<PathBuf> = None;
    let mut overlay_dir: Option<PathBuf> = None;
    let mut scratch_dir: Option<PathBuf> = None;
    let mut unimplemented_policy = UnimplementedPolicy::Log;
    let mut trace_jsonl: Option<PathBuf> = None;
    let mut guest_args: Vec<String> = Vec::new();
    let mut trace = false;
    let mut allow_stubs = false;
    let mut force_relocate = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--" => {
                guest_args.extend_from_slice(&args[i + 1..]);
                break;
            }
            "--dry-run" => dry_run = true,
            "--backend" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--backend requires a value: auto | d3d12 | software");
                    return ExitCode::from(2);
                };
                backend = match val.as_str() {
                    "auto" => BackendKind::Auto,
                    "d3d12" => BackendKind::NativeD3D12,
                    "software" => BackendKind::Software,
                    other => {
                        eprintln!("unknown backend '{other}' (expected auto, d3d12, or software)");
                        return ExitCode::from(2);
                    }
                };
            }
            "--windowed" => windowed = true,
            "--fsr" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--fsr requires a quality mode (e.g. quality, balanced, performance, off)");
                    return ExitCode::from(2);
                };
                let Some(mode) = FsrQualityMode::parse(val) else {
                    eprintln!("unknown --fsr mode '{val}'");
                    return ExitCode::from(2);
                };
                fsr.mode = mode;
            }
            "--fsr-sharpness" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--fsr-sharpness requires a float in 0.0..=1.0");
                    return ExitCode::from(2);
                };
                match val.parse::<f32>() {
                    Ok(s) => fsr.sharpness = s.clamp(0.0, 1.0),
                    Err(_) => {
                        eprintln!("invalid --fsr-sharpness value '{val}'");
                        return ExitCode::from(2);
                    }
                }
            }
            "--output-res" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--output-res requires WxH (e.g. 1920x1080)");
                    return ExitCode::from(2);
                };
                let Some((w, h)) = parse_resolution(val) else {
                    eprintln!("invalid --output-res '{val}' (expected WxH, e.g. 1920x1080)");
                    return ExitCode::from(2);
                };
                fsr.output_resolution = Some((w, h));
            }
            "--shader-cache" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--shader-cache requires a directory path");
                    return ExitCode::from(2);
                };
                shader_cache_dir = Some(PathBuf::from(val));
            }
            "--rtc-offset" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--rtc-offset requires a signed integer number of seconds");
                    return ExitCode::from(2);
                };
                match val.parse::<i64>() {
                    Ok(sec) => rtc.offset_seconds = sec,
                    Err(_) => {
                        eprintln!("invalid --rtc-offset value '{val}'");
                        return ExitCode::from(2);
                    }
                }
            }
            "--live-audio" => live_audio = true,
            "--dump-audio" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--dump-audio requires an output .wav path");
                    return ExitCode::from(2);
                };
                dump_audio = Some(PathBuf::from(val));
            }
            "--dll-dir" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--dll-dir requires a directory path");
                    return ExitCode::from(2);
                };
                extra_dll_dirs.push(PathBuf::from(val));
            }
            "--game-dir" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--game-dir requires a directory path");
                    return ExitCode::from(2);
                };
                game_dir = Some(PathBuf::from(val));
            }
            "--overlay" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--overlay requires a directory path");
                    return ExitCode::from(2);
                };
                overlay_dir = Some(PathBuf::from(val));
            }
            "--scratch-dir" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--scratch-dir requires a directory path");
                    return ExitCode::from(2);
                };
                scratch_dir = Some(PathBuf::from(val));
            }
            "--unimplemented" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--unimplemented requires: log | fail | stop");
                    return ExitCode::from(2);
                };
                let Some(p) = UnimplementedPolicy::parse(val) else {
                    eprintln!("unknown --unimplemented policy '{val}' (expected log, fail, or stop)");
                    return ExitCode::from(2);
                };
                unimplemented_policy = p;
            }
            "--trace-jsonl" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--trace-jsonl requires an output .jsonl file path");
                    return ExitCode::from(2);
                };
                trace_jsonl = Some(PathBuf::from(val));
            }
            "--dump-frame" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--dump-frame requires an output path");
                    return ExitCode::from(2);
                };
                dump_frame = Some(PathBuf::from(val));
            }
            "--trace" => trace = true,
            "--allow-stubs" => allow_stubs = true,
            "--force-relocate" => force_relocate = true,
            other => {
                eprintln!("unknown option: {other}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }

    let exe_path = Path::new(path);
    let bytes = match fs::read(exe_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("failed to read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let pe = match PeImage::parse(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("PE32+ parse error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut vfs = VirtualFs::default_temp();
    if let Some(g) = game_dir {
        vfs.game_root = g;
    } else if let Some(parent) = exe_path.parent() {
        if !parent.as_os_str().is_empty() {
            vfs.game_root = parent.to_path_buf();
        }
    }
    if let Some(s) = scratch_dir {
        vfs.save_root = s.join("saves");
        vfs.overlay_root = s.join("overlay");
        vfs.scratch_root = s;
    }
    if let Some(ov) = overlay_dir {
        vfs.overlay_root = ov;
    }

    set_unimplemented_policy(unimplemented_policy);
    set_guest_command_line(path, &guest_args);

    let cfg = TranslatorConfig {
        backend,
        trace_calls: trace,
        fsr,
        windowed,
        shader_cache_dir,
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::with_options(cfg, vfs, rtc, live_audio, trace);
    let tracer_opt = if let Some(tp) = trace_jsonl {
        match StartupTracer::create(&tp) {
            Ok(t) => {
                let arc = Arc::new(t);
                runtime.set_startup_tracer(Arc::clone(&arc));
                Some(arc)
            }
            Err(e) => {
                eprintln!("failed to create --trace-jsonl file {}: {e}", tp.display());
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let _guard = RuntimeGuard::activate(runtime.clone());

    let registry = ModuleRegistry::with_policy(allow_stubs || dry_run, unimplemented_policy);
    let search_dirs = default_search_dirs(exe_path, &extra_dll_dirs);
    let main_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("guest.exe")
        .to_string();

    if dry_run {
        // Non-executing dry-run report: inspect PE imports and classify shim vs unimplemented HRESULT/other
        let mut shim_count = 0usize;
        let mut unimpl_hresult = 0usize;
        let mut unimpl_other = 0usize;
        println!(
            "DRY-RUN {}  preferred=0x{:016X} size=0x{:X} entry=+0x{:X}",
            main_name, pe.image_base, pe.size_of_image, pe.entry_point_rva
        );
        let strict = ModuleRegistry::with_policy(false, unimplemented_policy);
        for desc in pe.imports.iter().chain(pe.delay_imports.iter()) {
            for entry in &desc.entries {
                if xsx_loader::ImportResolver::resolve(&strict, &desc.dll_name, &entry.symbol)
                    .is_some()
                {
                    shim_count += 1;
                } else {
                    let sym_name = match &entry.symbol {
                        ImportSymbol::Name { name, .. } => name.as_str(),
                        ImportSymbol::Ordinal(_) => "",
                    };
                    let cls = classify_import_return_class(&desc.dll_name, sym_name);
                    match cls {
                        ReturnClass::HResult => {
                            unimpl_hresult += 1;
                            println!("  UNIMPLEMENTED {}!{}  [returns HRESULT]", desc.dll_name, entry.symbol);
                        }
                        ReturnClass::Other => {
                            unimpl_other += 1;
                            println!("  UNIMPLEMENTED {}!{}", desc.dll_name, entry.symbol);
                        }
                    }
                }
            }
        }
        println!(
            "Summary: shim={} unimplemented={} (hresult={} other={})",
            shim_count,
            unimpl_hresult + unimpl_other,
            unimpl_hresult,
            unimpl_other
        );
        if let Some(t) = &tracer_opt {
            t.record_exit(0);
        }
        return ExitCode::SUCCESS;
    }

    let module_set = match unsafe {
        LoadedModuleSet::load_with_dependencies(
            main_name.clone(),
            &pe,
            &registry,
            search_dirs,
            force_relocate,
        )
    } {
        Ok(set) => set,
        Err(e) => {
            eprintln!("loader error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let main_img = Arc::clone(module_set.main_image());
    if let Some(t) = &tracer_opt {
        t.record_map(
            &main_name,
            main_img.base_addr(),
            pe.size_of_image as usize,
            main_img.relocations_applied() > 0,
        );
    }
    println!(
        "Loaded {path} at 0x{:016X} (preferred 0x{:016X}, {} relocations, {} imports, {} secondary DLLs, {} TLS callbacks, {} .pdata entries)",
        main_img.base_addr(),
        main_img.preferred_base(),
        main_img.relocations_applied(),
        main_img.resolved_imports().len(),
        module_set.loaded_dll_names().len(),
        main_img.tls_callbacks().len(),
        main_img.runtime_functions().len()
    );

    let modules_arc = Arc::new(Mutex::new(module_set));
    runtime.attach_modules(Arc::clone(&modules_arc));

    let exit_code = unsafe { main_img.execute_entry() };
    if let Some(t) = &tracer_opt {
        t.record_exit(exit_code as i32);
    }
    println!("Guest exited with code {exit_code}");
    println!("RTC (UTC): {}", runtime.rtc().format_utc_iso8601());

    for msg in runtime.debug_messages() {
        println!("[Guest OutputDebugStringA] {msg}");
    }

    let frames = runtime.presented_frames();
    let fsr_stats = runtime.fsr_stats();
    println!("Presented frames: {}", frames.len());
    if let Some(st) = fsr_stats.last() {
        println!(
            "FSR ({}): {}x{} -> {}x{} (EASU={}, RCAS={}, sharpness={:.2}, {:.2}ms)",
            st.mode.name(),
            st.input_width,
            st.input_height,
            st.output_width,
            st.output_height,
            st.easu_ran,
            st.rcas_ran,
            st.sharpness,
            st.duration_us as f64 / 1000.0
        );
    }
    if let Some(tr) = runtime.d3d12_translator() {
        let (hits, misses) = tr.shader_cache_stats();
        if hits > 0 || misses > 0 {
            println!("Shader cache: {hits} hit(s), {misses} miss(es)");
        }
    }
    if runtime.audio_samples_submitted() > 0 {
        println!(
            "Audio mixed: {} stereo 48kHz frames ({} bytes)",
            runtime.audio_samples_submitted(),
            runtime.audio_pcm_len()
        );
    }
    if runtime.dstorage_bytes_read() > 0 {
        println!("DirectStorage streamed: {} bytes", runtime.dstorage_bytes_read());
    }

    if let Some(wav_path) = dump_audio {
        match runtime.write_audio_wav(&wav_path) {
            Ok(()) => println!("Dumped mixed audio WAV to {}", wav_path.display()),
            Err(e) => {
                eprintln!("failed to dump audio WAV: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if let (Some(out_path), Some(frame)) = (dump_frame, frames.last()) {
        let ext = out_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let res = if ext == "ppm" {
            frame.write_ppm(&out_path)
        } else {
            frame.write_bmp(&out_path)
        };
        match res {
            Ok(()) => println!(
                "Dumped presented frame {} ({}x{}) to {}",
                frame.frame_index,
                frame.width,
                frame.height,
                out_path.display()
            ),
            Err(e) => {
                eprintln!("failed to dump frame: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if exit_code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(exit_code.min(255) as u8)
    }
}
