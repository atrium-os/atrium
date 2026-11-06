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

use navigator_backend::reverse::{Back, Forward};
use navigator_backend::session::{SessionId, SessionLimits, Sessions, Trail};
use navigator_backend::wire::*;
use navigator_backend::{image, ingest, Limits};
use std::io::{BufReader, Write};

/// ★ A `Sessions` holding at most ONE session: the same code the in-process
/// host runs, so navigation, the trail a save needs, and restore's replay
/// cannot drift apart between the two hosts. The worker has no clock, so it
/// passes 0; expiry is the broker's job, decided on the broker's clock.
struct Worker {
    sessions: Sessions,
    id: Option<SessionId>,
    progress: navigator_backend::progressive::Progress,
    /// The recording as opened: its normalizer inputs, for the display path.
    recording: Vec<u8>,
}

impl Worker {
    fn open(&mut self, recording: &[u8]) -> Result<String, String> {
        if self.id.is_some() {
            // ★ Refused rather than replaced. A worker holding a second
            // document is the thing this design exists to prevent, and a
            // silent replacement would make it look like it worked.
            return Err("this worker already holds a document".into());
        }
        let r = ingest(recording, &Limits::default()).map_err(|e| e.to_string())?;
        self.id = Some(self.sessions.open(&r, 0).map_err(|e| e.to_string())?);
        self.progress = navigator_backend::progressive::Progress::opened(recording);
        self.recording = recording.to_vec();
        Ok(self.triggers())
    }

    /// The first screen (`display`): see `wire::REQ_SCENE` for the shape.
    fn scene(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let nl = payload.iter().position(|&b| b == b'\n').filter(|&i| i <= 40).ok_or("no header line")?;
        let head = std::str::from_utf8(&payload[..nl]).map_err(|_| "header is not text")?;
        let f: Vec<&str> = head.split(' ').collect();
        let [initial, limit, n] = f.as_slice() else { return Err("header is not three fields".into()) };
        let initial = match *initial { "0" => false, "1" => true, _ => return Err("initial is not 0 or 1".into()) };
        let limit: u32 = limit.parse().map_err(|_| "limit is not a number")?;
        let n: usize = n.parse().map_err(|_| "length is not a number")?;
        if nl + 1 + n != payload.len() { return Err("cached length does not match the payload".into()) }
        let cached = (n > 0).then(|| &payload[nl + 1..]);
        let current = if initial { None } else { Some(self.session()?.document().dom.serialize()) };
        let a = navigator_backend::display::first_screen(&self.recording, current.as_deref(), cached, limit)?;
        let norm = a.normalized.unwrap_or_default();
        let mut out = format!("{}\n", norm.len()).into_bytes();
        out.extend_from_slice(&norm);
        out.extend_from_slice(a.nsg.as_bytes());
        Ok(out)
    }

    fn session(&mut self) -> Result<&mut navigator_backend::session::Session, String> {
        let id = self.id.ok_or("no document")?;
        self.sessions.get_mut(id, 0).ok_or_else(|| "no document".to_string())
    }

    fn triggers(&self) -> String {
        let Some(s) = self.id.and_then(|id| self.sessions.get(id)) else { return String::new() };
        s.transitions().iter().filter(|t| t.is_offerable())
            .map(|t| t.trigger.as_str()).collect::<Vec<_>>().join("\n")
    }

    fn url(&mut self) -> Result<String, String> {
        Ok(self.session()?.url.clone())
    }

    fn navigate(&mut self, trigger: &str) -> Result<String, String> {
        self.session()?.navigate(trigger).map_err(|e| e.to_string())?;
        Ok(String::new())
    }

    fn back(&mut self) -> Result<String, String> {
        Ok(match self.session()?.back().map_err(|e| e.to_string())? {
            Back::Stepped => "stepped",
            Back::AtStart => "at-start",
            Back::Forgotten => "forgotten",
        }.to_string())
    }

    fn forward(&mut self) -> Result<String, String> {
        Ok(match self.session()?.forward().map_err(|e| e.to_string())? {
            Forward::Stepped => "stepped",
            Forward::AtEnd => "at-end",
        }.to_string())
    }

    fn save(&mut self) -> Result<Vec<u8>, String> {
        let t = self.session()?.trail().map_err(|e| format!("cannot save: {e}"))?;
        Ok(t.to_json())
    }

    fn restore(&mut self, payload: &[u8]) -> Result<String, String> {
        let t = Trail::from_json(payload)?;
        self.session()?.replay(&t).map_err(|e| e.to_string())?;
        Ok(String::new())
    }

    /// ★ Decode one image of THIS document (threat model §2.4). The broker
    /// sends the bytes it read from the store, the document's remaining
    /// decoded-bytes budget and the size the image is painted at; the worker
    /// decodes and reduces to that size, checks the bytes against their
    /// address itself, and refuses on the header before allocating a frame.
    /// The answer is `<width> <height>\n` and the RGBA8 pixels, which the
    /// broker validates again before anything else sees them.
    fn image(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let nl = payload.iter().position(|&b| b == b'\n').ok_or("image request has no header")?;
        let head = std::str::from_utf8(&payload[..nl]).map_err(|_| "image request header is not text")?;
        let f: Vec<&str> = head.split(' ').collect();
        let [address, budget, tw, th] = f[..] else { return Err("image request header is not `<address> <budget> <tw> <th>`".into()) };
        // Digits only, one spelling, as the broker parses this worker's reply.
        let num = |v: &str| -> Result<usize, String> {
            if v.is_empty() || v.len() > 12 || !v.bytes().all(|b| b.is_ascii_digit()) { return Err(format!("image request field {v:?} is not a number")) }
            v.parse().map_err(|_| format!("image request field {v:?} is not a number"))
        };
        let (budget, tw, th) = (num(budget)?, num(tw)?, num(th)?);
        let target = match (tw, th) {
            (0, 0) => None,
            (w, h) if w >= 1 && h >= 1 && w <= u32::MAX as usize && h <= u32::MAX as usize => Some((w as u32, h as u32)),
            _ => return Err("image request painted size has a zero axis".into()),
        };
        let c = image::Ceilings { budget: budget.min(navigator_dom::profile::MAX_DECODED_IMAGE_BYTES), target, ..Default::default() };
        let px = image::decode(address, &payload[nl + 1..], c).map_err(|e| e.to_string())?;
        let mut out = format!("{} {}\n", px.width, px.height).into_bytes();
        out.extend_from_slice(&px.rgba);
        Ok(out)
    }

    fn upgrade(&mut self, recording: &[u8]) -> Result<String, String> {
        use navigator_backend::progressive::Upgrade;
        let id = self.id.ok_or("no document")?;
        Ok(match self.progress.upgrade(&mut self.sessions, id, recording, &Limits::default(), 0)? {
            Upgrade::Ready(d) => format!("ready {}", d.why()),
            Upgrade::Kept(d) => format!("kept {}", d.why()),
        })
    }

    fn swap(&mut self, payload: &str) -> Result<String, String> {
        use navigator_backend::progressive::{Anchor, How};
        let id = self.id.ok_or("no document")?;
        let (offset, path) = payload.split_once('\n').ok_or("swap request is not `<offset>\\n<path>`")?;
        let offset: i64 = offset.parse().map_err(|_| "swap offset is not a number")?;
        let sw = self.progress.swap(&mut self.sessions, id, &Anchor { path: path.to_string(), offset }, &Limits::default(), 0)?;
        let how = match sw.mapped.how { How::Same => "same", How::Nearest => "nearest", How::Top => "top" };
        let path = if sw.mapped.anchor.path.contains('\n') { String::new() } else { sw.mapped.anchor.path };
        Ok(format!("{how} {}\n{path}\n{}", sw.mapped.anchor.offset, self.triggers()))
    }

    fn attach(&mut self, payload: &[u8]) -> Result<String, String> {
        use navigator_backend::progressive::Attached;
        let id = self.id.ok_or("no document")?;
        Ok(match self.progress.attach(&mut self.sessions, id, payload, &Limits::default(), 0)? {
            Attached::Now => format!("now\n{}", self.triggers()),
            Attached::WithSwap => "with-swap".into(),
            Attached::NeedsSwap => "needs-swap".into(),
        })
    }

    fn links_to(&mut self, href: &str) -> Result<bool, String> {
        Ok(navigator_backend::links::document_carries(&self.session()?.document().dom, href))
    }

    /// What this worker holds. ★ The host does NOT trust this for its budget
    /// — a compromised worker would simply report zero — it charges what it
    /// measured locally before sending. This is for reporting to a person.
    fn size(&self) -> String {
        let n = self.id.and_then(|id| self.sessions.get(id)).map(|s| s.bytes()).unwrap_or(0);
        n.to_string()
    }
}

fn main() {
    let mut w = Worker { sessions: Sessions::new(SessionLimits { max_sessions: 1, ..SessionLimits::default() }), id: None,
                         progress: Default::default(), recording: vec![] };
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
        let answer: Result<Vec<u8>, String> = match f.tag.as_str() {
            REQ_OPEN => w.open(&f.payload).map(String::into_bytes),
            REQ_SAVE => w.save(),
            REQ_RESTORE => w.restore(&f.payload).map(String::into_bytes),
            REQ_NAVIGATE => w.navigate(&f.text()).map(String::into_bytes),
            REQ_BACK => w.back().map(String::into_bytes),
            REQ_FORWARD => w.forward().map(String::into_bytes),
            REQ_TRIGGERS => Ok(w.triggers().into_bytes()),
            REQ_BYTES => Ok(w.size().into_bytes()),
            REQ_URL => w.url().map(String::into_bytes),
            REQ_IMAGE => w.image(&f.payload),
            REQ_UPGRADE => w.upgrade(&f.payload).map(String::into_bytes),
            REQ_SWAP => w.swap(&f.text()).map(String::into_bytes),
            REQ_ATTACH => w.attach(&f.payload).map(String::into_bytes),
            REQ_HREF => w.links_to(&f.text()).map(|y| if y { "yes" } else { "no" }.into()),
            REQ_SCENE => w.scene(&f.payload),
            other => Err(format!("unknown request {other:?}")),
        };
        let reply = match answer {
            Ok(payload) => Frame::new(RSP_OK, payload),
            Err(why) => Frame::new(RSP_ERR, why),
        };
        // ★ The one frame allowed past MAX_FRAME is a pixel reply, and
        // only up to the budget the broker sent with the request.
        let max = if f.tag == REQ_IMAGE { MAX_PIXEL_FRAME } else { MAX_FRAME };
        if write_frame_limited(&mut output, &reply, max).is_err() { return }
        let _ = output.flush();
    }
}
