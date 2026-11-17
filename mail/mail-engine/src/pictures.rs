//! Remote pictures (mail.md §7.4): nothing remote is fetched until the
//! person asks. Then the engine fetches all of a message's loadable
//! pictures at once through its Nuntius (no cookies, no `Referer`, an
//! honest generic agent: the fetcher's own rules) and keeps them with the
//! message, so a second view, a reply or a search fetches nothing. Hidden
//! and tiny pictures (tracking pixels) are never in the list; remote CSS
//! and fonts never either. The bytes are decoded only by the doc-viewer.

use std::time::{Duration, Instant};

use mail_schema::{Document, Source, Use};
use mail_store::MsgId;
use nuntius::{Client, Deliver, Event, Limits, Open, Retry};

use crate::{Engine, Error};

/// Most pictures kept per message, the largest one, and all of them.
pub const PICTURES_MAX: usize = 256;
pub const PICTURE_BYTES_MAX: u64 = 16 << 20;
pub const PICTURES_BYTES_MAX: usize = 64 << 20;
/// How long one "Load pictures" may take in all.
pub const LOAD_TIME_MAX: Duration = Duration::from_secs(60);
/// Requests in flight at once (Nuntius allows 64 per instance).
const IN_FLIGHT: usize = 16;

/// The pictures of `doc` that "Load pictures" may fetch: (resource id, URL).
pub fn loadable(doc: &Document) -> Vec<(String, String)> {
    doc.resources
        .iter()
        .filter(|r| r.loadable())
        .filter_map(|r| match &r.source {
            Source::Remote(u) => Some((r.id.clone(), u.clone())),
            _ => None,
        })
        .take(PICTURES_MAX)
        .collect()
}

/// Fetch `want` (resource id, URL) through Nuntius, a few at a time; what
/// came whole, by resource id. A picture that failed is left out (it stays
/// a placeholder). Bounded in count, size and time.
pub fn fetch(client: &mut Client, want: &[(String, String)]) -> Vec<(String, Vec<u8>)> {
    let deadline = Instant::now() + LOAD_TIME_MAX;
    let mut out = Vec::new();
    let mut total = 0usize;
    let mut next = 0usize;
    // id on the wire → (index in `want`, bytes so far).
    let mut flying: Vec<(u32, usize, Vec<u8>)> = Vec::new();
    loop {
        while flying.len() < IN_FLIGHT && next < want.len() {
            let id = next as u32 + 1;
            let mut o = Open::get(id, &want[next].1);
            o.deliver = Deliver::Stream;
            o.limits = Limits { max_bytes: PICTURE_BYTES_MAX, connect_ms: 10_000, idle_ms: 15_000, total_ms: 30_000 };
            o.retry = Retry { max_attempts: 1, backoff_ms: 250 };
            o.accept = Some("image/*".into());
            if client.open(o).is_ok() {
                flying.push((id, next, Vec::new()));
            }
            next += 1;
        }
        if flying.is_empty() {
            break;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            for (id, ..) in &flying {
                let _ = client.cancel(*id);
            }
            break;
        }
        let ev = match client.recv_timeout(left.min(Duration::from_secs(1))) {
            Ok(Some(e)) => e,
            Ok(None) => continue,
            Err(_) => break,
        };
        let id = ev.id();
        let Some(k) = flying.iter().position(|f| f.0 == id) else { continue };
        match ev {
            Event::Data { bytes, .. } => {
                let f = &mut flying[k];
                if (f.2.len() + bytes.len()) as u64 > PICTURE_BYTES_MAX {
                    let _ = client.cancel(id);
                    flying.remove(k);
                } else {
                    f.2.extend_from_slice(&bytes);
                    // Stream delivery is flow-controlled: give the credit back.
                    let _ = client.budget(bytes.len() as u64);
                }
            }
            Event::Head { status, .. } if !(200..300).contains(&status) => {
                let _ = client.cancel(id);
                flying.remove(k);
            }
            Event::Done { .. } => {
                let (_, i, bytes) = flying.remove(k);
                if !bytes.is_empty() && total + bytes.len() <= PICTURES_BYTES_MAX {
                    total += bytes.len();
                    out.push((want[i].0.clone(), bytes));
                }
            }
            Event::Error { .. } => {
                flying.remove(k);
            }
            _ => {}
        }
    }
    out
}

impl Engine {
    /// What "Load pictures" would fetch for `msg` and isn't kept yet.
    pub fn pictures_to_load(&self, msg: MsgId) -> Result<Vec<(String, String)>, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let Some(doc) = self.open(msg)?.document else { return Ok(Vec::new()) };
        Ok(loadable(&doc).into_iter().filter(|(id, _)| !m.pictures.iter().any(|(k, _)| k == id)).collect())
    }

    /// Keep fetched pictures with the message (only ids "Load pictures"
    /// could have asked for).
    pub fn keep_pictures(&self, msg: MsgId, got: Vec<(String, Vec<u8>)>) -> Result<usize, Error> {
        let doc = self.open(msg)?.document;
        let want = self.pictures_to_load(msg)?;
        let mut kept = Vec::new();
        for (id, bytes) in got {
            if want.iter().any(|(w, _)| *w == id) && !kept.iter().any(|(k, _): &(String, _)| *k == id) {
                kept.push((id, self.store.put_blob(&bytes)?));
            }
        }
        let n = kept.len();
        // A background is drawn only at a known size: convert the document
        // again, now with the sizes (the jail reads them, not the engine).
        let background = doc.is_some_and(|d| d.resources.iter().any(|r| r.used_as == Use::Background && kept.iter().any(|(k, _)| *k == r.id)));
        self.store.write(|w| {
            w.add_pictures(msg, kept)?;
            if background {
                w.set_document(msg, None)?;
            }
            Ok(())
        })?;
        Ok(n)
    }

    /// The remote pictures kept with `msg`, for the doc-viewer.
    pub fn pictures(&self, msg: MsgId) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let mut out = Vec::new();
        for (id, h) in m.pictures {
            if let Ok(b) = self.store.blob(&h) {
                out.push((id, b));
            }
        }
        Ok(out)
    }
}
