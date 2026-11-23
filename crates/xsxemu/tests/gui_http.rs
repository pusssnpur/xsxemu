use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};

use xsxemu::gui::GuiServer;

fn http_req(addr: SocketAddr, req: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to GUI server");
    stream.write_all(req.as_bytes()).expect("write request");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read response");
    resp
}

#[test]
fn embedded_web_gui_serves_dashboard_and_apis() {
    let server = GuiServer::bind("127.0.0.1:0").expect("bind ephemeral port");
    let handle = server.spawn_background();
    let addr = handle.addr;

    // 1. Dashboard HTML
    let html = http_req(addr, "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(html.starts_with("HTTP/1.1 200 OK"));
    assert!(html.contains("xsxemu"));
    assert!(html.contains("AMD FSR"));

    // 2. Status JSON
    let status = http_req(addr, "GET /api/status HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(status.starts_with("HTTP/1.1 200 OK"));
    assert!(status.contains("\"default_fsr\":\"Quality\""));

    // 3. Package scan
    let pkgs = http_req(addr, "GET /api/packages?dir=. HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(pkgs.starts_with("HTTP/1.1 200 OK"));
    assert!(pkgs.contains("\"entries\":["));

    // 4. FSR Benchmark API
    let bench_body = "{\"width\":64,\"height\":64,\"iters\":2,\"fsr\":\"quality\",\"sharpness\":0.8}";
    let bench_req = format!(
        "POST /api/bench-fsr HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        bench_body.len(),
        bench_body
    );
    let bench = http_req(addr, &bench_req);
    assert!(bench.starts_with("HTTP/1.1 200 OK"));
    assert!(bench.contains("\"avg_ms\":"));
    assert!(bench.contains("\"frame_bmp_b64\":\"Qk0")); // base64("BM...")

    // 5. Built-in XSX Demo execution + FSR Quality framebuffer preview
    let demo_body = "{\"fsr\":\"quality\",\"backend\":\"software\",\"sharpness\":0.8}";
    let demo_req = format!(
        "POST /api/gen-demo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
        demo_body.len(),
        demo_body
    );
    let demo = http_req(addr, &demo_req);
    assert!(demo.starts_with("HTTP/1.1 200 OK"));
    assert!(demo.contains("\"exit_code\":0"));
    assert!(demo.contains("\"fsr_mode\":\"Quality\""));
    assert!(demo.contains("\"frame_bmp_b64\":\"Qk0"));
    assert!(demo.contains("\"audio_frames\":240"));
    assert!(demo.contains("\"audio_wav_b64\":\"UklGR")); // base64("RIFF...")
}
