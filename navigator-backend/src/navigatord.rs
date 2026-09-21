//! The broker: sessions, lifetimes, budgets, and the control-plane
//! vocabulary the UI speaks.
//!
//! ★★★ THE INVARIANT THIS MODULE EXISTS TO MAKE TESTABLE (spec §1):
//!
//! > The UI process holds no authority and contains no parser.
//! > Everything that touches untrusted bytes, and everything that holds a
//! > capability, lives behind the seam.
//!
//! And its consequence for the broker (spec §2): `navigatord` holds authority
//! but NEVER PARSES. The two dangerous jobs — holding capabilities, and
//! consuming hostile input — are never in the same process.
//!
//! ★★ SO THIS MODULE NAMES NO DOCUMENT TYPE. It cannot call the parser,
//! cannot hold a `Dom`, cannot touch a recording's bytes, because it never
//! sees any of those types: everything that does lives behind
//! `DocumentHost`. That is not a stylistic preference, it is the invariant
//! made structural — you cannot forget to obey a rule the type system will
//! not let you break.
//!
//! ★ WHAT IS NOT TRUE YET, stated plainly rather than implied by silence:
//! `InProcessHost` runs in this process. There is no jail here, no pipe, no
//! separate address space. The seam is real — the broker genuinely cannot
//! reach a parser — but the ISOLATION the spec describes is not, and will not
//! be until a host implementation spawns a Portcullis jail and speaks to it
//! over a pipe. What this buys today is that the swap changes one impl and no
//! broker logic, and that the test suite below is already written against the
//! boundary rather than against the internals.

use crate::reverse::Back;
use crate::session::{Expiry, Millis, SessionId, SessionLimits, Sessions, Status};

/// Everything the broker is not allowed to do itself.
///
/// Implementations own the parser, the DOM and the untrusted bytes. The
/// broker owns ids, clocks, budgets and decisions.
pub trait DocumentHost {
    /// Take a recording's bytes and turn them into a session. The bytes are
    /// untrusted; validating them is the host's job, and its failures come
    /// back as text because the broker has no business inspecting them.
    fn open(&mut self, recording: &[u8], now: Millis) -> Result<SessionId, String>;
    fn navigate(&mut self, id: SessionId, trigger: &str, now: Millis) -> Result<(), String>;
    fn back(&mut self, id: SessionId, now: Millis) -> Result<Back, String>;
    fn close(&mut self, id: SessionId) -> bool;
    fn expire(&mut self, now: Millis) -> Vec<(SessionId, Expiry)>;
    fn status(&self, id: SessionId) -> Status;
    /// What every open session costs, for the broker's own reporting.
    fn bytes(&self) -> usize;
    fn open_count(&self) -> usize;
    /// The triggers a reader could act on. The broker passes these to the UI
    /// without understanding them.
    fn triggers(&self, id: SessionId) -> Vec<String>;
    /// Every open session and what it costs — what §4.4a's caller-chosen
    /// eviction decides from.
    fn report(&self) -> Vec<(SessionId, usize)>;
}

/// What a UI asks for. The vocabulary of spec §3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    OpenSession { recording: Vec<u8> },
    Navigate { session: SessionId, trigger: String },
    Back { session: SessionId },
    Close { session: SessionId },
    /// What is open and what it costs — a UI showing a session list, or a
    /// caller deciding what to evict under §4.4a.
    Report,
}

/// What the UI hears back.
///
/// ★ Failures are events, not errors returned from a call. A broker that
/// returned `Result` to its UI would make every caller decide how to render a
/// refusal; making them events means a refusal travels the same path as a
/// success and cannot be dropped by a caller that only checked the happy one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    SessionOpened { session: SessionId, triggers: Vec<String> },
    SceneReady { session: SessionId, triggers: Vec<String> },
    /// Went back, or could not: `AtStart` and `Forgotten` reach the UI
    /// distinctly, because a reader who ran out of remembered history has not
    /// arrived at the beginning (§4.4).
    Rewound { session: SessionId, how: Back, triggers: Vec<String> },
    Closed { session: SessionId },
    /// A session ended on its own. Unsolicited: the UI did not ask, and needs
    /// to know before a reader clicks into a session that is gone.
    Expired { session: SessionId, why: Expiry },
    /// The request was well-formed and refused. `why` is for a person.
    Blocked { session: Option<SessionId>, why: String },
    /// The session named does not exist — and whether it once did.
    NoSuchSession { session: SessionId, status: Status },
    Report { sessions: Vec<(SessionId, usize)>, total_bytes: usize },
}

/// The broker.
pub struct Navigatord<H: DocumentHost> {
    host: H,
    /// ★ The clock is a field, not a call. Same discipline as `session`: the
    /// component that owns the lifecycle owns the clock, and a test drives it
    /// rather than waiting for it.
    now: Millis,
}

impl<H: DocumentHost> Navigatord<H> {
    pub fn new(host: H) -> Self { Navigatord { host, now: 0 } }

    /// Advance the clock, expiring whatever has run out.
    ///
    /// ★ Expiry events are emitted here, unprompted, rather than discovered
    /// when a reader next clicks. A UI that learned about an expiry only by
    /// failing a navigation would show a reader a page that is no longer
    /// there and then take it away under them.
    pub fn tick(&mut self, now: Millis) -> Vec<Event> {
        self.now = self.now.max(now);
        self.host.expire(self.now).into_iter()
            .map(|(session, why)| Event::Expired { session, why })
            .collect()
    }

    pub fn now(&self) -> Millis { self.now }

    /// Handle a request at the broker's current time.
    pub fn handle(&mut self, r: Request) -> Vec<Event> {
        let now = self.now;
        self.handle_at(r, now)
    }

    /// Handle a request that arrived at `now`.
    ///
    /// ★ A REQUEST IS A MOMENT IN TIME, and saying so is what lets an expiry
    /// fall due *because* a request arrived. Without this the clock advanced
    /// only on a separate tick, so a broker whose caller never ticked would
    /// hold expired sessions forever while answering requests about them —
    /// and the test for "an expiry is delivered alongside the answer" could
    /// only be written by expiring it first, which proves nothing.
    pub fn handle_at(&mut self, r: Request, now: Millis) -> Vec<Event> {
        // Every request is a chance to reclaim what has run out — and the
        // expiries go to the UI with the answer, not instead of it.
        let mut out = self.tick(now);
        match r {
            Request::OpenSession { recording } => {
                match self.host.open(&recording, self.now) {
                    Ok(session) => {
                        let triggers = self.host.triggers(session);
                        out.push(Event::SessionOpened { session, triggers });
                    }
                    Err(why) => out.push(Event::Blocked { session: None, why }),
                }
            }
            Request::Navigate { session, trigger } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.host.navigate(session, &trigger, self.now) {
                    Ok(()) => {
                        let triggers = self.host.triggers(session);
                        out.push(Event::SceneReady { session, triggers });
                    }
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Back { session } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.host.back(session, self.now) {
                    Ok(how) => {
                        let triggers = self.host.triggers(session);
                        out.push(Event::Rewound { session, how, triggers });
                    }
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Close { session } => {
                if self.host.close(session) {
                    out.push(Event::Closed { session });
                } else {
                    out.push(Event::NoSuchSession { session, status: self.host.status(session) });
                }
            }
            Request::Report => out.push(Event::Report {
                sessions: self.host.report(),
                total_bytes: self.host.bytes(),
            }),
        }
        out
    }

    /// ★ A request naming a session that is gone answers with WHY it is gone.
    /// "Expired" is an offer to reopen; "unknown" is a bug or a stale UI, and
    /// a reader should not be shown the same message for both.
    fn absent(&self, session: SessionId) -> Option<Event> {
        match self.host.status(session) {
            Status::Open => None,
            status => Some(Event::NoSuchSession { session, status }),
        }
    }

    pub fn open_sessions(&self) -> usize { self.host.open_count() }
}

/// The host as it exists today: in this process, with no jail.
///
/// ★ Everything dangerous is here — `ingest` parses untrusted JSON,
/// `Document::accept` parses untrusted HTML — and that is the point of it
/// being a separate type. When this is replaced by a jailed worker, the
/// broker above does not change.
pub struct InProcessHost {
    sessions: Sessions,
    limits: crate::Limits,
}

impl Default for InProcessHost {
    fn default() -> Self { InProcessHost::new(SessionLimits::default(), crate::Limits::default()) }
}

impl InProcessHost {
    pub fn new(sessions: SessionLimits, limits: crate::Limits) -> Self {
        InProcessHost { sessions: Sessions::new(sessions), limits }
    }
    pub fn sessions(&self) -> &Sessions { &self.sessions }
}

impl DocumentHost for InProcessHost {
    fn open(&mut self, recording: &[u8], now: Millis) -> Result<SessionId, String> {
        let r = crate::ingest(recording, &self.limits).map_err(|e| e.to_string())?;
        self.sessions.open(&r, now).map_err(|e| e.to_string())
    }

    fn navigate(&mut self, id: SessionId, trigger: &str, now: Millis) -> Result<(), String> {
        let s = self.sessions.get_mut(id, now).ok_or_else(|| "no such session".to_string())?;
        s.navigate(trigger).map_err(|e| e.to_string())
    }

    fn back(&mut self, id: SessionId, now: Millis) -> Result<Back, String> {
        let s = self.sessions.get_mut(id, now).ok_or_else(|| "no such session".to_string())?;
        s.back().map_err(|e| e.to_string())
    }

    fn close(&mut self, id: SessionId) -> bool { self.sessions.close(id) }
    fn expire(&mut self, now: Millis) -> Vec<(SessionId, Expiry)> { self.sessions.expire(now) }
    fn status(&self, id: SessionId) -> Status { self.sessions.status(id) }
    fn bytes(&self) -> usize { self.sessions.bytes() }
    fn open_count(&self) -> usize { self.sessions.len() }

    fn report(&self) -> Vec<(SessionId, usize)> {
        self.sessions.report().into_iter().map(|(id, _url, bytes)| (id, bytes)).collect()
    }

    fn triggers(&self, id: SessionId) -> Vec<String> {
        self.sessions.get(id)
            .map(|s| s.transitions().iter().filter(|t| t.anchored)
                 .map(|t| t.trigger.clone()).collect())
            .unwrap_or_default()
    }
}
