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
use crate::reverse::{Back, Forward, History, HistoryLimits};
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
    /// ★ The worker holding this session died — crashed, hung past its
    /// deadline, or sent something the host could not parse. Distinct from
    /// `Expired`, which is the system reclaiming an abandoned session on
    /// purpose: one is an offer to reopen, the other is a fault a reader
    /// should be told about and an operator should see.
    Failed,
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
    /// `replace` named a session that is not open.
    NoSuchSession,
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
            OpenError::NoSuchSession => write!(f, "no such session"),
        }
    }
}

/// Why a saved session could not be brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    /// The recording itself would not open now (bounds, profile).
    Open(OpenError),
    /// The record names a transition the recording does not have.
    NoSuchStep { index: usize },
    /// A step that applied when it was taken does not apply on replay.
    Replay { at: usize, why: ApplyError },
    /// The replay reached a different document than the one saved.
    Diverged,
    TrailTooLong,
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RestoreError::Open(e) => write!(f, "{e}"),
            RestoreError::NoSuchStep { index } => write!(f, "the saved session names step {index}, which its recording does not have"),
            RestoreError::Replay { at, why } => write!(f, "step {at} of the saved session no longer applies: {why}"),
            RestoreError::Diverged => write!(f, "replaying the saved session reached a different document than was saved"),
            RestoreError::TrailTooLong => write!(f, "more than {MAX_TRAIL} saved steps"),
        }
    }
}

pub type SessionId = u64;

fn effect_bytes(e: &crate::Effect) -> usize {
    use crate::Effect::*;
    match e {
        Attribute { target, name, from, to, .. } => target.len() + name.len()
            + from.as_ref().map_or(0, String::len) + to.as_ref().map_or(0, String::len),
        Insert { parent, html, .. } => parent.len() + html.len(),
        Remove { target } => target.len(),
        RemoveRange { parent, .. } => parent.len(),
        Truncated { .. } => 0,
    }
}

/// Each offerable trigger's first transition (see `Session::by_trigger`).
fn index_triggers(ts: &[Transition]) -> std::collections::HashMap<String, usize> {
    let mut m = std::collections::HashMap::new();
    for (i, t) in ts.iter().enumerate() {
        if t.is_offerable() { m.entry(t.trigger.clone()).or_insert(i); }
    }
    m
}

fn transitions_bytes(ts: &[Transition]) -> usize {
    ts.iter().map(|t| t.trigger.len() + t.event.len() + t.effects.iter().map(effect_bytes).sum::<usize>()).sum()
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
    /// ★ Each offerable trigger's FIRST transition, by trigger (replay part
    /// 1b): a click is a hash lookup, not a scan of the table with a string
    /// compare per entry. Built whenever `transitions` is set.
    by_trigger: std::collections::HashMap<String, usize>,
    /// ★ HOW TO GET BACK HERE: the steps taken from the published document,
    /// as indexes into `transitions`. A session's restorable state is this
    /// and the recording; the document is derived by replaying it, and
    /// checked against a digest (see `restore`). An error once a step was taken
    /// that is not in the table, or the trail outgrew `MAX_TRAIL` — such a
    /// session still works, it just cannot be saved, and says so.
    trail: Result<Vec<usize>, NotRestorable>,
    /// The steps backed out of, as indexes; the top is the next forward.
    ahead: Vec<usize>,
}

/// The longest trail a session will save. 4,096 steps is 64 times the
/// default history and a few tens of KB encoded; past it, saving is refused
/// rather than the record growing with every click for the life of a tab.
pub const MAX_TRAIL: usize = 4096;

/// Why a session cannot be saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotRestorable {
    /// A step was taken that is not in the recording's transition table, so
    /// replaying the recording cannot reproduce it.
    UnrecordedStep,
    /// More than `MAX_TRAIL` steps from the published document.
    TrailTooLong,
}

impl std::fmt::Display for NotRestorable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotRestorable::UnrecordedStep => write!(f, "a step this session took is not in its recording"),
            NotRestorable::TrailTooLong => write!(f, "more than {MAX_TRAIL} steps from the published document"),
        }
    }
}

/// What a session needs to be restored, beside its recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub url: String,
    /// The recording's content address, filled in by whoever holds its bytes.
    pub recording: String,
    pub steps: Vec<usize>,
    pub ahead: Vec<usize>,
    /// BLAKE3 of the reader's document as serialized when saved.
    pub document: String,
}

/// ★ A session's own half of a saved record: everything but where its
/// recording lives. The WORKER can produce and replay this; only the broker,
/// which holds the store, knows the recording's address. Splitting it is
/// what lets a worker with no capabilities take part in save and restore.
#[derive(Debug, Clone, PartialEq)]
pub struct Trail {
    pub url: String,
    pub steps: Vec<usize>,
    pub ahead: Vec<usize>,
    pub document: String,
}

impl Trail {
    pub fn into_saved(self, recording: String) -> Saved {
        Saved { url: self.url, recording, steps: self.steps, ahead: self.ahead, document: self.document }
    }
}

impl Saved {
    pub fn trail(&self) -> Trail {
        Trail { url: self.url.clone(), steps: self.steps.clone(), ahead: self.ahead.clone(), document: self.document.clone() }
    }
}

/// The digest a saved session is checked against.
pub fn digest(bytes: &[u8]) -> String { blake3::hash(bytes).to_hex().to_string() }

impl Session {
    pub fn document(&self) -> &Document { self.history.document() }

    pub fn go(&mut self, t: &Transition) -> Result<(), ApplyError> {
        // The recorded transition this is, if any: by its trigger first, the
        // scan only when that is not it (a trigger with several entries).
        let index = self.by_trigger.get(&t.trigger).copied().filter(|&i| &self.transitions[i] == t)
            .or_else(|| self.transitions.iter().position(|x| x == t));
        self.history.go(t)?;
        self.ahead.clear();
        match index { Some(i) => self.push_trail(i), None => self.trail = Err(NotRestorable::UnrecordedStep) }
        self.remeasure();
        Ok(())
    }

    fn go_index(&mut self, i: usize) -> Result<(), ApplyError> {
        // Borrowed, not cloned: a click copied every string of its transition.
        self.history.go(&self.transitions[i])?;
        self.ahead.clear();
        self.push_trail(i);
        self.remeasure();
        Ok(())
    }

    fn push_trail(&mut self, i: usize) {
        if let Ok(trail) = &mut self.trail {
            trail.push(i);
            if trail.len() > MAX_TRAIL { self.trail = Err(NotRestorable::TrailTooLong) }
        }
    }

    pub fn back(&mut self) -> Result<Back, ApplyError> {
        let r = self.history.back()?;
        if r == Back::Stepped {
            if let Some(i) = self.trail.as_mut().ok().and_then(Vec::pop) { self.ahead.push(i) }
            self.remeasure()
        }
        Ok(r)
    }

    pub fn forward(&mut self) -> Result<Forward, ApplyError> {
        let r = self.history.forward()?;
        if r == Forward::Stepped {
            if let Some(i) = self.ahead.pop() { self.push_trail(i) }
            self.remeasure()
        }
        Ok(r)
    }

    /// This session's half of a saved record (see `Trail`).
    pub fn trail(&self) -> Result<Trail, NotRestorable> {
        Ok(Trail {
            url: self.url.clone(), steps: self.trail.clone()?, ahead: self.ahead.clone(),
            document: digest(self.history.document().dom.serialize().as_bytes()),
        })
    }

    /// ★ Replay a trail onto this FRESHLY OPENED session: the steps, then
    /// the steps backed out of, taken and backed out of again, and the
    /// result checked against the digest. Errors leave the session in an
    /// unspecified state; the caller closes it.
    pub fn replay(&mut self, t: &Trail) -> Result<(), RestoreError> {
        if t.steps.len() + t.ahead.len() > MAX_TRAIL { return Err(RestoreError::TrailTooLong) }
        let n = self.transitions.len();
        if let Some(&bad) = t.steps.iter().chain(&t.ahead).find(|&&i| i >= n || !self.transitions[i].anchored) {
            return Err(RestoreError::NoSuchStep { index: bad });
        }
        for (at, &i) in t.steps.iter().chain(t.ahead.iter().rev()).enumerate() {
            self.go_index(i).map_err(|why| RestoreError::Replay { at, why })?;
        }
        for _ in 0..t.ahead.len() {
            match self.back() {
                Ok(Back::Stepped) => {}
                _ => return Err(RestoreError::Diverged),
            }
        }
        if digest(self.document().dom.serialize().as_bytes()) != t.document { return Err(RestoreError::Diverged) }
        Ok(())
    }

    /// What it takes to bring this session back, given its recording's
    /// address.
    pub fn saved(&self, recording: &str) -> Result<Saved, NotRestorable> {
        let steps = self.trail.clone()?;
        Ok(Saved {
            url: self.url.clone(), recording: recording.to_string(), steps,
            ahead: self.ahead.clone(),
            document: digest(self.history.document().dom.serialize().as_bytes()),
        })
    }

    /// How many states the reader can go forward to.
    pub fn ahead(&self) -> usize { self.history.ahead() }

    fn remeasure(&mut self) {
        // ★ KEPT, not re-measured: serializing the whole document to take
        // its length was ~99% of a click (830 µs of an 830 µs step; the
        // transition itself is under a microsecond), and even counting
        // without building the string was 565 µs. The document keeps its
        // exact length by what each step changes (`Document::bytes`).
        self.current_bytes = self.history.document().serialized_bytes();
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

    /// ★ Give a session the transition table that arrived after its
    /// document (legacy-web §5.4.5.7: exploration off the reader's path).
    /// Only onto a session that has none and has taken no step: a trail
    /// indexes the table, and a table swapped under a trail would make it
    /// name other transitions.
    pub fn attach_transitions(&mut self, transitions: Vec<Transition>) -> Result<(), String> {
        if !self.transitions.is_empty() { return Err("this session already has its transition table".into()) }
        if self.depth() > 0 || !self.ahead.is_empty() || self.trail.as_ref().map_or(true, |t| !t.is_empty()) {
            return Err("this session has already taken a step".into());
        }
        self.transitions_bytes = transitions_bytes(&transitions);
        self.by_trigger = index_triggers(&transitions);
        self.transitions = transitions;
        Ok(())
    }

    /// Whether the reader has changed the published document: a step taken
    /// (or one backed out of, which is still a history).
    pub fn at_published(&self) -> bool {
        self.depth() == 0 && self.ahead.is_empty() && self.trail.as_ref().is_ok_and(|t| t.is_empty())
    }

    /// The transition a trigger names, if this recording has one.
    ///
    /// ★ OFFERABLE ONLY (anchored and complete). An unanchored transition addresses a node the page's
    /// own scripts created, which is not in the published document — the
    /// converter reports them so the loss is visible, and offering one to a
    /// reader would produce a refusal at apply time for a reason the reader
    /// could do nothing about.
    pub fn transition_for(&self, trigger: &str) -> Option<&Transition> {
        self.by_trigger.get(trigger).map(|&i| &self.transitions[i])
    }

    /// The state key of the current document (`History::key`).
    pub fn state_key(&self) -> u64 { self.history.key() }

    /// The transitions a click can take from here: each offerable trigger's,
    /// in the table's order, with the state key it leads to.
    pub fn next_states(&self) -> Vec<(&Transition, u64)> {
        let mut ix: Vec<usize> = self.by_trigger.values().copied().collect();
        ix.sort_unstable();
        ix.into_iter().map(|i| (&self.transitions[i], crate::reverse::step_key(self.history.key(), &self.transitions[i]))).collect()
    }

    /// Take the transition a trigger names.
    pub fn navigate(&mut self, trigger: &str) -> Result<(), NavigateError> {
        let i = *self.by_trigger.get(trigger)
            .ok_or_else(|| NavigateError::NoSuchTrigger { trigger: trigger.to_string() })?;
        self.go_index(i).map_err(NavigateError::Refused)
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

        let id = self.next;
        let session = self.build(r, doc, now, 0)?;
        self.next += 1;
        self.open.insert(id, session);
        Ok(id)
    }

    /// ★ REPLACE A SESSION'S DOCUMENT IN PLACE (legacy-web §5.4.5.7): the
    /// scripted conversion arrived for the document a reader is already
    /// reading at tier 1. Same id, same age; a new document, history and
    /// table. Checked as `open` checks — the profile, then the budget, with
    /// the session's own current charge given back — and refused while the
    /// reader has changed the published document, whose state would be lost.
    pub fn replace(&mut self, id: SessionId, r: &Recording, now: Millis) -> Result<(), OpenError> {
        let doc = Document::accept(&r.document).map_err(OpenError::OutsideProfile)?;
        let (old_bytes, opened_at) = match self.open.get(&id) {
            Some(s) => (s.bytes(), s.opened_at),
            None => return Err(OpenError::NoSuchSession),
        };
        let mut session = self.build(r, doc, now, old_bytes)?;
        session.opened_at = opened_at;
        self.open.insert(id, session);
        Ok(())
    }

    /// A session for `r`, charged against the budget less `give_back`.
    fn build(&self, r: &Recording, doc: Document, now: Millis, give_back: usize) -> Result<Session, OpenError> {
        // Charged at twice the document: the published copy and the reader's.
        let transitions_bytes = transitions_bytes(&r.transitions);
        let needed = r.document.len() * 2 + transitions_bytes;
        let used = self.bytes().saturating_sub(give_back);
        let available = self.limits.max_total_bytes.saturating_sub(used);
        if needed > available {
            return Err(OpenError::WouldExceedMemory { needed, available });
        }
        Ok(Session {
            url: r.url.clone(),
            base_bytes: r.document.len(),
            // The reader's document starts as the published one. Its
            // serialization can differ in length from the recorded bytes —
            // the parser normalizes — so it is measured, not assumed.
            current_bytes: doc.dom.serialize().len(),
            by_trigger: index_triggers(&r.transitions),
            transitions: r.transitions.clone(),
            transitions_bytes,
            history: History::with_limits(doc, self.limits.history),
            opened_at: now,
            last_seen: now,
            trail: Ok(vec![]),
            ahead: vec![],
        })
    }

    /// ★ RESTORE BY REPLAY, CHECKED BY DIGEST. The saved record is untrusted
    /// — it comes back from a store, and the store does not vouch for it — so
    /// nothing in it is believed: the recording is opened through `open`
    /// (profile and budget checks included), every step goes through the
    /// ordinary apply path with its preconditions, and the document the
    /// replay reaches must hash to the one saved. A record that names a step
    /// out of range, a step that no longer applies, or a different end state
    /// is refused, and the half-built session is closed rather than handed
    /// to a reader as if it were theirs.
    ///
    /// Forward is rebuilt the way it was made: the steps backed out of are
    /// taken and then backed out of again, so `ahead` holds the same states.
    /// What is NOT reproduced exactly is how many early states a bounded
    /// history had forgotten; a replay under the same limits forgets by the
    /// same rule, which can differ from a session whose backs and steps were
    /// interleaved.
    pub fn restore(&mut self, r: &Recording, saved: &Saved, now: Millis) -> Result<SessionId, RestoreError> {
        let t = saved.trail();
        if t.steps.len() + t.ahead.len() > MAX_TRAIL { return Err(RestoreError::TrailTooLong) }
        let n = r.transitions.len();
        if let Some(&bad) = t.steps.iter().chain(&t.ahead).find(|&&i| i >= n || !r.transitions[i].anchored) {
            return Err(RestoreError::NoSuchStep { index: bad });
        }
        let id = self.open(r, now).map_err(RestoreError::Open)?;
        let result = self.open.get_mut(&id).expect("just opened").replay(&t);
        match result {
            Ok(()) => Ok(id),
            Err(e) => { self.close(id); Err(e) }
        }
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

/// The saved-session record's format name. Versioned like the recording's.
pub const SAVED_FORMAT: &str = "atrium-navigator-session/1";

/// A record past this size is refused before it is parsed. `MAX_TRAIL`
/// indexes of up to 20 digits plus the fixed fields fit with room to spare.
pub const MAX_SAVED_BYTES: usize = 128 * 1024;

/// The worker's answer to SAVE, and the broker's payload for RESTORE.
pub const TRAIL_FORMAT: &str = "atrium-navigator-trail/1";

impl Trail {
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::json!({
            "format": TRAIL_FORMAT, "url": self.url, "steps": self.steps, "ahead": self.ahead, "document": self.document,
        }).to_string().into_bytes()
    }

    /// ★ UNTRUSTED in both directions: the broker reads it from a worker
    /// that may be compromised, and the worker reads it from a store. Same
    /// rules as `Saved::from_json`.
    pub fn from_json(bytes: &[u8]) -> Result<Trail, String> {
        let (url, steps, ahead, document) = parse_record(bytes, TRAIL_FORMAT, false)?;
        Ok(Trail { url, steps, ahead, document })
    }
}

/// Shared strict parse of the two record shapes. Returns the recording
/// address too when `with_recording`.
fn parse_record(bytes: &[u8], format: &str, with_recording: bool) -> Result<(String, Vec<usize>, Vec<usize>, String), String> {
    parse_record_full(bytes, format, with_recording).map(|(u, _, s, a, d)| (u, s, a, d))
}

fn hex64(s: String, k: &str) -> Result<String, String> {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) { Ok(s) }
    else { Err(format!("{k:?} is not a BLAKE3 hex digest")) }
}

fn parse_record_full(bytes: &[u8], format: &str, with_recording: bool) -> Result<(String, String, Vec<usize>, Vec<usize>, String), String> {
    if bytes.len() > MAX_SAVED_BYTES {
        return Err(format!("saved session is {} bytes, over the {MAX_SAVED_BYTES} limit", bytes.len()));
    }
    let v: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| format!("not a saved session: {e}"))?;
    let o = v.as_object().ok_or("a saved session is a JSON object")?;
    let text = |k: &str| o.get(k).and_then(|x| x.as_str()).map(str::to_string)
        .ok_or_else(|| format!("saved session has no {k:?}"));
    if text("format")? != format { return Err(format!("not {format}")) }
    let list = |k: &str| -> Result<Vec<usize>, String> {
        let a = o.get(k).and_then(|x| x.as_array()).ok_or_else(|| format!("saved session has no {k:?}"))?;
        if a.len() > MAX_TRAIL { return Err(format!("{k:?} has more than {MAX_TRAIL} entries")) }
        a.iter().map(|x| x.as_u64().map(|n| n as usize).ok_or_else(|| format!("{k:?} holds a non-index")))
            .collect()
    };
    let recording = if with_recording { hex64(text("recording")?, "recording")? } else { String::new() };
    Ok((text("url")?, recording, list("steps")?, list("ahead")?, hex64(text("document")?, "document")?))
}

impl Saved {
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::json!({
            "format": SAVED_FORMAT, "url": self.url, "recording": self.recording,
            "steps": self.steps, "ahead": self.ahead, "document": self.document,
        }).to_string().into_bytes()
    }

    /// ★ UNTRUSTED, like a recording: bounded before parsing, every field
    /// typed, nothing defaulted. A missing field is a refusal, not a zero.
    pub fn from_json(bytes: &[u8]) -> Result<Saved, String> {
        let (url, recording, steps, ahead, document) = parse_record_full(bytes, SAVED_FORMAT, true)?;
        Ok(Saved { url, recording, steps, ahead, document })
    }
}
