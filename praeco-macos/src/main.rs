//! `praeco-macos` — notifications daemon.
//!
//! Listens on a unix socket for CLASS_NOTIFY POST
//! requests, mints a notification id, appends a record
//! to a log file. v0 doesn't route to the native macOS
//! Notification Center; the wire shape matches what
//! the production Praeco would consume.
//!
//! # Wire
//!
//! Op 0 = POST_NOTIFICATION
//!   payload:
//!     u8  urgency  (0=low, 1=normal, 2=high)
//!     u16 title_len LE
//!     <title_bytes UTF-8>
//!     u16 body_len LE
//!     <body_bytes UTF-8>
//!   response:
//!     u8  status (0=OK, !=0 error)
//!     u64 notification_id LE (zero on error)

use aqueduct::classes::CLASS_NOTIFY;
use aqueduct::envelope::{self, flag, Header};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{atomic::{AtomicU64, Ordering}, Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

const OP_POST_NOTIFICATION: u16 = 0;

const POST_STATUS_OK: u8 = 0;
const POST_STATUS_MALFORMED: u8 = 1;

fn main() -> ExitCode {
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    let socket_path = resolve_socket_path();
    let log_path = resolve_log_path();

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
                "praeco-macos: cannot open log file {}: {}",
                log_path.display(),
                e
            );
            return ExitCode::FAILURE;
        }
    };

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("praeco-macos: bind {}: {}", socket_path.display(), e);
            return ExitCode::FAILURE;
        }
    };

    eprintln!(
        "praeco-macos: listening on {} -> {}",
        socket_path.display(),
        log_path.display()
    );

    let next_id = Arc::new(AtomicU64::new(1));
    let log = Arc::new(Mutex::new(log_file));

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let next_id = next_id.clone();
        let log = log.clone();
        thread::spawn(move || handle_client(stream, next_id, log));
    }
    ExitCode::SUCCESS
}

fn handle_client(
    mut stream: UnixStream,
    next_id: Arc<AtomicU64>,
    log: Arc<Mutex<File>>,
) {
    loop {
        let req = match read_envelope(&mut stream) {
            Ok(r) => r,
            Err(_) => return,
        };
        if req.opcode_class != CLASS_NOTIFY || req.op != OP_POST_NOTIFICATION {
            continue;
        }

        // Decode urgency + title + body.
        let (urgency_str, title, body) = match decode_post(&req.payload) {
            Some(v) => v,
            None => {
                let _ = write_response(&mut stream, POST_STATUS_MALFORMED, 0);
                continue;
            }
        };

        let id = next_id.fetch_add(1, Ordering::SeqCst);

        let line = format!(
            "{}\tid={}\turgency={}\ttitle={:?}\tbody={:?}\n",
            timestamp_iso8601(),
            id,
            urgency_str,
            title,
            body,
        );
        {
            let mut f = log.lock().unwrap();
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }

        if write_response(&mut stream, POST_STATUS_OK, id).is_err() {
            return;
        }
    }
}

fn decode_post(payload: &[u8]) -> Option<(&'static str, String, String)> {
    if payload.is_empty() {
        return None;
    }
    let urgency = payload[0];
    let urgency_str = match urgency {
        0 => "low",
        1 => "normal",
        2 => "high",
        _ => "?",
    };
    let rest = &payload[1..];
    let (title_bytes, rest) = read_lp_string(rest)?;
    let (body_bytes, _rest) = read_lp_string(rest)?;
    let title = std::str::from_utf8(title_bytes).ok()?.to_string();
    let body = std::str::from_utf8(body_bytes).ok()?.to_string();
    Some((urgency_str, title, body))
}

fn read_lp_string(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    if buf.len() < 2 {
        return None;
    }
    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    if buf.len() < 2 + len {
        return None;
    }
    Some((&buf[2..2 + len], &buf[2 + len..]))
}

fn read_envelope(s: &mut UnixStream) -> std::io::Result<aqueduct::Message> {
    use std::io::Read;
    let mut hdr = [0u8; envelope::HEADER_LEN];
    s.read_exact(&mut hdr)?;
    let header = Header::decode(&hdr)?;
    let mut payload = vec![0u8; header.length as usize];
    s.read_exact(&mut payload)?;
    Ok(aqueduct::Message {
        opcode_class: header.opcode_class,
        op: header.op,
        flags: header.flags,
        kind: if (header.flags & flag::IS_RESPONSE) != 0 {
            aqueduct::MessageKind::Response
        } else {
            aqueduct::MessageKind::Request
        },
        payload,
    })
}

fn write_response(
    stream: &mut UnixStream,
    status: u8,
    id: u64,
) -> std::io::Result<()> {
    let mut payload = Vec::with_capacity(9);
    payload.push(status);
    payload.extend_from_slice(&id.to_le_bytes());

    let header = Header::new(
        CLASS_NOTIFY,
        OP_POST_NOTIFICATION,
        flag::IS_RESPONSE,
        payload.len() as u32,
    );
    stream.write_all(&header.encode())?;
    stream.write_all(&payload)?;
    stream.flush()
}

fn timestamp_iso8601() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    let days = secs / 86_400;
    let s_of_day = secs % 86_400;
    let h = s_of_day / 3600;
    let m = (s_of_day / 60) % 60;
    let s = s_of_day % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, h, m, s, ms
    )
}

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
    if let Ok(p) = std::env::var("INSULA_PRAECOD_SOCKET") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("praeco-macos.sock")
}

fn resolve_log_path() -> PathBuf {
    if let Ok(p) = std::env::var("INSULA_PRAECOD_LOG_FILE") {
        return PathBuf::from(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Library").join("Logs").join("praeco.log");
    }
    PathBuf::from("/tmp/praeco.log")
}

fn print_usage() {
    eprintln!(
        "Usage: praeco-macos [--help]

Listens on a unix socket for CLASS_NOTIFY POST_NOTIFICATION
requests and appends each notification to a log file.

Environment:
  INSULA_PRAECOD_SOCKET     listen path
  INSULA_PRAECOD_LOG_FILE   output log file"
    );
}
