//! Ingest for `atrium-navigator-recording/1`.
//!
//! ★★ THE RECORDING IS UNTRUSTED, INCLUDING OUR OWN. Backend spec §4.2 says
//! the scene graph is untrusted input; a recording is the same thing one step
//! earlier. It reaches this code from a content-addressed store that vouches
//! for the BYTES being what someone published, not for their meaning — so a
//! recording produced by a hostile converter, or corrupted, or from a future
//! version, must be refused rather than trusted.
//!
//! This stage does not build a scene graph. It answers one question — *is
//! this a recording, and is it within bounds* — and that is deliberately a
//! separate answer from *what does it render to*, because the two fail for
//! different reasons and a caller needs to tell them apart.

pub mod apply;
pub mod document;
pub mod ingest;
pub mod jailed;
pub mod limits;
pub mod navigatord;
pub mod reverse;
pub mod session;
pub mod wire;
pub use ingest::ingest;
pub use limits::Limits;

/// A recording that has passed ingest. Every field is bounded, and every
/// DERIVED field has been recomputed rather than believed.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub url: String,
    pub tier: Tier,
    pub tier_reason: String,
    pub measurements: Measurements,
    pub transitions: Vec<Transition>,
    /// The document, still as serialized HTML. Parsing it is the next stage's
    /// problem, and it is bounded here so that stage cannot be handed
    /// something unreasonable.
    pub document: String,
    /// Things worth reporting that are not grounds for refusal.
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier { One, Two }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Measurements {
    pub elements: u64,
    pub text_before: u64,
    pub text_after: u64,
    pub scripts_total: u64,
    pub scripts_failed: u64,
    pub interactive_found: u64,
    pub transitions_dropped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub trigger: String,
    pub event: String,
    pub anchored: bool,
    pub effects: Vec<Effect>,
}

impl Transition {
    /// ★ RECOMPUTED, NEVER READ. The recording carries an `attribute_only`
    /// flag; a consumer that believed it could be told a transition carries
    /// no content while it carries an Insert, and size its work accordingly.
    /// A derived field in untrusted input is a claim, not a fact.
    pub fn is_attribute_only(&self) -> bool {
        !self.effects.is_empty()
            && self.effects.iter().all(|e| matches!(e, Effect::Attribute { .. }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Attribute { target: String, name: String, from: Option<String>, to: Option<String> },
    /// ★ `index` is the position the markup occupies among the parent's
    /// children. `None` means a version 1 recording, which did not carry it —
    /// replay can only append, and says so rather than guessing a position.
    Insert { parent: String, index: Option<usize>, html: String },
    /// The inverse of an insert: take `count` nodes back out, starting at
    /// `index` under `parent`.
    ///
    /// ★★ THIS EXISTS SO THE FORMAT IS CLOSED UNDER INVERSION. Every effect
    /// kind's inverse is expressible in the same format, which is what lets an
    /// undo travel through the ordinary apply path — preconditions, profile
    /// check and all — instead of down a second, less examined one.
    RemoveRange { parent: String, index: usize, count: usize },
    Remove { target: String },
    Truncated { dropped: u64 },
}

/// Non-fatal observations. Reported rather than swallowed: a recording that
/// disagrees with itself is still usable, and is also evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The recording's own `attribute_only` disagreed with its effects.
    DerivedFieldDisagreed { trigger: String, claimed: bool, actual: bool },
    /// An effect named a kind this version does not implement; skipped.
    UnknownEffectKind { kind: String },
    /// Transitions beyond the limit were discarded at ingest.
    TransitionsTruncated { kept: usize, discarded: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    NotJson(String),
    NotAnObject,
    WrongFormat { found: String },
    MissingField(&'static str),
    BadField { field: &'static str, why: &'static str },
    TooLarge { what: &'static str, measured: usize, allowed: usize },
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reject::NotJson(e) => write!(f, "not JSON: {e}"),
            Reject::NotAnObject => write!(f, "top level is not an object"),
            Reject::WrongFormat { found } => write!(f, "unknown format: {found:?}"),
            Reject::MissingField(n) => write!(f, "missing field: {n}"),
            Reject::BadField { field, why } => write!(f, "bad field {field}: {why}"),
            Reject::TooLarge { what, measured, allowed } =>
                write!(f, "{what} too large: {measured} exceeds {allowed}"),
        }
    }
}

/// The version this backend writes and prefers.
pub const FORMAT: &str = "atrium-navigator-recording/2";
/// ★ Still accepted. Recordings live in a content-addressed store, so old
/// ones do not disappear when the producer moves on. A `/1` recording is
/// honoured exactly as far as it goes: its inserts carry no position, so they
/// append, and the consumer can tell because the field is `None` rather than
/// a plausible zero.
pub const FORMAT_V1: &str = "atrium-navigator-recording/1";
