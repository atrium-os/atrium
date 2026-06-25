//! # stoa-proto — the Stoa wire protocol
//!
//! The OS-agnostic core of Stoa (docs/spec/stoa.md §3): the MAC'd,
//! sequence-numbered datagram envelope and the anti-replay window that
//! together give Stoa its **flaky-connection tolerance** — the property
//! that motivates the whole service. This crate has no OS dependencies
//! and builds + tests identically on macOS (the dev/test host) and
//! FreeBSD (production), which is what lets slices S0–S2 be developed
//! entirely on the host with no VM in the loop (§11.1, §15).
//!
//! Layers, bottom-up:
//! - [`envelope`] — `ver|type|seq|payload|MAC[16]`, encode/decode with a
//!   truncated HMAC-SHA-256 keyed by the session `K_sess`.
//! - [`replay`] — the N=128 sliding anti-replay window; one per attach.
//!
//! Still to come (later slices, kept out of this foundation deliberately):
//! the typed `Input`/`DaemonCmd` dispositions (§3.2) and `StateDiff`
//! (§3.3) ride *inside* an [`envelope::Envelope`]'s payload; the SSP
//! predictor (§3.4) sits above the ack stream.

pub mod envelope;
pub mod replay;

pub use envelope::{Envelope, MsgType, ProtoError, MAC_LEN, VERSION};
pub use replay::{ReplayWindow, WINDOW};

#[cfg(test)]
mod integration {
    //! A round of the receive path end-to-end: decode (which authenticates)
    //! then admit through the replay window — the two checks every inbound
    //! datagram passes, in order.
    use super::*;

    fn recv(key: &[u8], win: &mut ReplayWindow, wire: &[u8]) -> Option<Envelope> {
        // 1. authenticate + parse; a bad MAC drops silently.
        let env = Envelope::decode(key, wire).ok()?;
        // 2. anti-replay; an old/duplicate seq drops silently.
        if !win.accept(env.seq) {
            return None;
        }
        Some(env)
    }

    #[test]
    fn authenticated_then_deduped() {
        let key = b"K_sess";
        let mut win = ReplayWindow::new();

        let a = Envelope::new(MsgType::Input, 1, b"a".to_vec()).encode(key);
        let b = Envelope::new(MsgType::Input, 2, b"b".to_vec()).encode(key);

        assert!(recv(key, &mut win, &a).is_some());
        assert!(recv(key, &mut win, &b).is_some());
        // A replayed datagram (same bytes) authenticates but is dropped by
        // the window — replay defeated even with a valid MAC.
        assert!(recv(key, &mut win, &a).is_none());

        // A forged datagram never gets as far as the window.
        let mut forged = a.clone();
        let n = forged.len();
        forged[n - 1] ^= 0xff;
        assert!(recv(key, &mut win, &forged).is_none());
    }
}
