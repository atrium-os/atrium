//! Cross-boundary tracing — Rust side. Mirrors atrium_trace.h's userspace flavor:
//! emits Chrome Trace Format (catapult) JSON to `$ATRIUM_TRACE_FILE.<pid>` when
//! the env var is set; no-op otherwise.
//!
//! Output is mergeable with C-side outputs via atrium-trace/scripts/merge_traces.py.
//!
//! Usage:
//!     atrium_trace::begin("guest.render_to_buffer");
//!     ...work...
//!     atrium_trace::end("guest.render_to_buffer");
//!     atrium_trace::instant("guest.fence_signaled");
//!
//! The static FILE is initialized once on first emit and reused for the
//! lifetime of the process. `flush()` is implicit via line-buffering; the
//! `_end` sentinel emitted by other tracers isn't needed since the merger
//! tolerates partial output.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static FILE: OnceLock<Option<Mutex<BufWriter<File>>>> = OnceLock::new();

fn file() -> Option<&'static Mutex<BufWriter<File>>> {
    FILE.get_or_init(|| {
        let env = std::env::var("ATRIUM_TRACE_FILE").ok()?;
        let pid = std::process::id();
        let path = format!("{env}.{pid}");
        let f = File::create(&path).ok()?;
        let mut bw = BufWriter::new(f);
        let _ = writeln!(bw, "[");
        Some(Mutex::new(bw))
    })
    .as_ref()
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn tid() -> u64 {
    let id = std::thread::current().id();
    let s = format!("{id:?}");
    s.trim_start_matches("ThreadId(")
        .trim_end_matches(')')
        .parse::<u64>()
        .unwrap_or(0)
}

pub fn emit(name: &str, ph: char) {
    emit_id(name, ph, 0);
}

pub fn emit_id(name: &str, ph: char, id: u64) {
    if let Some(m) = file() {
        let ts = now_us();
        let pid = std::process::id();
        let tid = tid();
        if let Ok(mut g) = m.lock() {
            if id != 0 {
                let _ = writeln!(
                    g,
                    "{{\"name\":\"{name}\",\"ph\":\"{ph}\",\"ts\":{ts},\"pid\":{pid},\"tid\":{tid},\"args\":{{\"id\":{id}}}}},"
                );
            } else {
                let _ = writeln!(
                    g,
                    "{{\"name\":\"{name}\",\"ph\":\"{ph}\",\"ts\":{ts},\"pid\":{pid},\"tid\":{tid}}},"
                );
            }
            let _ = g.flush();
        }
    }
}

pub fn begin(name: &str) { emit(name, 'B'); }
pub fn end(name: &str)   { emit(name, 'E'); }
pub fn instant(name: &str) { emit(name, 'i'); }
pub fn begin_id(name: &str, id: u64)   { emit_id(name, 'B', id); }
pub fn end_id(name: &str, id: u64)     { emit_id(name, 'E', id); }
pub fn instant_id(name: &str, id: u64) { emit_id(name, 'i', id); }

/// RAII scope guard. Drop emits 'E'.
pub struct Scope {
    name: &'static str,
}

impl Scope {
    pub fn new(name: &'static str) -> Self {
        emit(name, 'B');
        Self { name }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        emit(self.name, 'E');
    }
}
