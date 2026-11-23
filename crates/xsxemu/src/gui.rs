//! Zero-dependency GUI for `xsxemu`:
//! - Cross-platform embedded Web GUI (`std::net::TcpListener`, `xsxemu gui --web`)
//! - Native Win32 desktop launcher on Windows (`xsxemu gui`)

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use xsx_d3d12::fsr::{bench_frame, test_pattern};
use xsx_d3d12::{
    disassemble_rdna2, extract_fssm_program, extract_fspx_parts, format_rdna2_disassembly,
    lift_rdna2_to_hlsl, BackendKind, FsrConfig, FsrQualityMode, PresentedFrame, ShaderStage,
    TranslatorConfig,
};
use xsx_hle::{
    classify_import_return_class, set_active_title_id, set_guest_command_line,
    set_unimplemented_policy, GuestRuntime, ModuleRegistry, ReturnClass, RtcConfig, RuntimeGuard,
    UnimplementedPolicy, VirtualFs,
};
use xsx_loader::{
    default_search_dirs, GameConfig, ImportSymbol, LoadedModuleSet, PeImage, XvdContainer,
};
use xsx_testgen::build_actual_game_xsx_pe;

const WEB_HTML: &str = include_str!("gui_web.html");

/// Options for launching `xsxemu gui`.
#[derive(Debug, Clone)]
pub struct GuiOptions {
    pub web_only: bool,
    pub port: u16,
    pub port_pinned: bool,
    pub open_browser: bool,
}

impl Default for GuiOptions {
    fn default() -> Self {
        Self {
            web_only: false,
            port: 8080,
            port_pinned: false,
            open_browser: true,
        }
    }
}

/// Entry point for `xsxemu gui`.
pub fn launch_gui(opts: GuiOptions) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    if !opts.web_only {
        return win32::run_native_launcher(opts);
    }

    let addr = format!("127.0.0.1:{}", opts.port);
    let server = match GuiServer::bind(&addr) {
        Ok(s) => s,
        Err(_) if !opts.port_pinned => GuiServer::bind("127.0.0.1:0")?,
        Err(e) => return Err(e),
    };
    let bound = server.addr();
    let url = format!("http://{bound}/");
    println!("xsxemu Web GUI listening at {url} (press Ctrl+C to stop)");
    if opts.open_browser {
        open_in_browser(&url);
    }
    server.serve_forever();
    Ok(())
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for (prog, prefix) in [
            ("xdg-open", &[][..]),
            ("gio", &["open"][..]),
            ("sensible-browser", &[][..]),
            ("firefox", &[][..]),
            ("chromium", &[][..]),
            ("google-chrome", &[][..]),
        ] {
            let mut cmd = Command::new(prog);
            cmd.args(prefix)
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if cmd.spawn().is_ok() {
                break;
            }
        }
    }
}

/// Embedded HTTP/1.1 Web GUI server using `std::net::TcpListener` only.
pub struct GuiServer {
    listener: TcpListener,
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
}

/// Handle returned by [`GuiServer::spawn_background`] that stops the server on drop.
pub struct GuiHandle {
    pub addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Drop for GuiHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl GuiServer {
    pub fn bind(addr: &str) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        Ok(Self {
            listener,
            addr,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn serve_forever(&self) {
        for stream in self.listener.incoming() {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            if let Ok(stream) = stream {
                if self.stop.load(Ordering::SeqCst) {
                    break;
                }
                thread::spawn(move || {
                    let _ = handle_connection(stream);
                });
            }
        }
    }

    pub fn spawn_background(self) -> GuiHandle {
        let addr = self.addr;
        let stop = Arc::clone(&self.stop);
        let join = thread::spawn(move || self.serve_forever());
        GuiHandle {
            addr,
            stop,
            join: Some(join),
        }
    }
}

fn handle_connection(mut stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut req_line = String::new();
    if reader.read_line(&mut req_line)? == 0 {
        return Ok(());
    }
    let parts: Vec<&str> = req_line.split_whitespace().collect();
    if parts.len() < 2 {
        return write_response(&mut stream, "400 Bad Request", "text/plain", b"bad request");
    }
    let method = parts[0];
    let target = parts[1];

    let mut content_len = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_len = v.trim().parse().unwrap_or(0).min(4 * 1024 * 1024);
            }
        }
    }

    let mut body = vec![0u8; content_len];
    if content_len > 0 {
        reader.read_exact(&mut body)?;
    }
    let body_str = String::from_utf8_lossy(&body);

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            write_response(&mut stream, "200 OK", "text/html; charset=utf-8", WEB_HTML.as_bytes())
        }
        ("GET", "/api/status") => {
            let json = api_status();
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("GET", "/api/packages") => {
            let dir = query_param(query, "dir").unwrap_or_else(|| ".".to_string());
            let json = api_packages(&dir);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("POST", "/api/run") => {
            let json = api_run(&body_str);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("POST", "/api/gen-demo") => {
            let json = api_gen_demo(&body_str);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("POST", "/api/bench-fsr") => {
            let json = api_bench_fsr(&body_str);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("POST", "/api/inspect-pe") => {
            let json = api_inspect_pe(&body_str);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("POST", "/api/disasm") => {
            let json = api_disasm(&body_str);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("POST", "/api/lift-hlsl") => {
            let json = api_lift_hlsl(&body_str);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        ("POST", "/api/inspect-xvd") => {
            let json = api_inspect_xvd(&body_str);
            write_response(&mut stream, "200 OK", "application/json", json.as_bytes())
        }
        _ => write_response(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn write_response(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// API Handlers
// ---------------------------------------------------------------------------

fn api_status() -> String {
    let threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    format!(
        "{{\"version\":\"{}\",\"os\":\"{os}/{arch}\",\"threads\":{threads},\"default_fsr\":\"Quality\"}}",
        env!("CARGO_PKG_VERSION")
    )
}

fn api_packages(dir_str: &str) -> String {
    let dir = Path::new(dir_str);
    let mut entries = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten().take(64) {
            let p = ent.path();
            if p.is_dir() && p.join("MicrosoftGame.Config").is_file() {
                entries.push(("PKG", p.to_string_lossy().into_owned()));
            } else if p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("exe"))
            {
                entries.push(("EXE", p.to_string_lossy().into_owned()));
            }
        }
    }
    if dir.is_dir() && dir.join("MicrosoftGame.Config").is_file() {
        entries.insert(0, ("PKG", dir.to_string_lossy().into_owned()));
    }
    let mut out = String::from("{\"entries\":[");
    for (idx, (kind, path)) in entries.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"kind\":\"{}\",\"path\":\"{}\"}}",
            kind,
            json_escape(path)
        );
    }
    out.push_str("]}");
    out
}

fn api_bench_fsr(body: &str) -> String {
    let width = json_u32(body, "width").unwrap_or(1280).clamp(16, 3840);
    let height = json_u32(body, "height").unwrap_or(720).clamp(16, 2160);
    let iters = json_u32(body, "iters").unwrap_or(5).clamp(1, 50);
    let mode = json_str(body, "fsr")
        .and_then(|s| FsrQualityMode::parse(&s))
        .unwrap_or(FsrQualityMode::Quality);
    let sharpness = json_f32(body, "sharpness").unwrap_or(0.8).clamp(0.0, 1.0);

    let cfg = FsrConfig {
        mode,
        sharpness,
        enable_rcas: mode != FsrQualityMode::Off,
        output_resolution: None,
    };
    let frame = test_pattern(width, height);
    let res = bench_frame(&frame, &cfg, iters);
    let summary = format!(
        "FSR Benchmark ({}): {}x{} -> {}x{} | EASU={} RCAS={} | avg {:.2} ms, min {:.2} ms ({:.1} FPS avg, {} iters)",
        res.mode.name(),
        width,
        height,
        res.output_width,
        res.output_height,
        res.easu_ran,
        res.rcas_ran,
        res.avg_ms,
        res.min_ms,
        res.fps(),
        res.iterations
    );
    let bmp_b64 = base64_encode(&res.frame.encode_bmp());
    format!(
        "{{\"summary\":\"{}\",\"avg_ms\":{:.3},\"min_ms\":{:.3},\"fps\":{:.2},\"frame_bmp_b64\":\"{}\"}}",
        json_escape(&summary),
        res.avg_ms,
        res.min_ms,
        res.fps(),
        bmp_b64
    )
}

struct RunSummary {
    exit_code: u32,
    log: String,
    output_res: String,
    fsr_mode: String,
    fsr_ms: Option<f64>,
    frame: Option<PresentedFrame>,
    audio_frames: u64,
    audio_wav: Option<Vec<u8>>,
}

fn execute_target(
    target: &Path,
    fsr: FsrConfig,
    backend: BackendKind,
    unimpl: UnimplementedPolicy,
    dry_run: bool,
) -> Result<RunSummary, String> {
    let (exe_path, game_dir_opt) = if target.is_dir() {
        let (cfg, _) = GameConfig::load_from_package_dir(target)
            .map_err(|e| format!("failed to load MicrosoftGame.Config: {e}"))?;
        if let Some(tid) = cfg.title_id_u32 {
            set_active_title_id(tid);
        }
        let exe = cfg
            .resolve_primary_executable_path(target)
            .ok_or_else(|| "MicrosoftGame.Config lists no executable".to_string())?;
        (exe, Some(target.to_path_buf()))
    } else {
        (
            target.to_path_buf(),
            target.parent().map(|p| p.to_path_buf()),
        )
    };

    let bytes = fs::read(&exe_path).map_err(|e| format!("failed to read {}: {e}", exe_path.display()))?;
    let pe = PeImage::parse(&bytes).map_err(|e| format!("PE32+ parse error: {e}"))?;
    let main_name = exe_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("guest.exe")
        .to_string();

    if dry_run {
        let strict = ModuleRegistry::with_policy(false, unimpl);
        let mut shims = 0usize;
        let mut unimpl_hr = 0usize;
        let mut unimpl_other = 0usize;
        let mut log = format!(
            "DRY-RUN {}  preferred=0x{:016X} size=0x{:X} entry=+0x{:X}\n",
            main_name, pe.image_base, pe.size_of_image, pe.entry_point_rva
        );
        for desc in pe.imports.iter().chain(pe.delay_imports.iter()) {
            for entry in &desc.entries {
                if xsx_loader::ImportResolver::resolve(&strict, &desc.dll_name, &entry.symbol).is_some() {
                    shims += 1;
                } else {
                    let sym = match &entry.symbol {
                        ImportSymbol::Name { name, .. } => name.as_str(),
                        ImportSymbol::Ordinal(_) => "",
                    };
                    match classify_import_return_class(&desc.dll_name, sym) {
                        ReturnClass::HResult => {
                            unimpl_hr += 1;
                            let _ = writeln!(log, "  UNIMPLEMENTED {}!{} [HRESULT]", desc.dll_name, entry.symbol);
                        }
                        ReturnClass::Other => {
                            unimpl_other += 1;
                            let _ = writeln!(log, "  UNIMPLEMENTED {}!{}", desc.dll_name, entry.symbol);
                        }
                    }
                }
            }
        }
        let _ = writeln!(
            log,
            "Summary: shim={shims} unimplemented={} (hresult={unimpl_hr} other={unimpl_other})",
            unimpl_hr + unimpl_other
        );
        return Ok(RunSummary {
            exit_code: 0,
            log,
            output_res: "DRY-RUN".into(),
            fsr_mode: fsr.mode.name().into(),
            fsr_ms: None,
            frame: None,
            audio_frames: 0,
            audio_wav: None,
        });
    }

    let mut vfs = VirtualFs::default_temp();
    if let Some(g) = game_dir_opt {
        if !g.as_os_str().is_empty() {
            vfs.game_root = g;
        }
    }
    set_unimplemented_policy(unimpl);
    set_guest_command_line(&exe_path.to_string_lossy(), &[]);

    let cfg = TranslatorConfig {
        backend,
        fsr: fsr.clone(),
        ..TranslatorConfig::default()
    };
    let runtime = GuestRuntime::with_options(cfg, vfs, RtcConfig::default(), false, false);
    let _guard = RuntimeGuard::activate(runtime.clone());

    let registry = ModuleRegistry::with_policy(false, unimpl);
    let search_dirs = default_search_dirs(&exe_path, &[]);
    let module_set = unsafe {
        LoadedModuleSet::load_with_dependencies(main_name.clone(), &pe, &registry, search_dirs, false)
    }
    .map_err(|e| format!("loader error: {e}"))?;

    let main_img = Arc::clone(module_set.main_image());
    let mut log = format!(
        "Loaded {} at 0x{:016X} ({} imports, {} secondary DLLs)\n",
        main_name,
        main_img.base_addr(),
        main_img.resolved_imports().len(),
        module_set.loaded_dll_names().len()
    );
    runtime.attach_modules(Arc::new(std::sync::Mutex::new(module_set)));

    let exit_code = unsafe { main_img.execute_entry() };
    let _ = writeln!(log, "Guest exited with code {exit_code}");
    let _ = writeln!(log, "RTC (UTC): {}", runtime.rtc().format_utc_iso8601());
    for msg in runtime.debug_messages() {
        let _ = writeln!(log, "[Guest OutputDebugStringA] {msg}");
    }

    let frames = runtime.presented_frames();
    let fsr_stats = runtime.fsr_stats();
    let _ = writeln!(log, "Presented frames: {}", frames.len());
    let (output_res, fsr_ms) = if let Some(st) = fsr_stats.last() {
        let ms = st.duration_us as f64 / 1000.0;
        let _ = writeln!(
            log,
            "FSR ({}): {}x{} -> {}x{} (EASU={}, RCAS={}, {:.2} ms)",
            st.mode.name(),
            st.input_width,
            st.input_height,
            st.output_width,
            st.output_height,
            st.easu_ran,
            st.rcas_ran,
            ms
        );
        (format!("{}x{}", st.output_width, st.output_height), Some(ms))
    } else if let Some(f) = frames.last() {
        (format!("{}x{}", f.width, f.height), None)
    } else {
        ("none".into(), None)
    };

    let audio_frames = runtime.audio_samples_submitted();
    let audio_wav = if audio_frames > 0 {
        let _ = writeln!(
            log,
            "Audio mixed: {} stereo 48kHz frames ({} bytes)",
            audio_frames,
            runtime.audio_pcm_len()
        );
        Some(runtime.encode_audio_wav())
    } else {
        None
    };

    Ok(RunSummary {
        exit_code,
        log,
        output_res,
        fsr_mode: fsr.mode.name().into(),
        fsr_ms,
        frame: frames.into_iter().last(),
        audio_frames,
        audio_wav,
    })
}

fn run_summary_to_json(r: Result<RunSummary, String>) -> String {
    match r {
        Ok(s) => {
            let bmp = s
                .frame
                .as_ref()
                .map(|f| base64_encode(&f.encode_bmp()))
                .unwrap_or_default();
            let wav = s
                .audio_wav
                .as_ref()
                .map(|w| base64_encode(w))
                .unwrap_or_default();
            let ms_json = s
                .fsr_ms
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "null".into());
            format!(
                "{{\"exit_code\":{},\"output_res\":\"{}\",\"fsr_mode\":\"{}\",\"fsr_ms\":{},\"audio_frames\":{},\"log\":\"{}\",\"frame_bmp_b64\":\"{}\",\"audio_wav_b64\":\"{}\"}}",
                s.exit_code,
                json_escape(&s.output_res),
                json_escape(&s.fsr_mode),
                ms_json,
                s.audio_frames,
                json_escape(&s.log),
                bmp,
                wav
            )
        }
        Err(e) => format!("{{\"error\":\"{}\"}}", json_escape(&e)),
    }
}

fn parse_fsr_and_backend(body: &str) -> (FsrConfig, BackendKind, UnimplementedPolicy) {
    let mode = json_str(body, "fsr")
        .and_then(|s| FsrQualityMode::parse(&s))
        .unwrap_or(FsrQualityMode::Quality);
    let sharpness = json_f32(body, "sharpness").unwrap_or(0.8).clamp(0.0, 1.0);
    let backend = match json_str(body, "backend").as_deref() {
        Some("d3d12") => BackendKind::NativeD3D12,
        Some("software") => BackendKind::Software,
        _ => BackendKind::Auto,
    };
    let unimpl = json_str(body, "unimplemented")
        .and_then(|s| UnimplementedPolicy::parse(&s))
        .unwrap_or(UnimplementedPolicy::Log);
    (
        FsrConfig {
            mode,
            sharpness,
            enable_rcas: mode != FsrQualityMode::Off,
            output_resolution: None,
        },
        backend,
        unimpl,
    )
}

fn api_run(body: &str) -> String {
    let path_str = match json_str(body, "path") {
        Some(p) if !p.trim().is_empty() => p,
        _ => return "{\"error\":\"please enter a package directory or .exe path\"}".into(),
    };
    let (fsr, backend, unimpl) = parse_fsr_and_backend(body);
    let dry_run = json_bool(body, "dry_run").unwrap_or(false);
    run_summary_to_json(execute_target(Path::new(&path_str), fsr, backend, unimpl, dry_run))
}

fn api_gen_demo(body: &str) -> String {
    let (fsr, backend, unimpl) = parse_fsr_and_backend(body);
    let demo_path = std::env::temp_dir().join(format!("xsxemu_gui_demo_{}.exe", std::process::id()));
    if let Err(e) = fs::write(&demo_path, build_actual_game_xsx_pe()) {
        return format!("{{\"error\":\"failed to write demo binary: {}\"}}", json_escape(&e.to_string()));
    }
    let res = execute_target(&demo_path, fsr, backend, unimpl, false);
    let _ = fs::remove_file(&demo_path);
    run_summary_to_json(res)
}

fn api_inspect_pe(body: &str) -> String {
    let Some(path) = json_str(body, "path").filter(|s| !s.trim().is_empty()) else {
        return "{\"error\":\"enter a .exe or .dll path\"}".into();
    };
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => return format!("{{\"error\":\"{}\"}}", json_escape(&e.to_string())),
    };
    let pe = match PeImage::parse(&bytes) {
        Ok(p) => p,
        Err(e) => return format!("{{\"error\":\"PE32+ parse error: {}\"}}", json_escape(&e.to_string())),
    };
    let mut out = format!(
        "PE32+ {}\nType: {} | Machine: 0x{:04X} | Base: 0x{:016X} | Entry: +0x{:08X}\nSections: {} | Imports: {} DLLs | Delay-Load: {} DLLs\nXbox DLLs: {}\n",
        path,
        if pe.is_dll() { "DLL" } else { "EXE" },
        pe.machine,
        pe.image_base,
        pe.entry_point_rva,
        pe.sections.len(),
        pe.imports.len(),
        pe.delay_imports.len(),
        pe.xbox_dll_imports().join(", ")
    );
    for d in pe.imports.iter().chain(pe.delay_imports.iter()) {
        let _ = writeln!(out, "  {} ({} symbols)", d.dll_name, d.entries.len());
    }
    format!("{{\"output\":\"{}\"}}", json_escape(&out))
}

fn extract_isa_bytes(bytes: &[u8]) -> Vec<u8> {
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

fn api_disasm(body: &str) -> String {
    let Some(path) = json_str(body, "path").filter(|s| !s.trim().is_empty()) else {
        return "{\"error\":\"enter a shader .bin path\"}".into();
    };
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => return format!("{{\"error\":\"{}\"}}", json_escape(&e.to_string())),
    };
    let isa = extract_isa_bytes(&bytes);
    let insns = disassemble_rdna2(&isa);
    let out = format!(
        "; RDNA 2 Disassembly ({} bytes -> {} instructions)\n{}",
        isa.len(),
        insns.len(),
        format_rdna2_disassembly(&insns)
    );
    format!("{{\"output\":\"{}\"}}", json_escape(&out))
}

fn api_lift_hlsl(body: &str) -> String {
    let Some(path) = json_str(body, "path").filter(|s| !s.trim().is_empty()) else {
        return "{\"error\":\"enter a shader .bin path\"}".into();
    };
    let stage = json_str(body, "stage")
        .and_then(|s| ShaderStage::parse(&s))
        .unwrap_or(ShaderStage::Pixel);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => return format!("{{\"error\":\"{}\"}}", json_escape(&e.to_string())),
    };
    let isa = extract_isa_bytes(&bytes);
    match lift_rdna2_to_hlsl(&isa, stage) {
        Ok(lifted) => {
            let out = format!(
                "// Lifted {}/{} instructions -> {} ({})\n{}",
                lifted.lifted_count, lifted.instruction_count, lifted.entry_point, lifted.profile, lifted.hlsl
            );
            format!("{{\"output\":\"{}\"}}", json_escape(&out))
        }
        Err(e) => format!("{{\"error\":\"HLSL lift error: {}\"}}", json_escape(&e.to_string())),
    }
}

fn api_inspect_xvd(body: &str) -> String {
    let Some(path) = json_str(body, "path").filter(|s| !s.trim().is_empty()) else {
        return "{\"error\":\"enter an .xvd file path\"}".into();
    };
    match XvdContainer::open_file(Path::new(&path)) {
        Ok(xvd) => {
            let h = xvd.header();
            let out = format!(
                "XVD {}\nMagic: {} | Version: {} | Type: {}\nContent ID: {}\nEncrypted: {} | Integrity: {} | Drive Size: {} bytes",
                path, h.magic, h.format_version, h.content_type, h.content_id_hex(), h.is_encrypted, h.has_data_integrity, h.drive_size
            );
            format!("{{\"output\":\"{}\"}}", json_escape(&out))
        }
        Err(e) => format!("{{\"error\":\"XVD parse error: {}\"}}", json_escape(&e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Minimal Zero-Dependency JSON, URL-Query & Base64 Helpers
// ---------------------------------------------------------------------------

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

fn json_find_val<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let idx = json.find(&needle)?;
    let rest = json[idx + needle.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    Some(rest)
}

fn json_str(json: &str, key: &str) -> Option<String> {
    let rest = json_find_val(json, key)?;
    let inner = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            return Some(out);
        }
        if c == '\\' {
            match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => out.push(other),
            }
        } else {
            out.push(c);
        }
    }
    None
}

fn json_u32(json: &str, key: &str) -> Option<u32> {
    let rest = json_find_val(json, key)?;
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_f32(json: &str, key: &str) -> Option<f32> {
    let rest = json_find_val(json, key)?;
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_bool(json: &str, key: &str) -> Option<bool> {
    let rest = json_find_val(json, key)?;
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
    }
    out
}

// ---------------------------------------------------------------------------
// Native Win32 Desktop GUI Launcher (Windows only, zero external crates)
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[allow(non_snake_case, non_camel_case_types)]
mod win32 {
    use super::*;
    use std::ffi::c_void;
    use std::sync::Mutex;

    const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
    const WS_VISIBLE: u32 = 0x1000_0000;
    const WS_CHILD: u32 = 0x4000_0000;
    const WS_BORDER: u32 = 0x0080_0000;
    const WS_VSCROLL: u32 = 0x0020_0000;
    const ES_MULTILINE: u32 = 0x0004;
    const ES_READONLY: u32 = 0x0800;
    const ES_AUTOVSCROLL: u32 = 0x0040;
    const CBS_DROPDOWNLIST: u32 = 0x0003;
    const CB_ADDSTRING: u32 = 0x0143;
    const CB_SETCURSEL: u32 = 0x014E;
    const CB_GETCURSEL: u32 = 0x0147;
    const WM_CREATE: u32 = 0x0001;
    const WM_DESTROY: u32 = 0x0002;
    const WM_PAINT: u32 = 0x000F;
    const WM_SETFONT: u32 = 0x0030;
    const WM_COMMAND: u32 = 0x0111;
    const DEFAULT_GUI_FONT: i32 = 17;
    const SW_SHOW: i32 = 5;
    const SRCCOPY: u32 = 0x00CC_0020;
    const DIB_RGB_COLORS: u32 = 0;
    const COLOR_WINDOW: usize = 6;
    const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
    const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;

    const ID_PATH_EDIT: isize = 101;
    const ID_BTN_BROWSE: isize = 102;
    const ID_BTN_RUN: isize = 103;
    const ID_BTN_DEMO: isize = 104;
    const ID_BTN_WEB: isize = 105;
    const ID_FSR_COMBO: isize = 106;
    const ID_BACKEND_COMBO: isize = 107;
    const ID_LOG_EDIT: isize = 108;

    #[repr(C)]
    struct WNDCLASSEXW {
        cbSize: u32,
        style: u32,
        lpfnWndProc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize,
        cbClsExtra: i32,
        cbWndExtra: i32,
        hInstance: isize,
        hIcon: isize,
        hCursor: isize,
        hbrBackground: isize,
        lpszMenuName: *const u16,
        lpszClassName: *const u16,
        hIconSm: isize,
    }

    #[repr(C)]
    struct POINT {
        x: i32,
        y: i32,
    }

    #[repr(C)]
    struct MSG {
        hwnd: isize,
        message: u32,
        wParam: usize,
        lParam: isize,
        time: u32,
        pt: POINT,
    }

    #[repr(C)]
    struct RECT {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }

    #[repr(C)]
    struct PAINTSTRUCT {
        hdc: isize,
        fErase: i32,
        rcPaint: RECT,
        fRestore: i32,
        fIncUpdate: i32,
        rgbReserved: [u8; 32],
    }

    #[repr(C)]
    struct BITMAPINFOHEADER {
        biSize: u32,
        biWidth: i32,
        biHeight: i32,
        biPlanes: u16,
        biBitCount: u16,
        biCompression: u32,
        biSizeImage: u32,
        biXPelsPerMeter: i32,
        biYPelsPerMeter: i32,
        biClrUsed: u32,
        biClrImportant: u32,
    }

    #[repr(C)]
    struct BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER,
        bmiColors: [u32; 1],
    }

    #[repr(C)]
    struct OPENFILENAMEW {
        lStructSize: u32,
        hwndOwner: isize,
        hInstance: isize,
        lpstrFilter: *const u16,
        lpstrCustomFilter: *mut u16,
        nMaxCustFilter: u32,
        nFilterIndex: u32,
        lpstrFile: *mut u16,
        nMaxFile: u32,
        lpstrFileTitle: *mut u16,
        nMaxFileTitle: u32,
        lpstrInitialDir: *const u16,
        lpstrTitle: *const u16,
        Flags: u32,
        nFileOffset: u16,
        nFileExtension: u16,
        lpstrDefExt: *const u16,
        lCustData: isize,
        lpfnHook: *const c_void,
        lpTemplateName: *const u16,
        pvReserved: *mut c_void,
        dwReserved: u32,
        FlagsEx: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleW(lpModuleName: *const u16) -> isize;
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassExW(lpwcx: *const WNDCLASSEXW) -> u16;
        fn CreateWindowExW(
            dwExStyle: u32,
            lpClassName: *const u16,
            lpWindowName: *const u16,
            dwStyle: u32,
            X: i32,
            Y: i32,
            nWidth: i32,
            nHeight: i32,
            hWndParent: isize,
            hMenu: isize,
            hInstance: isize,
            lpParam: *mut c_void,
        ) -> isize;
        fn ShowWindow(hWnd: isize, nCmdShow: i32) -> i32;
        fn UpdateWindow(hWnd: isize) -> i32;
        fn GetMessageW(lpMsg: *mut MSG, hWnd: isize, wMsgFilterMin: u32, wMsgFilterMax: u32) -> i32;
        fn TranslateMessage(lpMsg: *const MSG) -> i32;
        fn DispatchMessageW(lpMsg: *const MSG) -> isize;
        fn DefWindowProcW(hWnd: isize, Msg: u32, wParam: usize, lParam: isize) -> isize;
        fn PostQuitMessage(nExitCode: i32);
        fn GetDlgItem(hDlg: isize, nIDDlgItem: i32) -> isize;
        fn SetWindowTextW(hWnd: isize, lpString: *const u16) -> i32;
        fn GetWindowTextW(hWnd: isize, lpString: *mut u16, nMaxCount: i32) -> i32;
        fn SendMessageW(hWnd: isize, Msg: u32, wParam: usize, lParam: isize) -> isize;
        fn BeginPaint(hWnd: isize, lpPaint: *mut PAINTSTRUCT) -> isize;
        fn EndPaint(hWnd: isize, lpPaint: *const PAINTSTRUCT) -> i32;
        fn InvalidateRect(hWnd: isize, lpRect: *const RECT, bErase: i32) -> i32;
        fn LoadCursorW(hInstance: isize, lpCursorName: isize) -> isize;
    }

    #[link(name = "gdi32")]
    extern "system" {
        fn GetStockObject(i: i32) -> isize;
        fn StretchDIBits(
            hdc: isize,
            xDest: i32,
            yDest: i32,
            DestWidth: i32,
            DestHeight: i32,
            xSrc: i32,
            ySrc: i32,
            SrcWidth: i32,
            SrcHeight: i32,
            lpBits: *const c_void,
            lpbmi: *const BITMAPINFO,
            iUsage: u32,
            rop: u32,
        ) -> i32;
    }

    #[link(name = "comdlg32")]
    extern "system" {
        fn GetOpenFileNameW(lpofn: *mut OPENFILENAMEW) -> i32;
    }

    struct LauncherState {
        web_port: u16,
        web_handle: Option<GuiHandle>,
        last_frame: Option<PresentedFrame>,
    }

    static STATE: Mutex<Option<LauncherState>> = Mutex::new(None);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn selected_fsr(hwnd: isize) -> FsrQualityMode {
        let cb = unsafe { GetDlgItem(hwnd, ID_FSR_COMBO as i32) };
        let idx = unsafe { SendMessageW(cb, CB_GETCURSEL, 0, 0) };
        match idx {
            1 => FsrQualityMode::UltraQuality,
            2 => FsrQualityMode::Balanced,
            3 => FsrQualityMode::Performance,
            4 => FsrQualityMode::UltraPerformance,
            5 => FsrQualityMode::NativeAA,
            6 => FsrQualityMode::Off,
            _ => FsrQualityMode::Quality,
        }
    }

    fn selected_backend(hwnd: isize) -> BackendKind {
        let cb = unsafe { GetDlgItem(hwnd, ID_BACKEND_COMBO as i32) };
        let idx = unsafe { SendMessageW(cb, CB_GETCURSEL, 0, 0) };
        match idx {
            1 => BackendKind::NativeD3D12,
            2 => BackendKind::Software,
            _ => BackendKind::Auto,
        }
    }

    fn set_log(hwnd: isize, text: &str) {
        let crlf = text.replace('\n', "\r\n");
        let w = wide(&crlf);
        let edit = unsafe { GetDlgItem(hwnd, ID_LOG_EDIT as i32) };
        unsafe { SetWindowTextW(edit, w.as_ptr()) };
    }

    fn apply_run(hwnd: isize, res: Result<RunSummary, String>) {
        match res {
            Ok(s) => {
                set_log(hwnd, &s.log);
                if let Ok(mut guard) = STATE.lock() {
                    if let Some(st) = guard.as_mut() {
                        st.last_frame = s.frame;
                    }
                }
                let rc = RECT { left: 460, top: 88, right: 940, bottom: 390 };
                unsafe { InvalidateRect(hwnd, &rc, 1) };
            }
            Err(e) => set_log(hwnd, &format!("Error: {e}")),
        }
    }

    unsafe extern "system" fn wnd_proc(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize {
        match msg {
            WM_CREATE => {
                let hinst = GetModuleHandleW(std::ptr::null());
                let font = GetStockObject(DEFAULT_GUI_FONT) as usize;
                let mk = |cls: &str, txt: &str, style: u32, x: i32, y: i32, w: i32, h: i32, id: isize| {
                    let c = wide(cls);
                    let t = wide(txt);
                    let ctrl = CreateWindowExW(0, c.as_ptr(), t.as_ptr(), WS_CHILD | WS_VISIBLE | style, x, y, w, h, hwnd, id, hinst, std::ptr::null_mut());
                    if font != 0 && ctrl != 0 {
                        SendMessageW(ctrl, WM_SETFONT, font, 0);
                    }
                    ctrl
                };
                mk("STATIC", "Package / EXE:", 0, 14, 16, 110, 22, 0);
                mk("EDIT", "", WS_BORDER, 128, 14, 580, 24, ID_PATH_EDIT);
                mk("BUTTON", "Browse...", 0, 718, 14, 96, 24, ID_BTN_BROWSE);
                mk("BUTTON", "Boot / Run", 0, 822, 14, 110, 24, ID_BTN_RUN);

                mk("STATIC", "AMD FSR:", 0, 14, 50, 72, 22, 0);
                let fsr_cb = mk("COMBOBOX", "", CBS_DROPDOWNLIST, 88, 48, 180, 180, ID_FSR_COMBO);
                for label in [
                    "Quality (1.5x Default)",
                    "UltraQuality (1.3x)",
                    "Balanced (1.7x)",
                    "Performance (2.0x)",
                    "UltraPerformance (3.0x)",
                    "NativeAA (1.0x)",
                    "Off",
                ] {
                    let w = wide(label);
                    SendMessageW(fsr_cb, CB_ADDSTRING, 0, w.as_ptr() as isize);
                }
                SendMessageW(fsr_cb, CB_SETCURSEL, 0, 0);

                mk("STATIC", "Backend:", 0, 280, 50, 66, 22, 0);
                let be_cb = mk("COMBOBOX", "", CBS_DROPDOWNLIST, 348, 48, 170, 120, ID_BACKEND_COMBO);
                for label in ["Auto (D3D12)", "Native D3D12", "Software"] {
                    let w = wide(label);
                    SendMessageW(be_cb, CB_ADDSTRING, 0, w.as_ptr() as isize);
                }
                SendMessageW(be_cb, CB_SETCURSEL, 0, 0);

                mk("BUTTON", "Run Built-in Demo", 0, 536, 48, 160, 26, ID_BTN_DEMO);
                mk("BUTTON", "Open Web Dashboard", 0, 706, 48, 170, 26, ID_BTN_WEB);

                mk(
                    "EDIT",
                    "xsxemu ready. Select a Xbox Series X package/.exe or click 'Run Built-in Demo'.",
                    WS_BORDER | WS_VSCROLL | ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL,
                    14,
                    88,
                    436,
                    380,
                    ID_LOG_EDIT,
                );
                0
            }
            WM_COMMAND => {
                let id = (wparam & 0xFFFF) as isize;
                match id {
                    ID_BTN_BROWSE => {
                        let mut buf = vec![0u16; 512];
                        let filter = wide("Xbox Executables (*.exe)\0*.exe\0All Files (*.*)\0*.*\0");
                        let title = wide("Select Xbox Series X Executable");
                        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
                        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
                        ofn.hwndOwner = hwnd;
                        ofn.lpstrFilter = filter.as_ptr();
                        ofn.lpstrFile = buf.as_mut_ptr();
                        ofn.nMaxFile = buf.len() as u32;
                        ofn.lpstrTitle = title.as_ptr();
                        ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST;
                        if GetOpenFileNameW(&mut ofn) != 0 {
                            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                            let s = String::from_utf16_lossy(&buf[..end]);
                            let w = wide(&s);
                            let edit = GetDlgItem(hwnd, ID_PATH_EDIT as i32);
                            SetWindowTextW(edit, w.as_ptr());
                        }
                    }
                    ID_BTN_RUN => {
                        let mut buf = vec![0u16; 512];
                        let edit = GetDlgItem(hwnd, ID_PATH_EDIT as i32);
                        let n = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32);
                        let p = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
                        let fsr = FsrConfig {
                            mode: selected_fsr(hwnd),
                            ..FsrConfig::default()
                        };
                        let res = execute_target(
                            Path::new(p.trim()),
                            fsr,
                            selected_backend(hwnd),
                            UnimplementedPolicy::Log,
                            false,
                        );
                        apply_run(hwnd, res);
                    }
                    ID_BTN_DEMO => {
                        let demo_path = std::env::temp_dir().join(format!("xsxemu_win_demo_{}.exe", std::process::id()));
                        let res = match fs::write(&demo_path, build_actual_game_xsx_pe()) {
                            Ok(()) => {
                                let fsr = FsrConfig {
                                    mode: selected_fsr(hwnd),
                                    ..FsrConfig::default()
                                };
                                let r = execute_target(&demo_path, fsr, selected_backend(hwnd), UnimplementedPolicy::Log, false);
                                let _ = fs::remove_file(&demo_path);
                                r
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        apply_run(hwnd, res);
                    }
                    ID_BTN_WEB => {
                        if let Ok(mut guard) = STATE.lock() {
                            if let Some(st) = guard.as_mut() {
                                if st.web_handle.is_none() {
                                    if let Ok(srv) = GuiServer::bind(&format!("127.0.0.1:{}", st.web_port)) {
                                        st.web_handle = Some(srv.spawn_background());
                                    }
                                }
                                if let Some(h) = &st.web_handle {
                                    let url = format!("http://{}/", h.addr);
                                    open_in_browser(&url);
                                    set_log(hwnd, &format!("Web Dashboard running at {url}"));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                0
            }
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(hwnd, &mut ps);
                if let Ok(guard) = STATE.lock() {
                    if let Some(frame) = guard.as_ref().and_then(|s| s.last_frame.as_ref()) {
                        let mut bgra = Vec::with_capacity(frame.rgba.len());
                        for px in frame.rgba.chunks_exact(4) {
                            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                        }
                        let bmi = BITMAPINFO {
                            bmiHeader: BITMAPINFOHEADER {
                                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                                biWidth: frame.width as i32,
                                biHeight: -(frame.height as i32),
                                biPlanes: 1,
                                biBitCount: 32,
                                biCompression: 0,
                                biSizeImage: 0,
                                biXPelsPerMeter: 0,
                                biYPelsPerMeter: 0,
                                biClrUsed: 0,
                                biClrImportant: 0,
                            },
                            bmiColors: [0],
                        };
                        StretchDIBits(
                            hdc,
                            464,
                            90,
                            468,
                            350,
                            0,
                            0,
                            frame.width as i32,
                            frame.height as i32,
                            bgra.as_ptr() as *const c_void,
                            &bmi,
                            DIB_RGB_COLORS,
                            SRCCOPY,
                        );
                    }
                }
                EndPaint(hwnd, &ps);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    pub fn run_native_launcher(opts: GuiOptions) -> io::Result<()> {
        if let Ok(mut guard) = STATE.lock() {
            *guard = Some(LauncherState {
                web_port: opts.port,
                web_handle: None,
                last_frame: None,
            });
        }
        unsafe {
            let hinst = GetModuleHandleW(std::ptr::null());
            let cls = wide("XsxemuLauncherWindow");
            let title = wide(&format!(
                "xsxemu v{} — Xbox Series X|S Translation Layer (AMD FSR Quality)",
                env!("CARGO_PKG_VERSION")
            ));
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: wnd_proc,
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinst,
                hIcon: 0,
                hCursor: LoadCursorW(0, 32512),
                hbrBackground: COLOR_WINDOW as isize,
                lpszMenuName: std::ptr::null(),
                lpszClassName: cls.as_ptr(),
                hIconSm: 0,
            };
            RegisterClassExW(&wc);
            let hwnd = CreateWindowExW(
                0,
                cls.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                960,
                520,
                0,
                0,
                hinst,
                std::ptr::null_mut(),
            );
            ShowWindow(hwnd, SW_SHOW);
            UpdateWindow(hwnd);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, 0, 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if let Ok(mut guard) = STATE.lock() {
            *guard = None;
        }
        Ok(())
    }
}
