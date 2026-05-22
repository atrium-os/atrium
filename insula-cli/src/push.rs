//! `insula push <subcommand>` — direct user-/dev-facing
//! wrapper around the tabellarius daemon's
//! CLASS_TABELLARIUS surface.
//!
//! Subcommands:
//!   - `insula push subscribe <purpose>`
//!   - `insula push list`
//!   - `insula push unsubscribe <key_id>`
//!
//! The CLI auto-spawns the daemon if it's not already
//! running (via `daemons::ensure_started`), reuses its
//! socket path under `<install_root>/run/`, then talks
//! to it directly over Aqueduct.

use crate::daemons::{self, Daemon};
use aqueduct::classes::CLASS_TABELLARIUS;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use std::path::Path;

const OP_SUBSCRIBE: u16 = 0;
const OP_UNSUBSCRIBE: u16 = 1;
const OP_LIST: u16 = 2;
const OP_GET_PUSH: u16 = 3;

pub fn cmd_push(args: &[String], install_root: &Path) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    match sub {
        "subscribe" => push_subscribe(&args[1..], install_root),
        "list" => push_list(install_root),
        "unsubscribe" => push_unsubscribe(&args[1..], install_root),
        "pending" => push_pending(install_root),
        other => Err(format!(
            "push: unknown subcommand '{}' \
             (use subscribe|list|unsubscribe|pending)",
            other
        )),
    }
}

/// Resolve tabellarius's socket — explicit env override
/// wins, otherwise use the auto-spawn path under the
/// install root (matching `cmd_launch`'s discipline).
fn connect_tabellarius(install_root: &Path) -> Result<Connection, String> {
    let sock = if let Ok(p) = std::env::var("INSULA_TABELLARIUSD_SOCKET") {
        std::path::PathBuf::from(p)
    } else {
        daemons::ensure_started(install_root, Daemon::Tabellarius)
            .ok_or_else(|| {
                "push: tabellarius-macos is not running and could not be \
                 auto-spawned (set INSULA_TABELLARIUSD_BIN or put the \
                 binary on PATH)".to_string()
            })?
    };
    Connection::connect(&sock)
        .map_err(|e| format!("push: connect {}: {}", sock.display(), e))
}

fn rpc(conn: &mut Connection, op: u16, payload: &[u8]) -> Result<Vec<u8>, String> {
    conn.send_message(CLASS_TABELLARIUS, op, flag::RESPONSE_EXPECTED, payload)
        .map_err(|e| format!("push: send: {}", e))?;
    loop {
        let msg = conn.recv_message()
            .map_err(|e| format!("push: recv: {}", e))?;
        if msg.opcode_class == CLASS_TABELLARIUS
            && msg.op == op
            && (msg.flags & flag::IS_RESPONSE) != 0
        {
            return Ok(msg.payload);
        }
    }
}

fn push_subscribe(args: &[String], install_root: &Path) -> Result<(), String> {
    let purpose = args.first().ok_or_else(|| {
        "push subscribe: missing <purpose> argument".to_string()
    })?;
    let mut conn = connect_tabellarius(install_root)?;
    let resp = rpc(&mut conn, OP_SUBSCRIBE, purpose.as_bytes())?;

    // Wire: [u8 key_id_len | key_id UTF-8 | 32B pubkey]
    if resp.is_empty() {
        return Err("push subscribe: empty response from daemon".to_string());
    }
    let id_len = resp[0] as usize;
    if resp.len() != 1 + id_len + 32 {
        return Err(format!(
            "push subscribe: malformed response ({} bytes)", resp.len()
        ));
    }
    let key_id = std::str::from_utf8(&resp[1..1 + id_len])
        .map_err(|e| format!("push subscribe: non-utf8 key_id: {}", e))?;
    let pubkey = &resp[1 + id_len..];
    let pk_hex: String = pubkey.iter().map(|b| format!("{:02x}", b)).collect();

    println!("subscribed (purpose = {}):", purpose);
    println!("  key_id: {}", key_id);
    println!("  pubkey: {}", pk_hex);
    println!();
    println!("Publish this pubkey to your push sender. Pushes for it");
    println!("will arrive once Tabellarius Phase B (relay traffic +");
    println!("wake-on-push) ships.");
    Ok(())
}

fn push_list(install_root: &Path) -> Result<(), String> {
    let mut conn = connect_tabellarius(install_root)?;
    let resp = rpc(&mut conn, OP_LIST, &[])?;
    if resp.len() < 2 {
        return Err("push list: malformed response".to_string());
    }
    let n = u16::from_le_bytes([resp[0], resp[1]]) as usize;
    if n == 0 {
        println!("(no active subscriptions)");
        return Ok(());
    }

    let mut cur = 2;
    for _ in 0..n {
        if cur + 1 > resp.len() {
            return Err("push list: truncated entry".to_string());
        }
        let id_len = resp[cur] as usize;
        cur += 1;
        if cur + id_len + 32 > resp.len() {
            return Err("push list: truncated entry payload".to_string());
        }
        let key_id = std::str::from_utf8(&resp[cur..cur + id_len])
            .map_err(|e| format!("push list: non-utf8 key_id: {}", e))?;
        cur += id_len;
        let pk = &resp[cur..cur + 32];
        cur += 32;
        let pk_prefix: String = pk[..8].iter()
            .map(|b| format!("{:02x}", b)).collect();
        println!("{}  pk={}…", key_id, pk_prefix);
    }
    Ok(())
}

fn push_unsubscribe(args: &[String], install_root: &Path) -> Result<(), String> {
    let key_id = args.first().ok_or_else(|| {
        "push unsubscribe: missing <key_id> argument".to_string()
    })?;
    let mut conn = connect_tabellarius(install_root)?;
    let resp = rpc(&mut conn, OP_UNSUBSCRIBE, key_id.as_bytes())?;
    if resp.len() != 1 {
        return Err(format!(
            "push unsubscribe: malformed response ({} bytes)", resp.len()
        ));
    }
    if resp[0] == 0 {
        println!("unsubscribed {}", key_id);
        Ok(())
    } else {
        Err(format!("no subscription with key_id {}", key_id))
    }
}

/// `insula push pending` — drain the device's
/// received-push queue, printing one line per push.
/// Each GET_PUSH op pops the oldest push; we loop
/// until the daemon reports the queue empty.
fn push_pending(install_root: &Path) -> Result<(), String> {
    let mut conn = connect_tabellarius(install_root)?;
    let mut count = 0usize;
    loop {
        let resp = rpc(&mut conn, OP_GET_PUSH, &[])?;
        if resp.is_empty() {
            return Err("push pending: empty response from daemon".to_string());
        }
        match resp[0] {
            1 => break, // queue empty
            0 => {
                // [0 | u8 id_len | key_id | u64 ts LE | blob]
                if resp.len() < 2 {
                    return Err("push pending: truncated response".to_string());
                }
                let id_len = resp[1] as usize;
                if resp.len() < 2 + id_len + 8 {
                    return Err("push pending: truncated response".to_string());
                }
                let key_id = std::str::from_utf8(&resp[2..2 + id_len])
                    .map_err(|e| format!("push pending: non-utf8 key_id: {}", e))?;
                let ts_off = 2 + id_len;
                let ts = u64::from_le_bytes(
                    resp[ts_off..ts_off + 8].try_into().unwrap()
                );
                let blob = &resp[ts_off + 8..];
                println!(
                    "push  key_id={}  ts={}  {} byte(s)",
                    key_id, ts, blob.len()
                );
                count += 1;
            }
            other => {
                return Err(format!(
                    "push pending: unknown status byte {}", other
                ));
            }
        }
    }
    if count == 0 {
        println!("(no pending pushes)");
    } else {
        println!();
        println!("drained {} push(es)", count);
    }
    Ok(())
}
