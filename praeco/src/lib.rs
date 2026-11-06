//! Praeco — notifications (docs/spec/tabula.md §5).
//!
//! Apps post; Forum draws. The posting app supplies only an urgency, a
//! title and a body. Praeco decides who it is (the Portcullis launch
//! registry, from the socket's peer uid), makes the text plain and
//! bounded, rate-limits, and streams each notification to the
//! subscribers on its allowlist (Forum). Nothing an app sends is drawn
//! as markup, and nothing names an app but the registry.

pub mod proto {
    use serde::{Deserialize, Serialize};

    pub use aqueduct::classes::CLASS_NOTIFY as CLASS;

    pub mod op {
        /// v0 (praeco-macos, libatrium `atrium_notify`): payload
        /// `u8 urgency | u16 title_len | title | u16 body_len | body`;
        /// reply `u8 status | u64 id`.
        pub const POST: u16 = 0x00;
        /// `Withdraw { id }`; an app may withdraw only its own.
        pub const WITHDRAW: u16 = 0x01;
        /// Subscribers only: start the stream of `Event`s.
        pub const SUBSCRIBE: u16 = 0x10;
        /// Pushed to subscribers.
        pub const EV: u16 = 0x80;
    }

    pub mod status {
        pub const OK: u8 = 0;
        pub const MALFORMED: u8 = 1;
        /// The caller is not an app Portcullis launched.
        pub const NOT_AN_APP: u8 = 2;
        pub const RATE_LIMITED: u8 = 3;
        pub const NOT_YOURS: u8 = 4;
        pub const NOT_ALLOWED: u8 = 5;
    }

    pub mod urgency {
        pub const LOW: u8 = 0;
        pub const NORMAL: u8 = 1;
        pub const HIGH: u8 = 2;
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Withdraw {
        pub id: u64,
    }

    /// What a subscriber is told. `app` is the registry's app id, never
    /// text from the app.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Notification {
        pub id: u64,
        pub app: String,
        /// The user the app runs for (a subscriber shows only its own user's).
        pub user: String,
        pub urgency: u8,
        pub title: String,
        pub body: String,
        /// Unix ms.
        pub at_ms: u64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Event {
        Posted(Notification),
        Withdrawn { id: u64 },
    }

    /// `Notification::user` of a post from a system service (praecod's
    /// `PRAECO_SYSTEM_POSTERS`): shown to every user's shelf.
    pub const SYSTEM_USER: &str = "*";

    /// Encode a v0 POST payload.
    pub fn encode_post(urgency: u8, title: &str, body: &str) -> Vec<u8> {
        let mut p = Vec::with_capacity(5 + title.len() + body.len());
        p.push(urgency);
        p.extend_from_slice(&(title.len().min(u16::MAX as usize) as u16).to_le_bytes());
        p.extend_from_slice(&title.as_bytes()[..title.len().min(u16::MAX as usize)]);
        p.extend_from_slice(&(body.len().min(u16::MAX as usize) as u16).to_le_bytes());
        p.extend_from_slice(&body.as_bytes()[..body.len().min(u16::MAX as usize)]);
        p
    }

    /// Decode a v0 POST payload: `(urgency, title, body)`, bytes as sent.
    pub fn decode_post(p: &[u8]) -> Option<(u8, &[u8], &[u8])> {
        let (&urgency, rest) = p.split_first()?;
        let tl = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
        let title = rest.get(2..2 + tl)?;
        let rest = &rest[2 + tl..];
        let bl = u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize;
        let body = rest.get(2..2 + bl)?;
        if rest.len() != 2 + bl {
            return None;
        }
        Some((urgency, title, body))
    }
}

pub mod text {
    pub const TITLE_MAX: usize = 64;
    pub const BODY_MAX: usize = 512;

    /// Characters that change how text around them is shown: bidi
    /// overrides and isolates (which can make "evil.exe" read as
    /// "exe.live"), zero-width joiners and the like.
    fn is_invisible_control(c: char) -> bool {
        matches!(c,
            '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}')
    }

    /// Plain, bounded text: invalid UTF-8 replaced, control and
    /// direction-changing characters dropped (a body keeps newlines, at
    /// most three in a row collapsed), whitespace trimmed, then cut at
    /// `max` bytes on a character boundary with an ellipsis.
    pub fn clean(raw: &[u8], max: usize, keep_newlines: bool) -> String {
        let s = String::from_utf8_lossy(raw);
        let mut out = String::with_capacity(s.len().min(max + 4));
        let mut newlines = 0;
        for c in s.chars() {
            if c == '\n' && keep_newlines {
                newlines += 1;
                if newlines <= 2 {
                    out.push('\n');
                }
                continue;
            }
            if c.is_control() || is_invisible_control(c) {
                continue;
            }
            newlines = 0;
            out.push(c);
        }
        let t = out.trim();
        if t.len() <= max {
            return t.to_string();
        }
        let mut end = max.saturating_sub('…'.len_utf8());
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", t[..end].trim_end())
    }
}

pub mod policy {
    use std::collections::HashMap;
    use std::time::Instant;

    /// Per app: bursts of `BURST`, refilled at `PER_MINUTE` a minute.
    pub const BURST: f64 = 3.0;
    pub const PER_MINUTE: f64 = 5.0;
    /// Notifications kept for the history.
    pub const HISTORY: usize = 500;

    #[derive(Default)]
    pub struct RateLimit {
        buckets: HashMap<u32, (f64, Instant)>,
    }

    impl RateLimit {
        /// Whether `uid` may post now; spends one if so.
        pub fn allow(&mut self, uid: u32, now: Instant) -> bool {
            let (tokens, at) = self.buckets.entry(uid).or_insert((BURST, now));
            let refill = now.saturating_duration_since(*at).as_secs_f64() * PER_MINUTE / 60.0;
            *tokens = (*tokens + refill).min(BURST);
            *at = now;
            if *tokens >= 1.0 {
                *tokens -= 1.0;
                true
            } else {
                false
            }
        }
    }

}

pub mod server {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use aqueduct::{flag, Connection};

    use crate::policy::{RateLimit, HISTORY};
    use crate::proto::{self, op, status, Event, Notification};
    use crate::text;

    /// Who a uid is: `(user, app id)` from the Portcullis launch registry,
    /// or `None` (not an app Portcullis launched).
    pub type Resolve = dyn Fn(u32) -> Option<(String, String)> + Send + Sync;

    pub struct Shared {
        resolve: Box<Resolve>,
        /// The uids allowed to subscribe (Forum's).
        subscribers_allowed: Vec<u32>,
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        next_id: u64,
        rate: RateLimit,
        history: VecDeque<(u32, Notification)>,
        subscribers: Vec<mpsc::Sender<Event>>,
    }

    impl Shared {
        pub fn new(resolve: Box<Resolve>, subscribers_allowed: Vec<u32>) -> Arc<Self> {
            Arc::new(Self { resolve, subscribers_allowed, state: Mutex::new(State::default()) })
        }

        fn broadcast(st: &mut State, ev: Event) {
            st.subscribers.retain(|s| s.send(ev.clone()).is_ok());
        }

        /// Post as `uid`: the v0 payload. Returns `(status, id)`.
        pub fn post(&self, uid: u32, payload: &[u8], now: Instant) -> (u8, u64) {
            let Some((user, app)) = (self.resolve)(uid) else { return (status::NOT_AN_APP, 0) };
            let Some((urgency, title, body)) = proto::decode_post(payload) else { return (status::MALFORMED, 0) };
            if urgency > proto::urgency::HIGH {
                return (status::MALFORMED, 0);
            }
            let title = text::clean(title, text::TITLE_MAX, false);
            let body = text::clean(body, text::BODY_MAX, true);
            if title.is_empty() && body.is_empty() {
                return (status::MALFORMED, 0);
            }
            let mut st = self.state.lock().unwrap();
            if !st.rate.allow(uid, now) {
                return (status::RATE_LIMITED, 0);
            }
            st.next_id += 1;
            let at_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
            let n = Notification { id: st.next_id, app, user, urgency, title, body, at_ms };
            if st.history.len() == HISTORY {
                st.history.pop_front();
            }
            st.history.push_back((uid, n.clone()));
            Self::broadcast(&mut st, Event::Posted(n));
            (status::OK, st.next_id)
        }

        /// Withdraw `id`, if `uid` posted it.
        pub fn withdraw(&self, uid: u32, id: u64) -> u8 {
            let mut st = self.state.lock().unwrap();
            let Some(i) = st.history.iter().position(|(u, n)| n.id == id && *u == uid) else {
                return status::NOT_YOURS;
            };
            st.history.remove(i);
            Self::broadcast(&mut st, Event::Withdrawn { id });
            status::OK
        }

        /// A subscriber's stream: the history so far, then what comes.
        fn subscribe(&self, uid: u32) -> Option<(Vec<Notification>, mpsc::Receiver<Event>)> {
            if !self.subscribers_allowed.contains(&uid) {
                return None;
            }
            let (tx, rx) = mpsc::channel();
            let mut st = self.state.lock().unwrap();
            st.subscribers.push(tx);
            Some((st.history.iter().map(|(_, n)| n.clone()).collect(), rx))
        }
    }

    fn reply(conn: &mut Connection, o: u16, st: u8, id: u64) -> io::Result<()> {
        let mut p = Vec::with_capacity(9);
        p.push(st);
        p.extend_from_slice(&id.to_le_bytes());
        conn.send_message(proto::CLASS, o, flag::IS_RESPONSE, &p)
    }

    /// Serve one connection; `uid` is the kernel's answer for the peer.
    pub fn serve(mut conn: Connection, uid: u32, shared: Arc<Shared>) -> io::Result<()> {
        loop {
            let m = conn.recv_message()?;
            if m.opcode_class != proto::CLASS {
                continue;
            }
            match m.op {
                op::POST => {
                    let (st, id) = shared.post(uid, &m.payload, Instant::now());
                    reply(&mut conn, op::POST, st, id)?;
                }
                op::WITHDRAW => {
                    let st = match postcard::from_bytes::<proto::Withdraw>(&m.payload) {
                        Ok(w) => shared.withdraw(uid, w.id),
                        Err(_) => status::MALFORMED,
                    };
                    reply(&mut conn, op::WITHDRAW, st, 0)?;
                }
                op::SUBSCRIBE => {
                    let Some((history, rx)) = shared.subscribe(uid) else {
                        reply(&mut conn, op::SUBSCRIBE, status::NOT_ALLOWED, 0)?;
                        continue;
                    };
                    reply(&mut conn, op::SUBSCRIBE, status::OK, 0)?;
                    // From here this connection only streams.
                    for n in history {
                        send_ev(&mut conn, &Event::Posted(n))?;
                    }
                    loop {
                        match rx.recv_timeout(Duration::from_secs(30)) {
                            Ok(ev) => send_ev(&mut conn, &ev)?,
                            Err(mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                        }
                    }
                }
                _ => reply(&mut conn, m.op, status::MALFORMED, 0)?,
            }
        }
    }

    fn send_ev(conn: &mut Connection, ev: &Event) -> io::Result<()> {
        let p = postcard::to_stdvec(ev).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        conn.send_message(proto::CLASS, op::EV, flag::ASYNC_EVENT, &p)
    }
}

/// Posting, for a system service (one of praecod's system posters).
pub mod client {
    use std::io;
    use std::path::Path;

    use aqueduct::{flag, Connection};

    use crate::proto::{self, op, status};

    /// Post a notification through praecod at `sock`; its id.
    pub fn post(sock: &Path, urgency: u8, title: &str, body: &str) -> io::Result<u64> {
        let mut c = Connection::connect(sock)?;
        c.send_message(proto::CLASS, op::POST, flag::RESPONSE_EXPECTED, &proto::encode_post(urgency, title, body))?;
        loop {
            let m = c.recv_message()?;
            if m.opcode_class == proto::CLASS && m.op == op::POST && m.flags & flag::IS_RESPONSE != 0 {
                return match (m.payload.first(), m.payload.get(1..9)) {
                    (Some(&status::OK), Some(id)) => Ok(u64::from_le_bytes(id.try_into().unwrap())),
                    (Some(&st), _) => Err(io::Error::other(format!("praecod refused the post (status {st})"))),
                    _ => Err(io::Error::new(io::ErrorKind::InvalidData, "short reply")),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::text::clean;
    use std::time::{Duration, Instant};

    #[test]
    fn text_is_plain_and_bounded() {
        assert_eq!(clean(b"  Hello\x1b[31m world\x07 ", 64, false), "Hello[31m world");
        // A bidi override that would reverse "exe.txt".
        assert_eq!(clean("invoice\u{202E}txt.exe".as_bytes(), 64, false), "invoicetxt.exe");
        assert_eq!(clean("a\n\n\n\n\nb".as_bytes(), 64, true), "a\n\nb");
        assert_eq!(clean(b"a\nb", 64, false), "ab");
        let long = "é".repeat(100);
        let c = clean(long.as_bytes(), 64, false);
        assert!(c.len() <= 64 && c.ends_with('…'), "{c:?}");
        assert_eq!(clean(&[0xff, b'o', b'k'], 64, false), "\u{FFFD}ok");
    }

    #[test]
    fn rate_limit_bursts_then_refills() {
        let mut r = super::policy::RateLimit::default();
        let t = Instant::now();
        assert!(r.allow(1, t) && r.allow(1, t) && r.allow(1, t));
        assert!(!r.allow(1, t));
        assert!(r.allow(2, t), "per app");
        assert!(r.allow(1, t + Duration::from_secs(12)), "one back after 12 s");
    }
}
