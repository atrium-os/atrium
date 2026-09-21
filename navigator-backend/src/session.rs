//! Sessions, and how many of them may exist at once.
//!
//! A session is one recording being read: the published document, the
//! document as the reader has changed it, and the history that gets them
//! back. Nothing bounded how many could exist, so a caller could open them
//! until the process died — and the memory is not the caller's own, it is the
//! documents, which come from untrusted input.
//!
//! ★★ MECHANISM HERE, POLICY WITH THE CALLER. This module refuses to exceed
//! a bound and it evicts exactly what it is told to evict. It does not choose
//! WHICH session a reader should lose, because it does not know which window
//! is in front of them — that is `navigatord`'s question, and the same split
//! the converter and the tier policy already use: measure here, decide there.
//!
//! ★ AND THE REFUSAL IS DELIBERATE, NOT A PLACEHOLDER FOR AN EVICTION
//! HEURISTIC. Silently discarding a session resets a reader's place with no
//! signal they can act on: they come back to a tab and it has forgotten where
//! they were. Refusing to open a new one is visible, recoverable, and the
//! caller can respond by closing something. An automatic policy would need a
//! measurement of real reader behaviour, which does not exist yet — and
//! guessing one here would bury it where the guess is hardest to find.

use crate::apply::ApplyError;
use crate::document::Document;
use crate::reverse::{Back, History, HistoryLimits};
use crate::{Recording, Transition};
use std::collections::HashMap;

/// How much of this process the open sessions may occupy.
///
/// ★ DERIVED from the corpus: a document is a median of 97 KiB, p99 1.5 MiB,
/// max 2.0 MiB, against the profile's 8 MiB ceiling. A session holds the
/// published document and the reader's current one, so its floor is about
/// twice the document — and a *bound* has to assume the ceiling, not the
/// median, because a hostile recording is free to sit at it.
///
/// 16 sessions at the 8 MiB ceiling is 256 MiB, which is why the byte budget
/// exists as well as the count: at measured sizes those same 16 sessions cost
/// about 3 MiB, and the budget is what stops the count alone from being a
/// promise the process cannot keep.
#[derive(Debug, Clone, Copy)]
pub struct SessionLimits {
    pub max_sessions: usize,
    /// Across every open session: documents, current state, and history.
    pub max_total_bytes: usize,
    pub history: HistoryLimits,
    /// No interaction for this long and the reader has walked away.
    pub max_idle_ms: Millis,
    /// ★ A hard cap regardless of activity. Idleness alone is not enough:
    /// anything that touches a session on a timer — a poll, a keep-alive, a
    /// page that moves itself — keeps it alive forever, and "forever" is not
    /// a lifetime.
    pub max_age_ms: Millis,
    /// How many expired ids to remember, so a returning reader can be told
    /// their session EXPIRED rather than that it never existed.
    pub remember_expired: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_sessions: 16,
            max_total_bytes: 64 * 1024 * 1024,
            history: HistoryLimits::default(),
            max_idle_ms: 30 * 60 * 1000,
            max_age_ms: 8 * 60 * 60 * 1000,
            remember_expired: 64,
        }
    }
}

/// Milliseconds from any fixed origin the caller likes, required to be
/// monotonic.
///
/// ★★ THE LIBRARY NEVER READS A CLOCK. Every entry point that could expire a
/// session takes `now` from the caller. Three reasons, and the third is the
/// one that matters:
///
///   - A test that has to sleep to prove an expiry is a slow test that will
///     eventually be a flaky one; here expiry is exact and instant.
///   - The converter already had to make its clock injectable to get
///     byte-reproducible output, and a second component reaching for wall
///     time would undo that lesson locally.
///   - `navigatord` owns the session lifecycle, so it owns the clock. A
///     library that read the system clock would be making a policy decision —
///     which clock, monotonic or not, whose idea of "now" — inside a module
///     whose whole discipline is to leave those to the caller.
pub type Millis = u64;

/// Why a session is no longer open — the distinction a returning reader needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// No interaction for `max_idle_ms`.
    Idle,
    /// Open for `max_age_ms`, however active.
    Age,
}

/// ★ `Expired` AND `Unknown` ARE DIFFERENT, for the same reason `Forgotten`
/// and `AtStart` are: a reader whose session timed out should be told it
/// timed out and offered it back, not told their id is meaningless. The
/// distinction fades — only the most recent expiries are remembered — and
/// that boundary is visible rather than pretended away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Open,
    Expired(Expiry),
    /// Never existed, or expired so long ago it is no longer remembered.
    Unknown,
}

/// Why a reader's click did nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigateError {
    /// ★ The recording has no transition for this trigger. Distinct from a
    /// refusal: nothing was wrong with the document, the page simply never
    /// did anything here when the converter tried it.
    NoSuchTrigger { trigger: String },
    /// There is one, and applying it was refused.
    Refused(ApplyError),
}

impl std::fmt::Display for NavigateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NavigateError::NoSuchTrigger { trigger } =>
                write!(f, "nothing recorded for {trigger:?}"),
            NavigateError::Refused(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// The count bound. Names both numbers so a caller can say what to close.
    TooManySessions { open: usize, allowed: usize },
    /// The memory bound, which one large document can hit on its own.
    WouldExceedMemory { needed: usize, available: usize },
    /// The recording's document is outside the Document Profile.
    OutsideProfile(Vec<navigator_dom::profile::Violation>),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::TooManySessions { open, allowed } =>
                write!(f, "{open} sessions open, at the limit of {allowed}"),
            OpenError::WouldExceedMemory { needed, available } =>
                write!(f, "session needs {needed} bytes, {available} left in the budget"),
            OpenError::OutsideProfile(v) => {
                write!(f, "document is outside the profile: ")?;
                for (i, x) in v.iter().enumerate() {
                    if i > 0 { write!(f, ", ")? }
                    write!(f, "{x}")?;
                }
                Ok(())
            }
        }
    }
}

pub type SessionId = u64;

fn effect_bytes(e: &crate::Effect) -> usize {
    use crate::Effect::*;
    match e {
        Attribute { target, name, from, to } => target.len() + name.len()
            + from.as_ref().map_or(0, String::len) + to.as_ref().map_or(0, String::len),
        Insert { parent, html, .. } => parent.len() + html.len(),
        Remove { target } => target.len(),
        RemoveRange { parent, .. } => parent.len(),
        Truncated { .. } => 0,
    }
}

/// One recording being read.
pub struct Session {
    pub url: String,
    history: History,
    /// The published document's size, which does not change. The reader's
    /// current document and the history are measured as they move.
    base_bytes: usize,
    /// ★ The current document's size, recomputed ONCE PER NAVIGATION rather
    /// than on every query. Measuring it means serializing the tree, and
    /// `Sessions::bytes()` is called on every open — so computing it lazily
    /// made opening a session cost a serialization of every other session's
    /// document. A document only changes when the reader moves it, which is
    /// exactly when this is updated.
    current_bytes: usize,
    /// Fixed at open: the transition table does not change.
    transitions_bytes: usize,
    opened_at: Millis,
    last_seen: Millis,
    /// ★ THE TRANSITION TABLE STAYS WITH THE SESSION. A reader clicks an
    /// element; something has to turn that into the transition the converter
    /// recorded for it. Dropping the table at open time made a session able
    /// to hold a document and unable to do anything with it.
    transitions: Vec<Transition>,
}

impl Session {
    pub fn document(&self) -> &Document { self.history.document() }

    pub fn go(&mut self, t: &Transition) -> Result<(), ApplyError> {
        self.history.go(t)?;
        self.remeasure();
        Ok(())
    }

    pub fn back(&mut self) -> Result<Back, ApplyError> {
        let r = self.history.back()?;
        if r == Back::Stepped { self.remeasure() }
        Ok(r)
    }

    fn remeasure(&mut self) {
        self.current_bytes = self.history.document().dom.serialize().len();
    }
    pub fn depth(&self) -> usize { self.history.depth() }
    pub fn forgotten(&self) -> usize { self.history.forgotten() }

    /// What this session costs right now.
    ///
    /// ★ The current document is MEASURED, not assumed equal to the base. A
    /// session whose reader has expanded every collapsed section holds more
    /// than the recording published, and a budget that charged only for the
    /// base would drift further from the truth the longer a session lived.
    pub fn bytes(&self) -> usize {
        self.base_bytes + self.current_bytes + self.history.cost().undo_bytes
            + self.transitions_bytes
    }

    pub fn transitions(&self) -> &[Transition] { &self.transitions }

    /// The transition a trigger names, if this recording has one.
    ///
    /// ★ ANCHORED ONLY. An unanchored transition addresses a node the page's
    /// own scripts created, which is not in the published document — the
    /// converter reports them so the loss is visible, and offering one to a
    /// reader would produce a refusal at apply time for a reason the reader
    /// could do nothing about.
    pub fn transition_for(&self, trigger: &str) -> Option<&Transition> {
        self.transitions.iter().find(|t| t.anchored && t.trigger == trigger)
    }

    /// Take the transition a trigger names.
    pub fn navigate(&mut self, trigger: &str) -> Result<(), NavigateError> {
        let t = self.transition_for(trigger)
            .ok_or_else(|| NavigateError::NoSuchTrigger { trigger: trigger.to_string() })?
            .clone();
        self.go(&t).map_err(NavigateError::Refused)
    }

    pub fn opened_at(&self) -> Millis { self.opened_at }
    pub fn last_seen(&self) -> Millis { self.last_seen }

    /// Whether this session has run out, and which way.
    ///
    /// ★ `saturating_sub` on both: a caller whose clock goes backwards — a
    /// wall clock stepped by NTP, a test that rewinds — must not have every
    /// session read as infinitely old or infinitely fresh. Going backwards is
    /// treated as no time passing, which is the conservative direction: a
    /// session stays open rather than vanishing under a reader.
    pub fn expired(&self, now: Millis, limits: &SessionLimits) -> Option<Expiry> {
        if now.saturating_sub(self.opened_at) >= limits.max_age_ms { return Some(Expiry::Age) }
        if now.saturating_sub(self.last_seen) >= limits.max_idle_ms { return Some(Expiry::Idle) }
        None
    }
}

/// Every open session, and the budget they share.
pub struct Sessions {
    limits: SessionLimits,
    open: HashMap<SessionId, Session>,
    next: SessionId,
    /// Recently expired ids, oldest first, bounded by `remember_expired`.
    expired: std::collections::VecDeque<(SessionId, Expiry)>,
}

impl Default for Sessions {
    fn default() -> Self { Sessions::new(SessionLimits::default()) }
}

impl Sessions {
    pub fn new(limits: SessionLimits) -> Self {
        Sessions { limits, open: HashMap::new(), next: 1, expired: Default::default() }
    }

    pub fn limits(&self) -> &SessionLimits { &self.limits }

    /// Close every session that has run out, returning what was closed.
    ///
    /// ★ CALLER-DRIVEN, like eviction. There is no background thread here and
    /// no clock; a library that spawned one would be deciding on a runtime
    /// for its embedder. `open` sweeps before it refuses, so an abandoned
    /// session never blocks a new one on its own, but a caller that wants
    /// memory back while idle has to ask.
    pub fn expire(&mut self, now: Millis) -> Vec<(SessionId, Expiry)> {
        let limits = self.limits;
        let done: Vec<(SessionId, Expiry)> = self.open.iter()
            .filter_map(|(&id, s)| s.expired(now, &limits).map(|e| (id, e)))
            .collect();
        for &(id, why) in &done {
            self.open.remove(&id);
            self.expired.push_back((id, why));
        }
        while self.expired.len() > self.limits.remember_expired { self.expired.pop_front(); }
        done
    }

    /// Whether a session is open, expired, or unheard of.
    pub fn status(&self, id: SessionId) -> Status {
        if self.open.contains_key(&id) { return Status::Open }
        match self.expired.iter().find(|(e, _)| *e == id) {
            Some(&(_, why)) => Status::Expired(why),
            None => Status::Unknown,
        }
    }

    /// Mark a session as interacted with. A reader who is merely LOOKING at a
    /// page produces no mutation, so the caller has to say so — the library
    /// cannot see attention.
    pub fn touch(&mut self, id: SessionId, now: Millis) -> bool {
        match self.open.get_mut(&id) {
            Some(s) => { s.last_seen = s.last_seen.max(now); true }
            None => false,
        }
    }

    pub fn len(&self) -> usize { self.open.len() }
    pub fn is_empty(&self) -> bool { self.open.is_empty() }
    pub fn get(&self, id: SessionId) -> Option<&Session> { self.open.get(&id) }
    /// Mutable access, which counts as interaction and so takes the clock.
    pub fn get_mut(&mut self, id: SessionId, now: Millis) -> Option<&mut Session> {
        let s = self.open.get_mut(&id)?;
        s.last_seen = s.last_seen.max(now);
        Some(s)
    }
    pub fn ids(&self) -> impl Iterator<Item = SessionId> + '_ { self.open.keys().copied() }

    /// Total bytes held by every open session.
    pub fn bytes(&self) -> usize { self.open.values().map(Session::bytes).sum() }

    /// Open a recording, or refuse and say which bound stopped it.
    pub fn open(&mut self, r: &Recording, now: Millis) -> Result<SessionId, OpenError> {
        // ★ SWEEP BEFORE REFUSING. Turning a reader away because of a session
        // they abandoned an hour ago would be the bound working against the
        // person it protects.
        self.expire(now);
        if self.open.len() >= self.limits.max_sessions {
            return Err(OpenError::TooManySessions {
                open: self.open.len(), allowed: self.limits.max_sessions,
            });
        }
        // ★ The profile check comes BEFORE the budget check on purpose. A
        // document the renderer would refuse anyway should be refused as
        // that, not reported as a memory problem — a caller told it is out of
        // memory will close sessions to make room for something that was
        // never going to open.
        let doc = Document::accept(&r.document).map_err(OpenError::OutsideProfile)?;

        // Charged at twice the document: the published copy and the reader's.
        let transitions_bytes: usize = r.transitions.iter()
            .map(|t| t.trigger.len() + t.event.len()
                 + t.effects.iter().map(effect_bytes).sum::<usize>())
            .sum();
        let needed = r.document.len() * 2 + transitions_bytes;
        let used = self.bytes();
        let available = self.limits.max_total_bytes.saturating_sub(used);
        if needed > available {
            return Err(OpenError::WouldExceedMemory { needed, available });
        }

        let id = self.next;
        self.next += 1;
        self.open.insert(id, Session {
            url: r.url.clone(),
            base_bytes: r.document.len(),
            // The reader's document starts as the published one. Its
            // serialization can differ in length from the recorded bytes —
            // the parser normalizes — so it is measured, not assumed.
            current_bytes: doc.dom.serialize().len(),
            transitions: r.transitions.clone(),
            transitions_bytes,
            history: History::with_limits(doc, self.limits.history),
            opened_at: now,
            last_seen: now,
        });
        Ok(id)
    }

    /// Close a session. Returns whether there was one — closing twice is not
    /// an error, and a caller cleaning up after a failure should not have to
    /// know which of two paths already did it.
    pub fn close(&mut self, id: SessionId) -> bool {
        self.open.remove(&id).is_some()
    }

    /// ★ EVICTION IS THE CALLER'S DECISION, EXECUTED HERE.
    ///
    /// The caller names the sessions to drop, because only the caller knows
    /// which window a reader is looking at. This exists so the knowledge and
    /// the mechanism stay in the components that respectively have them,
    /// rather than this module inventing a least-recently-used rule out of no
    /// measurement of how people actually read.
    pub fn close_all(&mut self, ids: impl IntoIterator<Item = SessionId>) -> usize {
        ids.into_iter().filter(|&id| self.close(id)).count()
    }

    /// What a caller needs to choose: every open session, its url, and what
    /// it currently costs.
    pub fn report(&self) -> Vec<(SessionId, String, usize)> {
        let mut v: Vec<_> = self.open.iter()
            .map(|(&id, s)| (id, s.url.clone(), s.bytes()))
            .collect();
        v.sort_by_key(|(id, _, _)| *id);
        v
    }
}
