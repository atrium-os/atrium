//! `insula-logd` — log-forwarding daemon for Insula
//! apps on macOS.
//!
//! Listens on a unix socket for Aqueduct connections,
//! decodes `CLASS_ECHO` op=0 messages (the libatrium
//! log-forward shape — see `libatrium/src/lib.rs`),
//! appends each one to a log file with a structured
//! timestamp prefix.
//!
//! # Configuration
//!
//! Environment variables (with defaults):
//!
//! - `INSULA_LOGD_SOCKET` — listen path.
//!   Default: `$XDG_RUNTIME_DIR/insula-logd.sock` if set,
//!   else `$TMPDIR/insula-logd.sock`,
//!   else `/tmp/insula-logd.sock`.
//! - `INSULA_LOGD_LOG_FILE` — output file path.
//!   Default: `$HOME/Library/Logs/insula.log` (on macOS),
//!   else `/tmp/insula.log`.
//! - `INSULA_LOGD_ALSO_STDERR` — if `1`, mirror writes
//!   to stderr (useful in `insula-cli launch` mode).
//!   Default: `0`.
//!
//! # Lifecycle
//!
//! v0: simple long-running daemon. Bind, accept,
//! per-connection thread, log lines append to file
//! protected by a Mutex. SIGINT / SIGTERM exits the
//! main accept loop and the process; in-flight messages
//! finish before the worker threads die.

use aqueduct::classes::CLASS_LOG;
use aqueduct::Connection;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let socket_path = resolve_socket_path();
    let log_path = resolve_log_path();
    let also_stderr = std::env::var("INSULA_LOGD_ALSO_STDERR")
        .ok()
        .as_deref()
        == Some("1");

    // Remove a stale socket from a prior crashed run.
    let _ = std::fs::remove_file(&socket_path);

    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let log_file = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "insula-logd: cannot open log file {}: {}",
                log_path.display(),
                e
            );
            return ExitCode::FAILURE;
        }
    };
    let log = Arc::new(Mutex::new(log_file));

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "insula-logd: cannot bind {}: {}",
                socket_path.display(),
                e
            );
            return ExitCode::FAILURE;
        }
    };

    eprintln!(
        "insula-logd: listening on {} -> {}",
        socket_path.display(),
        log_path.display()
    );

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("insula-logd: accept error: {}", e);
                continue;
            }
        };
        let log = log.clone();
        thread::spawn(move || {
            handle_connection(stream, log, also_stderr);
        });
    }

    ExitCode::SUCCESS
}

fn handle_connection(
    stream: std::os::unix::net::UnixStream,
    log: Arc<Mutex<File>>,
    also_stderr: bool,
) {
    let mut conn = match Connection::wrap(stream) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("insula-logd: wrap connection: {}", e);
            return;
        }
    };

    loop {
        match conn.recv_message() {
            Ok(msg) => {
                // Only handle CLASS_LOG op=0 (the
                // libatrium log-forward shape). Anything
                // else gets dropped silently — this
                // daemon is single-purpose.
                if msg.opcode_class != CLASS_LOG || msg.op != 0 {
                    continue;
                }
                if let Err(e) = write_log_line(&msg.payload, &log, also_stderr) {
                    eprintln!("insula-logd: write failure: {}", e);
                }
            }
            Err(_) => break, // client disconnect / EOF
        }
    }
}

fn write_log_line(
    payload: &[u8],
    log: &Arc<Mutex<File>>,
    also_stderr: bool,
) -> std::io::Result<()> {
    if payload.is_empty() {
        return Ok(());
    }

    let level_byte = payload[0];
    let msg_bytes = &payload[1..];
    let level_str = match level_byte {
        0 => "TRACE",
        1 => "DEBUG",
        2 => "INFO",
        3 => "WARN",
        4 => "ERROR",
        _ => "?",
    };

    let text = String::from_utf8_lossy(msg_bytes);
    let line = format!(
        "{}\t{}\t{}\n",
        timestamp_iso8601(),
        level_str,
        text
    );

    let mut f = log.lock().unwrap();
    f.write_all(line.as_bytes())?;
    f.flush()?;

    if also_stderr {
        eprint!("{}", line);
    }

    Ok(())
}

/// Render the current wall-clock time in ISO-8601
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`). No external dependency
/// — we just want a sortable timestamp; chrono / time
/// would be overkill for v0.
fn timestamp_iso8601() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();

    let days = secs / 86_400;
    let secs_of_day = secs % 86_400;
    let h = secs_of_day / 3600;
    let m = (secs_of_day / 60) % 60;
    let s = secs_of_day % 60;

    // Days since 1970-01-01 → Y-M-D via the standard
    // civil_from_days algorithm.
    let (year, month, day) = civil_from_days(days as i64);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, h, m, s, ms
    )
}

/// Howard Hinnant's "civil_from_days" — convert Unix
/// days into (year, month, day) without a libc call.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let yy = if m <= 2 { y + 1 } else { y };
    (yy as i32, m as u32, d as u32)
}

fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_LOGD_SOCKET") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("insula-logd.sock")
}

fn resolve_log_path() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_LOGD_LOG_FILE") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Library").join("Logs").join("insula.log");
    }
    PathBuf::from("/tmp/insula.log")
}

fn print_usage() {
    eprintln!(
        "Usage: insula-logd [--help]

Listens on a unix socket for libatrium clients, decodes their
log messages, appends to a log file.

Environment:
  INSULA_LOGD_SOCKET     listen path (default: $XDG_RUNTIME_DIR/insula-logd.sock)
  INSULA_LOGD_LOG_FILE   output log file (default: ~/Library/Logs/insula.log)
  INSULA_LOGD_ALSO_STDERR  '1' to mirror writes to stderr"
    );
}
