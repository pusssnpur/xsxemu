//! `xsxemu` CLI — Xbox Series X|S clean-room game translation layer runner.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use xsx_d3d12::{BackendKind, TranslatorConfig};
use xsx_hle::{GuestRuntime, ModuleRegistry, RuntimeGuard};
use xsx_loader::{LoadedImage, PeImage};
use xsx_testgen::build_hello_xsx_pe;

fn print_usage() {
    eprintln!(
        "xsxemu — Clean-room Xbox Series X|S Game Translation Layer (v{})\n\
         \n\
         USAGE:\n\
           xsxemu gen-sample <output.exe>\n\
           xsxemu inspect <binary.exe>\n\
           xsxemu run <binary.exe> [OPTIONS]\n\
         \n\
         RUN OPTIONS:\n\
           --backend <auto|d3d12|software>   Select GPU backend (default: auto)\n\
           --dump-frame <path.bmp|path.ppm>  Write presented frame to image file\n\
           --trace                           Enable HLE & D3D12.x call tracing\n\
           --allow-stubs                     Stub unresolved imports instead of failing\n\
           --force-relocate                  Force base relocation away from preferred ImageBase",
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

fn cmd_inspect(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu inspect <binary.exe>");
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
    println!("Machine:           0x{:04X} (AMD64)", pe.machine);
    println!("Preferred Base:    0x{:016X}", pe.image_base);
    println!("Entry Point RVA:   0x{:08X}", pe.entry_point_rva);
    println!("Size Of Image:     0x{:08X} ({} bytes)", pe.size_of_image, pe.size_of_image);
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

    let total_relocs: usize = pe.relocations.iter().map(|b| b.entries.len()).sum();
    println!(
        "Base Relocations:  {} blocks ({} entries)",
        pe.relocations.len(),
        total_relocs
    );
    ExitCode::SUCCESS
}

fn cmd_run(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: xsxemu run <binary.exe> [OPTIONS]");
        return ExitCode::from(2);
    };

    let mut backend = BackendKind::Auto;
    let mut dump_frame: Option<PathBuf> = None;
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

    let registry = ModuleRegistry::new_default(allow_stubs);
    let loaded = match LoadedImage::load(&pe, &registry, force_relocate) {
        Ok(img) => img,
        Err(e) => {
            eprintln!("loader error: {e}");
            return ExitCode::FAILURE;
        }
    };

    println!(
        "Loaded {path} at 0x{:016X} (preferred 0x{:016X}, {} relocations, {} imports resolved)",
        loaded.base_addr(),
        loaded.preferred_base(),
        loaded.relocations_applied(),
        loaded.resolved_imports().len()
    );

    let cfg = TranslatorConfig {
        backend,
        trace_calls: trace,
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::new(cfg, trace);
    let _guard = RuntimeGuard::activate(runtime.clone());

    let exit_code = unsafe { loaded.execute_entry() };
    println!("Guest exited with code {exit_code}");

    for msg in runtime.debug_messages() {
        println!("[Guest OutputDebugStringA] {msg}");
    }

    let frames = runtime.presented_frames();
    println!("Presented frames: {}", frames.len());

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
