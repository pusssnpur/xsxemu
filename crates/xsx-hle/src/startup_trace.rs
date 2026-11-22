//! Crash-safe JSONL startup & import diagnostic tracer (`--trace-jsonl <path>`).
//!
//! Writes one UTF-8 JSON object per line and flushes immediately after each event so
//! first-boot diagnostics survive guest crashes cleanly.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

pub struct StartupTracer {
    file: Mutex<File>,
    start: Instant,
    seq: AtomicU64,
}

impl StartupTracer {
    /// Open a new JSONL trace file (creating or truncating it) and emit `trace_open`.
    pub fn create(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = File::create(path)?;
        let tracer = Self {
            file: Mutex::new(file),
            start: Instant::now(),
            seq: AtomicU64::new(0),
        };
        tracer.emit("trace_open", &format!("\"pid\":{}", std::process::id()));
        Ok(tracer)
    }

    pub fn emit(&self, kind: &str, fields_json: &str) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let t = self.start.elapsed().as_secs_f64();
        let line = if fields_json.is_empty() {
            format!("{{\"seq\":{seq},\"t\":{t:.6},\"kind\":\"{kind}\"}}\n")
        } else {
            format!("{{\"seq\":{seq},\"t\":{t:.6},\"kind\":\"{kind}\",{fields_json}}}\n")
        };
        if let Ok(mut f) = self.file.lock() {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }

    pub fn record_map(&self, module: &str, base: usize, size: usize, relocated: bool) {
        self.emit(
            "map",
            &format!(
                "\"module\":{},\"base\":\"0x{base:X}\",\"size\":{size},\"relocated\":{relocated}",
                json_quote(module)
            ),
        );
    }

    pub fn record_unimplemented(&self, import_name: &str, return_class: &str) {
        self.emit(
            "unimplemented_call",
            &format!(
                "\"import\":{},\"return_class\":{}",
                json_quote(import_name),
                json_quote(return_class)
            ),
        );
    }

    pub fn record_debug_string(&self, text: &str) {
        self.emit("debug_string", &format!("\"text\":{}", json_quote(text)));
    }

    pub fn record_exit(&self, code: i32) {
        self.emit("exit", &format!("\"code\":{code}"));
    }
}

pub fn json_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_flushed_jsonl_events() {
        let p = std::env::temp_dir().join(format!("xsxemu_trace_test_{}.jsonl", std::process::id()));
        let tracer = StartupTracer::create(&p).unwrap();
        tracer.record_map("game.exe", 0x1_4000_0000, 0x20000, false);
        tracer.record_debug_string("hello \"xbox\"\n");
        tracer.record_exit(0);
        drop(tracer);

        let content = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("\"kind\":\"trace_open\""));
        assert!(lines[1].contains("\"kind\":\"map\""));
        assert!(lines[2].contains("\\\"xbox\\\"\\n"));
        assert!(lines[3].contains("\"code\":0"));
        let _ = std::fs::remove_file(&p);
    }
}
