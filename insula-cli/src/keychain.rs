//! `insula keychain <subcommand>` — direct user-/dev-
//! facing wrapper around the vestibulum daemon's
//! CLASS_VESTIBULUM surface.
//!
//! Subcommands:
//!   - `insula keychain pubkey <service>`
//!   - `insula keychain sign <service> <hex-challenge>`
//!
//! Mirrors `insula push`'s shape: connects via
//! Aqueduct directly; auto-spawns the daemon if it's
//! not running.

use crate::daemons::{self, Daemon};
use aqueduct::classes::CLASS_VESTIBULUM;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use std::path::Path;

const OP_PUBKEY: u16 = 0;
const OP_SIGN: u16 = 1;

pub fn cmd_keychain(args: &[String], install_root: &Path) -> Result<(), String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "pubkey" => keychain_pubkey(&args[1..], install_root),
        "sign" => keychain_sign(&args[1..], install_root),
        "" => Err(
            "keychain: missing subcommand (use pubkey|sign)".to_string(),
        ),
        other => Err(format!(
            "keychain: unknown subcommand '{}' (use pubkey|sign)", other
        )),
    }
}

fn connect_vestibulum(install_root: &Path) -> Result<Connection, String> {
    let sock = if let Ok(p) = std::env::var("INSULA_VESTIBULUMD_SOCKET") {
        std::path::PathBuf::from(p)
    } else {
        daemons::ensure_started(install_root, Daemon::Vestibulum)
            .ok_or_else(|| {
                "keychain: vestibulum-macos is not running and could \
                 not be auto-spawned (set INSULA_VESTIBULUMD_BIN or \
                 put the binary on PATH)".to_string()
            })?
    };
    Connection::connect(&sock)
        .map_err(|e| format!("keychain: connect {}: {}", sock.display(), e))
}

fn rpc(conn: &mut Connection, op: u16, payload: &[u8]) -> Result<Vec<u8>, String> {
    conn.send_message(CLASS_VESTIBULUM, op, flag::RESPONSE_EXPECTED, payload)
        .map_err(|e| format!("keychain: send: {}", e))?;
    loop {
        let msg = conn.recv_message()
            .map_err(|e| format!("keychain: recv: {}", e))?;
        if msg.opcode_class == CLASS_VESTIBULUM
            && msg.op == op
            && (msg.flags & flag::IS_RESPONSE) != 0
        {
            return Ok(msg.payload);
        }
    }
}

fn keychain_pubkey(args: &[String], install_root: &Path) -> Result<(), String> {
    let service = args.first().ok_or_else(|| {
        "keychain pubkey: missing <service> argument".to_string()
    })?;
    let mut conn = connect_vestibulum(install_root)?;
    let resp = rpc(&mut conn, OP_PUBKEY, service.as_bytes())?;
    if resp.len() != 32 {
        return Err(format!(
            "keychain pubkey: malformed response ({} bytes)", resp.len()
        ));
    }
    let hex: String = resp.iter().map(|b| format!("{:02x}", b)).collect();
    println!("{}", hex);
    Ok(())
}

fn keychain_sign(args: &[String], install_root: &Path) -> Result<(), String> {
    let service = args.first().ok_or_else(|| {
        "keychain sign: missing <service> argument".to_string()
    })?;
    let challenge_hex = args.get(1).ok_or_else(|| {
        "keychain sign: missing <hex-challenge> argument".to_string()
    })?;
    let challenge = decode_hex(challenge_hex)
        .map_err(|e| format!("keychain sign: bad hex challenge: {}", e))?;

    // Payload: [u16 LE name_len | name | challenge]
    let mut payload = Vec::with_capacity(2 + service.len() + challenge.len());
    let name_len: u16 = service.len()
        .try_into()
        .map_err(|_| "keychain sign: service name too long".to_string())?;
    payload.extend_from_slice(&name_len.to_le_bytes());
    payload.extend_from_slice(service.as_bytes());
    payload.extend_from_slice(&challenge);

    let mut conn = connect_vestibulum(install_root)?;
    let resp = rpc(&mut conn, OP_SIGN, &payload)?;
    if resp.len() != 64 {
        return Err(format!(
            "keychain sign: malformed response ({} bytes)", resp.len()
        ));
    }
    let hex: String = resp.iter().map(|b| format!("{:02x}", b)).collect();
    println!("{}", hex);
    Ok(())
}

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd number of hex digits".to_string());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_digit(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("not a hex digit: 0x{:02x}", b)),
    }
}
