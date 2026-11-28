//! Praeco — notifications (docs/spec/tabula.md §5).
//!
//! Apps post; Forum draws. The posting app supplies only an urgency, a
//! title and a body. Praeco decides who it is (the Portcullis launch
//! registry, from the socket's peer uid), makes the text plain and
//! bounded, rate-limits, and streams each notification to the
//! subscribers on its allowlist (Forum). Nothing an app sends is drawn
//! as markup, and nothing names an app but the registry.

pub mod reminders;

pub mod proto {
    use serde::{Deserialize, Serialize};

    pub use aqueduct::classes::CLASS_NOTIFY as CLASS;

    pub mod op {
        /// v0 (praeco-macos, libatrium `atrium_notify`): payload
        /// `u8 urgency | u16 title_len | title | u16 body_len | body`;
        /// reply `u8 status | u64 id`.
        pub const POST: u16 = 0x00;
        pub const POST_REMINDER: u16 = 0x04;
        pub const WAIT_REMINDER: u16 = 0x13;
        pub const REMINDER: u16 = 0x82;
        /// `Withdraw { id }`; an app may withdraw only its own.
        pub const WITHDRAW: u16 = 0x01;
        /// v1: a postcard [`super::Post`], with actions; reply as `POST`.
        pub const POST_ACTIONS: u16 = 0x02;
        /// Subscribers only (Forum): [`super::Act`], the person pressed an
        /// action. Reply `OK` (the app heard it) or `NOT_RUNNING` (held
        /// for [`super::HOLD_MS`] for the app to start and listen).
        pub const ACT: u16 = 0x03;
        /// Subscribers only: start the stream of `Event`s.
        pub const SUBSCRIBE: u16 = 0x10;
        /// Apps: this connection hears the app's own actions
        /// ([`super::Acted`], op `ACTED`) from here on, the held ones first.
        pub const LISTEN: u16 = 0x11;
        /// Apps: [`super::RaiseMe`], bring my window forward for the action
        /// I was just told of (within [`super::RAISE_MS`], once).
        pub const RAISE: u16 = 0x12;
        /// Pushed to subscribers.
        pub const EV: u16 = 0x80;
        /// Pushed to a listening app.
        pub const ACTED: u16 = 0x81;
    }

    pub mod status {
        pub const OK: u8 = 0;
        pub const MALFORMED: u8 = 1;
        /// The caller is not an app Portcullis launched.
        pub const NOT_AN_APP: u8 = 2;
        pub const RATE_LIMITED: u8 = 3;
        pub const NOT_YOURS: u8 = 4;
        pub const NOT_ALLOWED: u8 = 5;
        /// `ACT`: the app isn't listening; the action waits for it.
        pub const NOT_RUNNING: u8 = 6;
        /// `ACT`: frescod didn't vouch for the press.
        pub const NO_PRESS: u8 = 7;
    }

    /// At most this many actions on a notification.
    pub const ACTIONS_MAX: usize = 3;
    /// An action's id: 1-32 of `[a-z0-9._-]`, the app's own name for it.
    pub const ACTION_ID_MAX: usize = 32;
    /// An action's label, cleaned like a title.
    pub const LABEL_MAX: usize = 24;
    /// How long an action waits for its app to start and listen.
    pub const HOLD_MS: u64 = 30_000;
    /// How long after the press an app may ask for its window forward
    /// (frescod's own limit for the raise is two minutes).
    pub const RAISE_MS: u64 = 110_000;

    /// An app: raise my window now, for the action pressed on my
    /// notification `id` (a Reply that opened a compose window).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RaiseMe {
        pub id: u64,
    }

    /// A button on a notification: the app's id for it and its label.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Action {
        pub id: String,
        pub label: String,
    }

    /// v1 post (`POST_ACTIONS`).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Post {
        pub urgency: u8,
        pub title: String,
        pub body: String,
        pub actions: Vec<Action>,
    }

    /// Forum: the person pressed `action` on notification `id`. `token`
    /// is the `GRANTED` press frescod minted for the posting app's uid
    /// (tabula.md §2.4); praecod spends it as `ACTION`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Act {
        pub id: u64,
        pub action: String,
        pub token: [u8; 16],
    }

    /// What a listening app hears: its notification `id`'s `action` was
    /// pressed, by the person, in Forum.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Acted {
        pub id: u64,
        pub action: String,
    }

    /// Whether `id` is a plausible action id.
    pub fn action_id_ok(id: &str) -> bool {
        !id.is_empty() && id.len() <= ACTION_ID_MAX
            && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
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
        /// Its buttons, labels cleaned (none for a v0 post).
        pub actions: Vec<Action>,
        /// The posting app's uid, for Forum to grant a press to.
        pub uid: u32,
        /// The persona the app runs in (registry), None for Personal: an
        /// app started for an action starts there.
        pub persona: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Event {
        Posted(Notification),
        Withdrawn { id: u64 },
        /// First on a subscription by an app Portcullis launched (Forum's
        /// shelf, jailed): the person it is for. Only that person's
        /// notifications (and system services') follow.
        Person { user: String },
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
    use crate::proto::{self, op, status, Acted, Event, Notification};
    use fresco_protocol::activation::{purpose, Consume, ConsumeResult};
    use tabula::gate::Gate;
    use crate::text;

    /// Who a uid is: `(user, app id)` from the Portcullis launch registry,
    /// or `None` (not an app Portcullis launched).
    pub type Resolve = dyn Fn(u32) -> Option<(String, String)> + Send + Sync;

    /// uid → its persona (None: Personal).
    pub type PersonaOf = dyn Fn(u32) -> Option<String> + Send + Sync;

    pub struct Shared {
        resolve: Box<Resolve>,
        persona_of: Box<PersonaOf>,
        /// frescod, to spend an action's press.
        gate: Mutex<Box<dyn Gate>>,
        /// The uids allowed to subscribe (Forum's).
        subscribers_allowed: Vec<u32>,
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        next_id: u64,
        rate: RateLimit,
        history: VecDeque<(u32, Notification)>,
        /// Each with the person it is for (None: a host subscriber, all).
        subscribers: Vec<(Option<String>, mpsc::Sender<Event>)>,
        /// Apps listening for their actions, by uid.
        listeners: Vec<(u32, mpsc::Sender<Acted>)>,
        /// Actions whose app wasn't listening yet.
        held: Vec<(u32, Acted, Instant)>,
        /// (uid, notification) whose press may still raise that app's window.
        raisable: Vec<(u32, u64, Instant)>,
    }

    impl Shared {
        pub fn new(resolve: Box<Resolve>, subscribers_allowed: Vec<u32>) -> Arc<Self> {
            Arc::new(Self { resolve, persona_of: Box::new(|_| None), gate: Mutex::new(Box::new(tabula::gate::DenyAll)), subscribers_allowed, state: Mutex::new(State::default()) })
        }

        /// With `persona_of` telling each uid's persona and `gate` (frescod)
        /// vouching for action presses; without a gate every action is
        /// refused.
        pub fn with(resolve: Box<Resolve>, subscribers_allowed: Vec<u32>, persona_of: Box<PersonaOf>, gate: Box<dyn Gate>) -> Arc<Self> {
            Arc::new(Self { resolve, persona_of, gate: Mutex::new(gate), subscribers_allowed, state: Mutex::new(State::default()) })
        }

        fn broadcast(st: &mut State, ev: Event) {
            st.subscribers.retain(|(p, s)| match (&ev, p) {
                (Event::Posted(n), Some(p)) if n.user != *p && n.user != proto::SYSTEM_USER => true,
                _ => s.send(ev.clone()).is_ok(),
            });
        }

        /// Post as `uid`: the v0 payload. Returns `(status, id)`.
        pub fn post(&self, uid: u32, payload: &[u8], now: Instant) -> (u8, u64) {
            let Some((urgency, title, body)) = proto::decode_post(payload) else { return (status::MALFORMED, 0) };
            self.add(uid, urgency, title, body, Vec::new(), now)
        }

        /// Post as `uid` with actions: the v1 payload.
        pub fn post_actions(&self, uid: u32, payload: &[u8], now: Instant) -> (u8, u64) {
            let Ok(p) = postcard::from_bytes::<proto::Post>(payload) else { return (status::MALFORMED, 0) };
            if p.actions.len() > proto::ACTIONS_MAX {
                return (status::MALFORMED, 0);
            }
            let mut actions: Vec<proto::Action> = Vec::new();
            for a in p.actions {
                let label = text::clean(a.label.as_bytes(), proto::LABEL_MAX, false);
                if !proto::action_id_ok(&a.id) || label.is_empty() || actions.iter().any(|b| b.id == a.id) {
                    return (status::MALFORMED, 0);
                }
                actions.push(proto::Action { id: a.id, label });
            }
            self.add(uid, p.urgency, p.title.as_bytes(), p.body.as_bytes(), actions, now)
        }

        fn add(&self, uid: u32, urgency: u8, title: &[u8], body: &[u8], actions: Vec<proto::Action>, now: Instant) -> (u8, u64) {
            let Some((user, app)) = (self.resolve)(uid) else { return (status::NOT_AN_APP, 0) };
            // A system service has nothing to start for an action.
            if user == proto::SYSTEM_USER && !actions.is_empty() {
                return (status::NOT_ALLOWED, 0);
            }
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
            let persona = (self.persona_of)(uid);
            let n = Notification { id: st.next_id, app, user, urgency, title, body, at_ms, actions, uid, persona };
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

        /// Forum (`by`, a subscriber): the person pressed `a.action` on
        /// `a.id`. The press must be one frescod granted to the posting
        /// app's uid; it is spent here. `OK`: a listener heard it;
        /// `NOT_RUNNING`: held for the app to start.
        pub fn act(&self, by: u32, a: proto::Act, now: Instant) -> u8 {
            if !self.subscribers_allowed.contains(&by) {
                return status::NOT_ALLOWED;
            }
            let found = {
                let st = self.state.lock().unwrap();
                st.history.iter().find(|(_, n)| n.id == a.id).map(|(u, n)| (*u, n.actions.iter().any(|x| x.id == a.action), n.app.clone()))
            };
            let Some((uid, known, app)) = found else { return status::NOT_YOURS };
            if !known {
                return status::MALFORMED;
            }
            match self.gate.lock().unwrap().consume(Consume { token: a.token, presenter_uid: uid, purpose: purpose::ACTION }) {
                ConsumeResult::Ok { .. } => {}
                ConsumeResult::Refused { reason } => {
                    eprintln!("praecod: action {:?} on {} for {app}: no press (frescod: {reason})", a.action, a.id);
                    return status::NO_PRESS;
                }
            }
            let acted = Acted { id: a.id, action: a.action };
            let mut st = self.state.lock().unwrap();
            let keep = Duration::from_millis(proto::RAISE_MS);
            st.raisable.retain(|(_, _, at)| now.saturating_duration_since(*at) < keep);
            st.raisable.push((uid, acted.id, now));
            let mut heard = false;
            st.listeners.retain(|(u, tx)| *u != uid || match tx.send(acted.clone()) {
                Ok(()) => { heard = true; true }
                Err(_) => false,
            });
            eprintln!("praecod: action {:?} on {} for {app} (uid {uid}): {}", acted.action, acted.id, if heard { "delivered" } else { "held for it to start" });
            if heard {
                return status::OK;
            }
            st.held.retain(|(_, _, at)| now.saturating_duration_since(*at) < Duration::from_millis(proto::HOLD_MS));
            st.held.push((uid, acted, now));
            status::NOT_RUNNING
        }

        /// `uid` asks for its window forward, for the action on its
        /// notification `id`: once, within `RAISE_MS` of the press.
        pub fn raise(&self, uid: u32, id: u64, now: Instant) -> u8 {
            {
                let mut st = self.state.lock().unwrap();
                let keep = Duration::from_millis(proto::RAISE_MS);
                st.raisable.retain(|(_, _, at)| now.saturating_duration_since(*at) < keep);
                let Some(i) = st.raisable.iter().position(|(u, n, _)| *u == uid && *n == id) else { return status::NOT_ALLOWED };
                st.raisable.remove(i);
            }
            if self.gate.lock().unwrap().raise(uid) {
                eprintln!("praecod: raised uid {uid}'s window for its action on {id}");
                status::OK
            } else {
                eprintln!("praecod: frescod raised nothing for uid {uid} ({id})");
                status::NO_PRESS
            }
        }

        /// An app listens for its actions: those held for it first.
        fn listen(&self, uid: u32, now: Instant) -> Option<(Vec<Acted>, mpsc::Receiver<Acted>)> {
            (self.resolve)(uid)?;
            let (tx, rx) = mpsc::channel();
            let mut st = self.state.lock().unwrap();
            let hold = Duration::from_millis(proto::HOLD_MS);
            let mut held = Vec::new();
            st.held.retain(|(u, a, at)| {
                if now.saturating_duration_since(*at) >= hold {
                    return false;
                }
                if *u == uid {
                    held.push(a.clone());
                    return false;
                }
                true
            });
            st.listeners.push((uid, tx));
            Some((held, rx))
        }

        /// A subscriber's stream: the history so far, then what comes.
        fn subscribe(&self, uid: u32) -> Option<(Option<String>, Vec<Notification>, mpsc::Receiver<Event>)> {
            if !self.subscribers_allowed.contains(&uid) {
                return None;
            }
            let person = (self.resolve)(uid).map(|(u, _)| u).filter(|u| u != proto::SYSTEM_USER);
            let (tx, rx) = mpsc::channel();
            let mut st = self.state.lock().unwrap();
            st.subscribers.push((person.clone(), tx));
            let history = st.history.iter().map(|(_, n)| n.clone())
                .filter(|n| person.as_ref().is_none_or(|p| n.user == *p || n.user == proto::SYSTEM_USER)).collect();
            Some((person, history, rx))
        }
    }

    fn reply(conn: &mut Connection, o: u16, st: u8, id: u64) -> io::Result<()> {
        let mut p = Vec::with_capacity(9);
        p.push(st);
        p.extend_from_slice(&id.to_le_bytes());
        conn.send_message(proto::CLASS, o, flag::IS_RESPONSE, &p)
    }

    /// Serve one connection; `uid` is the kernel's answer for the peer.
    pub fn serve(conn: Connection, uid: u32, shared: Arc<Shared>) -> io::Result<()> {
        serve_with_reminders(conn, uid, shared, None)
    }

    pub fn serve_with_reminders(mut conn: Connection, uid: u32, shared: Arc<Shared>, reminders: Option<&crate::reminders::Mailbox>) -> io::Result<()> {
        loop {
            let m = conn.recv_message()?;
            if m.opcode_class != proto::CLASS {
                continue;
            }
            match m.op {
                op::POST_REMINDER => {
                    let due = <[u8; 8]>::try_from(m.payload.as_slice()).ok().map(u64::from_le_bytes);
                    let st = match (reminders, due) {
                        (Some(mailbox), Some(due)) if crate::reminders::fresh(due) && mailbox.binding(uid).is_some() => {
                            let mut state = shared.state.lock().map_err(|_| io::Error::other("Praeco unavailable"))?;
                            if state.rate.allow(uid, Instant::now()) {
                                state.next_id = state.next_id.checked_add(1).ok_or_else(|| io::Error::other("ID limit"))?;
                                let id = state.next_id;
                                drop(state);
                                mailbox.deliver(uid, id, due)
                            } else { status::RATE_LIMITED }
                        }
                        _ => status::NOT_ALLOWED,
                    };
                    reply(&mut conn, m.op, st, 0)?;
                }
                op::WAIT_REMINDER => {
                    let Some(mailbox) = reminders.filter(|_| m.payload.is_empty() && shared.subscribers_allowed.contains(&uid)) else {
                        return reply(&mut conn, m.op, status::NOT_ALLOWED, 0);
                    };
                    let Some((binding, rx)) = mailbox.wait(uid) else {
                        return reply(&mut conn, m.op, status::NOT_ALLOWED, 0);
                    };
                    let ready = reply(&mut conn, m.op, status::OK, 0);
                    let event = ready.as_ref().ok().and_then(|_| rx.recv_timeout(Duration::from_secs(3)).ok());
                    mailbox.finish(uid);
                    ready?;
                    let mut payload = Vec::new();
                    if let Some(n) = event.filter(|n| crate::reminders::fresh(n.at_ms) && mailbox.binding(uid).as_ref() == Some(&binding)) {
                        payload = postcard::to_stdvec(&n).map_err(io::Error::other)?;
                    }
                    return conn.send_message(proto::CLASS, op::REMINDER, flag::ASYNC_EVENT, &payload);
                }
                op::POST => {
                    let (st, id) = shared.post(uid, &m.payload, Instant::now());
                    reply(&mut conn, op::POST, st, id)?;
                }
                op::POST_ACTIONS => {
                    let (st, id) = shared.post_actions(uid, &m.payload, Instant::now());
                    reply(&mut conn, op::POST_ACTIONS, st, id)?;
                }
                op::ACT => {
                    let st = match postcard::from_bytes::<proto::Act>(&m.payload) {
                        Ok(a) => shared.act(uid, a, Instant::now()),
                        Err(_) => status::MALFORMED,
                    };
                    reply(&mut conn, op::ACT, st, 0)?;
                }
                op::LISTEN => {
                    let Some((held, rx)) = shared.listen(uid, Instant::now()) else {
                        reply(&mut conn, op::LISTEN, status::NOT_AN_APP, 0)?;
                        continue;
                    };
                    reply(&mut conn, op::LISTEN, status::OK, 0)?;
                    for a in held {
                        send_acted(&mut conn, &a)?;
                    }
                    // Watch the app's end too: once it has gone, this
                    // listener must stop counting, so the next action is
                    // held and Forum starts the app again.
                    loop {
                        loop {
                            match rx.try_recv() {
                                Ok(a) => send_acted(&mut conn, &a)?,
                                Err(mpsc::TryRecvError::Empty) => break,
                                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                            }
                        }
                        // Anything the app sends now is ignored; its end
                        // (an error) ends the listener.
                        conn.recv_message_or_timeout(Duration::from_millis(100))?;
                    }
                }
                op::RAISE => {
                    let st = match postcard::from_bytes::<proto::RaiseMe>(&m.payload) {
                        Ok(r) => shared.raise(uid, r.id, Instant::now()),
                        Err(_) => status::MALFORMED,
                    };
                    reply(&mut conn, op::RAISE, st, 0)?;
                }
                op::WITHDRAW => {
                    let st = match postcard::from_bytes::<proto::Withdraw>(&m.payload) {
                        Ok(w) => shared.withdraw(uid, w.id),
                        Err(_) => status::MALFORMED,
                    };
                    reply(&mut conn, op::WITHDRAW, st, 0)?;
                }
                op::SUBSCRIBE => {
                    let Some((person, history, rx)) = shared.subscribe(uid) else {
                        reply(&mut conn, op::SUBSCRIBE, status::NOT_ALLOWED, 0)?;
                        continue;
                    };
                    reply(&mut conn, op::SUBSCRIBE, status::OK, 0)?;
                    // From here this connection only streams.
                    if let Some(user) = person {
                        send_ev(&mut conn, &Event::Person { user })?;
                    }
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

    fn send_acted(conn: &mut Connection, a: &Acted) -> io::Result<()> {
        let p = postcard::to_stdvec(a).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        conn.send_message(proto::CLASS, op::ACTED, flag::ASYNC_EVENT, &p)
    }

    fn send_ev(conn: &mut Connection, ev: &Event) -> io::Result<()> {
        let p = postcard::to_stdvec(ev).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        conn.send_message(proto::CLASS, op::EV, flag::ASYNC_EVENT, &p)
    }
}

/// Posting, for an app (named by the launch registry) or a system service
/// (one of praecod's system posters).
pub mod client {
    use std::io;
    use std::path::Path;

    use aqueduct::{flag, Connection};

    use crate::proto::{self, op, status};

    /// Where an app finds praecod: `PRAECO_SOCKET`; else, in a jail with
    /// `notify = true`, `/atrium/sockets/notify/notify.sock` (the jail's
    /// mount of praecod's socket directory); else the host's
    /// `/var/run/atrium/praecod.sock`.
    pub fn socket() -> std::path::PathBuf {
        if let Some(p) = std::env::var_os("PRAECO_SOCKET").filter(|p| !p.is_empty()) {
            return p.into();
        }
        let jailed = std::path::Path::new("/atrium/sockets/notify/notify.sock");
        if jailed.exists() { jailed.into() } else { "/var/run/atrium/praecod.sock".into() }
    }

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

    /// Post with buttons: `actions` as `(id, label)`, at most
    /// [`proto::ACTIONS_MAX`]. A press on one reaches a [`Listener`].
    pub fn post_with_actions(sock: &Path, urgency: u8, title: &str, body: &str, actions: &[(&str, &str)]) -> io::Result<u64> {
        let post = proto::Post { urgency, title: title.into(), body: body.into(),
            actions: actions.iter().map(|(id, label)| proto::Action { id: id.to_string(), label: label.to_string() }).collect() };
        let p = postcard::to_stdvec(&post).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut c = Connection::connect(sock)?;
        c.send_message(proto::CLASS, op::POST_ACTIONS, flag::RESPONSE_EXPECTED, &p)?;
        loop {
            let m = c.recv_message()?;
            if m.opcode_class == proto::CLASS && m.op == op::POST_ACTIONS && m.flags & flag::IS_RESPONSE != 0 {
                return match (m.payload.first(), m.payload.get(1..9)) {
                    (Some(&status::OK), Some(id)) => Ok(u64::from_le_bytes(id.try_into().unwrap())),
                    (Some(&st), _) => Err(io::Error::other(format!("praecod refused the post (status {st})"))),
                    _ => Err(io::Error::new(io::ErrorKind::InvalidData, "short reply")),
                };
            }
        }
    }

    /// An app's ear for its notifications' actions. Open it when the app
    /// starts: an action pressed while it wasn't running waits
    /// [`proto::HOLD_MS`] for it.
    pub struct Listener {
        c: Connection,
    }

    impl Listener {
        pub fn connect(sock: &Path) -> io::Result<Self> {
            let mut c = Connection::connect(sock)?;
            c.send_message(proto::CLASS, op::LISTEN, flag::RESPONSE_EXPECTED, &[])?;
            loop {
                let m = c.recv_message()?;
                if m.opcode_class == proto::CLASS && m.op == op::LISTEN && m.flags & flag::IS_RESPONSE != 0 {
                    return match m.payload.first() {
                        Some(&status::OK) => Ok(Listener { c }),
                        Some(&st) => Err(io::Error::other(format!("praecod refused to listen (status {st})"))),
                        None => Err(io::Error::new(io::ErrorKind::InvalidData, "short reply")),
                    };
                }
            }
        }

        /// The next action, waiting up to `wait` (`None` on timeout).
        pub fn next(&mut self, wait: std::time::Duration) -> io::Result<Option<proto::Acted>> {
            let deadline = std::time::Instant::now() + wait;
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                let Some(m) = self.c.recv_message_or_timeout(left)? else { return Ok(None) };
                if m.opcode_class == proto::CLASS && m.op == op::ACTED {
                    return postcard::from_bytes(&m.payload).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
                }
            }
        }
    }

    /// Forum: tell praecod the person pressed `action` on `id`, with the
    /// press frescod granted to the app. The status: `OK` (heard),
    /// `NOT_RUNNING` (held; start the app), or a refusal.
    pub fn act(sock: &Path, id: u64, action: &str, token: [u8; 16]) -> io::Result<u8> {
        let p = postcard::to_stdvec(&proto::Act { id, action: action.into(), token }).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut c = Connection::connect(sock)?;
        c.send_message(proto::CLASS, op::ACT, flag::RESPONSE_EXPECTED, &p)?;
        loop {
            let m = c.recv_message()?;
            if m.opcode_class == proto::CLASS && m.op == op::ACT && m.flags & flag::IS_RESPONSE != 0 {
                return m.payload.first().copied().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "short reply"));
            }
        }
    }

    /// After an action on notification `id` (say Reply, which opened a
    /// window): ask for the app's top window to come forward. Once per
    /// press, within [`proto::RAISE_MS`]; the press is the person's.
    pub fn raise(sock: &Path, id: u64) -> io::Result<()> {
        let p = postcard::to_stdvec(&proto::RaiseMe { id }).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut c = Connection::connect(sock)?;
        c.send_message(proto::CLASS, op::RAISE, flag::RESPONSE_EXPECTED, &p)?;
        loop {
            let m = c.recv_message()?;
            if m.opcode_class == proto::CLASS && m.op == op::RAISE && m.flags & flag::IS_RESPONSE != 0 {
                return match m.payload.first() {
                    Some(&status::OK) => Ok(()),
                    Some(&st) => Err(io::Error::other(format!("praecod raised nothing (status {st})"))),
                    None => Err(io::Error::new(io::ErrorKind::InvalidData, "short reply")),
                };
            }
        }
    }

    /// Take back one's own notification `id`.
    pub fn withdraw(sock: &Path, id: u64) -> io::Result<()> {
        let mut c = Connection::connect(sock)?;
        let p = postcard::to_stdvec(&proto::Withdraw { id }).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        c.send_message(proto::CLASS, op::WITHDRAW, flag::RESPONSE_EXPECTED, &p)?;
        loop {
            let m = c.recv_message()?;
            if m.opcode_class == proto::CLASS && m.op == op::WITHDRAW && m.flags & flag::IS_RESPONSE != 0 {
                return match m.payload.first() {
                    Some(&status::OK) => Ok(()),
                    Some(&st) => Err(io::Error::other(format!("praecod refused the withdraw (status {st})"))),
                    None => Err(io::Error::new(io::ErrorKind::InvalidData, "short reply")),
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
