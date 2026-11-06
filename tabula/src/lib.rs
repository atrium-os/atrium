//! Tabula — the clipboard service (docs/spec/tabula.md).
//!
//! - [`proto`]: the `CLASS_CLIPBOARD` wire.
//! - [`gate`]: proof of a user gesture. Every copy, cut and paste spends
//!   an activation token that frescod minted for a press in the caller's
//!   own focused window; Tabula checks it with frescod and never with the
//!   app. Without a reachable frescod, [`gate::DenyAll`] refuses
//!   everything: the service fails closed.
//! - [`store`]: the one entry, its limits, sensitive entries, the audit
//!   log. Content is bytes: Tabula never parses or converts it.
//! - [`server`]: one client connection.

pub mod proto {
    use serde::{Deserialize, Serialize};

    pub use aqueduct::classes::CLASS_CLIPBOARD as CLASS;
    pub use fresco_protocol::activation::Token;

    pub mod op {
        pub const SET: u16 = 0x01;
        pub const OFFERS: u16 = 0x02;
        pub const GET: u16 = 0x03;
        pub const CLEAR: u16 = 0x04;
        /// Hold a drag's payload until a drop (§4).
        pub const DRAG_BEGIN: u16 = 0x10;
        /// Take a drop's payload, once.
        pub const DROP_GET: u16 = 0x11;
        /// The source gives up its drag.
        pub const DRAG_CANCEL: u16 = 0x12;
        /// Pushed to every client when the entry changes.
        pub const EV_CHANGED: u16 = 0x80;
    }

    /// One format of an entry: its type and the CAS hash of its bytes. On
    /// `SET` the bytes must have been uploaded on this connection first
    /// (proven possession, aqueduct.md §6.6); on `GET` Tabula uploads them
    /// to the client before replying.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Format {
        pub mime: String,
        pub hash: aqueduct::Hash,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Set {
        pub token: Token,
        pub formats: Vec<Format>,
        /// A password or other secret (Arca sets it): cleared after
        /// [`crate::store::SENSITIVE_SECS`], offered as `text/plain` only.
        pub sensitive: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Get {
        pub token: Token,
        /// Wanted formats, most wanted first.
        pub accept: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DragBegin {
        /// The press that started the drag (purpose `DRAG`).
        pub token: Token,
        pub formats: Vec<Format>,
    }

    /// `token` is the `DROP` token frescod sent the target with `EV_DROP`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DropGet {
        pub token: Token,
        pub accept: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DragCancel {
        pub drag_id: u64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Clear {
        pub token: Token,
    }

    /// Why a request was refused (also what the audit log records).
    pub mod refusal {
        /// The token did not prove a gesture (frescod's reason follows
        /// in `detail`).
        pub const NO_GESTURE: u8 = 1;
        /// A format names bytes this connection never uploaded.
        pub const NOT_UPLOADED: u8 = 2;
        pub const TOO_MANY_FORMATS: u8 = 3;
        pub const BAD_MIME: u8 = 4;
        pub const TOO_BIG: u8 = 5;
        pub const MALFORMED: u8 = 6;
        pub const NO_FORMATS: u8 = 7;
        pub const DUPLICATE_MIME: u8 = 8;
        /// No such drag, or it was dropped, cancelled or timed out.
        pub const NO_DRAG: u8 = 9;
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Reply {
        Ok,
        /// The drag's id, to hand to frescod's drag begin.
        Dragging { drag_id: u64 },
        Offers { formats: Vec<String>, generation: u64 },
        /// The format chosen from `accept`; its bytes were uploaded to the
        /// client just before this reply.
        Got { format: Format },
        /// Nothing on the clipboard in an accepted format.
        Empty,
        Refused { reason: u8, detail: u8 },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Changed {
        pub generation: u64,
    }
}

pub mod gate {
    use fresco_protocol::activation::{Consume, ConsumeResult, Token};

    /// Proof of a user gesture: frescod's `ACTIVATION_CONSUME`.
    pub trait Gate: Send {
        fn consume(&mut self, c: Consume) -> ConsumeResult;
    }

    /// No frescod: refuse everything.
    pub struct DenyAll;

    impl Gate for DenyAll {
        fn consume(&mut self, _: Consume) -> ConsumeResult {
            ConsumeResult::Refused { reason: fresco_protocol::activation::refusal::NOT_ALLOWED }
        }
    }

    /// frescod's activation socket (only `_tabula` and the other
    /// allowlisted brokers can reach it). A broken connection refuses.
    pub struct Frescod {
        conn: Option<aqueduct::Connection>,
        path: std::path::PathBuf,
    }

    impl Frescod {
        pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
            Self { conn: None, path: path.into() }
        }

        fn ask(&mut self, c: Consume) -> std::io::Result<ConsumeResult> {
            use aqueduct::flag;
            if self.conn.is_none() {
                self.conn = Some(aqueduct::Connection::connect(&self.path)?);
            }
            let conn = self.conn.as_mut().unwrap();
            let payload = postcard::to_stdvec(&c).map_err(invalid)?;
            conn.send_message(
                aqueduct::classes::CLASS_DISPLAY,
                fresco_protocol::activation::op::CONSUME,
                flag::RESPONSE_EXPECTED,
                &payload,
            )?;
            conn.set_read_timeout(Some(std::time::Duration::from_millis(500)))?;
            let m = conn.recv_message()?;
            if m.op != fresco_protocol::activation::op::CONSUME || m.flags & flag::IS_RESPONSE == 0 {
                return Err(invalid("unexpected reply"));
            }
            postcard::from_bytes(&m.payload).map_err(invalid)
        }
    }

    fn invalid<E: std::fmt::Display>(e: E) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    }

    impl Gate for Frescod {
        fn consume(&mut self, c: Consume) -> ConsumeResult {
            match self.ask(c) {
                Ok(r) => r,
                Err(_) => {
                    self.conn = None;
                    ConsumeResult::Refused { reason: fresco_protocol::activation::refusal::NOT_ALLOWED }
                }
            }
        }
    }

    /// A stand-in for frescod in tests: mint tokens for a uid and kind,
    /// consume them with frescod's rules (one use, 1 s, same uid, kind
    /// fits the purpose). Clones share one table, so a test keeps a handle
    /// to mint with while the server holds another.
    #[derive(Clone, Default)]
    pub struct Fake(std::sync::Arc<std::sync::Mutex<FakeTable>>);

    #[derive(Default)]
    pub struct FakeTable {
        live: std::collections::HashMap<Token, (u32, u8, std::time::Instant, u64)>,
        next: u64,
    }

    impl Fake {
        pub fn mint(&self, uid: u32, kind: u8) -> Token {
            let mut t = self.0.lock().unwrap();
            t.next += 1;
            let mut tok = [0u8; 16];
            tok[..8].copy_from_slice(&t.next.to_le_bytes());
            tok[8] = 0xa5;
            t.live.insert(tok, (uid, kind, std::time::Instant::now(), 0));
            tok
        }

        /// What frescod does on a drop: a `DROP` token for the window's
        /// uid, bound to the drag.
        pub fn mint_drop(&self, uid: u32, drag_id: u64) -> Token {
            let tok = self.mint(uid, fresco_protocol::activation::kind::DROP);
            self.0.lock().unwrap().live.get_mut(&tok).unwrap().3 = drag_id;
            tok
        }

        /// Age a token past its lifetime.
        pub fn expire(&self, tok: &Token) {
            if let Some(e) = self.0.lock().unwrap().live.get_mut(tok) {
                e.2 -= std::time::Duration::from_secs(2);
            }
        }
    }

    impl Gate for Fake {
        fn consume(&mut self, c: Consume) -> ConsumeResult {
            use fresco_protocol::activation::{kind, purpose, refusal};
            let Some((uid, k, at, drag_id)) = self.0.lock().unwrap().live.remove(&c.token) else {
                return ConsumeResult::Refused { reason: refusal::UNKNOWN };
            };
            if at.elapsed() > std::time::Duration::from_secs(1) {
                return ConsumeResult::Refused { reason: refusal::EXPIRED };
            }
            if uid != c.presenter_uid {
                return ConsumeResult::Refused { reason: refusal::WRONG_UID };
            }
            let fits = match c.purpose {
                purpose::COPY | purpose::PASTE => k == kind::BUTTON || k == kind::KEY || k == kind::GRANTED,
                purpose::DROP => k == kind::DROP,
                _ => k == kind::BUTTON,
            };
            if !fits {
                return ConsumeResult::Refused { reason: refusal::WRONG_KIND };
            }
            ConsumeResult::Ok { window_id: 1, kind: k, minted_ns: 0, drag_id }
        }
    }
}

pub mod store {
    use std::time::{Duration, Instant};

    use crate::proto::refusal;

    pub const MAX_FORMATS: usize = 16;
    pub const MAX_MIME: usize = 127;
    /// Per format: what one aqueduct upload carries.
    pub const MAX_FORMAT_BYTES: usize = aqueduct::MAX_PAYLOAD as usize;
    /// Per entry.
    pub const MAX_ENTRY_BYTES: usize = 64 * 1024 * 1024;
    pub const SENSITIVE_SECS: u64 = 60;
    pub const AUDIT_KEEP: usize = 1024;

    /// `type/subtype` in `[a-z0-9.+-]`, nothing else (no parameters): a
    /// format name is a label, never parsed further.
    pub fn valid_mime(m: &str) -> bool {
        let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '+' | '-');
        let Some((a, b)) = m.split_once('/') else { return false };
        !m.is_empty() && m.len() <= MAX_MIME && !a.is_empty() && !b.is_empty() && a.chars().all(ok) && b.chars().all(ok)
    }

    #[derive(Debug, Clone)]
    struct Entry {
        formats: Vec<(String, Vec<u8>)>,
        owner: u32,
        sensitive: bool,
        at: Instant,
    }

    /// Who did what (uids and times, never contents).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Audit {
        pub uid: u32,
        pub op: u16,
        /// 0 = done, else a `proto::refusal`.
        pub outcome: u8,
        pub at: Instant,
    }

    /// A drag not yet dropped lives this long.
    pub const DRAG_SECS: u64 = 60;

    #[derive(Debug, Clone)]
    struct Drag {
        owner: u32,
        formats: Vec<(String, Vec<u8>)>,
        at: Instant,
    }

    #[derive(Debug, Default)]
    pub struct Store {
        entry: Option<Entry>,
        /// Pending drags by id; one per source uid.
        drags: std::collections::HashMap<u64, Drag>,
        next_drag: u64,
        pub generation: u64,
        audit: std::collections::VecDeque<Audit>,
    }

    impl Store {
        pub fn new() -> Self { Self::default() }

        pub fn audit(&self) -> impl Iterator<Item = &Audit> { self.audit.iter() }

        pub fn record(&mut self, uid: u32, op: u16, outcome: u8, at: Instant) {
            if self.audit.len() == AUDIT_KEEP {
                self.audit.pop_front();
            }
            self.audit.push_back(Audit { uid, op, outcome, at });
        }

        /// Check an entry before any gesture is spent on it.
        pub fn validate(formats: &[(String, usize)]) -> Result<(), u8> {
            if formats.is_empty() {
                return Err(refusal::NO_FORMATS);
            }
            if formats.len() > MAX_FORMATS {
                return Err(refusal::TOO_MANY_FORMATS);
            }
            let mut total = 0usize;
            for (i, (m, n)) in formats.iter().enumerate() {
                if !valid_mime(m) {
                    return Err(refusal::BAD_MIME);
                }
                if formats[..i].iter().any(|(o, _)| o == m) {
                    return Err(refusal::DUPLICATE_MIME);
                }
                if *n > MAX_FORMAT_BYTES {
                    return Err(refusal::TOO_BIG);
                }
                total += n;
            }
            if total > MAX_ENTRY_BYTES {
                return Err(refusal::TOO_BIG);
            }
            Ok(())
        }

        /// Replace the entry (the caller has validated it and spent a
        /// gesture).
        pub fn set(&mut self, owner: u32, formats: Vec<(String, Vec<u8>)>, sensitive: bool, now: Instant) {
            let formats = if sensitive {
                // A secret is offered as text only, whatever else came with it.
                formats.into_iter().filter(|(m, _)| m == "text/plain").collect()
            } else {
                formats
            };
            self.entry = Some(Entry { formats, owner, sensitive, at: now });
            self.generation += 1;
        }

        pub fn clear(&mut self) {
            if self.entry.take().is_some() {
                self.generation += 1;
            }
        }

        /// Drop a sensitive entry past its time. Returns whether it did.
        pub fn expire(&mut self, now: Instant) -> bool {
            let old = self.entry.as_ref().is_some_and(|e| e.sensitive && now.duration_since(e.at) >= Duration::from_secs(SENSITIVE_SECS));
            if old {
                self.clear();
            }
            old
        }

        /// Format names only.
        pub fn offers(&mut self, now: Instant) -> Vec<String> {
            self.expire(now);
            self.entry.as_ref().map(|e| e.formats.iter().map(|(m, _)| m.clone()).collect()).unwrap_or_default()
        }

        /// The first of `accept` the entry has.
        pub fn get(&mut self, accept: &[String], now: Instant) -> Option<(String, Vec<u8>)> {
            self.expire(now);
            let e = self.entry.as_ref()?;
            accept.iter().find_map(|a| e.formats.iter().find(|(m, _)| m == a).cloned())
        }

        pub fn owner(&self) -> Option<u32> { self.entry.as_ref().map(|e| e.owner) }

        /// Hold a drag's payload. A source has one drag at a time: a new
        /// one replaces its old one.
        pub fn drag_begin(&mut self, owner: u32, formats: Vec<(String, Vec<u8>)>, now: Instant) -> u64 {
            self.drags.retain(|_, d| d.owner != owner && now.duration_since(d.at) < Duration::from_secs(DRAG_SECS));
            self.next_drag += 1;
            self.drags.insert(self.next_drag, Drag { owner, formats, at: now });
            self.next_drag
        }

        /// Release a dropped drag's payload in the first accepted format;
        /// the drag is gone either way.
        pub fn drop_take(&mut self, drag_id: u64, accept: &[String], now: Instant) -> Result<Option<(String, Vec<u8>)>, u8> {
            let d = self.drags.remove(&drag_id).ok_or(refusal::NO_DRAG)?;
            if now.duration_since(d.at) >= Duration::from_secs(DRAG_SECS) {
                return Err(refusal::NO_DRAG);
            }
            Ok(accept.iter().find_map(|a| d.formats.iter().find(|(m, _)| m == a).cloned()))
        }

        /// Only its source can cancel a drag.
        pub fn drag_cancel(&mut self, owner: u32, drag_id: u64) -> bool {
            match self.drags.get(&drag_id) {
                Some(d) if d.owner == owner => self.drags.remove(&drag_id).is_some(),
                _ => false,
            }
        }

        pub fn pending_drags(&self) -> usize { self.drags.len() }
    }
}

pub mod server {
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use aqueduct::{flag, Connection, Message};
    use fresco_protocol::activation::{purpose, Consume, ConsumeResult};

    use crate::gate::Gate;
    use crate::proto::{self, op, refusal, Format, Reply};
    use crate::store::{self, Store};

    /// Shared by every connection.
    pub struct Shared {
        pub store: Mutex<Store>,
        pub gate: Mutex<Box<dyn Gate>>,
    }

    impl Shared {
        pub fn new(gate: Box<dyn Gate>) -> Arc<Self> {
            Arc::new(Self { store: Mutex::new(Store::new()), gate: Mutex::new(gate) })
        }
    }

    /// Serve one client until it hangs up. `uid` is the kernel's answer
    /// for the peer (`portcullis_peer::uid_gid`), never the client's claim.
    pub fn serve(mut conn: Connection, uid: u32, shared: Arc<Shared>) -> io::Result<()> {
        // Room for one entry's uploads.
        conn.set_cache_cap(store::MAX_ENTRY_BYTES + store::MAX_FORMAT_BYTES);
        let mut seen = shared.store.lock().unwrap().generation;
        loop {
            // Wake now and then to tell this client the entry changed:
            // events go out on this thread, so they never interleave with
            // a reply.
            let m = conn.recv_message_or_timeout(Duration::from_millis(100))?;
            let now_gen = {
                let mut s = shared.store.lock().unwrap();
                s.expire(Instant::now());
                s.generation
            };
            if now_gen != seen {
                seen = now_gen;
                let ev = postcard::to_stdvec(&proto::Changed { generation: now_gen }).unwrap();
                conn.send_message(proto::CLASS, op::EV_CHANGED, flag::ASYNC_EVENT, &ev)?;
            }
            let Some(m) = m else { continue };
            if m.opcode_class != proto::CLASS {
                continue;
            }
            let reply = handle(&mut conn, uid, &shared, &m);
            let gen_after = shared.store.lock().unwrap().generation;
            // The caller learns of its own change from the reply.
            seen = gen_after;
            let payload = postcard::to_stdvec(&reply).unwrap();
            conn.send_message(proto::CLASS, m.op, flag::IS_RESPONSE, &payload)?;
        }
    }

    fn gesture(shared: &Shared, uid: u32, token: proto::Token, p: u8) -> Result<(), Reply> {
        let r = shared.gate.lock().unwrap().consume(Consume { token, presenter_uid: uid, purpose: p });
        match r {
            ConsumeResult::Ok { .. } => Ok(()),
            ConsumeResult::Refused { reason } => Err(Reply::Refused { reason: refusal::NO_GESTURE, detail: reason }),
        }
    }

    fn handle(conn: &mut Connection, uid: u32, shared: &Shared, m: &Message) -> Reply {
        let now = Instant::now();
        let reply = handle_inner(conn, uid, shared, m, now);
        let outcome = match &reply {
            Reply::Refused { reason, .. } => *reason,
            _ => 0,
        };
        if m.op != op::OFFERS {
            shared.store.lock().unwrap().record(uid, m.op, outcome, now);
        }
        reply
    }

    /// The bytes of `formats`, all of which must have been uploaded on
    /// this connection (no naming content by hash, aqueduct.md §6.6),
    /// checked against the limits.
    fn uploaded(conn: &mut Connection, formats: &[Format]) -> Result<Vec<(String, Vec<u8>)>, Reply> {
        let mut out = Vec::with_capacity(formats.len());
        for f in formats {
            match conn.cache_get(&f.hash) {
                Some(b) => out.push((f.mime.clone(), b)),
                None => return Err(Reply::Refused { reason: refusal::NOT_UPLOADED, detail: 0 }),
            }
        }
        let sizes: Vec<(String, usize)> = out.iter().map(|(m, b)| (m.clone(), b.len())).collect();
        Store::validate(&sizes).map_err(|r| Reply::Refused { reason: r, detail: 0 })?;
        Ok(out)
    }

    fn malformed() -> Reply { Reply::Refused { reason: refusal::MALFORMED, detail: 0 } }

    fn handle_inner(conn: &mut Connection, uid: u32, shared: &Shared, m: &Message, now: Instant) -> Reply {
        match m.op {
            op::SET => {
                let Ok(req) = postcard::from_bytes::<proto::Set>(&m.payload) else { return malformed() };
                let formats = match uploaded(conn, &req.formats) {
                    Ok(f) => f,
                    Err(r) => return r,
                };
                if req.sensitive && !formats.iter().any(|(m, _)| m == "text/plain") {
                    // A secret is kept as text only; nothing would be left.
                    return Reply::Refused { reason: refusal::NO_FORMATS, detail: 0 };
                }
                if let Err(r) = gesture(shared, uid, req.token, purpose::COPY) {
                    return r;
                }
                shared.store.lock().unwrap().set(uid, formats, req.sensitive, now);
                Reply::Ok
            }
            op::OFFERS => {
                let mut s = shared.store.lock().unwrap();
                let formats = s.offers(now);
                Reply::Offers { formats, generation: s.generation }
            }
            op::GET => {
                let Ok(req) = postcard::from_bytes::<proto::Get>(&m.payload) else { return malformed() };
                if req.accept.len() > store::MAX_FORMATS || req.accept.iter().any(|a| !store::valid_mime(a)) {
                    return malformed();
                }
                if let Err(r) = gesture(shared, uid, req.token, purpose::PASTE) {
                    return r;
                }
                let got = shared.store.lock().unwrap().get(&req.accept, now);
                match got {
                    None => Reply::Empty,
                    Some((mime, bytes)) => match conn.upload_blob(&bytes) {
                        Ok(hash) => Reply::Got { format: Format { mime, hash } },
                        Err(_) => malformed(),
                    },
                }
            }
            op::DRAG_BEGIN => {
                let Ok(req) = postcard::from_bytes::<proto::DragBegin>(&m.payload) else { return malformed() };
                let formats = match uploaded(conn, &req.formats) {
                    Ok(f) => f,
                    Err(r) => return r,
                };
                if let Err(r) = gesture(shared, uid, req.token, purpose::DRAG) {
                    return r;
                }
                let drag_id = shared.store.lock().unwrap().drag_begin(uid, formats, now);
                Reply::Dragging { drag_id }
            }
            op::DROP_GET => {
                let Ok(req) = postcard::from_bytes::<proto::DropGet>(&m.payload) else { return malformed() };
                if req.accept.len() > store::MAX_FORMATS || req.accept.iter().any(|a| !store::valid_mime(a)) {
                    return malformed();
                }
                // frescod bound the token to this window's uid and to the
                // drag that was dropped there: the target cannot name
                // another drag.
                let r = shared.gate.lock().unwrap().consume(Consume { token: req.token, presenter_uid: uid, purpose: purpose::DROP });
                let drag_id = match r {
                    ConsumeResult::Ok { drag_id, .. } => drag_id,
                    ConsumeResult::Refused { reason } => return Reply::Refused { reason: refusal::NO_GESTURE, detail: reason },
                };
                let got = shared.store.lock().unwrap().drop_take(drag_id, &req.accept, now);
                match got {
                    Err(r) => Reply::Refused { reason: r, detail: 0 },
                    Ok(None) => Reply::Empty,
                    Ok(Some((mime, bytes))) => match conn.upload_blob(&bytes) {
                        Ok(hash) => Reply::Got { format: Format { mime, hash } },
                        Err(_) => malformed(),
                    },
                }
            }
            op::DRAG_CANCEL => {
                let Ok(req) = postcard::from_bytes::<proto::DragCancel>(&m.payload) else { return malformed() };
                if shared.store.lock().unwrap().drag_cancel(uid, req.drag_id) {
                    Reply::Ok
                } else {
                    Reply::Refused { reason: refusal::NO_DRAG, detail: 0 }
                }
            }
            op::CLEAR => {
                let Ok(req) = postcard::from_bytes::<proto::Clear>(&m.payload) else { return malformed() };
                if let Err(r) = gesture(shared, uid, req.token, purpose::COPY) {
                    return r;
                }
                shared.store.lock().unwrap().clear();
                Reply::Ok
            }
            _ => malformed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::store::*;
    use std::time::{Duration, Instant};

    #[test]
    fn mime_names_are_labels_only() {
        assert!(valid_mime("text/plain"));
        assert!(valid_mime("image/svg+xml"));
        assert!(valid_mime("application/vnd.atrium.offer-1"));
        for bad in ["text", "/plain", "text/", "Text/plain", "text/plain; charset=utf-8", "text/pl ain", "a/b/c\0"] {
            assert!(!valid_mime(bad), "{bad:?}");
        }
        assert!(!valid_mime(&format!("text/{}", "a".repeat(200))));
    }

    #[test]
    fn limits() {
        let f = |m: &str, n| (m.to_string(), n);
        assert_eq!(Store::validate(&[]), Err(crate::proto::refusal::NO_FORMATS));
        assert_eq!(Store::validate(&[f("text/plain", 1), f("text/plain", 1)]), Err(crate::proto::refusal::DUPLICATE_MIME));
        assert_eq!(Store::validate(&[f("text/plain", MAX_FORMAT_BYTES + 1)]), Err(crate::proto::refusal::TOO_BIG));
        let many: Vec<_> = (0..17).map(|i| f(&format!("text/x{i}"), 1)).collect();
        assert_eq!(Store::validate(&many), Err(crate::proto::refusal::TOO_MANY_FORMATS));
        let big: Vec<_> = (0..5).map(|i| f(&format!("image/x{i}"), MAX_FORMAT_BYTES)).collect();
        assert_eq!(Store::validate(&big), Err(crate::proto::refusal::TOO_BIG));
    }

    #[test]
    fn a_secret_is_text_only_and_expires() {
        let mut s = Store::new();
        let t0 = Instant::now();
        s.set(7, vec![("text/html".into(), b"<b>x</b>".to_vec()), ("text/plain".into(), b"x".to_vec())], true, t0);
        assert_eq!(s.offers(t0), vec!["text/plain".to_string()]);
        assert!(s.get(&["text/html".into()], t0).is_none());
        let later = t0 + Duration::from_secs(SENSITIVE_SECS);
        assert!(s.offers(later).is_empty());
    }

    #[test]
    fn get_takes_the_first_accepted_format() {
        let mut s = Store::new();
        let t = Instant::now();
        s.set(7, vec![("text/plain".into(), b"a".to_vec()), ("text/html".into(), b"<i>a</i>".to_vec())], false, t);
        let (m, _) = s.get(&["image/png".into(), "text/html".into(), "text/plain".into()], t).unwrap();
        assert_eq!(m, "text/html");
    }
}
