//! Clean-room predictive echo (stoa.md §3.4) — the SSP idea, reimplemented
//! from the *paper* (Winstein & Balakrishnan, USENIX ATC 2012) and observable
//! behaviour, not from mosh's GPL `statesync/` (Atrium licensing policy).
//!
//! The win: on a high-latency link, characters you type appear *instantly*
//! instead of after a round-trip, because the client predicts the terminal
//! will echo them. The server stays authoritative — when its real output
//! arrives, predictions are confirmed (and the duplicate echo suppressed) or,
//! on divergence, dropped and the screen repainted from the server's grid
//! mirror (the [`Control::Redraw`](crate::Control) we already have).
//!
//! Scope (matching the paper's conservative predictor): predict only
//! **printable ASCII** echo; queue silently until confirmations show the
//! terminal really is echoing (warm-up), then display predictions; back off
//! to silent on any divergence (so full-screen apps like vi, where keys
//! don't echo at the cursor, never garble). This is the byte-stream form;
//! the cursor-accurate grid form is a later refinement.

use std::collections::VecDeque;

/// Consecutive confirmed predictions before we start *displaying* them.
const WARMUP: u32 = 6;

/// Predictive-echo state. Pure: methods return the bytes to write and whether
/// a server repaint is needed; the caller does the I/O (so it's testable).
#[derive(Debug, Default)]
pub struct Predictor {
    /// Predicted echoes awaiting server confirmation: (byte, shown_locally).
    queue: VecDeque<(u8, bool)>,
    /// Are we currently *displaying* predictions (vs queuing silently)?
    display: bool,
    /// Consecutive confirmations since the last miss (warm-up counter).
    hits: u32,
    /// Input escape-sequence state — bytes inside a sequence (arrow/function
    /// keys) aren't echoed literally, so we don't predict/queue them.
    seq: SeqState,
}

/// Minimal input escape-sequence tracker (enough to not predict cursor keys).
#[derive(Debug, Default, PartialEq, Eq)]
enum SeqState {
    #[default]
    Normal,
    /// Just saw ESC.
    AfterEsc,
    /// In a CSI/SS3 sequence (ESC `[`/`O` …), until a final byte 0x40–0x7e.
    Csi,
}

impl Predictor {
    pub fn new() -> Self {
        Predictor::default()
    }

    /// Whether predictions are currently being displayed (warmed up).
    pub fn displaying(&self) -> bool {
        self.display
    }

    /// A keystroke headed to the server. Returns `Some(byte)` to echo locally
    /// now (only when displaying + printable), else `None`. Always also send
    /// the key as Input — prediction is *in addition to*, not instead of.
    pub fn on_input(&mut self, byte: u8) -> Option<u8> {
        match self.seq {
            SeqState::AfterEsc => {
                // ESC [ (CSI) or ESC O (SS3) continue; ESC <other> is a
                // 2-byte sequence that ends here.
                self.seq = if byte == b'[' || byte == b'O' {
                    SeqState::Csi
                } else {
                    SeqState::Normal
                };
                return None;
            }
            SeqState::Csi => {
                // Final byte 0x40–0x7e ends the sequence.
                if (0x40..=0x7e).contains(&byte) {
                    self.seq = SeqState::Normal;
                }
                return None;
            }
            SeqState::Normal => {}
        }
        if byte == 0x1b {
            self.seq = SeqState::AfterEsc; // ESC — arrow/function key etc.
            return None;
        }
        if (0x20..0x7f).contains(&byte) {
            self.queue.push_back((byte, self.display));
            if self.display {
                return Some(byte);
            }
        }
        // Other non-printable keystrokes aren't predicted; pending printable
        // predictions before them stay queued (confirmed/dropped by output).
        None
    }

    /// Server output arrived. Returns `(bytes to write to stdout, need_redraw)`.
    /// The confirmed-prediction prefix is consumed: bytes predicted *and shown*
    /// are suppressed (already on screen); silently-predicted bytes are emitted
    /// (the server echo is their first display). On divergence the wrong
    /// predictions are dropped and, if any were shown, a repaint is requested.
    pub fn on_output(&mut self, server: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(server.len());
        let mut i = 0;
        while i < server.len() {
            match self.queue.front().copied() {
                Some((b, shown)) if b == server[i] => {
                    self.queue.pop_front();
                    if !shown {
                        out.push(b); // silent prediction → show the server's echo
                    }
                    i += 1;
                    self.hits += 1;
                }
                _ => break,
            }
        }

        let mut need_redraw = false;
        if i < server.len() && !self.queue.is_empty() {
            // Divergence: the remaining queued predictions were wrong.
            let any_shown = self.queue.iter().any(|&(_, s)| s);
            self.queue.clear();
            self.hits = 0;
            self.display = false; // back off to silent until we re-warm
            need_redraw = any_shown; // repaint to erase garbled local echo
        } else if self.hits >= WARMUP {
            self.display = true; // the terminal is reliably echoing → predict
        }

        out.extend_from_slice(&server[i..]); // authoritative output beyond echoes
        (out, need_redraw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a warm-up: typing N chars that the server echoes back turns on
    /// display. Helper returns the predictor warmed (displaying).
    fn warmed() -> Predictor {
        let mut p = Predictor::new();
        for _ in 0..WARMUP {
            p.on_input(b'x'); // silent (display off)
            let (out, rd) = p.on_output(b"x"); // server echoes
            assert_eq!(out, b"x"); // silent prediction shown via server echo
            assert!(!rd);
        }
        assert!(p.displaying(), "warmed up after {WARMUP} confirmations");
        p
    }

    #[test]
    fn warms_up_then_predicts_and_suppresses_echo() {
        let mut p = warmed();
        // Now displaying: a keystroke echoes locally immediately...
        assert_eq!(p.on_input(b'a'), Some(b'a'));
        // ...and the server's later echo is suppressed (already shown).
        let (out, rd) = p.on_output(b"a");
        assert_eq!(out, b""); // suppressed — no double 'a'
        assert!(!rd);
    }

    #[test]
    fn silent_until_warmed() {
        let mut p = Predictor::new();
        assert_eq!(p.on_input(b'a'), None); // not displaying yet
        let (out, rd) = p.on_output(b"a");
        assert_eq!(out, b"a"); // server echo shown normally
        assert!(!rd);
    }

    #[test]
    fn non_echoed_output_passes_through() {
        let mut p = warmed();
        // Output the user didn't type (a prompt / command result) just shows.
        let (out, rd) = p.on_output(b"$ hello\r\n");
        assert_eq!(out, b"$ hello\r\n");
        assert!(!rd);
    }

    #[test]
    fn divergence_drops_predictions_and_requests_redraw() {
        let mut p = warmed();
        // Type "ab" (shown locally) but the server echoes "aX" (e.g. autocorrect).
        assert_eq!(p.on_input(b'a'), Some(b'a'));
        assert_eq!(p.on_input(b'b'), Some(b'b'));
        let (out, rd) = p.on_output(b"aX");
        assert_eq!(out, b"X"); // 'a' confirmed+suppressed; 'X' is authoritative
        assert!(rd); // 'b' was shown but wrong → repaint
        assert!(!p.displaying()); // backed off to silent
    }

    #[test]
    fn non_printable_input_not_predicted() {
        let mut p = warmed();
        assert_eq!(p.on_input(b'\r'), None); // CR not predicted
    }

    #[test]
    fn escape_sequence_bytes_not_predicted() {
        let mut p = warmed();
        // Up arrow = ESC [ A — none of these should echo locally, including
        // the printable '[' and 'A'.
        assert_eq!(p.on_input(0x1b), None);
        assert_eq!(p.on_input(b'['), None); // printable, but mid-sequence
        assert_eq!(p.on_input(b'A'), None); // final byte ends the sequence
        // back to normal printable prediction afterwards
        assert_eq!(p.on_input(b'z'), Some(b'z'));
    }

    #[test]
    fn partial_confirmation_across_datagrams() {
        let mut p = warmed();
        p.on_input(b'a');
        p.on_input(b'b');
        p.on_input(b'c');
        // server confirms in two pieces
        let (o1, _) = p.on_output(b"a");
        assert_eq!(o1, b""); // suppressed
        let (o2, _) = p.on_output(b"bc");
        assert_eq!(o2, b""); // suppressed
    }
}
