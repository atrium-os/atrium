//! prelumd's state and its jobs (printing.md §6.4–§6.5, §7.2–§7.3).
//!
//! One lock holds everything: people's saved printers (loaded when first
//! asked), what each printer last said, and the jobs. One runner thread per
//! printer with work takes its jobs in order, one at a time; it drops the
//! lock whenever it waits on a worker.
//!
//! prelumd parses nothing from outside: a printer's answers arrive as
//! `prelum_proto::ipp::Ev` values from prelum-ipp, the document's pages as
//! opaque bytes from prelum-render, which go to a spool file and on to
//! prelum-ipp without being looked at.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use prelum_ipp_codec::ops::{ColorMode, Format, JobIdentity, Quality, Sides, Ticket};
use prelum_ipp_codec::printer::{PrinterInfo, PrinterState, Reason, StateReason};
use prelum_proto::frame::{self, DATA_CHUNK};
use prelum_proto::ipp::{self, JobState};
use prelum_proto::print::{self, status, Alert, Changed, Ended, Phase};
use prelum_proto::render;

use crate::launch::{Launcher, Proc, Stopper};
use crate::store::{self, Saved};

/// How long a finished job stays in the list (history keeps it longer).
const KEEP_ENDED: usize = 200;
/// History per person at most (§6.5 keeps 30 days).
const KEEP_HISTORY: usize = 2000;
/// How long `WATCH` waits for a change.
const WATCH_FOR: Duration = Duration::from_secs(25);
/// Curia (its Printers page) and prelum-status (the Forum status item,
/// Praeco notices): the apps that act for their person (§6.4).
pub const CURIA_APP: &str = "org.atrium.curia";
pub const STATUS_APP: &str = "org.atrium.prelum-status";
/// Spool per person (§6.5).
pub const SPOOL_CAP: u64 = 2 << 30;
/// Job state polling (§6.3): every 2 s for a minute, then every 10 s.
const POLL_FAST: Duration = Duration::from_secs(2);
const POLL_SLOW: Duration = Duration::from_secs(10);
/// A printer that stops answering after it took the whole job is taken to
/// have printed it after this long.
const POLL_GIVE_UP: Duration = Duration::from_secs(300);
/// Offline: try again after this, doubling up to the max.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(30);
/// The name a job gives the printer for us (§6.3).
const USER: &str = "atrium";

pub struct Config {
    pub state: PathBuf,
    pub spool: PathBuf,
    pub sigillum: PathBuf,
    pub policy_root: PathBuf,
    /// The network we are on now, as atrium-net names it (§7.3).
    pub network: Box<dyn Fn() -> String + Send + Sync>,
    /// Where prelum-browse's sockets come from (§7.1): atrium-net, asked
    /// each time the list opens. `None`: no discovery here.
    pub mdns: Option<Box<dyn Fn() -> io::Result<crate::nearby::MdnsSockets> + Send + Sync>>,
    /// prelum-sheet's uid: the one peer that may act for an app's person
    /// (`FOR`, §5.2).
    pub sheet_uid: Option<u32>,
    /// An app's uid to its person's uid, persona and app id (the launch
    /// registry): whose printers a sheet opened for that app uses, and which
    /// app is asking.
    pub person_of: Box<dyn Fn(u32) -> Option<(u32, Option<String>, String)> + Send + Sync>,
}

pub type Fail = (u8, String);

fn visible_to(printer: &Saved, persona: &str) -> bool {
    printer.shared || printer.persona == persona
}

const CROSSING_REFUSED: &str =
    "This printer belongs to another persona. Allow printing in Settings → Personas → Between personas, or share it in Settings → Printers.";

fn fail<T>(st: u8, msg: impl Into<String>) -> Result<T, Fail> {
    Err((st, msg.into()))
}

/// What a printer said the last time it was asked.
#[derive(Default, Clone)]
struct Live {
    changed: Option<Changed>,
    offline: bool,
}

struct Person {
    file: store::File,
    live: HashMap<u32, Live>,
    /// Loaded when first needed.
    history: Option<store::History>,
}

struct JobRec {
    uid: u32,
    /// The persona it was printed from (history, §9).
    persona: String,
    pin: Option<ipp::PinGrant>,
    view: print::Job,
    req: print::Print,
    pdf: OwnedFd,
    cancel: bool,
    stops: Vec<Stopper>,
    /// The printer's id for it, once it has one.
    remote: Option<i32>,
}

#[derive(Default)]
struct State {
    people: HashMap<u32, Person>,
    jobs: BTreeMap<u64, JobRec>,
    pending_shares: HashMap<(u32, u32), u64>,
    share_ceremony: u64,
    next_job: u64,
    /// (uid, printer) with a runner.
    running: HashSet<(u32, u32)>,
    /// Bumped on every change to a job (`WATCH`).
    generation: u64,
}

pub struct Daemon {
    cfg: Config,
    launcher: Box<dyn Launcher>,
    st: Mutex<State>,
    /// Woken when a job is released or cancelled.
    cv: Condvar,
    /// Woken on every change to a job (`WATCH`).
    watch_cv: Condvar,
    /// Discovery (§7.1): its own lock, never held with `st`'s.
    pub(crate) near: Mutex<crate::nearby::Near>,
}

/// One worker conversation: send a request, read its events.
struct Ipp {
    p: Proc,
}

impl Ipp {
    fn send(&mut self, r: &ipp::Req) -> io::Result<()> {
        frame::write(&mut self.p.sock, r)
    }
    fn next(&mut self) -> io::Result<ipp::Ev> {
        frame::read(&mut self.p.sock)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "prelum-ipp ended"))
    }
    fn ask(&mut self, r: &ipp::Req) -> io::Result<ipp::Ev> {
        self.send(r)?;
        self.next()
    }
}

impl Drop for Ipp {
    fn drop(&mut self) {
        let _ = self.p.sock.shutdown(std::net::Shutdown::Both);
        self.p.stop.stop();
    }
}

pub fn fingerprint(k: &[u8; 32]) -> String {
    use base64::Engine;
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(k))
}

/// Why a stopped printer stopped, from what it reports (§6.6).
fn alert_of(reasons: &[StateReason]) -> Alert {
    let has = |r: &[Reason]| reasons.iter().any(|s| r.contains(&s.reason));
    if has(&[Reason::MediaEmpty, Reason::MediaNeeded]) {
        Alert::OutOfPaper
    } else if has(&[Reason::MediaJam]) {
        Alert::Jam
    } else if has(&[Reason::DoorOpen, Reason::CoverOpen, Reason::InputTrayMissing, Reason::OutputAreaFull]) {
        Alert::Open
    } else {
        Alert::Stopped
    }
}

fn stopped_words(name: &str, a: Alert) -> String {
    match a {
        Alert::OutOfPaper => format!("{name} is out of paper"),
        Alert::Jam => format!("paper is jammed in {name}"),
        Alert::Open => format!("{name} needs attention"),
        _ => format!("{name} is stopped"),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A PWG media keyword whose size Prelum knows how to read (`..._210x297mm`).
pub(crate) fn media_ok(k: &str) -> bool {
    let Some(d) = k.rsplit('_').next() else { return false };
    let Some(d) = d.strip_suffix("mm").or_else(|| d.strip_suffix("in")) else { return false };
    let mut it = d.split('x');
    let ok = |s: Option<&str>| s.and_then(|s| s.parse::<f64>().ok()).is_some_and(|v| v > 0.0 && v < 2000.0);
    ok(it.next()) && ok(it.next()) && it.next().is_none()
}

/// The candidate URIs for what the person typed (§7.2): `ipps://…` and
/// `ipp://…` as given; a bare host tries IPPS, then IPP, on 631.
pub fn candidates(input: &str) -> Result<Vec<(String, bool, String)>, String> {
    let input = input.trim();
    let ok = |s: &str| !s.is_empty() && s.len() <= 1023 && s.bytes().all(|b| b.is_ascii_graphic()) && !s.contains('@');
    if !ok(input) {
        return Err("not a printer address".into());
    }
    let split = |rest: &str| -> (String, String) {
        match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/ipp/print".to_string()),
        }
    };
    let with_port = |h: &str| if h.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !h.ends_with(']') { h.to_string() } else { format!("{h}:631") };
    if let Some(r) = input.strip_prefix("ipps://") {
        let (h, p) = split(r);
        return Ok(vec![(format!("ipps://{}{p}", with_port(&h)), true, with_port(&h))]);
    }
    if let Some(r) = input.strip_prefix("ipp://") {
        let (h, p) = split(r);
        return Ok(vec![(format!("ipp://{}{p}", with_port(&h)), false, with_port(&h))]);
    }
    if input.contains("://") {
        return Err("only ipps:// and ipp:// printers".into());
    }
    let (h, p) = split(input);
    let h = with_port(&h);
    Ok(vec![(format!("ipps://{h}{p}"), true, h.clone()), (format!("ipp://{h}{p}"), false, h)])
}

/// Resolved once, here, before the worker starts (§6.3: no rebinding).
/// IPv4 first: a worker's network grant names IPv4 addresses.
fn resolve(hostport: &str) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = hostport.to_socket_addrs().map_err(|e| format!("{hostport}: {e}"))?.collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().ok_or_else(|| format!("{hostport}: no address"))
}

fn pick_dpi(mut list: Vec<i32>, q: Option<Quality>) -> Option<i32> {
    list.sort();
    list.dedup();
    match q {
        Some(Quality::Draft) => list.first().copied(),
        Some(Quality::High) => list.last().copied(),
        _ => list.iter().copied().min_by_key(|d| ((d - 300).abs(), -d)),
    }
}

/// How a job goes to this printer: format, raster arguments and ticket.
pub fn plan(info: &PrinterInfo, raster_only: bool, req: &print::Print) -> Result<(Format, Vec<String>, Ticket), String> {
    let format = info.choose_format(raster_only).ok_or("the printer takes none of the formats Prelum sends")?;
    let color_ok = match format {
        Format::Pdf => info.color_modes.iter().any(|m| m == "color" || m == "auto"),
        Format::PwgRaster => info.pwg_types.iter().any(|t| t == "srgb_8"),
        Format::Urf => info.urf.iter().any(|t| t == "SRGB24"),
    };
    let gray = req.color == ColorMode::Monochrome || !color_ok;
    let media = req
        .media
        .clone()
        .filter(|m| info.media_supported.is_empty() || info.media_supported.contains(m))
        .or_else(|| info.media_default.clone())
        .filter(|m| media_ok(m))
        .unwrap_or_else(|| "iso_a4_210x297mm".into());
    let sides = if req.sides != Sides::OneSided && info.sides_supported.iter().any(|s| s == req.sides.keyword()) { req.sides } else { Sides::OneSided };
    // Raster is drawn at the printer's own resolution for the quality.
    let raster = |f: Format| -> Result<(i32, String, &'static str), String> {
        match f {
            Format::Urf => Ok((pick_dpi(info.urf_resolutions().collect(), req.quality).ok_or("no URF resolution")?, info.urf_sheet_back().unwrap_or("normal").into(), "urf")),
            _ => Ok((
                pick_dpi(info.pwg_resolutions.iter().filter(|(x, y)| x == y).map(|(x, _)| *x).collect(), req.quality).ok_or("no PWG resolution")?,
                info.pwg_sheet_back.clone().unwrap_or_else(|| "normal".into()),
                "pwg",
            )),
        }
    };
    let mut args = Vec::new();
    let mut arg = |k: &str, v: String| {
        args.push(k.to_string());
        args.push(v);
    };
    let mut resolution = None;
    match format {
        Format::Pdf => {
            arg("--format", "pdf".into());
            // If the document can't be rewritten: the printer's raster.
            let fb = if info.formats.iter().any(|f| f == "image/pwg-raster") && !info.pwg_resolutions.is_empty() {
                Some(Format::PwgRaster)
            } else if info.formats.iter().any(|f| f == "image/urf") {
                Some(Format::Urf)
            } else {
                None
            };
            if let Some(fb) = fb {
                let (dpi, back, name) = raster(fb)?;
                arg("--fallback", name.into());
                arg("--dpi", dpi.to_string());
                arg("--sheet-back", back);
            }
        }
        f => {
            let (dpi, back, name) = raster(f)?;
            arg("--format", name.into());
            arg("--dpi", dpi.to_string());
            arg("--sheet-back", back);
            resolution = info.ticket_resolution((dpi, dpi));
        }
    }
    arg("--media", media.clone());
    if let Some(p) = &req.pages {
        arg("--pages", p.clone());
    }
    arg("--per-sheet", req.per_sheet.clamp(1, 4).to_string());
    arg("--color", if gray { "gray" } else { "color" }.into());
    arg("--duplex", match sides {
        Sides::OneSided => "simplex",
        Sides::TwoSidedLongEdge => "long",
        Sides::TwoSidedShortEdge => "short",
    }
    .into());
    arg("--scale", match req.scale {
        print::Scale::Fit => "fit".into(),
        print::Scale::ShrinkToFit => "shrink".into(),
        print::Scale::Percent(p) => p.clamp(10, 400).to_string(),
    });
    if let Some(q) = req.quality {
        arg("--quality", (q as i32).to_string());
    }
    let ticket = Ticket {
        copies: req.copies.clamp(1, 999),
        sides,
        color: if gray && info.color_modes.iter().any(|m| m == "monochrome") { ColorMode::Monochrome } else { ColorMode::Color },
        media: (info.media_supported.contains(&media)).then_some(media),
        quality: req.quality.filter(|q| info.qualities.contains(&(*q as i32))),
        resolution,
    };
    Ok((format, args, ticket))
}

/// A worker's message, as plain text to show.
pub(crate) fn short(m: &str) -> String {
    prelum_ipp_codec::display_text(m, 200)
}

/// A worker's description of a printer, cut again here: prelum-ipp checks
/// it, but prelum-ipp is what a hostile printer would attack, so prelumd
/// keeps nothing it couldn't have said honestly (§6.3 bounds).
fn clean(mut i: PrinterInfo) -> PrinterInfo {
    use prelum_ipp_codec::display_text;
    use prelum_ipp_codec::printer::{MAX_LIST, MAX_TEXT};
    let text = |s: &mut String| *s = display_text(s, MAX_TEXT);
    let word = |v: &mut Vec<String>| {
        v.truncate(MAX_LIST);
        v.retain(|k| !k.is_empty() && k.len() <= 255 && k.bytes().all(|b| b.is_ascii_graphic()));
    };
    text(&mut i.make_and_model);
    text(&mut i.info);
    text(&mut i.location);
    for v in [&mut i.formats, &mut i.pwg_types, &mut i.urf, &mut i.media_supported, &mut i.sides_supported, &mut i.color_modes, &mut i.identify_actions] {
        word(v);
    }
    i.uris.truncate(8);
    i.uris.retain(|(u, s)| u.len() <= 1023 && u.bytes().all(|b| b.is_ascii_graphic()) && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_graphic()));
    i.media_default = i.media_default.filter(|m| m.len() <= 255 && m.bytes().all(|b| b.is_ascii_graphic()));
    i.pwg_sheet_back = i.pwg_sheet_back.filter(|m| m.len() <= 32 && m.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'));
    i.uuid = i.uuid.filter(|u| u.len() == 36 && u.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
    i.state_reasons.truncate(MAX_LIST);
    i.pwg_resolutions.truncate(MAX_LIST);
    i.printer_resolutions.truncate(MAX_LIST);
    i.qualities.truncate(8);
    i.operations.truncate(MAX_LIST);
    i.markers.truncate(32);
    for m in &mut i.markers {
        m.name = display_text(&m.name, 64);
        m.color = display_text(&m.color, 64);
        m.level = m.level.clamp(-3, 100);
    }
    i
}

/// How a runner's job ended, or why it stopped for now.
enum Outcome {
    Done(String),
    Failed(String),
    Cancelled,
    /// Waits (offline, elsewhere): try again later.
    Retry(String),
    /// Needs the person: identity changed.
    Held(String),
}

impl Daemon {
    pub fn new(cfg: Config, launcher: Box<dyn Launcher>) -> Arc<Daemon> {
        Arc::new(Daemon {
            cfg,
            launcher,
            st: Mutex::new(State { next_job: 1, ..Default::default() }),
            cv: Condvar::new(),
            watch_cv: Condvar::new(),
            near: Mutex::new(Default::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn person<'a>(&self, st: &'a mut State, uid: u32) -> &'a mut Person {
        st.people.entry(uid).or_insert_with(|| Person { file: store::load(&self.cfg.state, uid), live: HashMap::new(), history: None })
    }

    fn saved(&self, uid: u32, id: u32) -> Result<Saved, Fail> {
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        p.file.printer.iter().find(|s| s.id == id).cloned().ok_or((status::NOT_FOUND, "no such printer".into()))
    }

    fn policy(&self, uid: u32) -> Result<portcullis_policy::Policy, Fail> {
        let user = portcullis_peer::username(uid)
            .ok_or((status::NOT_ALLOWED, "Unknown account for printer policy".into()))?;
        portcullis_policy::Policy::load(&self.cfg.policy_root.join(user).join("policy.toml"))
            .map_err(|e| (status::FAILED, format!("Cannot read printer policy: {e}")))
    }

    fn can_print(&self, uid: u32, persona: &str, printer: &Saved) -> bool {
        let named = |p: &str| !p.is_empty();
        visible_to(printer, persona) || self.policy(uid).is_ok_and(|policy| {
            policy.may_cross(named(persona).then_some(persona),
                named(&printer.persona).then_some(printer.persona.as_str()), "printers")
        })
    }

    fn needs_ipps(&self, uid: u32, persona: &str) -> bool {
        !persona.is_empty() && self.policy(uid).ok()
            .and_then(|p| p.personas.get(persona).and_then(|p| p.print_require_ipps)).unwrap_or(true)
    }

    pub(crate) fn route(&self, uid: u32, persona: &str) -> Result<Option<u32>, Fail> {
        if persona.is_empty() {
            return Ok(None);
        }
        let policy = self.policy(uid)?;
        let def = policy.personas.get(persona)
            .ok_or((status::NOT_ALLOWED, "The printing persona no longer exists".into()))?;
        def.vpn.as_deref().map(|vpn| {
            atrium_netconf::route_domain::fib_for(vpn)
                .map_err(|e| (status::OFFLINE, format!("Cannot use {persona}'s printer route: {e}")))
        }).transpose()
    }

    /// The saved printer of `uid` with this identity, if any.
    pub(crate) fn saved_as(&self, uid: u32, persona: Option<&str>, uuid: Option<&str>, key: Option<&str>) -> Option<u32> {
        uuid?;
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        p.file.printer.iter().find(|o| persona.is_none_or(|p| self.can_print(uid, p, o)) && o.uuid.as_deref() == uuid && o.key.as_deref() == key).map(|o| o.id)
    }

    pub(crate) fn cfg(&self) -> &Config {
        &self.cfg
    }

    pub(crate) fn launcher(&self) -> &dyn Launcher {
        &*self.launcher
    }

    pub(crate) fn mdns_source(&self) -> Option<&(dyn Fn() -> io::Result<crate::nearby::MdnsSockets> + Send + Sync)> {
        self.cfg.mdns.as_deref()
    }

    pub(crate) fn network_now(&self) -> String {
        (self.cfg.network)()
    }

    fn persist(&self, uid: u32, st: &mut State) {
        let f = self.person(st, uid).file.clone();
        if let Err(e) = store::save(&self.cfg.state, uid, &f) {
            eprintln!("prelumd: saving printers of {uid}: {e}");
        }
    }

    fn view(&self, s: &Saved, live: Option<&Live>) -> print::Printer {
        let i = &s.info;
        print::Printer {
            id: s.id,
            name: s.name.clone(),
            uri: s.uri.clone(),
            tls: s.tls,
            fingerprint: s.key.as_deref().and_then(store::unhex).map(|k| fingerprint(&k)),
            uuid: s.uuid.clone(),
            make_and_model: i.make_and_model.clone(),
            location: i.location.clone(),
            network: s.network.clone(),
            elsewhere: s.network != (self.cfg.network)(),
            state: if live.is_some_and(|l| l.offline) { None } else { i.state },
            reasons: i.state_reasons.clone(),
            markers: i.markers.clone(),
            media_supported: i.media_supported.clone(),
            media_default: i.media_default.clone(),
            duplex: i.sides_supported.iter().any(|s| s != "one-sided"),
            color: i.color_modes.iter().any(|m| m == "color"),
            new: s.jobs_sent == 0,
            changed: live.and_then(|l| l.changed.clone()),
            persona: s.persona.clone(),
            shared: s.shared,
        }
    }

    pub fn printers(&self, uid: u32, persona: Option<&str>) -> Vec<print::Printer> {
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        p.file.printer.iter()
            .filter(|s| persona.is_none_or(|p| self.can_print(uid, p, s)))
            .map(|s| self.view(s, p.live.get(&s.id)))
            .collect()
    }

    fn worker(&self, s: &Saved, learn: bool, pin: bool, login_user: Option<u32>, fib: Option<u32>) -> Result<Ipp, Fail> {
        let addr: SocketAddr = s.addr.parse().map_err(|_| (status::FAILED, "bad saved address".to_string()))?;
        let login = login_user.and_then(|uid| printer_login_grant(s, uid));
        let p = self.launcher.ipp(addr, pin || login.is_some(), fib).map_err(|e| (status::FAILED, format!("can't start prelum-ipp: {e}")))?;
        let mut w = Ipp { p };
        let start = ipp::Start {
            uri: s.uri.clone(),
            addr,
            tls: s.tls,
            key: if learn { None } else { s.key.as_deref().and_then(store::unhex) },
            uuid: if learn { None } else { s.uuid.clone() },
            login,
        };
        frame::write(&mut w.p.sock, &start).map_err(|e| (status::FAILED, format!("prelum-ipp: {e}")))?;
        Ok(w)
    }

    /// Ask a printer who it is, learning its key (§7.2).
    pub(crate) fn learn(&self, s: &Saved, fib: Option<u32>) -> Result<(PrinterInfo, Option<[u8; 32]>), Fail> {
        self.learn_within(s, None, fib)
    }

    /// [`Self::learn`], giving up after `limit` (a printer found on the LAN,
    /// which may never answer).
    pub(crate) fn learn_within(&self, s: &Saved, limit: Option<Duration>, fib: Option<u32>) -> Result<(PrinterInfo, Option<[u8; 32]>), Fail> {
        let mut w = self.worker(s, true, false, None, fib)?;
        if limit.is_some() {
            w.p.sock.set_read_timeout(limit).map_err(|e| (status::FAILED, e.to_string()))?;
        }
        match w.ask(&ipp::Req::Query).map_err(|e| (status::FAILED, format!("prelum-ipp: {e}")))? {
            ipp::Ev::Printer { info, key } => {
                if s.tls && key.is_none() {
                    return fail(status::FAILED, "the printer showed no TLS key");
                }
                Ok((clean(*info), key))
            }
            ipp::Ev::Unreachable(m) => fail(status::OFFLINE, short(&m)),
            ipp::Ev::Refused { message, .. } => fail(status::FAILED, format!("the printer refused: {}", short(&message))),
            ipp::Ev::Failed(m) => fail(status::FAILED, short(&m)),
            other => fail(status::FAILED, short(&format!("unexpected answer {other:?}"))),
        }
    }

    pub fn add(&self, uid: u32, persona: &str, a: &print::Add) -> Result<print::Printer, Fail> {
        if a.name.len() > 128 || a.name.chars().any(char::is_control) {
            return fail(status::MALFORMED, "bad name");
        }
        let fib = self.route(uid, persona)?;
        let cands = candidates(&a.uri).map_err(|e| (status::MALFORMED, e))?;
        let require_ipps = self.needs_ipps(uid, persona);
        let network = (self.cfg.network)();
        let mut last = (status::FAILED, "no printer there".to_string());
        for (uri, tls, hostport) in cands {
            if require_ipps && !tls {
                return fail(status::NOT_ALLOWED, "This persona requires encrypted IPPS printing");
            }
            if fib.is_some() && hostport.parse::<SocketAddr>().is_err() {
                return fail(status::NOT_ALLOWED, "Enter a numeric printer address for a VPN persona");
            }
            let addr = match resolve(&hostport) {
                Ok(a) => a,
                Err(e) => return fail(status::OFFLINE, e),
            };
            let mut s = Saved {
                id: 0,
                name: a.name.clone(),
                uri,
                addr: addr.to_string(),
                tls,
                key: None,
                uuid: None,
                network: network.clone(),
                raster_only: false,
                titles_off: false,
                jobs_sent: 0,
                persona: persona.to_string(),
                shared: false,
                info: PrinterInfo::default(),
            };
            match self.learn(&s, fib) {
                Ok((info, key)) => {
                    s.key = key.map(|k| store::hex(&k));
                    s.uuid = info.uuid.clone();
                    if s.name.is_empty() {
                        s.name = [&info.info, &info.make_and_model].into_iter().find(|n| !n.is_empty()).cloned().unwrap_or_else(|| hostport.clone());
                    }
                    s.info = info;
                    let mut st = self.lock();
                    let p = self.person(&mut st, uid);
                    // The same printer again (same identity): what was saved.
                    if let Some(old) = p.file.printer.iter().find(|o| o.uuid.is_some() && o.uuid == s.uuid && o.key == s.key && o.uri == s.uri) {
                        if !self.can_print(uid, persona, old) {
                            return fail(status::NOT_ALLOWED, CROSSING_REFUSED);
                        }
                        return Ok(self.view(old, p.live.get(&old.id)));
                    }
                    s.id = p.file.printer.iter().map(|o| o.id).max().unwrap_or(0) + 1;
                    p.file.printer.push(s.clone());
                    self.persist(uid, &mut st);
                    return Ok(self.view(&s, None));
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    pub fn remove(&self, uid: u32, id: u32) -> Result<String, Fail> {
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        let n = p.file.printer.len();
        p.file.printer.retain(|s| s.id != id);
        if p.file.printer.len() == n {
            return fail(status::NOT_FOUND, "no such printer");
        }
        p.live.remove(&id);
        st.pending_shares.remove(&(uid, id));
        self.persist(uid, &mut st);
        // Its waiting jobs go with it.
        let ids: Vec<u64> = st.jobs.iter().filter(|(_, j)| j.uid == uid && j.view.printer == id && !j.view.phase.ended()).map(|(k, _)| *k).collect();
        drop(st);
        for j in ids {
            let _ = self.cancel(uid, j);
        }
        Ok("removed".into())
    }

    pub fn rename(&self, uid: u32, r: &print::Rename) -> Result<String, Fail> {
        if r.name.is_empty() || r.name.len() > 128 || r.name.chars().any(char::is_control) {
            return fail(status::MALFORMED, "bad name");
        }
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        let s = p.file.printer.iter_mut().find(|s| s.id == r.printer).ok_or((status::NOT_FOUND, "no such printer".to_string()))?;
        s.name = r.name.clone();
        self.persist(uid, &mut st);
        Ok("renamed".into())
    }

    /// Publishing to all personas requires a fresh, successful Sigillum ceremony.
    pub fn share(&self, uid: u32, r: &print::Share) -> Result<String, Fail> {
        let key = (uid, r.printer);
        let mut st = self.lock();
        if !r.shared {
            st.pending_shares.remove(&key);
            return self.set_sharing(&mut st, uid, r.printer, false);
        }

        let snapshot = self.person(&mut st, uid).file.printer.iter().find(|s| s.id == r.printer)
            .cloned().ok_or((status::NOT_FOUND, "no such printer".into()))?;
        if snapshot.shared {
            return Ok("shared".into());
        }
        if st.pending_shares.contains_key(&key) {
            return fail(status::NOT_ALLOWED, "Printer sharing is already waiting for verification");
        }
        let user = portcullis_peer::username(uid)
            .ok_or((status::NOT_ALLOWED, "Unknown account for printer sharing".into()))?;
        st.share_ceremony = st.share_ceremony.wrapping_add(1);
        let ceremony = st.share_ceremony;
        st.pending_shares.insert(key, ceremony);
        drop(st);

        // The person may take time to answer. Other printer operations can proceed.
        let verified = crate::sharing::verify(&self.cfg.sigillum, ceremony, &user, &snapshot.name);
        let mut st = self.lock();
        if st.pending_shares.get(&key) != Some(&ceremony) {
            return fail(status::NOT_ALLOWED, "Printer sharing was cancelled");
        }
        st.pending_shares.remove(&key);
        verified.map_err(|message| (status::NOT_ALLOWED, message))?;
        let current = self.person(&mut st, uid).file.printer.iter().find(|s| s.id == r.printer);
        if !current.is_some_and(|s| {
            s.name == snapshot.name && s.persona == snapshot.persona && s.uri == snapshot.uri
                && s.addr == snapshot.addr && s.tls == snapshot.tls && s.key == snapshot.key
                && s.uuid == snapshot.uuid && s.network == snapshot.network
        }) {
            return fail(status::NOT_ALLOWED, "The printer changed while you were verifying. Try again.");
        }
        self.set_sharing(&mut st, uid, r.printer, true)
    }

    fn set_sharing(&self, st: &mut State, uid: u32, printer: u32, shared: bool) -> Result<String, Fail> {
        let p = self.person(st, uid);
        let mut file = p.file.clone();
        let saved = file.printer.iter_mut().find(|s| s.id == printer)
            .ok_or((status::NOT_FOUND, "no such printer".into()))?;
        if saved.shared != shared {
            saved.shared = shared;
            store::save(&self.cfg.state, uid, &file)
                .map_err(|e| (status::FAILED, format!("Could not save printer sharing: {e}")))?;
            p.file = file;
        }
        Ok(if shared { "shared".into() } else { "not shared".into() })
    }

    /// The last 30 days' ended jobs, newest first.
    pub fn printed_notice(&self, uid: u32) -> bool {
        !self.person(&mut self.lock(), uid).file.mute_printed_notice
    }

    pub fn set_printed_notice(&self, uid: u32, enabled: bool) -> Result<String, Fail> {
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        if p.file.mute_printed_notice == enabled {
            let mut next = p.file.clone();
            next.mute_printed_notice = !enabled;
            store::save(&self.cfg.state, uid, &next)
                .map_err(|e| (status::FAILED, format!("Could not save the Printed notice preference: {e}")))?;
            p.file = next;
            self.changed(&mut st);
        }
        Ok("saved".into())
    }

    pub fn history(&self, uid: u32) -> Vec<Ended> {
        let mut st = self.lock();
        let h = self.history_of(&mut st, uid);
        let mut v = h.job.clone();
        v.reverse();
        v
    }

    pub fn clear_history(&self, uid: u32) -> Result<String, Fail> {
        let mut st = self.lock();
        self.history_of(&mut st, uid).job.clear();
        self.persist_history(uid, &mut st);
        Ok("cleared".into())
    }

    fn history_of<'a>(&self, st: &'a mut State, uid: u32) -> &'a mut store::History {
        let state = &self.cfg.state;
        let p = self.person(st, uid);
        let h = p.history.get_or_insert_with(|| store::load_history(state, uid));
        let old = now().saturating_sub(print::HISTORY_DAYS * 24 * 3600);
        h.job.retain(|e| e.at >= old);
        h
    }

    fn persist_history(&self, uid: u32, st: &mut State) {
        let h = self.history_of(st, uid).clone();
        if let Err(e) = store::save_history(&self.cfg.state, uid, &h) {
            eprintln!("prelumd: saving history of {uid}: {e}");
        }
    }

    /// A job ended: into its person's history (§6.5), without its document.
    fn record(&self, st: &mut State, job: u64) {
        let Some(j) = st.jobs.get(&job) else { return };
        let (uid, view, persona) = (j.uid, j.view.clone(), j.persona.clone());
        let saved = self.person(st, uid).file.printer.iter().find(|s| s.id == view.printer).map(|s| (s.name.clone(), s.titles_off));
        let (printer, titles_off) = saved.unwrap_or_else(|| ("a removed printer".into(), false));
        let e = Ended {
            job,
            title: if titles_off { "Document".into() } else { view.title },
            printer,
            sheets: view.sheets,
            at: now(),
            result: view.phase,
            message: view.message,
            persona,
        };
        let h = self.history_of(st, uid);
        h.job.push(e);
        if h.job.len() > KEEP_HISTORY {
            let n = h.job.len() - KEEP_HISTORY;
            h.job.drain(..n);
        }
        self.persist_history(uid, st);
    }

    /// Something about a job changed: wake `WATCH`.
    fn changed(&self, st: &mut State) {
        st.generation += 1;
        self.watch_cv.notify_all();
    }

    /// The person's jobs once they differ from `after`, or after 25 s.
    pub fn watch(&self, uid: u32, after: u64) -> print::Watched {
        let until = Instant::now() + WATCH_FOR;
        let mut st = self.lock();
        while st.generation == after {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self.watch_cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        print::Watched { generation: st.generation, jobs: st.jobs.values().filter(|j| j.uid == uid).map(|j| j.view.clone()).collect() }
    }

    /// What a job may need the person for; `None` clears it. The time it
    /// began is kept while it stays the same.
    fn alert(&self, job: u64, a: Option<Alert>) {
        let mut st = self.lock();
        let Some(j) = st.jobs.get_mut(&job) else { return };
        if j.view.alert != a {
            j.view.alert = a;
            j.view.alert_since = if a.is_some() { now() } else { 0 };
            self.changed(&mut st);
        }
    }

    /// "Use the new identity" (§7.3): learn the printer again at its saved
    /// address and release the jobs held for it.
    pub fn repin(self: &Arc<Self>, uid: u32, id: u32) -> Result<print::Printer, Fail> {
        let s = self.saved(uid, id)?;
        let (info, key) = self.learn(&s, self.route(uid, &s.persona)?)?;
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        let Some(s) = p.file.printer.iter_mut().find(|s| s.id == id) else { return fail(status::NOT_FOUND, "no such printer") };
        s.key = key.map(|k| store::hex(&k));
        s.uuid = info.uuid.clone();
        s.info = info;
        s.network = (self.cfg.network)();
        let s = s.clone();
        p.live.insert(id, Live::default());
        self.persist(uid, &mut st);
        let mut any = false;
        for j in st.jobs.values_mut().filter(|j| j.uid == uid && j.view.printer == id && j.view.phase == Phase::Held) {
            j.view.phase = Phase::Pending;
            j.view.message.clear();
            any = true;
        }
        let v = self.view(&s, None);
        if any {
            self.kick(&mut st, uid, id);
        }
        Ok(v)
    }

    pub fn identify(&self, uid: u32, id: u32) -> Result<String, Fail> {
        let s = self.saved(uid, id)?;
        let mut w = self.worker(&s, false, false, None, self.route(uid, &s.persona)?)?;
        let mut actions: Vec<String> = ["sound", "flash", "display"].iter().filter(|a| s.info.identify_actions.iter().any(|x| x == *a)).map(|a| a.to_string()).collect();
        if actions.is_empty() {
            actions.push("sound".into());
        }
        match w.ask(&ipp::Req::Identify { actions }).map_err(|e| (status::FAILED, format!("prelum-ipp: {e}")))? {
            ipp::Ev::Done => Ok("the printer was asked to identify itself".into()),
            ev => Err(self.ev_fail(uid, id, ev)),
        }
    }

    /// An error event as a refusal, noting a changed identity or offline.
    fn ev_fail(&self, uid: u32, id: u32, ev: ipp::Ev) -> Fail {
        match ev {
            ipp::Ev::IdentityChanged { key, uuid } => {
                self.mark_changed(uid, id, key, uuid);
                (status::IDENTITY_CHANGED, "the printer's identity changed".into())
            }
            ipp::Ev::Unreachable(m) => {
                self.set_offline(uid, id, true);
                (status::OFFLINE, format!("the printer is offline ({})", short(&m)))
            }
            ipp::Ev::Refused { message, status: s } => (status::FAILED, format!("the printer refused (0x{s:04x}): {}", short(&message))),
            ipp::Ev::Failed(m) => (status::FAILED, short(&m)),
            other => (status::FAILED, short(&format!("unexpected answer {other:?}"))),
        }
    }

    fn mark_changed(&self, uid: u32, id: u32, key: Option<[u8; 32]>, uuid: Option<String>) {
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        let saved_uuid = p.file.printer.iter().find(|s| s.id == id).and_then(|s| s.uuid.clone());
        p.live.entry(id).or_default().changed =
            Some(Changed { fingerprint: key.map(|k| fingerprint(&k)), uuid: uuid.filter(|u| u.len() <= 64).map(|u| short(&u)).or(saved_uuid) });
    }

    fn set_offline(&self, uid: u32, id: u32, off: bool) {
        let mut st = self.lock();
        self.person(&mut st, uid).live.entry(id).or_default().offline = off;
    }

    /// Ask the printer again now (the sheet opening, Curia's page).
    pub fn refresh(&self, uid: u32, id: u32) -> Result<print::Printer, Fail> {
        let s = self.saved(uid, id)?;
        if s.network != (self.cfg.network)() {
            return Ok(self.view(&s, None));
        }
        let mut w = self.worker(&s, false, false, None, self.route(uid, &s.persona)?)?;
        let ev = w.ask(&ipp::Req::Query).map_err(|e| (status::FAILED, format!("prelum-ipp: {e}")))?;
        match ev {
            ipp::Ev::Printer { info, .. } => {
                let info = clean(*info);
                if info.uuid != s.uuid {
                    self.mark_changed(uid, id, None, info.uuid.clone());
                } else {
                    let mut st = self.lock();
                    let p = self.person(&mut st, uid);
                    p.live.insert(id, Live::default());
                    if let Some(s) = p.file.printer.iter_mut().find(|s| s.id == id) {
                        s.info = info;
                    }
                    self.persist(uid, &mut st);
                }
            }
            ipp::Ev::IdentityChanged { key, uuid } => self.mark_changed(uid, id, key, uuid),
            ipp::Ev::Unreachable(_) => self.set_offline(uid, id, true),
            ev => return Err(self.ev_fail(uid, id, ev)),
        }
        let mut st = self.lock();
        let p = self.person(&mut st, uid);
        let s = p.file.printer.iter().find(|s| s.id == id).ok_or((status::NOT_FOUND, "no such printer".to_string()))?;
        Ok(self.view(s, p.live.get(&id)))
    }

    pub fn jobs(&self, uid: u32) -> Vec<print::Job> {
        self.lock().jobs.values().filter(|j| j.uid == uid).map(|j| j.view.clone()).collect()
    }

    pub fn submit(self: &Arc<Self>, uid: u32, persona: &str, req: print::Print, pdf: OwnedFd) -> Result<u64, Fail> {
        self.enqueue(uid, persona, req, pdf, None)
    }

    pub fn printer_binding(&self, uid: u32, persona: &str, printer: u32, login: bool) -> Result<print::PinBinding, Fail> {
        let s = self.saved(uid, printer)?;
        if !self.can_print(uid, persona, &s) {
            return fail(status::NOT_ALLOWED, CROSSING_REFUSED);
        }
        if login && s.persona != persona {
            return fail(status::NOT_ALLOWED, "Set the printer login from its owning persona");
        }
        let Some(key) = s.key.as_deref().and_then(store::unhex).filter(|_| s.tls) else {
            return fail(status::NOT_ALLOWED, "This printer needs pinned IPPS for PIN jobs");
        };
        let Some(uuid) = s.uuid else {
            return fail(status::NOT_ALLOWED, "This printer has no pinned UUID");
        };
        if !login && (s.info.job_password_max.is_none()
            || !s.info.supports(prelum_ipp_codec::ops::op::CREATE_JOB)
            || !s.info.supports(prelum_ipp_codec::ops::op::SEND_DOCUMENT))
        {
            return fail(status::NOT_ALLOWED, "This printer does not support PIN jobs");
        }
        let user = portcullis_peer::username(uid).ok_or((status::NOT_ALLOWED, "unknown account for PIN printing".into()))?;
        let persona = if persona.is_empty() { "personal" } else { persona };
        Ok(print::PinBinding { user, persona: persona.into(), key, uuid })
    }

    pub fn submit_pin(self: &Arc<Self>, uid: u32, persona: &str, req: print::Print, pdf: OwnedFd, handle: [u8; 16]) -> Result<u64, Fail> {
        let user = portcullis_peer::username(uid).ok_or((status::NOT_ALLOWED, "unknown account for PIN printing".into()))?;
        let persona_name = if persona.is_empty() { "personal" } else { persona };
        let grant = ipp::PinGrant { handle, user, persona: persona_name.into() };
        self.enqueue(uid, persona, req, pdf, Some(grant))
    }

    fn enqueue(self: &Arc<Self>, uid: u32, persona: &str, req: print::Print, pdf: OwnedFd, pin: Option<ipp::PinGrant>) -> Result<u64, Fail> {
        if req.title.len() > 512 || req.copies == 0 || req.copies > 999 || ![1, 2, 4].contains(&req.per_sheet) {
            return fail(status::MALFORMED, "bad print settings");
        }
        if req.pages.as_ref().is_some_and(|p| p.len() > 256 || !p.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b',' || b == b' ')) {
            return fail(status::MALFORMED, "bad page range");
        }
        if req.media.as_ref().is_some_and(|m| !media_ok(m)) {
            return fail(status::MALFORMED, "bad paper size");
        }
        let file = File::from(pdf);
        match file.metadata() {
            Ok(m) if m.is_file() => {}
            _ => return fail(status::MALFORMED, "the document must be a file"),
        }
        let mut st = self.lock();
        let s = self.person(&mut st, uid).file.printer.iter().find(|s| s.id == req.printer)
            .ok_or((status::NOT_FOUND, "no such printer".into()))?;
        if !self.can_print(uid, persona, s) {
            return fail(status::NOT_ALLOWED, CROSSING_REFUSED);
        }
        if !s.tls && self.needs_ipps(uid, persona) {
            return fail(status::NOT_ALLOWED, "This persona requires encrypted IPPS printing");
        }
        if pin.is_some() && (!s.tls || s.key.is_none() || s.uuid.is_none()) {
            return fail(status::NOT_ALLOWED, "PIN jobs require a pinned IPPS printer");
        }
        let printer = s.id;
        let id = st.next_job;
        st.next_job += 1;
        let title: String = req.title.chars().filter(|c| !c.is_control()).take(200).collect();
        st.jobs.insert(id, JobRec {
            uid,
            persona: persona.to_string(),
            pin,
            view: print::Job {
                id,
                printer,
                title,
                phase: Phase::Pending,
                sheets: 0,
                of: 0,
                message: String::new(),
                submitted: now(),
                alert: None,
                alert_since: 0,
            },
            req,
            pdf: file.into(),
            cancel: false,
            stops: Vec::new(),
            remote: None,
        });
        // Forget the oldest ended jobs.
        let ended: Vec<u64> = st.jobs.iter().filter(|(_, j)| j.view.phase.ended()).map(|(k, _)| *k).collect();
        if ended.len() > KEEP_ENDED {
            for k in &ended[..ended.len() - KEEP_ENDED] {
                st.jobs.remove(k);
            }
        }
        self.changed(&mut st);
        self.kick(&mut st, uid, printer);
        Ok(id)
    }

    pub fn cancel(&self, uid: u32, job: u64) -> Result<String, Fail> {
        let mut st = self.lock();
        let j = st.jobs.get_mut(&job).filter(|j| j.uid == uid).ok_or((status::NOT_FOUND, "no such job".to_string()))?;
        if j.view.phase.ended() {
            return fail(status::NOT_FOUND, "the job has ended");
        }
        j.cancel = true;
        // Not started (or held): it just ends. A running job's runner sees
        // the flag once its workers are stopped, and cancels at the printer.
        let running = matches!(j.view.phase, Phase::Preparing | Phase::Sending | Phase::Printing);
        if !running {
            j.pin = None;
            j.view.phase = Phase::Cancelled;
            j.view.message.clear();
            j.view.alert = None;
        }
        for s in j.stops.drain(..) {
            s.stop();
        }
        if !running {
            self.record(&mut st, job);
        }
        self.changed(&mut st);
        self.cv.notify_all();
        Ok("cancelled".into())
    }

    pub fn release(self: &Arc<Self>, uid: u32, job: u64) -> Result<String, Fail> {
        let mut st = self.lock();
        let j = st.jobs.get_mut(&job).filter(|j| j.uid == uid).ok_or((status::NOT_FOUND, "no such job".to_string()))?;
        if j.view.phase != Phase::Held {
            return fail(status::NOT_FOUND, "the job is not held");
        }
        j.view.phase = Phase::Pending;
        j.view.message.clear();
        j.view.alert = None;
        let p = j.view.printer;
        self.changed(&mut st);
        self.kick(&mut st, uid, p);
        Ok("released".into())
    }

    /// Make sure the printer has a runner.
    fn kick(self: &Arc<Self>, st: &mut State, uid: u32, printer: u32) {
        self.cv.notify_all();
        if st.running.insert((uid, printer)) {
            let d = self.clone();
            std::thread::spawn(move || d.runner(uid, printer));
        }
    }

    fn runner(self: Arc<Self>, uid: u32, printer: u32) {
        let mut wait = RETRY_FIRST;
        loop {
            let next = {
                let mut st = self.lock();
                let next = st.jobs.iter().find(|(_, j)| j.uid == uid && j.view.printer == printer && j.view.phase == Phase::Pending).map(|(k, _)| *k);
                if next.is_none() {
                    st.running.remove(&(uid, printer));
                    return;
                }
                next.unwrap()
            };
            let out = self.run(uid, printer, next);
            let mut st = self.lock();
            let Some(j) = st.jobs.get_mut(&next) else { continue };
            j.stops.clear();
            let retry = matches!(out, Outcome::Retry(_));
            match out {
                Outcome::Done(m) => {
                    j.view.phase = Phase::Done;
                    j.view.sheets = j.view.of;
                    j.view.message = m;
                }
                Outcome::Failed(m) => {
                    j.view.phase = Phase::Failed;
                    j.view.message = m;
                }
                Outcome::Cancelled => {
                    j.view.phase = Phase::Cancelled;
                    j.view.message.clear();
                }
                Outcome::Held(m) => {
                    j.view.phase = Phase::Held;
                    j.view.message = m;
                }
                Outcome::Retry(m) => {
                    j.view.phase = Phase::Pending;
                    j.view.message = m;
                }
            }
            if !matches!(j.view.phase, Phase::Pending | Phase::Held) {
                j.view.alert = None;
            }
            let ended = j.view.phase.ended();
            if ended {
                j.pin = None;
                self.record(&mut st, next);
            }
            self.changed(&mut st);
            let _ = std::fs::remove_file(self.spool_path(uid, next));
            if retry {
                // Wait, unless something changes (a cancel, a release).
                let (st2, _) = self.cv.wait_timeout(st, wait).unwrap_or_else(|e| e.into_inner());
                drop(st2);
                wait = (wait * 2).min(RETRY_MAX);
            } else {
                wait = RETRY_FIRST;
            }
        }
    }

    fn spool_path(&self, uid: u32, job: u64) -> PathBuf {
        self.cfg.spool.join(uid.to_string()).join(format!("{job}.spool"))
    }

    fn set(&self, job: u64, f: impl FnOnce(&mut JobRec)) -> bool {
        let mut st = self.lock();
        let ok = match st.jobs.get_mut(&job) {
            Some(j) => {
                f(j);
                !j.cancel
            }
            None => return false,
        };
        self.changed(&mut st);
        ok
    }

    fn cancelled(&self, job: u64) -> bool {
        self.lock().jobs.get(&job).is_none_or(|j| j.cancel)
    }

    /// Register a worker's stopper; false (and the worker stopped) if the
    /// job was cancelled meanwhile.
    fn hold(&self, job: u64, s: &Stopper) -> bool {
        let ok = self.set(job, |j| j.stops.push(s.clone()));
        if !ok {
            s.stop();
        }
        ok
    }

    /// One attempt at one job.
    fn run(self: &Arc<Self>, uid: u32, printer: u32, job: u64) -> Outcome {
        let (s, req, pdf, pin, persona) = {
            let mut st = self.lock();
            let Some(s) = self.person(&mut st, uid).file.printer.iter().find(|s| s.id == printer).cloned() else {
                return Outcome::Failed("the printer was removed".into());
            };
            let Some(j) = st.jobs.get(&job) else {
                return Outcome::Failed("the document is gone".into());
            };
            if !self.can_print(uid, &j.persona, &s) {
                return Outcome::Failed(CROSSING_REFUSED.into());
            }
            if !s.tls && self.needs_ipps(uid, &j.persona) {
                return Outcome::Failed("This persona requires encrypted IPPS printing".into());
            }
            let Ok(pdf) = j.pdf.try_clone() else {
                return Outcome::Failed("the document is gone".into());
            };
            (s, j.req.clone(), pdf, j.pin.clone(), j.persona.clone())
        };
        if s.network != (self.cfg.network)() {
            return Outcome::Retry(format!("{} is not on this network", s.name));
        }
        if !self.set(job, |j| {
            j.view.phase = Phase::Preparing;
            j.view.message.clear();
        }) {
            return Outcome::Cancelled;
        }

        let fib = match self.route(uid, &persona) {
            Ok(fib) => fib,
            Err((_, message)) => return Outcome::Failed(message),
        };
        // Who is it, and what does it take? (§7.3: before any job byte.)
        let mut w = match self.worker(&s, false, pin.is_some(), Some(uid), fib) {
            Ok(w) => w,
            Err((_, m)) => return Outcome::Failed(m),
        };
        if !self.hold(job, &w.p.stop) {
            return Outcome::Cancelled;
        }
        let info = match w.ask(&ipp::Req::Query) {
            Ok(ipp::Ev::Printer { info, .. }) if info.uuid == s.uuid => {
                self.set_offline(uid, printer, false);
                clean(*info)
            }
            Ok(ipp::Ev::Printer { info, .. }) => {
                self.mark_changed(uid, printer, None, info.uuid.clone());
                self.alert(job, Some(Alert::IdentityChanged));
                return Outcome::Held(format!("{}'s identity changed", s.name));
            }
            Ok(ipp::Ev::IdentityChanged { key, uuid }) => {
                self.mark_changed(uid, printer, key, uuid);
                self.alert(job, Some(Alert::IdentityChanged));
                return Outcome::Held(format!("{}'s identity changed", s.name));
            }
            Ok(ipp::Ev::Unreachable(_)) => {
                self.set_offline(uid, printer, true);
                self.alert(job, Some(Alert::Offline));
                return Outcome::Retry(format!("{} is offline", s.name));
            }
            Ok(ev) => return Outcome::Failed(self.ev_fail(uid, printer, ev).1),
            Err(_) if self.cancelled(job) => return Outcome::Cancelled,
            Err(e) => return Outcome::Failed(format!("prelum-ipp: {e}")),
        };
        if info.state == Some(PrinterState::Stopped) {
            let a = alert_of(&info.state_reasons);
            self.alert(job, Some(a));
            return Outcome::Retry(stopped_words(&s.name, a));
        }
        self.alert(job, None);
        let (_, args, ticket) = match plan(&info, s.raster_only, &req) {
            Ok(p) => p,
            Err(m) => return Outcome::Failed(m),
        };

        // Render into the spool, and send from it as it grows (§6.5).
        let spool = self.spool_path(uid, job);
        let spool_file = match open_spool(&spool) {
            Ok(f) => f,
            Err(e) => return Outcome::Failed(format!("spool: {e}")),
        };
        let r = match self.launcher.render(pdf, &args) {
            Ok(r) => r,
            Err(e) => return Outcome::Failed(format!("can't start prelum-render: {e}")),
        };
        if !self.hold(job, &r.stop) {
            return Outcome::Cancelled;
        }
        let prog = Arc::new((Mutex::new(Progress::default()), Condvar::new()));
        let reader = {
            let prog = prog.clone();
            let d_job = job;
            let d = self.clone();
            let set_sheets: Arc<dyn Fn(u32, u32) + Send + Sync> = Arc::new(move |n, of| {
                d.set(d_job, |j| {
                    j.view.sheets = n;
                    if of > 0 {
                        j.view.of = of;
                    }
                });
            });
            std::thread::spawn(move || read_render(r.sock, spool_file, prog, set_sheets))
        };
        let format = {
            let (m, cv) = &*prog;
            let mut p = m.lock().unwrap();
            while p.format.is_none() && p.end.is_none() {
                p = cv.wait(p).unwrap();
            }
            match (&p.format, &p.end) {
                (Some(f), _) => *f,
                (None, Some(Err(e))) => {
                    let e = e.clone();
                    drop(p);
                    let _ = reader.join();
                    return if self.cancelled(job) { Outcome::Cancelled } else { Outcome::Failed(e) };
                }
                _ => unreachable!(),
            }
        };
        let identity = JobIdentity {
            user: USER.into(),
            job_name: if s.titles_off { "Document".into() } else { req.title.clone() },
            job_password: None,
        };
        let pin_job = pin.is_some();
        let request = match pin {
            Some(grant) => ipp::Req::SubmitPin { identity, format, ticket, grant },
            None => ipp::Req::Submit { identity, format, ticket },
        };
        let sent = self.send(&mut w, job, &spool, &prog, request);
        let rendered = reader.join().unwrap_or_else(|_| Err("prelum-render's reader failed".into()));
        let remote = self.lock().jobs.get(&job).and_then(|j| j.remote);
        if self.cancelled(job) {
            drop(w);
            if let Some(id) = remote {
                self.cancel_remote(&s, uid, id, fib);
            }
            return Outcome::Cancelled;
        }
        if let Err(e) = rendered {
            drop(w);
            if let Some(id) = remote {
                self.cancel_remote(&s, uid, id, fib);
            }
            return Outcome::Failed(e);
        }
        match sent {
            Ok(()) => {}
            Err(SendErr::Ev(ipp::Ev::IdentityChanged { key, uuid })) => {
                self.mark_changed(uid, printer, key, uuid);
                self.alert(job, Some(Alert::IdentityChanged));
                return Outcome::Held(format!("{}'s identity changed", s.name));
            }
            Err(SendErr::Ev(ipp::Ev::Unreachable(_))) if remote.is_none() && !pin_job => {
                self.set_offline(uid, printer, true);
                self.alert(job, Some(Alert::Offline));
                return Outcome::Retry(format!("{} is offline", s.name));
            }
            Err(SendErr::Ev(ev)) => return Outcome::Failed(self.ev_fail(uid, printer, ev).1),
            Err(SendErr::Msg(m)) => return Outcome::Failed(m),
        }
        let _ = std::fs::remove_file(&spool);
        {
            let mut st = self.lock();
            let p = self.person(&mut st, uid);
            if let Some(s) = p.file.printer.iter_mut().find(|s| s.id == printer) {
                s.jobs_sent = s.jobs_sent.saturating_add(1);
            }
            self.persist(uid, &mut st);
        }
        let Some(remote) = remote else { return Outcome::Done(String::new()) };
        self.set(job, |j| j.view.phase = Phase::Printing);
        self.follow(&s, w, uid, job, remote, fib)
    }

    /// Submit, then the spool's bytes as they come, then the end.
    fn send(&self, w: &mut Ipp, job: u64, spool: &PathBuf, prog: &Arc<(Mutex<Progress>, Condvar)>, request: ipp::Req) -> Result<(), SendErr> {
        let io = |e: io::Error| SendErr::Msg(format!("prelum-ipp: {e}"));
        w.send(&request).map_err(io)?;
        let mut f = File::open(spool).map_err(|e| SendErr::Msg(format!("spool: {e}")))?;
        let mut at = 0u64;
        let mut buf = vec![0u8; DATA_CHUNK];
        let (m, cv) = &**prog;
        let mut broken = None;
        loop {
            let (written, end) = {
                let mut p = m.lock().unwrap();
                while p.written == at && p.end.is_none() {
                    p = cv.wait(p).unwrap();
                }
                (p.written, p.end.clone())
            };
            if end.as_ref().is_some_and(|e| e.is_err()) {
                // The document failed: stop without an End, so the printer
                // never takes a partial document as whole.
                return Err(SendErr::Msg(end.unwrap().unwrap_err()));
            }
            if end.is_some() && written == at {
                self.set(job, |j| j.view.phase = Phase::Sending);
                break;
            }
            f.seek(SeekFrom::Start(at)).map_err(|e| SendErr::Msg(format!("spool: {e}")))?;
            let n = ((written - at) as usize).min(buf.len());
            f.read_exact(&mut buf[..n]).map_err(|e| SendErr::Msg(format!("spool: {e}")))?;
            if let Err(e) = w.send(&ipp::Req::Data(buf[..n].to_vec())) {
                broken = Some(e);
                break;
            }
            at += n as u64;
        }
        if broken.is_none() {
            if let Err(e) = w.send(&ipp::Req::End) {
                broken = Some(e);
            }
        }
        // The worker's answers: maybe the job's id, then Sent or why not.
        loop {
            match w.next() {
                Ok(ipp::Ev::JobCreated { id }) => {
                    self.set(job, |j| j.remote = Some(id));
                }
                Ok(ipp::Ev::Sent) if broken.is_none() => return Ok(()),
                Ok(ev) => return Err(SendErr::Ev(ev)),
                Err(e) => return Err(io(broken.unwrap_or(e))),
            }
        }
    }

    /// Follow the job at the printer until it ends (§6.3 polling).
    fn follow(&self, s: &Saved, mut w: Ipp, uid: u32, job: u64, remote: i32, fib: Option<u32>) -> Outcome {
        let began = Instant::now();
        let mut failing_since: Option<Instant> = None;
        loop {
            let pause = if began.elapsed() < Duration::from_secs(60) { POLL_FAST } else { POLL_SLOW };
            {
                let st = self.lock();
                let (st, _) = self.cv.wait_timeout_while(st, pause, |st| st.jobs.get(&job).is_some_and(|j| !j.cancel)).unwrap_or_else(|e| e.into_inner());
                if st.jobs.get(&job).is_none_or(|j| j.cancel) {
                    drop(st);
                    drop(w);
                    self.cancel_remote(s, uid, remote, fib);
                    return Outcome::Cancelled;
                }
            }
            let ev = w.ask(&ipp::Req::Job { id: remote, user: USER.into() });
            match ev {
                Ok(ipp::Ev::Job { state, impressions, .. }) => {
                    failing_since = None;
                    if let Some(n) = impressions {
                        self.set(job, |j| {
                            if j.view.of > 0 {
                                j.view.sheets = (n as u32).min(j.view.of);
                            }
                        });
                    }
                    match state {
                        JobState::Completed => return Outcome::Done(String::new()),
                        JobState::Canceled => return Outcome::Cancelled,
                        JobState::Aborted => return Outcome::Failed("the printer stopped the job".into()),
                        JobState::Stopped => {
                            // Why: the printer's own reasons (out of paper,
                            // a jam), asked of it now.
                            let a = match w.ask(&ipp::Req::Query) {
                                Ok(ipp::Ev::Printer { info, .. }) => alert_of(&clean(*info).state_reasons),
                                _ => Alert::Stopped,
                            };
                            self.alert(job, Some(a));
                            self.set(job, |j| j.view.message = stopped_words(&s.name, a));
                        }
                        _ => {
                            self.alert(job, None);
                            self.set(job, |j| j.view.message.clear());
                        }
                    }
                }
                other => {
                    let since = *failing_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > POLL_GIVE_UP {
                        return Outcome::Done("the printer stopped answering; it has the whole document".into());
                    }
                    if other.is_err() {
                        // The worker died: a new one.
                        match self.worker(s, false, false, Some(uid), fib) {
                            Ok(nw) => {
                                let ok = self.hold(job, &nw.p.stop);
                                w = nw;
                                if !ok {
                                    continue;
                                }
                            }
                            Err(_) => continue,
                        }
                    }
                }
            }
        }
    }

    fn cancel_remote(&self, s: &Saved, uid: u32, remote: i32, fib: Option<u32>) {
        if let Ok(mut w) = self.worker(s, false, false, Some(uid), fib) {
            let _ = w.ask(&ipp::Req::Cancel { id: remote, user: USER.into() });
        }
    }
}

/// Ask Arca for a one-use reference only. The IPP worker redeems it.
fn printer_login_grant(s: &Saved, uid: u32) -> Option<ipp::LoginGrant> {
    use arca_store::codec::Writer;
    use arcad::proto;
    if !s.tls { return None; }
    let (key, uuid) = (store::unhex(s.key.as_deref()?)?, s.uuid.as_deref()?);
    let user = portcullis_peer::username(uid)?;
    let mut request = Writer::new();
    request.str(&user);
    request.str(if s.persona.is_empty() { "personal" } else { &s.persona });
    request.raw(&key);
    request.str(uuid);
    let path = std::env::var_os("PRELUMD_ARCA_SOCKET")
        .unwrap_or_else(|| "/atrium/sockets/arca/arcad.sock".into());
    let mut conn = UnixStream::connect(path).ok()?;
    conn.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    conn.set_write_timeout(Some(Duration::from_secs(3))).ok()?;
    proto::write_request(&mut conn, proto::op::PRINT_LOGIN_GRANT, &request.buf).ok()?;
    let (status, value) = proto::read_reply(&mut conn).ok()?;
    if status != proto::status::OK { return None; }
    Some(ipp::LoginGrant(value.as_ref().try_into().ok()?))
}

enum SendErr {
    Ev(ipp::Ev),
    Msg(String),
}

#[derive(Default)]
struct Progress {
    format: Option<Format>,
    written: u64,
    /// Ok: the document is complete; Err: why it can't be.
    end: Option<Result<(), String>>,
}

fn open_spool(p: &PathBuf) -> io::Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(p.parent().unwrap())?;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(p)
}

/// prelum-render's frames: data to the spool, progress to the job.
fn read_render(mut sock: UnixStream, mut spool: File, prog: Arc<(Mutex<Progress>, Condvar)>, sheets: Arc<dyn Fn(u32, u32) + Send + Sync>) -> Result<(), String> {
    let (m, cv) = &*prog;
    let finish = |r: Result<(), String>| {
        let mut p = m.lock().unwrap();
        if p.end.is_none() {
            p.end = Some(r.clone());
        }
        cv.notify_all();
        r
    };
    let mut of = 0;
    let mut total = 0u64;
    loop {
        let ev = match frame::read::<render::Ev>(&mut sock) {
            Ok(Some(ev)) => ev,
            Ok(None) => return finish(Err("the document could not be prepared".into())),
            Err(e) => return finish(Err(format!("the document could not be prepared ({e})"))),
        };
        match ev {
            render::Ev::Started { sheets: n, format } => {
                of = n;
                sheets(0, n);
                m.lock().unwrap().format = Some(format);
                cv.notify_all();
            }
            render::Ev::Data(d) => {
                total += d.len() as u64;
                if total > SPOOL_CAP {
                    return finish(Err("the document is too large to print".into()));
                }
                if let Err(e) = spool.write_all(&d) {
                    return finish(Err(format!("spool: {e}")));
                }
                m.lock().unwrap().written = total;
                cv.notify_all();
            }
            render::Ev::Sheet(n) => sheets(n, of),
            render::Ev::Done { .. } => {
                let _ = spool.flush();
                return finish(Ok(()));
            }
            render::Ev::Failed(e) => return finish(Err(short(&e))),
            render::Ev::Sheets(_) | render::Ev::Preview { .. } => return finish(Err("the renderer answered out of turn".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_cannot_list_or_submit_to_a_private_personal_printer() {
        let dir = tempfile::tempdir().unwrap();
        let d = Daemon::new(Config {
            state: PathBuf::new(),
            spool: PathBuf::new(),
            sigillum: PathBuf::new(),
            policy_root: dir.path().into(),
            network: Box::new(|| "test".into()),
            mdns: None,
            sheet_uid: None,
            person_of: Box::new(|_| None),
        }, Box::new(crate::launch::Direct { bin_dir: PathBuf::new() }));
        let uid = unsafe { libc::getuid() };
        let printer = toml::from_str(r#"
            id = 1
            name = "Home"
            uri = "ipp://localhost:631/ipp/print"
            addr = "127.0.0.1:631"
            tls = false
            network = "test"
        "#).unwrap();
        d.person(&mut d.lock(), uid).file.printer = vec![printer];

        assert_eq!(d.printers(uid, Some(""))[0].id, 1);
        assert!(d.printers(uid, Some("work")).is_empty());
        let pdf = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        let err = d.submit(uid, "work", print::Print::new(1, "Report"), pdf.into()).unwrap_err();
        assert_eq!(err, (status::NOT_ALLOWED, CROSSING_REFUSED.to_owned()));
        assert!(d.jobs(uid).is_empty());

        let user = portcullis_peer::username(uid).unwrap();
        let policy = dir.path().join(user).join("policy.toml");
        std::fs::create_dir_all(policy.parent().unwrap()).unwrap();
        std::fs::write(&policy, r#"
            [personas.work]
            colour = "blue"
            [[crossings]]
            from = "work"
            to = "personal"
            kinds = ["printers"]
        "#).unwrap();
        assert_eq!(d.route(uid, "work"), Ok(None));
        assert!(d.route(uid, "missing").is_err());
        assert_eq!(d.printers(uid, Some("work"))[0].id, 1);
        let pdf = File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
        assert_eq!(d.submit(uid, "work", print::Print::new(1, "Report"), pdf.into()).unwrap_err().1,
            "This persona requires encrypted IPPS printing");
        std::fs::remove_file(policy).unwrap();
        assert!(d.printers(uid, Some("work")).is_empty());

        d.person(&mut d.lock(), uid).file.printer[0].shared = true;
        assert_eq!(d.printers(uid, Some("work"))[0].id, 1);
    }

    #[test]
    fn sharing_stays_private_until_sigillum_verifies_and_unshare_needs_no_prompt() {
        use std::os::unix::net::UnixListener;
        use sigillum_proto::client::{Method, Purpose, Refusal, Reply, Request, Strength, MAX_MESSAGE};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("sigillum.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let d = Daemon::new(Config {
            state: dir.path().into(), spool: dir.path().into(), sigillum: socket, policy_root: dir.path().into(),
            network: Box::new(|| "test".into()), mdns: None, sheet_uid: None,
            person_of: Box::new(|_| None),
        }, Box::new(crate::launch::Direct { bin_dir: PathBuf::new() }));
        let uid = unsafe { libc::getuid() };
        let user = portcullis_peer::username(uid).unwrap();
        let printer = toml::from_str(r#"
            id = 1
            name = "Home"
            uri = "ipp://localhost:631/ipp/print"
            addr = "127.0.0.1:631"
            tls = false
            network = "test"
        "#).unwrap();
        d.person(&mut d.lock(), uid).file.printer = vec![printer];

        let serve = |answer: fn(u64) -> Reply, cancel| {
            let d = Arc::clone(&d);
            std::thread::scope(|scope| {
                let enable = scope.spawn(|| d.share(uid, &print::Share { printer: 1, shared: true }));
                let until = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < until => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("Sigillum test accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let Some(Request::VerifyUser(request)) = sigillum_proto::read_msg(&mut stream, MAX_MESSAGE).unwrap() else {
                    panic!("expected printer sharing verification");
                };
                assert_eq!(request.user, user);
                assert_eq!(request.purpose, Purpose::SharePrinter { printer: "Home".into() });
                assert_eq!(request.min_strength, Strength::BiometricSecure);
                assert!(d.printers(uid, Some("work")).is_empty());
                if cancel {
                    d.share(uid, &print::Share { printer: 1, shared: false }).unwrap();
                }
                sigillum_proto::write_msg(&mut stream, &answer(request.ceremony), MAX_MESSAGE).unwrap();
                enable.join().unwrap()
            })
        };
        assert!(serve(|ceremony| Reply::Refused { ceremony, reason: Refusal::NotAllowed }, false).is_err());
        assert!(serve(|ceremony| Reply::Verified {
            ceremony, method: Method::Fingerprint, strength: Strength::BiometricPlain,
        }, false).is_err());
        assert!(serve(|ceremony| Reply::Verified {
            ceremony: ceremony + 1, method: Method::Passcode, strength: Strength::Passcode,
        }, false).is_err());
        let verified = |ceremony| Reply::Verified { ceremony, method: Method::Passcode, strength: Strength::Passcode };
        assert!(serve(verified, true).is_err());
        assert!(serve(verified, false).is_ok());
        assert!(store::load(dir.path(), uid).printer[0].shared);
        assert_eq!(d.printers(uid, Some("work"))[0].id, 1);
        drop(listener);
        d.share(uid, &print::Share { printer: 1, shared: false }).unwrap();
        assert!(!store::load(dir.path(), uid).printer[0].shared);
        assert!(d.printers(uid, Some("work")).is_empty());
    }

    #[test]
    fn typed_addresses() {
        let c = candidates("printer.lan").unwrap();
        assert_eq!(c[0], ("ipps://printer.lan:631/ipp/print".into(), true, "printer.lan:631".into()));
        assert_eq!(c[1].0, "ipp://printer.lan:631/ipp/print");
        assert_eq!(candidates("ipp://10.0.0.5:8631/ipp/print").unwrap(), vec![("ipp://10.0.0.5:8631/ipp/print".into(), false, "10.0.0.5:8631".into())]);
        assert!(candidates("http://x").is_err());
        assert!(candidates("ipp://u@x").is_err());
    }

    #[test]
    fn media_names() {
        assert!(media_ok("iso_a4_210x297mm"));
        assert!(media_ok("na_letter_8.5x11in"));
        assert!(!media_ok("letter"));
        assert!(!media_ok("iso_a4_210x297x3mm"));
    }

    #[test]
    fn plans() {
        let info = PrinterInfo {
            formats: vec!["image/pwg-raster".into(), "image/urf".into()],
            pwg_resolutions: vec![(150, 150), (300, 300), (600, 600)],
            pwg_types: vec!["sgray_8".into(), "srgb_8".into()],
            media_supported: vec!["iso_a4_210x297mm".into(), "na_letter_8.5x11in".into()],
            media_default: Some("na_letter_8.5x11in".into()),
            sides_supported: vec!["one-sided".into(), "two-sided-long-edge".into()],
            color_modes: vec!["color".into(), "monochrome".into()],
            printer_resolutions: vec![(300, 300)],
            ..Default::default()
        };
        let mut req = print::Print::new(1, "t");
        let (f, args, t) = plan(&info, false, &req).unwrap();
        assert_eq!(f, Format::PwgRaster);
        assert!(args.windows(2).any(|w| w == ["--dpi", "300"]));
        assert!(args.windows(2).any(|w| w == ["--media", "na_letter_8.5x11in"]));
        assert_eq!(t.resolution, Some((300, 300)));
        req.quality = Some(Quality::High);
        req.color = ColorMode::Monochrome;
        req.sides = Sides::TwoSidedShortEdge;
        let (_, args, t) = plan(&info, false, &req).unwrap();
        assert!(args.windows(2).any(|w| w == ["--dpi", "600"]));
        assert!(args.windows(2).any(|w| w == ["--color", "gray"]));
        assert_eq!((t.color, t.sides, t.resolution), (ColorMode::Monochrome, Sides::OneSided, None));
    }
}
