//! One document, one process, zero capabilities.
//!
//! ★ THIS IS THE ONLY PROCESS THAT PARSES. It receives a recording's bytes on
//! stdin, and its single output is answers on stdout. It opens no files,
//! makes no network calls, and holds no store handle — so "the document
//! exfiltrates what it parsed" has no vehicle even before a jail is applied
//! (spec §2).
//!
//! ★★ IT HOLDS EXACTLY ONE SESSION. Spec §2: one jail per document, "the
//! default, not a mitigation". A worker that multiplexed sessions would put
//! two documents in one address space and reintroduce precisely the sharing
//! that Site Isolation had to be retrofitted elsewhere to undo.
//!
//! It is deliberately not a library: everything here is I/O and dispatch, and
//! the logic it dispatches to is tested directly in the crate.

use navigator_backend::document::Document;
use navigator_backend::reverse::{Back, History};
use navigator_backend::wire::*;
use navigator_backend::{ingest, Limits, Transition};
use std::io::{BufReader, Write};

struct Worker {
    history: Option<History>,
    transitions: Vec<Transition>,
}

impl Worker {
    fn open(&mut self, recording: &[u8]) -> Result<String, String> {
        if self.history.is_some() {
            // ★ Refused rather than replaced. A worker holding a second
            // document is the thing this design exists to prevent, and a
            // silent replacement would make it look like it worked.
            return Err("this worker already holds a document".into());
        }
        let r = ingest(recording, &Limits::default()).map_err(|e| e.to_string())?;
        let doc = Document::accept(&r.document)
            .map_err(|v| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", "))?;
        self.transitions = r.transitions;
        self.history = Some(History::new(doc));
        Ok(self.triggers())
    }

    fn triggers(&self) -> String {
        self.transitions.iter().filter(|t| t.anchored)
            .map(|t| t.trigger.as_str()).collect::<Vec<_>>().join("\n")
    }

    fn navigate(&mut self, trigger: &str) -> Result<String, String> {
        let t = self.transitions.iter()
            .find(|t| t.anchored && t.trigger == trigger)
            .ok_or_else(|| format!("nothing recorded for {trigger:?}"))?
            .clone();
        let h = self.history.as_mut().ok_or("no document")?;
        h.go(&t).map_err(|e| e.to_string())?;
        Ok(String::new())
    }

    fn back(&mut self) -> Result<String, String> {
        let h = self.history.as_mut().ok_or("no document")?;
        Ok(match h.back().map_err(|e| e.to_string())? {
            Back::Stepped => "stepped",
            Back::AtStart => "at-start",
            Back::Forgotten => "forgotten",
        }.to_string())
    }

    /// What this worker holds. ★ The host does NOT trust this for its budget
    /// — a compromised worker would simply report zero — it charges what it
    /// measured locally before sending. This is for reporting to a person.
    fn size(&self) -> String {
        let n = self.history.as_ref()
            .map(|h| h.document().dom.serialize().len() + h.cost().undo_bytes)
            .unwrap_or(0);
        n.to_string()
    }
}

fn main() {
    let mut w = Worker { history: None, transitions: vec![] };
    let mut input = BufReader::new(std::io::stdin());
    let mut output = std::io::stdout();
    loop {
        let f = match read_frame(&mut input) {
            Ok(f) => f,
            // A closed stdin is the host going away: exit quietly.
            Err(WireError::Closed) => return,
            Err(e) => {
                // Anything else is the host sending nonsense. Say so on
                // stderr, which the host collects, and stop — continuing
                // after a framing error means reading a stream we have lost
                // our place in.
                eprintln!("worker: {e}");
                std::process::exit(2);
            }
        };
        let answer = match f.tag.as_str() {
            REQ_OPEN => w.open(&f.payload),
            REQ_NAVIGATE => w.navigate(&f.text()),
            REQ_BACK => w.back(),
            REQ_TRIGGERS => Ok(w.triggers()),
            REQ_BYTES => Ok(w.size()),
            other => Err(format!("unknown request {other:?}")),
        };
        let reply = match answer {
            Ok(payload) => Frame::new(RSP_OK, payload),
            Err(why) => Frame::new(RSP_ERR, why),
        };
        if write_frame(&mut output, &reply).is_err() { return }
        let _ = output.flush();
    }
}
