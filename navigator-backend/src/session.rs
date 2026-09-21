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
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_sessions: 16,
            max_total_bytes: 64 * 1024 * 1024,
            history: HistoryLimits::default(),
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
    }
}

/// Every open session, and the budget they share.
pub struct Sessions {
    limits: SessionLimits,
    open: HashMap<SessionId, Session>,
    next: SessionId,
}

impl Default for Sessions {
    fn default() -> Self { Sessions::new(SessionLimits::default()) }
}

impl Sessions {
    pub fn new(limits: SessionLimits) -> Self {
        Sessions { limits, open: HashMap::new(), next: 1 }
    }

    pub fn len(&self) -> usize { self.open.len() }
    pub fn is_empty(&self) -> bool { self.open.is_empty() }
    pub fn get(&self, id: SessionId) -> Option<&Session> { self.open.get(&id) }
    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut Session> { self.open.get_mut(&id) }
    pub fn ids(&self) -> impl Iterator<Item = SessionId> + '_ { self.open.keys().copied() }

    /// Total bytes held by every open session.
    pub fn bytes(&self) -> usize { self.open.values().map(Session::bytes).sum() }

    /// Open a recording, or refuse and say which bound stopped it.
    pub fn open(&mut self, r: &Recording) -> Result<SessionId, OpenError> {
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
        let needed = r.document.len() * 2;
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
            history: History::with_limits(doc, self.limits.history),
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
