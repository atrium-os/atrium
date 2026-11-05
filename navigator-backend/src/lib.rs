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
pub mod carried;
pub mod document;
pub mod image;
pub mod ingest;
pub mod jailed;
pub mod limits;
pub mod links;
pub mod navigatord;
pub mod progressive;
pub mod resample;
pub mod reverse;
pub mod session;
pub mod store;
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
    /// Whether a reader can be offered this step: anchored to the published
    /// document, and recorded whole.
    ///
    /// ★ The broker used to offer every anchored trigger and then refuse the
    /// incomplete ones when clicked — doc13's sidebar toggle, whose probe
    /// dropped 82 effects, was a control that could only ever say no. Replay
    /// already refuses an incomplete step (`ApplyError::Incomplete`), so the
    /// offer has to agree with it.
    pub fn is_offerable(&self) -> bool {
        self.anchored && !self.effects.iter().any(|e| matches!(e, Effect::Truncated { .. }))
    }

    pub fn is_attribute_only(&self) -> bool {
        !self.effects.is_empty()
            && self.effects.iter().all(|e| matches!(e, Effect::Attribute { .. }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// `at` is where among the element's attributes a value the element does
    /// not yet hold is put. Recordings never carry it (`None`: append, which
    /// is what `setAttribute` does); an UNDO restoring a removed attribute
    /// does, so going back restores the bytes and not merely the meaning.
    Attribute { target: String, name: String, from: Option<String>, to: Option<String>, at: Option<usize> },
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
    /// Template instances not expanded: past `max_transitions`, or past the
    /// bytes a flat recording could have carried (`max_total_bytes`).
    TemplateInstancesTruncated { kept: usize, discarded: usize },
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
/// ★ What the converter writes now: `/2` plus `stylesheets` and `images`,
/// the inputs the NORMALIZER needs and cannot fetch for itself. The backend
/// replays transitions and needs neither, so a `/3` recording is read
/// exactly as a `/2` one and the two fields are left alone (they still count
/// against `max_total_bytes`). Refusing it meant every fresh recording was
/// "unknown format" the moment it reached the jailed lane.
pub const FORMAT_V3: &str = "atrium-navigator-recording/3";
/// ★ `/3` plus `templates` (legacy-web §5.4.5.6c): one transition recorded
/// once over the sibling elements that share it. They are EXPANDED AT INGEST
/// into ordinary transitions, so everything after this stage (apply,
/// inversion, sessions, restore) sees the same vocabulary it always has and
/// a template is never a second, less examined path. The converter writes
/// `/4` only when a recording has templates; a `/3` recording that carries
/// a `templates` field is refused, since a `/3` reader would have replayed a
/// subset of it with nothing to say so.
pub const FORMAT_V4: &str = "atrium-navigator-recording/4";

/// ★ A transition table delivered APART from its document (legacy-web
/// §5.4.5.7): exploration's output, which arrives after the reader already
/// has the readable recording. Bound to that recording by `readable`, the
/// BLAKE3 of its bytes; `ingest_transitions` reads it.
pub const TRANSITIONS_FORMAT: &str = "atrium-navigator-transitions/1";
/// ★ The first display's images, delivered after it (legacy-web §5.4.5.7,
/// the image-free first display); see `progressive::FirstImages`.
pub const IMAGES_FORMAT: &str = "atrium-navigator-images/1";
