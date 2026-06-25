//! # stoa — daemon + client shared bits
//!
//! At S1 the shell lives in `stoad`'s session table, independent of any
//! client, so a dropped client leaves the session running and a later
//! attach resumes the *same* shell — the property `ssh + tmux` can't give
//! (ssh owns the shell's lifetime). The **mint handshake** makes that real
//! over the network:
//!
//! - Each session has its **own UDP port + its own `K_sess`** (a random
//!   32-byte key). The arriving port identifies the session, so `stoad`
//!   knows which key to authenticate with before decoding (the structural
//!   reason the spec gives each session a port, stoa.md §2).
//! - A client obtains `{port, K_sess}` by **minting** over `stoad`'s local
//!   control socket. Run locally that is a direct connect; run as
//!   `ssh host stoa-shell <name>` the mint reply travels back inside the
//!   confidential SSH channel (the mosh handoff, generalized in
//!   aqueduct-remote.md §2). The SSH connection may then drop; the UDP
//!   session is independent.
//!
//! Deferred (spec, not yet here): anchoring `K_sess` in the SSH session id
//! (aqueduct-remote.md §8 open question — needs sshd plumbing we have not
//! verified); the SSP predictor (§3.4); real terminal-grid `StateDiff`
//! (§3.3 — the byte stream is carried raw for now); scrollback (S3).

use stoa_proto::MsgType;

/// Default `stoad` local control socket (where mints happen). Overridable
/// via `$STOA_CTL`. Production uses `/var/run/atrium/stoad.sock`; the dev
/// build uses a per-uid path under the temp dir.
pub fn default_ctl() -> String {
    if let Ok(s) = std::env::var("STOA_CTL") {
        return s;
    }
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{}/stoad-{uid}.ctl", tmp.trim_end_matches('/'))
}

/// The detach key in the raw input stream: Ctrl-] (0x1d, telnet-style).
/// Detach is purely client-side now — the client just exits; `stoad` keeps
/// the shell running and routes output to wherever the client next speaks
/// from (roaming is automatic). A real client uses `Ctrl-B d` (S2).
pub const DETACH_BYTE: u8 = 0x1d;

/// Length of a session key in bytes.
pub const KEY_LEN: usize = 32;

/// Datagram carrying raw client→shell keystrokes.
pub const INPUT: MsgType = MsgType::Input;
/// Datagram carrying raw shell→client output (real grid diffs are S2).
pub const OUTPUT: MsgType = MsgType::StateDiff;
/// Datagram carrying a [`Control`] message.
pub const CONTROL: MsgType = MsgType::Control;
/// Empty datagram a client sends first so `stoad` learns its address (and
/// lazily spawns the shell) before any output is produced.
pub const KEEPALIVE: MsgType = MsgType::Keepalive;

/// Control-channel messages (carried in a [`CONTROL`] datagram).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Server → client: the session's shell exited; nothing to resume.
    Bye,
    /// Client → server: the client's terminal size. Sent as the first
    /// datagram (so the shell spawns at the right size) and again on every
    /// SIGWINCH. stoad applies it to the pty (TIOCSWINSZ → SIGWINCH inside).
    Resize { cols: u16, rows: u16 },
}

impl Control {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Control::Bye => vec![4],
            Control::Resize { cols, rows } => {
                let mut v = vec![5];
                v.extend_from_slice(&cols.to_be_bytes());
                v.extend_from_slice(&rows.to_be_bytes());
                v
            }
        }
    }
    pub fn decode(payload: &[u8]) -> Option<Control> {
        match payload.first()? {
            4 => Some(Control::Bye),
            5 if payload.len() >= 5 => Some(Control::Resize {
                cols: u16::from_be_bytes([payload[1], payload[2]]),
                rows: u16::from_be_bytes([payload[3], payload[4]]),
            }),
            _ => None,
        }
    }
}

/// Build one wire datagram: stamp `seq`, MAC with `key`.
pub fn seal(key: &[u8], seq: u32, msg_type: MsgType, payload: &[u8]) -> Vec<u8> {
    stoa_proto::Envelope::new(msg_type, seq, payload.to_vec()).encode(key)
}

/// A fresh random session key from the OS CSPRNG (`arc4random_buf`, present
/// in libc on both macOS and FreeBSD; never blocks, always seeded).
pub fn gen_key() -> [u8; KEY_LEN] {
    let mut k = [0u8; KEY_LEN];
    // SAFETY: fills our buffer of KEY_LEN bytes.
    unsafe { libc::arc4random_buf(k.as_mut_ptr() as *mut libc::c_void, k.len()) };
    k
}

/// Lowercase-hex encode.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Decode lowercase/uppercase hex; `None` on odd length or a non-hex digit.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trips() {
        assert_eq!(Control::decode(&Control::Bye.encode()), Some(Control::Bye));
        let r = Control::Resize { cols: 203, rows: 51 };
        assert_eq!(Control::decode(&r.encode()), Some(r));
        assert_eq!(Control::decode(&[]), None);
        assert_eq!(Control::decode(&[99]), None);
        assert_eq!(Control::decode(&[5, 0]), None); // truncated Resize
    }

    #[test]
    fn hex_round_trips() {
        let k = gen_key();
        let h = to_hex(&k);
        assert_eq!(h.len(), KEY_LEN * 2);
        assert_eq!(from_hex(&h).as_deref(), Some(&k[..]));
    }

    #[test]
    fn hex_rejects_bad() {
        assert_eq!(from_hex("abc"), None); // odd
        assert_eq!(from_hex("zz"), None); // non-hex
    }

    #[test]
    fn keys_differ() {
        assert_ne!(gen_key(), gen_key());
    }
}
