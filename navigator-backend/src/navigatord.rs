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

use crate::carried::Carried;
use crate::image::{Ceilings, Pixels};
use crate::links::{decide, LinkAction, SchemeAllowlist};
use crate::reverse::{Back, Forward};
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
    fn forward(&mut self, id: SessionId, now: Millis) -> Result<Forward, String>;
    /// Save a session to the host's store, returning the saved record's
    /// address. A host with no store says so.
    fn save(&mut self, _id: SessionId) -> Result<String, String> {
        Err("this host has no store to save sessions to".into())
    }
    /// Reopen a saved session by its record's address.
    fn restore(&mut self, _saved: &str, _now: Millis) -> Result<SessionId, String> {
        Err("this host has no store to restore sessions from".into())
    }
    fn close(&mut self, id: SessionId) -> bool;
    fn expire(&mut self, now: Millis) -> Vec<(SessionId, Expiry)>;
    fn status(&self, id: SessionId) -> Status;
    /// What every open session costs, for the broker's own reporting.
    fn bytes(&self) -> usize;
    fn open_count(&self) -> usize;
    /// The triggers a reader could act on. The broker passes these to the UI
    /// without understanding them.
    fn triggers(&self, id: SessionId) -> Vec<String>;
    /// The URL of the session's document, which a relative link resolves
    /// against. It is the recording's own claim, and that is enough: it only
    /// chooses a base, and the broker decides on the URL the link RESOLVES
    /// to — a host lying about it can do no more than a page writing absolute
    /// links already could.
    fn document_url(&mut self, id: SessionId, now: Millis) -> Result<String, String>;
    /// Every open session and what it costs — what §4.4a's caller-chosen
    /// eviction decides from.
    fn report(&self) -> Vec<(SessionId, usize)>;
    /// ★ What the session's recording carries, from the bytes the BROKER
    /// was handed at open (or read from its store at restore) — never from a
    /// worker's answer (`carried`). `None` for a session that is not open —
    /// and by default, so a host that keeps nothing is refused every image
    /// and every link rather than trusted.
    fn carried(&self, _id: SessionId) -> Option<&Carried> { None }
    /// Decode one image of the session's document where the document's
    /// hostile bytes are already parsed — the worker, for a jailed host —
    /// and NEVER in the broker (threat model §2.4). `bytes` were read from
    /// the blob store by the broker and checked against `address` there; the
    /// decoder checks them again. The broker validates what comes back.
    fn decode_image(&mut self, _id: SessionId, _address: &str, _bytes: &[u8], _c: Ceilings, _now: Millis)
        -> Result<Pixels, String>
    {
        Err("this host does not decode images".into())
    }
    /// ★ The first screen of a session's document as NSG (`display`), down
    /// to `limit_px`. `initial` is the BROKER's judgement that the document
    /// is still its recording's untouched initial state; only then may
    /// `cached` (a normalized document the broker stored for that state) be
    /// used, and only then does the answer carry the normalized document
    /// for the broker to store. The broker validates what comes back.
    fn scene(&mut self, _id: SessionId, _cached: Option<&[u8]>, _initial: bool, _limit_px: u32, _now: Millis)
        -> Result<crate::display::SceneAnswer, String>
    {
        Err("this host does not lay documents out".into())
    }
    /// ★ The session's CONTENT SOURCE (pergola.md §10.4, backend §4.8): a
    /// `want` or background work, answered with the source's events and the
    /// groups it wrote as NSG. `generation` changes whenever the session's
    /// document does, and the host builds its source again then; `cached`
    /// gives the normalized document the broker stored, read only when the
    /// host builds from the untouched initial state (`initial`). The broker
    /// validates what comes back (`check_bands`).
    fn bands(&mut self, _id: SessionId, _ask: crate::display::BandsAsk, _initial: bool, _generation: u64,
             _cached: &mut dyn FnMut() -> Option<Vec<u8>>, _now: Millis) -> Result<crate::display::BandsAnswer, String>
    {
        Err("this host does not lay documents out in bands".into())
    }
    /// Whether an `<a>` or `<area>` in the session's CURRENT document — after
    /// whatever transitions the reader took — carries exactly `href`. The
    /// precise half of the link check; the broker's own bound (`carried`)
    /// does not depend on this answer. Refuses by default.
    fn links_to(&mut self, _id: SessionId, _href: &str, _now: Millis) -> Result<bool, String> {
        Err("this host cannot say what its document links to".into())
    }
    /// The scripted conversion's readable recording, for the document the
    /// session shows at tier 1 (§5.4.5.7). The host compares and holds it.
    fn upgrade(&mut self, _id: SessionId, _recording: &[u8], _now: Millis) -> Result<crate::progressive::Upgrade, String> {
        Err("this host does not take a scripted conversion after the first one".into())
    }
    /// Replace the shown document with the held one, the reader kept at
    /// `anchor`; answers where that is in the new document.
    fn swap(&mut self, _id: SessionId, _anchor: &crate::progressive::Anchor, _now: Millis) -> Result<crate::progressive::Mapped, String> {
        Err("this host does not swap documents".into())
    }
    /// A transitions artifact, delivered after its document.
    fn attach(&mut self, _id: SessionId, _transitions: &[u8], _now: Millis) -> Result<crate::progressive::Attached, String> {
        Err("this host does not take a transition table apart from its recording".into())
    }
    /// An images artifact for the first document (`progressive::FirstImages`).
    /// What the session carries afterwards is the merged recording's, from
    /// bytes the broker holds.
    fn images(&mut self, _id: SessionId, _images: &[u8], _now: Millis) -> Result<crate::progressive::ImagesAttached, String> {
        Err("this host does not take images apart from its recording".into())
    }
    /// ★ Whether each document runs CONFINED, as the host itself states it.
    /// Defaults to false: a host has to claim confinement to get it, so a new
    /// host that forgets is refused by `Navigatord::start`, not trusted.
    fn is_confined(&self) -> bool { false }
}

/// What a UI asks for. The vocabulary of spec §3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    OpenSession { recording: Vec<u8> },
    Navigate { session: SessionId, trigger: String },
    Back { session: SessionId },
    Forward { session: SessionId },
    /// Keep a session so it can be reopened later: after a restart, on
    /// another day, after it expired.
    Save { session: SessionId },
    /// Reopen a saved session by the address `Saved` answered with.
    Restore { saved: String },
    Close { session: SessionId },
    /// What is open and what it costs — a UI showing a session list, or a
    /// caller deciding what to evict under §4.4a.
    Report,
    /// The reader activated a link in the session's scene. `href` is the
    /// link's target as the scene carries it; the broker decides what it does.
    Activate { session: SessionId, href: String },
    /// Pixels for one image the session's scene names, by content address
    /// (`blake3:<hex>`). Decoded by the host, validated here (§2.4).
    ///
    /// `painted` is the size the scene paints it at: the largest tile among
    /// the scene's `image` nodes for this address, in NSG units (1/64 CSS
    /// px). The broker multiplies it by the device scale and the image is
    /// decoded to at most that (never larger than it is), and charged at
    /// that. `None` asks for the natural size. ★ It is not a grant: it can
    /// only make the reply smaller than the natural size, and the budget
    /// binds either way. A request at a new size (zoom, a scale change)
    /// decodes again and replaces the charge.
    Image { session: SessionId, address: String, painted: Option<(u32, u32)> },
    /// ★ PROGRESSIVE DISPLAY (legacy-web §5.4.5.7; `progressive`). The
    /// scripted conversion of the document this session shows at tier 1:
    /// its readable recording. Compared and held, never shown by itself.
    Upgrade { session: SessionId, recording: Vec<u8> },
    /// The UI is at an idle moment (no scroll, no input, nothing
    /// animating) and the reader is at `anchor`: swap the held document in.
    Swap { session: SessionId, anchor: crate::progressive::Anchor },
    /// The transitions artifact, whenever exploration finished.
    Attach { session: SessionId, transitions: Vec<u8> },
    /// ★ An images artifact for the first document, as its images arrive
    /// (§5.4.5.7, the image-free first display). From the converter, like
    /// `Upgrade`; never from a page.
    Images { session: SessionId, images: Vec<u8> },
    /// The first screen of a session's document, down to `limit_px` (backend
    /// §4.8, bands step 1): answered by `Event::Scene`.
    Scene { session: SessionId, limit_px: u32 },
    /// ★ A view's wish for a session's content (pergola.md §10.4): keep
    /// `range` (1/64 px, the layout at `zoom_64`) ready, `prefetch` either
    /// side. Answered by `Event::Bands`.
    Want { session: SessionId, range: (i64, i64), zoom_64: u32, prefetch: i64 },
    /// Background layout while the view is idle, up to `bands` bands;
    /// answered by `Event::Bands`.
    BandsIdle { session: SessionId, bands: u32 },
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
    /// Went forward again, or there was nothing ahead.
    Advanced { session: SessionId, how: Forward, triggers: Vec<String> },
    /// The session was saved; `saved` is the address to restore it by.
    Saved { session: SessionId, saved: String },
    Closed { session: SessionId },
    /// A session ended on its own. Unsolicited: the UI did not ask, and needs
    /// to know before a reader clicks into a session that is gone.
    Expired { session: SessionId, why: Expiry },
    /// The request was well-formed and refused. `why` is for a person.
    Blocked { session: Option<SessionId>, why: String },
    /// The session named does not exist — and whether it once did.
    NoSuchSession { session: SessionId, status: Status },
    Report { sessions: Vec<(SessionId, usize)>, total_bytes: usize },
    /// What an activated link does: scroll within the document, open a page
    /// (marked when it is `http`), or hand an allowlisted scheme to its
    /// handler. A refused link is `Blocked`, like any other refusal.
    Link { session: SessionId, action: LinkAction },
    /// An image's pixels, decoded in the document's own worker and checked
    /// here: RGBA8, straight alpha, `width × height × 4` bytes, within the
    /// profile's ceilings. This is the only form in which an image reaches
    /// the compositor — Fresco never sees encoded bytes.
    Image { session: SessionId, address: String, pixels: Pixels },
    /// The scripted document is held: swap it in at the next idle moment
    /// (`Request::Swap`). `why` says what differs, for a person.
    UpgradeReady { session: SessionId, why: String },
    /// The scripted document is not meaningfully different; the reader
    /// keeps what they have and nothing on screen changes.
    UpgradeKept { session: SessionId, why: String },
    /// The document was replaced; present the new scene at `anchor` in ONE
    /// frame (the old scene stays up until the new one is composed).
    Replaced { session: SessionId, anchor: crate::progressive::Mapped, triggers: Vec<String> },
    /// The transition table is on the shown document: these are offered.
    TransitionsAttached { session: SessionId, triggers: Vec<String> },
    /// The table belongs to the held document and goes in with the swap.
    TransitionsHeld { session: SessionId },
    /// Images of the shown document arrived. `arrived` are addresses the
    /// UI may now ask pixels for (`Request::Image`). `resized` empty: they
    /// fill boxes already laid out, and nothing moves. Otherwise the layout
    /// changes for those `src`s: the UI lays out again at an idle moment, at
    /// the reader's anchor, and presents it in one frame, as for a swap.
    /// `complete`: no image of this document is pending any more.
    ImagesAttached { session: SessionId, arrived: Vec<String>, resized: Vec<String>, complete: bool },
    /// A first screen as NSG text, checked by the broker (`check_scene`).
    /// `from_cache`: laid out from the normalized document the broker had
    /// stored for this recording, without normalizing again.
    Scene { session: SessionId, nsg: String, from_cache: bool },
    /// ★ What a session's content source did (pergola.md §10.4), checked
    /// by the broker (`check_bands`): its events (`extent`, `band_ready`,
    /// `band_updated`, `band_evicted`), the groups it wrote — each band's
    /// group clipped to its range, and the overlay — as NSG, and the groups
    /// it cleared. `from_cache` as for `Scene`.
    Bands { session: SessionId, events: Vec<navigator_render::source::Event>, puts: Vec<crate::display::GroupPut>,
            clears: Vec<navigator_render::source::GroupId>, from_cache: bool },
}

/// The broker.
pub struct Navigatord<H: DocumentHost> {
    host: H,
    /// ★ The clock is a field, not a call. Same discipline as `session`: the
    /// component that owns the lifecycle owns the clock, and a test drives it
    /// rather than waiting for it.
    now: Millis,
    /// Schemes beyond `https` and `http` a link may hand off. Empty unless
    /// the user names one (threat model §2.10).
    schemes: SchemeAllowlist,
    /// Where image bytes are read from: the converter's blob store, named by
    /// the hex of each blob's BLAKE3 address. ★ The broker's capability,
    /// never a worker's: the worker is handed only blobs its recording names.
    blobs: Option<Box<dyn crate::store::Store>>,
    /// Decoded bytes charged to each open session, per image address, so the
    /// profile's per-document total binds across requests. An address decoded
    /// again is not charged twice.
    decoded: std::collections::BTreeMap<SessionId, std::collections::BTreeMap<String, usize>>,
    /// Device pixels per CSS px, in 1/64 (64 = 1x, 128 = 2x). The display's
    /// property, so the broker's, not a page's.
    device_scale: u32,
    /// ★ Each session's page zoom, in 1/64 (backend §4.9); absent is 100%.
    /// The reader's choice, set by the UI. It multiplies the device scale
    /// for that session's images, so an image is decoded again at its new
    /// painted size after a zoom — within the same decode budget.
    zoom: std::collections::BTreeMap<SessionId, u32>,
    /// ★ NORMALIZED ONCE (`display`): each recording's normalized initial
    /// document, by the recording's BLAKE3 hex → the stored document's, in
    /// `blobs`. Written only from a session the broker knows is untouched,
    /// for the recording that session holds: a worker that lied could only
    /// change what THAT recording shows, which its author controls anyway.
    normalized: std::collections::HashMap<String, String>,
    /// Sessions whose document is no longer its recording's untouched
    /// initial state (navigated, forwarded, restored or swapped).
    touched: std::collections::BTreeSet<SessionId>,
    /// Each session's document generation: bumped whenever its document
    /// changes, so a host builds its content source again.
    generation: std::collections::BTreeMap<SessionId, u64>,
}

/// The largest first-screen scene a host may answer, bytes of NSG.
pub const MAX_SCENE_BYTES: usize = 8 << 20;
/// The largest normalized document a host may hand back to be stored.
pub const MAX_NORMALIZED_BYTES: usize = 16 << 20;

/// ★ The broker's own check of a scene answer, whatever host gave it:
/// sizes, UTF-8 (a `String` already), and the NSG header. The scene graph's
/// full validator is §4.2's (open); this bounds what is passed on.
pub fn check_scene(a: &crate::display::SceneAnswer) -> Result<(), String> {
    if a.nsg.len() > MAX_SCENE_BYTES { return Err(format!("scene is {} bytes, over {MAX_SCENE_BYTES}", a.nsg.len())) }
    if !a.nsg.starts_with("nsg 0.") { return Err("scene does not start with an NSG header".into()) }
    if a.normalized.as_ref().is_some_and(|n| n.len() > MAX_NORMALIZED_BYTES) { return Err("normalized document over its bound".into()) }
    if a.normalized.as_ref().is_some_and(|n| std::str::from_utf8(n).is_err()) { return Err("normalized document is not UTF-8".into()) }
    Ok(())
}

/// What one bands answer may carry: events, groups and NSG bytes in all.
pub const MAX_BAND_EVENTS: usize = 4096;
pub const MAX_BAND_PUTS: usize = 64;
pub const MAX_BANDS_BYTES: usize = 20 << 20;

/// ★ The broker's check of a bands answer, whatever host gave it: counts
/// and sizes within their bounds, every group NSG, every band group written
/// only for a band the same answer announced (ready or updated) with a
/// range that is a range, and the normalized document as for a scene.
pub fn check_bands(a: &crate::display::BandsAnswer) -> Result<(), String> {
    use navigator_render::source::{Event as E, GroupId};
    if a.events.len() > MAX_BAND_EVENTS || a.clears.len() > MAX_BAND_EVENTS { return Err("too many band events".into()) }
    if a.puts.len() > MAX_BAND_PUTS { return Err(format!("{} groups in one answer, over {MAX_BAND_PUTS}", a.puts.len())) }
    let total: usize = a.puts.iter().map(|p| p.nsg.len()).sum();
    if total > MAX_BANDS_BYTES { return Err(format!("{total} bytes of groups, over {MAX_BANDS_BYTES}")) }
    let mut announced = std::collections::BTreeMap::new();
    for e in &a.events {
        match e {
            E::BandReady { band, range } | E::BandUpdated { band, range } => {
                if range.1 <= range.0 { return Err(format!("band {band} has an empty range")) }
                announced.insert(*band, *range);
            }
            E::Extent { height, .. } => if *height < 0 { return Err("a negative extent".into()) },
            E::BandEvicted { .. } => {}
        }
    }
    for p in &a.puts {
        if p.nsg.len() > MAX_SCENE_BYTES { return Err(format!("a group is {} bytes, over {MAX_SCENE_BYTES}", p.nsg.len())) }
        if !p.nsg.starts_with("nsg 0.") { return Err("a group does not start with an NSG header".into()) }
        match p.group {
            GroupId::Band(b) => match announced.get(&b) {
                Some(r) if p.clip == Some(*r) => {}
                Some(_) => return Err(format!("band {b}'s group is not clipped to its range")),
                None => return Err(format!("a group for band {b}, which the answer did not announce")),
            },
            GroupId::Overlay => if p.clip.is_some() { return Err("the overlay carries no band clip".into()) },
        }
    }
    if a.normalized.as_ref().is_some_and(|n| n.len() > MAX_NORMALIZED_BYTES) { return Err("normalized document over its bound".into()) }
    if a.normalized.as_ref().is_some_and(|n| std::str::from_utf8(n).is_err()) { return Err("normalized document is not UTF-8".into()) }
    Ok(())
}

/// Page zoom bounds, in 1/64: 25% to 500%.
pub const MIN_ZOOM_64: u32 = 16;
pub const MAX_ZOOM_64: u32 = 320;

/// NSG's unit: 1/64 CSS px.
const NSG_UNIT: u64 = 64;

impl<H: DocumentHost> Navigatord<H> {
    /// Read image bytes from `blobs` (see the field).
    pub fn with_blobs(mut self, blobs: Box<dyn crate::store::Store>) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Set the device scale (1/64: 128 is 2x). Images requested with a
    /// painted size are decoded to that size times this.
    pub fn with_device_scale(mut self, scale_64: u32) -> Self {
        self.device_scale = scale_64.max(1);
        self
    }

    /// Device pixels for a painted size in NSG units, rounded up so the box
    /// is covered, at least 1 per axis.
    pub fn device_size(&self, painted: (u32, u32)) -> (u32, u32) {
        self.device_size_at(painted, NSG_UNIT as u32)
    }

    /// The same at a page zoom (1/64): device scale x zoom.
    pub fn device_size_at(&self, painted: (u32, u32), zoom_64: u32) -> (u32, u32) {
        let s = self.device_scale as u64 * zoom_64 as u64;
        let d = |v: u32| (v as u64 * s).div_ceil(NSG_UNIT * NSG_UNIT * NSG_UNIT).clamp(1, u32::MAX as u64) as u32;
        (d(painted.0), d(painted.1))
    }

    /// Set a session's page zoom (1/64), within `MIN_ZOOM_64..=MAX_ZOOM_64`.
    pub fn set_zoom(&mut self, session: SessionId, zoom_64: u32) -> Result<(), String> {
        if !(MIN_ZOOM_64..=MAX_ZOOM_64).contains(&zoom_64) {
            return Err(format!("zoom {zoom_64}/64 is outside {MIN_ZOOM_64}..={MAX_ZOOM_64}"));
        }
        if self.host.carried(session).is_none() { return Err("no such session".into()) }
        if zoom_64 == NSG_UNIT as u32 { self.zoom.remove(&session); } else { self.zoom.insert(session, zoom_64); }
        Ok(())
    }

    /// A session's page zoom (1/64).
    pub fn zoom(&self, session: SessionId) -> u32 { self.zoom.get(&session).copied().unwrap_or(NSG_UNIT as u32) }

    /// Decoded image bytes charged to a session so far.
    pub fn decoded_bytes(&self, session: SessionId) -> usize {
        self.decoded.get(&session).map(|m| m.values().sum()).unwrap_or(0)
    }

    /// Let links of `scheme` be handed to its handler. ★ The user's choice,
    /// so a page cannot make it; `javascript` is refused (see `links`).
    pub fn allow_scheme(&mut self, scheme: &str) -> Result<(), String> {
        self.schemes.allow(scheme)
    }

    /// ★★ THE PRODUCTION CONSTRUCTOR, and it refuses an unconfined host
    /// (threat model §2.3). `require_confinement()` existed, but nothing on
    /// the production path called it, so the shipped default was a worker
    /// running hostile input with the user's authority. Now there is no way
    /// to build a broker over an unconfined host outside a test build.
    pub fn start(host: H) -> Result<Self, String> {
        if !host.is_confined() {
            return Err("navigatord refuses to start: the document host is not confined \
                        (configure a launcher, e.g. `portcullis exec --daemon --instance {instance}`)".into());
        }
        Ok(Navigatord { host, now: 0, schemes: SchemeAllowlist::default(), blobs: None, decoded: Default::default(), device_scale: NSG_UNIT as u32, zoom: Default::default(), normalized: Default::default(), touched: Default::default(), generation: Default::default() })
    }

    /// An unconfined broker, for tests: in-process hosts, bare worker
    /// processes. ★ Compiled only into test builds — this crate's own tests,
    /// or a dependent that enables the `unconfined-for-testing` feature —
    /// so a shipped binary that calls it does not link.
    #[cfg(any(test, feature = "unconfined-for-testing"))]
    pub fn unconfined_for_testing(host: H) -> Self {
        Navigatord { host, now: 0, schemes: SchemeAllowlist::default(), blobs: None, decoded: Default::default(), device_scale: NSG_UNIT as u32, zoom: Default::default(), normalized: Default::default(), touched: Default::default(), generation: Default::default() }
    }

    /// The host, for an operator (or a test) inspecting it.
    pub fn host(&self) -> &H { &self.host }

    /// Advance the clock, expiring whatever has run out.
    ///
    /// ★ Expiry events are emitted here, unprompted, rather than discovered
    /// when a reader next clicks. A UI that learned about an expiry only by
    /// failing a navigation would show a reader a page that is no longer
    /// there and then take it away under them.
    pub fn tick(&mut self, now: Millis) -> Vec<Event> {
        self.now = self.now.max(now);
        let out: Vec<Event> = self.host.expire(self.now).into_iter()
            .map(|(session, why)| Event::Expired { session, why })
            .collect();
        // A session that ended for any reason — expired, closed, or a worker
        // retired on a lie — gives its image budget back.
        let host = &self.host;
        self.decoded.retain(|&id, _| host.status(id) == Status::Open);
        out
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
                        self.touched.insert(session);
                        *self.generation.entry(session).or_default() += 1;
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
                        *self.generation.entry(session).or_default() += 1;
                        let triggers = self.host.triggers(session);
                        out.push(Event::Rewound { session, how, triggers });
                    }
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Forward { session } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.host.forward(session, self.now) {
                    Ok(how) => {
                        self.touched.insert(session);
                        *self.generation.entry(session).or_default() += 1;
                        let triggers = self.host.triggers(session);
                        out.push(Event::Advanced { session, how, triggers });
                    }
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Save { session } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.host.save(session) {
                    Ok(saved) => out.push(Event::Saved { session, saved }),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            // ★ A restored session is announced exactly like an opened one:
            // to the UI it is a session it can read, with the triggers it
            // can take. Where the reader is inside it is the document's
            // business, delivered with the scene.
            Request::Restore { saved } => {
                match self.host.restore(&saved, self.now) {
                    Ok(session) => {
                        self.touched.insert(session);
                        *self.generation.entry(session).or_default() += 1;
                        let triggers = self.host.triggers(session);
                        out.push(Event::SessionOpened { session, triggers });
                    }
                    Err(why) => out.push(Event::Blocked { session: None, why }),
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
            // ★★ The link decision is the broker's, made here on every
            // activation: the href came from a scene a worker rendered from
            // hostile bytes, so nothing upstream of this line is in a
            // position to have made it (threat model §2.10).
            Request::Activate { session, href } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                if let Err(why) = self.carries_link(session, &href) {
                    out.push(Event::Blocked { session: Some(session), why });
                    return out;
                }
                let base = match self.host.document_url(session, self.now) {
                    Ok(u) => u,
                    Err(why) => { out.push(Event::Blocked { session: Some(session), why }); return out }
                };
                match decide(&base, &href, &self.schemes) {
                    Ok(action) => out.push(Event::Link { session, action }),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why: why.to_string() }),
                }
            }
            Request::Upgrade { session, recording } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                use crate::progressive::Upgrade;
                match self.host.upgrade(session, &recording, self.now) {
                    Ok(Upgrade::Ready(d)) => out.push(Event::UpgradeReady { session, why: d.why() }),
                    Ok(Upgrade::Kept(d)) => out.push(Event::UpgradeKept { session, why: d.why() }),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Swap { session, anchor } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.host.swap(session, &anchor, self.now) {
                    Ok(anchor) => {
                        self.touched.insert(session);
                        *self.generation.entry(session).or_default() += 1;
                        // A new document names its own images: what the old
                        // one's decodes cost is given back.
                        self.decoded.remove(&session);
                        let triggers = self.host.triggers(session);
                        out.push(Event::Replaced { session, anchor, triggers });
                    }
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Attach { session, transitions } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                use crate::progressive::Attached;
                match self.host.attach(session, &transitions, self.now) {
                    Ok(Attached::Now) => {
                        let triggers = self.host.triggers(session);
                        out.push(Event::TransitionsAttached { session, triggers });
                    }
                    Ok(Attached::WithSwap) => out.push(Event::TransitionsHeld { session }),
                    Ok(Attached::NeedsSwap) => out.push(Event::UpgradeReady { session,
                        why: "the interactions arrived for the scripted document (no visible change)".into() }),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Images { session, images } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.host.images(session, &images, self.now) {
                    Ok(a) => out.push(Event::ImagesAttached { session, arrived: a.arrived, resized: a.resized, complete: a.complete }),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Scene { session, limit_px } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.scene(session, limit_px) {
                    Ok(e) => out.push(e),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Want { session, range, zoom_64, prefetch } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.bands(session, crate::display::BandsAsk::Want { range, zoom_64, prefetch }) {
                    Ok(e) => out.push(e),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::BandsIdle { session, bands } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.bands(session, crate::display::BandsAsk::Idle { bands }) {
                    Ok(e) => out.push(e),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
            Request::Image { session, address, painted } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.image(session, address, painted) {
                    Ok(e) => out.push(e),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
        }
        out
    }

    /// ★★ A LINK IS ACTIVATED ONLY IF THE DOCUMENT CARRIES IT (threat model
    /// §2.10). Two checks, in this order:
    ///
    /// 1. the BOUND, the broker's own: the href is spelled, as the converter
    ///    writes a link, somewhere in the recording the broker holds
    ///    (`links::recording_spellings`, a byte search). A compromised worker
    ///    cannot pass it with a URL the recording does not already contain,
    ///    so it cannot invent a destination — `https://evil.test/?<what it
    ///    read>` — however it answers below;
    /// 2. the PRECISE check, the host's: an `<a>`/`<area>` in the CURRENT
    ///    document carries it, so a link a transition removed, or one that
    ///    appears only in a script's text, is refused by an honest worker.
    ///
    /// Bounded before either: an over-long href is refused unscanned.
    fn carries_link(&mut self, session: SessionId, href: &str) -> Result<(), String> {
        if href.len() > crate::links::MAX_HREF {
            return Err(crate::links::LinkRefusal::TooLong(href.len()).to_string());
        }
        let carried = self.host.carried(session).ok_or("no such session")?;
        if !crate::links::recording_spellings(href).iter().any(|n| carried.contains(n)) {
            return Err("the document does not carry this link".into());
        }
        match self.host.links_to(session, href, self.now)? {
            true => Ok(()),
            false => Err("the current document does not carry this link".into()),
        }
    }

    /// ★★ AN IMAGE, IN THE ORDER THAT MAKES A HOSTILE ONE CHEAP TO REFUSE.
    /// The address is checked for shape and against the recording the broker
    /// holds (so a worker cannot pull another document's blob out of a
    /// shared store), the bytes are read and hash-checked by the store, the
    /// remaining budget is sent with them, and the host's answer is validated
    /// for shape before it becomes an event.
    /// ★ A FIRST SCREEN, normalized once per recording (see `normalized`).
    fn scene(&mut self, session: SessionId, limit_px: u32) -> Result<Event, String> {
        let key = blake3::hash(self.host.carried(session).ok_or("no such session")?.bytes()).to_hex().to_string();
        let initial = !self.touched.contains(&session);
        let cached = if initial {
            match (self.normalized.get(&key), self.blobs.as_ref()) { (Some(h), Some(b)) => b.get(h).ok(), _ => None }
        } else { None };
        let a = self.host.scene(session, cached.as_deref(), initial, limit_px, self.now)?;
        check_scene(&a)?;
        if let (true, None, Some(n), Some(b)) = (initial, &cached, &a.normalized, self.blobs.as_mut()) {
            let h = b.put(n)?;
            self.normalized.insert(key, h);
        }
        Ok(Event::Scene { session, nsg: a.nsg, from_cache: cached.is_some() })
    }

    /// ★ A content-source ask (see `Request::Want`), normalized once per
    /// recording as a scene is.
    fn bands(&mut self, session: SessionId, ask: crate::display::BandsAsk) -> Result<Event, String> {
        ask.check()?;
        let key = blake3::hash(self.host.carried(session).ok_or("no such session")?.bytes()).to_hex().to_string();
        let initial = !self.touched.contains(&session);
        let generation = self.generation.get(&session).copied().unwrap_or(0);
        let mut used_cache = false;
        let a = {
            let (normalized, blobs) = (&self.normalized, &self.blobs);
            let mut cached = || -> Option<Vec<u8>> {
                if !initial { return None }
                let v = match (normalized.get(&key), blobs.as_ref()) { (Some(h), Some(b)) => b.get(h).ok(), _ => None };
                used_cache = v.is_some();
                v
            };
            self.host.bands(session, ask, initial, generation, &mut cached, self.now)?
        };
        check_bands(&a)?;
        if let (true, Some(n), Some(b)) = (initial, &a.normalized, self.blobs.as_mut()) {
            if !self.normalized.contains_key(&key) {
                let h = b.put(n)?;
                self.normalized.insert(key, h);
            }
        }
        Ok(Event::Bands { session, events: a.events, puts: a.puts, clears: a.clears, from_cache: used_cache })
    }

    fn image(&mut self, session: SessionId, address: String, painted: Option<(u32, u32)>) -> Result<Event, String> {
        if crate::image::address_hex(&address).is_none() {
            return Err(format!("{address:?} is not an image address (blake3:<64 hex>)"));
        }
        if painted.is_some_and(|(w, h)| w == 0 || h == 0) {
            return Err("an image painted with a zero dimension is not painted".into());
        }
        let carried = self.host.carried(session).ok_or("no such session")?;
        if !carried.image(&address) {
            return Err(format!("the document does not name the image {address}"));
        }
        let blobs = self.blobs.as_ref().ok_or("this broker has no blob store to read images from")?;
        let hex = crate::image::address_hex(&address).expect("checked");
        let bytes = blobs.get(hex)?;
        if bytes.len() > crate::image::MAX_ENCODED_IMAGE_BYTES {
            return Err(format!("image is {} bytes encoded, over {}", bytes.len(), crate::image::MAX_ENCODED_IMAGE_BYTES));
        }
        let charged = self.decoded.get(&session);
        let used: usize = charged.map(|m| m.values().sum()).unwrap_or(0);
        let mine = charged.and_then(|m| m.get(&address)).copied().unwrap_or(0);
        let c = Ceilings {
            budget: navigator_dom::profile::MAX_DECODED_IMAGE_BYTES.saturating_sub(used - mine),
            // ★ Charged at what is kept: the painted size, not the source's.
            target: painted.map(|p| self.device_size_at(p, self.zoom(session))),
            ..Ceilings::default()
        };
        let pixels = self.host.decode_image(session, &address, &bytes, c, self.now)?;
        // ★ Re-checked whatever the host is. A jailed host has already
        // checked (and retired a worker that lied); this is the broker's own
        // check, so a new host cannot forget it.
        crate::image::validate(pixels.width, pixels.height, pixels.rgba.len(), c)
            .map_err(|why| format!("the host returned a bad image: {why}"))?;
        self.decoded.entry(session).or_default().insert(address.clone(), pixels.rgba.len());
        Ok(Event::Image { session, address, pixels })
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
    /// Where recordings and saved sessions go, if anywhere.
    store: Option<Box<dyn crate::store::Store>>,
    /// Each open session's recording address, in `store`.
    recordings: std::collections::HashMap<SessionId, String>,
    carried: std::collections::HashMap<SessionId, Carried>,
    /// Each session's way from its first document to its scripted one.
    progress: std::collections::HashMap<SessionId, crate::progressive::Progress>,
    /// Each session's content source (`bands`).
    bands: std::collections::HashMap<SessionId, crate::display::Bands>,
}

impl Default for InProcessHost {
    fn default() -> Self { InProcessHost::new(SessionLimits::default(), crate::Limits::default()) }
}

impl InProcessHost {
    pub fn new(sessions: SessionLimits, limits: crate::Limits) -> Self {
        InProcessHost { sessions: Sessions::new(sessions), limits, store: None, recordings: Default::default(), carried: Default::default(),
                        progress: Default::default(), bands: Default::default() }
    }

    /// With a store, so sessions can be saved and restored.
    pub fn with_store(mut self, store: Box<dyn crate::store::Store>) -> Self {
        self.store = Some(store);
        self
    }
    pub fn sessions(&self) -> &Sessions { &self.sessions }

    /// Keep what a session now shows in the store, so it saves and restores
    /// as that: the recording, with its table composed in when it has one.
    fn store_parts(&mut self, id: SessionId, recording: &[u8], transitions: Option<&[u8]>) -> Result<(), String> {
        let Some(store) = &mut self.store else { return Ok(()) };
        let whole = match transitions {
            Some(t) => crate::progressive::compose(recording, t)?,
            None => recording.to_vec(),
        };
        let a = store.put(&whole).map_err(|e| format!("could not store the recording: {e}"))?;
        self.recordings.insert(id, a);
        Ok(())
    }
}

impl DocumentHost for InProcessHost {
    fn open(&mut self, recording: &[u8], now: Millis) -> Result<SessionId, String> {
        let r = crate::ingest(recording, &self.limits).map_err(|e| e.to_string())?;
        let id = self.sessions.open(&r, now).map_err(|e| e.to_string())?;
        // ★ The recording goes into the store when it is OPENED, not when a
        // session is saved: by then the bytes are gone, and a saved session
        // that names a recording nobody kept cannot be restored.
        if let Some(store) = &mut self.store {
            match store.put(recording) {
                Ok(a) => { self.recordings.insert(id, a); }
                Err(why) => { self.sessions.close(id); return Err(format!("could not store the recording: {why}")) }
            }
        }
        self.carried.insert(id, Carried::new(recording));
        self.progress.insert(id, crate::progressive::Progress::opened(recording));
        Ok(id)
    }

    fn upgrade(&mut self, id: SessionId, recording: &[u8], now: Millis) -> Result<crate::progressive::Upgrade, String> {
        let p = self.progress.get_mut(&id).ok_or("no such session")?;
        p.upgrade(&mut self.sessions, id, recording, &self.limits, now)
    }

    fn swap(&mut self, id: SessionId, anchor: &crate::progressive::Anchor, now: Millis) -> Result<crate::progressive::Mapped, String> {
        let p = self.progress.get_mut(&id).ok_or("no such session")?;
        let sw = p.swap(&mut self.sessions, id, anchor, &self.limits, now)?;
        // ★ What the session CARRIES follows what it shows: the link and
        // image bounds are the new recording's (and its table's), never
        // still the first one's.
        let mut c = Carried::new(&sw.recording);
        if let Some(t) = &sw.transitions { c.extend(t) }
        self.carried.insert(id, c);
        self.store_parts(id, &sw.recording, sw.transitions.as_deref())?;
        Ok(sw.mapped)
    }

    fn attach(&mut self, id: SessionId, transitions: &[u8], now: Millis) -> Result<crate::progressive::Attached, String> {
        let p = self.progress.get_mut(&id).ok_or("no such session")?;
        let a = p.attach(&mut self.sessions, id, transitions, &self.limits, now)?;
        if a == crate::progressive::Attached::Now {
            // An inserted effect's links are the document's links too.
            if let Some(c) = self.carried.get_mut(&id) { c.extend(transitions) }
            let shown = self.recordings.get(&id).cloned();
            if let (Some(store), Some(addr)) = (&self.store, shown) {
                let readable = store.get(&addr)?;
                self.store_parts(id, &readable, Some(transitions))?;
            }
        }
        Ok(a)
    }

    fn images(&mut self, id: SessionId, images: &[u8], _now: Millis) -> Result<crate::progressive::ImagesAttached, String> {
        let p = self.progress.get_mut(&id).ok_or("no such session")?;
        let a = p.images.attach(images, &self.limits)?;
        // ★ What the session CARRIES is the merged recording's: the image
        // bound now names the addresses that arrived, from bytes the broker
        // was handed (an inserted effect's, from a table attached later,
        // are added to it then, as before).
        self.carried.insert(id, Carried::new(&a.merged));
        self.store_parts(id, &a.merged, None)?;
        Ok(a)
    }

    fn save(&mut self, id: SessionId) -> Result<String, String> {
        let store = self.store.as_mut().ok_or("this host has no store to save sessions to")?;
        let s = self.sessions.get(id).ok_or("no such session")?;
        let recording = self.recordings.get(&id).ok_or("this session's recording was not stored")?;
        let saved = s.saved(recording).map_err(|e| format!("cannot save: {e}"))?;
        store.put(&saved.to_json())
    }

    fn restore(&mut self, saved: &str, now: Millis) -> Result<SessionId, String> {
        let store = self.store.as_ref().ok_or("this host has no store to restore sessions from")?;
        let record = crate::session::Saved::from_json(&store.get(saved)?)?;
        let bytes = store.get(&record.recording)?;
        let r = crate::ingest(&bytes, &self.limits).map_err(|e| e.to_string())?;
        let id = self.sessions.restore(&r, &record, now).map_err(|e| e.to_string())?;
        self.recordings.insert(id, record.recording);
        self.carried.insert(id, Carried::new(&bytes));
        self.progress.insert(id, crate::progressive::Progress::opened(&bytes));
        Ok(id)
    }

    fn navigate(&mut self, id: SessionId, trigger: &str, now: Millis) -> Result<(), String> {
        let s = self.sessions.get_mut(id, now).ok_or_else(|| "no such session".to_string())?;
        s.navigate(trigger).map_err(|e| e.to_string())
    }

    fn back(&mut self, id: SessionId, now: Millis) -> Result<Back, String> {
        let s = self.sessions.get_mut(id, now).ok_or_else(|| "no such session".to_string())?;
        s.back().map_err(|e| e.to_string())
    }

    fn forward(&mut self, id: SessionId, now: Millis) -> Result<Forward, String> {
        let s = self.sessions.get_mut(id, now).ok_or_else(|| "no such session".to_string())?;
        s.forward().map_err(|e| e.to_string())
    }

    fn close(&mut self, id: SessionId) -> bool {
        self.recordings.remove(&id);
        self.carried.remove(&id);
        self.progress.remove(&id);
        self.bands.remove(&id);
        self.sessions.close(id)
    }
    fn expire(&mut self, now: Millis) -> Vec<(SessionId, Expiry)> {
        let gone = self.sessions.expire(now);
        for (id, _) in &gone { self.recordings.remove(id); self.carried.remove(id); self.progress.remove(id); }
        gone
    }
    fn carried(&self, id: SessionId) -> Option<&Carried> { self.carried.get(&id) }
    fn links_to(&mut self, id: SessionId, href: &str, _now: Millis) -> Result<bool, String> {
        let s = self.sessions.get(id).ok_or("no such session")?;
        Ok(crate::links::document_carries(&s.document().dom, href))
    }
    /// ★ In this process, because this host IS this process: it exists for
    /// tests, and decodes with the same function the worker runs.
    fn decode_image(&mut self, id: SessionId, address: &str, bytes: &[u8], c: Ceilings, _now: Millis)
        -> Result<Pixels, String>
    {
        if self.sessions.get(id).is_none() { return Err("no such session".into()) }
        crate::image::decode(address, bytes, c).map_err(|e| e.to_string())
    }
    fn bands(&mut self, id: SessionId, ask: crate::display::BandsAsk, initial: bool, generation: u64,
             cached: &mut dyn FnMut() -> Option<Vec<u8>>, _now: Millis) -> Result<crate::display::BandsAnswer, String>
    {
        let mut produced = None;
        if self.bands.get(&id).is_none_or(|b| b.generation != generation) {
            let s = self.sessions.get(id).ok_or("no such session")?;
            let current = (!initial).then(|| s.document().dom.serialize());
            let rec = self.carried.get(&id).ok_or("no recording for this session")?;
            let c = if initial { cached() } else { None };
            let (b, n) = crate::display::Bands::open(rec.bytes(), current.as_deref(), c.as_deref(), generation)?;
            produced = n;
            self.bands.insert(id, b);
        }
        let mut a = self.bands.get_mut(&id).expect("built").ask(ask)?;
        a.normalized = produced;
        Ok(a)
    }
    fn scene(&mut self, id: SessionId, cached: Option<&[u8]>, initial: bool, limit_px: u32, _now: Millis)
        -> Result<crate::display::SceneAnswer, String>
    {
        let s = self.sessions.get(id).ok_or("no such session")?;
        let current = (!initial).then(|| s.document().dom.serialize());
        let rec = self.carried.get(&id).ok_or("no recording for this session")?;
        crate::display::first_screen(rec.bytes(), current.as_deref(), cached, limit_px)
    }
    fn status(&self, id: SessionId) -> Status { self.sessions.status(id) }
    fn bytes(&self) -> usize { self.sessions.bytes() }
    fn open_count(&self) -> usize { self.sessions.len() }

    fn report(&self) -> Vec<(SessionId, usize)> {
        self.sessions.report().into_iter().map(|(id, _url, bytes)| (id, bytes)).collect()
    }

    fn document_url(&mut self, id: SessionId, _now: Millis) -> Result<String, String> {
        self.sessions.get(id).map(|s| s.url.clone()).ok_or_else(|| "no such session".into())
    }

    fn triggers(&self, id: SessionId) -> Vec<String> {
        self.sessions.get(id)
            .map(|s| s.transitions().iter().filter(|t| t.is_offerable())
                 .map(|t| t.trigger.clone()).collect())
            .unwrap_or_default()
    }
}
