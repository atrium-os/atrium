//! `insula notify <title> <body> [--urgency low|normal|high]`
//! — direct user-/dev-facing wrapper around the
//! praeco daemon's CLASS_NOTIFY surface.
//!
//! Same shape as `insula push` / `insula keychain`:
//! auto-spawn the daemon if it isn't running, talk to
//! it directly over Aqueduct, return the assigned
//! notification id on stdout.

use crate::daemons::{self, Daemon};
use aqueduct::classes::CLASS_NOTIFY;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use std::path::Path;

const OP_POST_NOTIFICATION: u16 = 0;

pub fn cmd_notify(args: &[String], install_root: &Path) -> Result<(), String> {
    // Parse: <title> <body> [--urgency low|normal|high]
    let mut title: Option<&str> = None;
    let mut body: Option<&str> = None;
    let mut urgency: u8 = 1; // normal
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--urgency" => {
                let v = args.get(i + 1).ok_or_else(|| {
                    "notify: --urgency needs a value (low|normal|high)".to_string()
                })?;
                urgency = match v.as_str() {
                    "low" => 0,
                    "normal" => 1,
                    "high" => 2,
                    other => return Err(format!(
                        "notify: --urgency must be low|normal|high, got {:?}", other
                    )),
                };
                i += 2;
            }
            other if !other.starts_with("--") => {
                if title.is_none() {
                    title = Some(other);
                } else if body.is_none() {
                    body = Some(other);
                } else {
                    return Err(format!(
                        "notify: unexpected positional argument {:?}", other
                    ));
                }
                i += 1;
            }
            other => return Err(format!("notify: unknown flag {:?}", other)),
        }
    }
    let title = title.ok_or_else(|| "notify: missing <title>".to_string())?;
    let body = body.ok_or_else(|| "notify: missing <body>".to_string())?;

    let sock = if let Ok(p) = std::env::var("INSULA_PRAECOD_SOCKET") {
        std::path::PathBuf::from(p)
    } else {
        daemons::ensure_started(install_root, Daemon::Praeco)
            .ok_or_else(|| {
                "notify: praeco-macos is not running and could not be \
                 auto-spawned (set INSULA_PRAECOD_BIN or put the binary \
                 on PATH)".to_string()
            })?
    };
    let mut conn = Connection::connect(&sock)
        .map_err(|e| format!("notify: connect {}: {}", sock.display(), e))?;

    // Wire: [u8 urgency | u16 LE title_len | title | u16 LE body_len | body]
    let title_bytes = title.as_bytes();
    let body_bytes = body.as_bytes();
    let title_len: u16 = title_bytes.len().try_into()
        .map_err(|_| "notify: title too long (>65535 bytes)".to_string())?;
    let body_len: u16 = body_bytes.len().try_into()
        .map_err(|_| "notify: body too long (>65535 bytes)".to_string())?;
    let mut payload = Vec::with_capacity(1 + 2 + title_bytes.len() + 2 + body_bytes.len());
    payload.push(urgency);
    payload.extend_from_slice(&title_len.to_le_bytes());
    payload.extend_from_slice(title_bytes);
    payload.extend_from_slice(&body_len.to_le_bytes());
    payload.extend_from_slice(body_bytes);

    conn.send_message(
        CLASS_NOTIFY, OP_POST_NOTIFICATION,
        flag::RESPONSE_EXPECTED, &payload,
    ).map_err(|e| format!("notify: send: {}", e))?;

    // Response: [u8 status | u64 LE id]
    loop {
        let msg = conn.recv_message()
            .map_err(|e| format!("notify: recv: {}", e))?;
        if msg.opcode_class == CLASS_NOTIFY
            && msg.op == OP_POST_NOTIFICATION
            && (msg.flags & flag::IS_RESPONSE) != 0
        {
            if msg.payload.len() < 9 {
                return Err(format!(
                    "notify: malformed response ({} bytes)", msg.payload.len()
                ));
            }
            let status = msg.payload[0];
            let id = u64::from_le_bytes(msg.payload[1..9].try_into().unwrap());
            if status != 0 {
                return Err(format!(
                    "notify: daemon rejected the post (status = {})", status
                ));
            }
            println!("{}", id);
            return Ok(());
        }
    }
}
