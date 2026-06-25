//! # stoa — daemon + client shared bits
//!
//! At S1 the bridge moves onto the [`stoa_net`] datagram transport and
//! `stoad` grows a **session table**: the shell lives in `stoad`,
//! independent of any client connection, so a dropped client (flaky wifi,
//! closed laptop) leaves the session running and a later attach resumes
//! the *same* shell. That is the property `ssh + tmux` can't give you —
//! ssh owns the shell's lifetime, so a dropped ssh connection kills it.
//!
//! What S1 still omits: real terminal-grid `StateDiff`s (§3.3) — the pty
//! byte stream is carried raw inside `StateDiff`/`Input` payloads for now;
//! scrollback/persistence across `stoad` restarts (S3); the SSP predictor
//! (§3.4); and the real `K_sess` from the SSH handshake — until that
//! lands, client and daemon share [`DEV_KEY`].

use stoa_proto::MsgType;

/// Default `stoad` UDP address for the dev build. Overridable via
/// `$STOA_ADDR`. (Production binds per-session ports announced at the SSH
/// handoff; one fixed port is the single-key dev simplification.)
pub fn default_addr() -> String {
    std::env::var("STOA_ADDR").unwrap_or_else(|_| "127.0.0.1:7654".into())
}

/// Shared dev `K_sess`. **Placeholder.** Real sessions derive a per-session
/// key from the SSH session id at handoff (stoa.md §2); until that path
/// exists, client and daemon agree on this constant so the envelope MAC is
/// exercised end-to-end. Not a secret; never ship it.
pub const DEV_KEY: &[u8] = b"stoa-dev-shared-K_sess-NOT-FOR-PRODUCTION";

/// The detach key in the raw input stream: Ctrl-] (0x1d, telnet-style).
/// A real client uses the tmux-compatible `Ctrl-B d` prefix map (S2); for
/// S1 a single escape byte keeps the client tiny. Detach leaves the shell
/// running in `stoad`; reattach resumes it.
pub const DETACH_BYTE: u8 = 0x1d;

/// Control-channel messages (carried as the payload of a
/// [`MsgType::Control`] datagram). A hand-rolled 1-byte-tag encoding —
/// small and dependency-free; the richer `DaemonCmd` set (§3.2) is S2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// Client → server: attach to (creating if absent) the named session.
    Attach { name: String },
    /// Client → server: detach; leave the shell running.
    Detach,
    /// Server → client: attach acknowledged.
    Attached,
    /// Server → client: the session's shell exited; nothing to resume.
    Bye,
}

impl Control {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Control::Attach { name } => {
                let mut v = vec![1u8];
                v.extend_from_slice(name.as_bytes());
                v
            }
            Control::Detach => vec![2],
            Control::Attached => vec![3],
            Control::Bye => vec![4],
        }
    }

    pub fn decode(payload: &[u8]) -> Option<Control> {
        let (&tag, rest) = payload.split_first()?;
        Some(match tag {
            1 => Control::Attach {
                name: String::from_utf8_lossy(rest).into_owned(),
            },
            2 => Control::Detach,
            3 => Control::Attached,
            4 => Control::Bye,
            _ => return None,
        })
    }
}

/// Build one wire datagram: stamp `seq`, MAC with `key`. The client's
/// split tx/rx halves use this directly (rather than a full `Session`)
/// because its send and receive run on separate threads.
pub fn seal(key: &[u8], seq: u32, msg_type: MsgType, payload: &[u8]) -> Vec<u8> {
    stoa_proto::Envelope::new(msg_type, seq, payload.to_vec()).encode(key)
}

/// The datagram type carrying raw client→shell keystrokes.
pub const INPUT: MsgType = MsgType::Input;
/// The datagram type carrying raw shell→client output (real grid diffs S2).
pub const OUTPUT: MsgType = MsgType::StateDiff;
/// The datagram type carrying [`Control`] messages.
pub const CONTROL: MsgType = MsgType::Control;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_round_trips() {
        for c in [
            Control::Attach { name: "work-build".into() },
            Control::Attach { name: String::new() },
            Control::Detach,
            Control::Attached,
            Control::Bye,
        ] {
            assert_eq!(Control::decode(&c.encode()), Some(c));
        }
    }

    #[test]
    fn empty_payload_is_none() {
        assert_eq!(Control::decode(&[]), None);
        assert_eq!(Control::decode(&[99]), None);
    }
}
