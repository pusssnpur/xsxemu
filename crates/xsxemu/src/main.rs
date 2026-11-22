//! `xsxemu` CLI — Xbox Series X|S clean-room game translation layer runner.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use xsx_d3d12::{BackendKind, FsrConfig, FsrQualityMode, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RuntimeGuard, VirtualFs};
use xsx_loader::{default_search_dirs, LoadedModuleSet, PeImage};
use xsx_testgen::{
    build_full_stack_xsx_pe, build_hello_xsx_pe, build_helper_dll_pe, build_phase2_xsx_pe,
    FULLSTACK_ASSET_PAYLOAD,
};

fn print_usage() {
    eprintln!(
        "xsxemu — Clean-room Xbox Series X|S Game Translation Layer (v{})\n\
         \n\
         USAGE:\n\
           xsxemu gen-sample <output.exe>\n\
           xsxemu gen-phase2 <output_dir>\n\
           xsxemu gen-fullstack <output_dir>\n\
           xsxemu inspect <binary.exe|dll>\n\
           xsxemu run <binary.exe> [OPTIONS]\n\
         \n\
         RUN OPTIONS:\n\
           --backend <auto|d3d12|software>       Select GPU backend (default: auto)\n\
           --windowed                            Show visible Win32 swapchain window on Windows\n\
           --fsr <quality|balanced|performance|  AMD FSR upscaling preset (default: quality)\n\
                  ultra-quality|ultra-performance|native-aa|off>\n\
           --fsr-sharpness <0.0..=1.0>           RCAS sharpening strength (default: 0.8)\n\
           --output-res <WxH>                    Explicit FSR output resolution (e.g. 1920x1080)\n\
           --dll-dir <path>                      Extra search directory for secondary guest DLLs\n\
           --game-dir <path>                     Host directory mapped to Xbox G:\\ (and D:\\)\n\
           --scratch-dir <path>                  Host directory mapped to Xbox T:\\\n\
           --dump-frame <path.bmp|path.ppm>      Write presented (FSR-upscaled) frame to disk\n\
           --trace                               Enable HLE, D3D12.x, and FSR call tracing\n\
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
        "gen-sample" => cmd_gen_sample(&args[1..]),
        "gen-phase2" => cmd_gen_phase2(&args[1..]),
        "gen-fullstack" => cmd_gen_fullstack(&args[1..]),
        "inspect" => cmd_inspect(&args[1..]),
        "run" => cmd_run(&args[1..]),
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

    let mut backend = BackendKind::Auto;
    let mut windowed = false;
    let mut fsr = FsrConfig::default(); // Enabled by default (Quality 1.5x EASU + RCAS)
    let mut dump_frame: Option<PathBuf> = None;
    let mut extra_dll_dirs: Vec<PathBuf> = Vec::new();
    let mut game_dir: Option<PathBuf> = None;
    let mut scratch_dir: Option<PathBuf> = None;
    let mut trace = false;
    let mut allow_stubs = false;
    let mut force_relocate = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
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
            "--scratch-dir" => {
                i += 1;
                let Some(val) = args.get(i) else {
                    eprintln!("--scratch-dir requires a directory path");
                    return ExitCode::from(2);
                };
                scratch_dir = Some(PathBuf::from(val));
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
        vfs.scratch_root = s;
    }

    let cfg = TranslatorConfig {
        backend,
        trace_calls: trace,
        fsr,
        windowed,
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::with_vfs(cfg, vfs, trace);
    let _guard = RuntimeGuard::activate(runtime.clone());

    let registry = ModuleRegistry::new_default(allow_stubs);
    let search_dirs = default_search_dirs(exe_path, &extra_dll_dirs);
    let main_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("guest.exe")
        .to_string();

    let module_set = match unsafe {
        LoadedModuleSet::load_with_dependencies(
            main_name,
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
    println!("Guest exited with code {exit_code}");

    for msg in runtime.debug_messages() {
        println!("[Guest OutputDebugStringA] {msg}");
    }

    let frames = runtime.presented_frames();
    let fsr_stats = runtime.fsr_stats();
    println!("Presented frames: {}", frames.len());
    if let Some(st) = fsr_stats.last() {
        println!(
            "FSR ({}): {}x{} -> {}x{} (EASU={}, RCAS={}, sharpness={:.2}, {}us)",
            st.mode.name(),
            st.input_width,
            st.input_height,
            st.output_width,
            st.output_height,
            st.easu_ran,
            st.rcas_ran,
            st.sharpness,
            st.duration_us
        );
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
