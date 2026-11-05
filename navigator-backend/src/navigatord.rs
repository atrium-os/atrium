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
    Image { session: SessionId, address: String },
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
}

impl<H: DocumentHost> Navigatord<H> {
    /// Read image bytes from `blobs` (see the field).
    pub fn with_blobs(mut self, blobs: Box<dyn crate::store::Store>) -> Self {
        self.blobs = Some(blobs);
        self
    }

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
        Ok(Navigatord { host, now: 0, schemes: SchemeAllowlist::default(), blobs: None, decoded: Default::default() })
    }

    /// An unconfined broker, for tests: in-process hosts, bare worker
    /// processes. ★ Compiled only into test builds — this crate's own tests,
    /// or a dependent that enables the `unconfined-for-testing` feature —
    /// so a shipped binary that calls it does not link.
    #[cfg(any(test, feature = "unconfined-for-testing"))]
    pub fn unconfined_for_testing(host: H) -> Self {
        Navigatord { host, now: 0, schemes: SchemeAllowlist::default(), blobs: None, decoded: Default::default() }
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
            Request::Forward { session } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.host.forward(session, self.now) {
                    Ok(how) => {
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
                let base = match self.host.document_url(session, self.now) {
                    Ok(u) => u,
                    Err(why) => { out.push(Event::Blocked { session: Some(session), why }); return out }
                };
                match decide(&base, &href, &self.schemes) {
                    Ok(action) => out.push(Event::Link { session, action }),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why: why.to_string() }),
                }
            }
            Request::Image { session, address } => {
                if let Some(e) = self.absent(session) { out.push(e); return out }
                match self.image(session, address) {
                    Ok(e) => out.push(e),
                    Err(why) => out.push(Event::Blocked { session: Some(session), why }),
                }
            }
        }
        out
    }

    /// ★★ AN IMAGE, IN THE ORDER THAT MAKES A HOSTILE ONE CHEAP TO REFUSE.
    /// The address is checked for shape and against the recording the broker
    /// holds (so a worker cannot pull another document's blob out of a
    /// shared store), the bytes are read and hash-checked by the store, the
    /// remaining budget is sent with them, and the host's answer is validated
    /// for shape before it becomes an event.
    fn image(&mut self, session: SessionId, address: String) -> Result<Event, String> {
        if crate::image::address_hex(&address).is_none() {
            return Err(format!("{address:?} is not an image address (blake3:<64 hex>)"));
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
}

impl Default for InProcessHost {
    fn default() -> Self { InProcessHost::new(SessionLimits::default(), crate::Limits::default()) }
}

impl InProcessHost {
    pub fn new(sessions: SessionLimits, limits: crate::Limits) -> Self {
        InProcessHost { sessions: Sessions::new(sessions), limits, store: None, recordings: Default::default(), carried: Default::default() }
    }

    /// With a store, so sessions can be saved and restored.
    pub fn with_store(mut self, store: Box<dyn crate::store::Store>) -> Self {
        self.store = Some(store);
        self
    }
    pub fn sessions(&self) -> &Sessions { &self.sessions }
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
        Ok(id)
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
        self.sessions.close(id)
    }
    fn expire(&mut self, now: Millis) -> Vec<(SessionId, Expiry)> {
        let gone = self.sessions.expire(now);
        for (id, _) in &gone { self.recordings.remove(id); self.carried.remove(id); }
        gone
    }
    fn carried(&self, id: SessionId) -> Option<&Carried> { self.carried.get(&id) }
    /// ★ In this process, because this host IS this process: it exists for
    /// tests, and decodes with the same function the worker runs.
    fn decode_image(&mut self, id: SessionId, address: &str, bytes: &[u8], c: Ceilings, _now: Millis)
        -> Result<Pixels, String>
    {
        if self.sessions.get(id).is_none() { return Err("no such session".into()) }
        crate::image::decode(address, bytes, c).map_err(|e| e.to_string())
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
