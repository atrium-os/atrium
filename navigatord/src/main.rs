//! navigatord: the Navigator's broker as a daemon (navigator-ui.md §14
//! step 2).
//!
//! - **Control socket** (`NAVIGATORD_SOCKET`): the UI opens pages by
//!   address and asks what a link does (`navigator_control::Request`). An
//!   opened page comes back as a one-shot ticket, never as bytes: the UI
//!   holds no document (navigator-ui.md §1).
//! - **Page socket** (`NAVIGATORD_PAGE_SOCKET`): the doc-viewer child
//!   (navigator-page, Limen L6) redeems the ticket for the recording and
//!   the address it was converted from.
//! - **Each page** is fetched through a per-conversion fetcher
//!   (`NAVIGATORD_FETCHD`, e.g. `jail:org.atrium.navigator.fetcher`) and
//!   converted in a one-shot jail (`NAVIGATORD_CONVERTER`, e.g.
//!   `jail:org.atrium.navigator.converter`), both started by portcullisd's
//!   `ExecInstance` on pipes navigatord passes (`worker.rs`; a command line
//!   instead runs an ordinary child, for tests). Settings come from the
//!   environment, else the bundle's `etc/navigatord.conf` (`settings.rs`),
//!   since a jail gets no environment of its caller's. The fetches
//!   relayed under the per-conversion byte budget, as the prerender driver
//!   does (backend §4.7a).
//! - **The origin** of the page the UI shows is set in frescod's strip over
//!   the chrome channel (`FRESCOD_CHROME_SOCK`, F4d): on `Hello { chrome }`
//!   navigatord opens the UI's frescod connection there and hands it over,
//!   so the UI's window is one navigatord may name. When the UI asks
//!   (`Hello { limen }`) and `NAVIGATORD_LIMEN` is set, navigatord opens the
//!   UI's limend connection too, so limend's parent is the same owner.
//!   Jailed as the Navigator app (navigator-ui.md §2.3a), navigatord is
//!   that owner, whoever runs the UI.
//!
//! - **Parked pages** (navigator-ui.md §4.4): every page opened is kept in
//!   a content-addressed store (`NAVIGATORD_STORE`, a directory; in memory
//!   without it) as its recording and a small page record binding the
//!   address it came from to that recording. `Opened` gives the UI the
//!   record's address; an `Open` that brings it back reopens that version
//!   from the store, with no fetch, if the record is for that address.
//!
//! `--unconfined-for-testing` allows what a shipped daemon refuses: the
//! converter as an ordinary process (`NAVIGATORD_PRERENDER`, the prerender
//! binary in serve mode) and pages from a fixture directory
//! (`NAVIGATORD_FIXTURES`: `<dir>/<host>/<path>`, `index.html` for a
//! directory) instead of the network.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod files;
mod portal;
mod vpn;
mod mail_sheet;
mod credential_get;
mod inbox_fixture;
mod live_page;
mod press;
mod settings;
mod worker;
mod widget;
mod widget_video;

use navigator_backend::links::SchemeAllowlist;
use navigator_backend::store::{DirStore, MemoryStore, Store};
use navigator_control::{self as ctl, AppAbout, Event, Jar, LinkAction, Request, Source, Ticket};
use navigator_cookies::{Arca as _, ArcadSocket, Broker, Fetch, Launcher, Why};
use navigator_prerender::fetch::{FetchBudget, FetchdFetcher, Reply};
use navigator_prerender::serve::{self, Ask, Job};

/// Arca's accounts for a site as the UI is told them: known kinds, clean
/// names, bounded.
fn accounts_of(v: Vec<(u8, String)>) -> Vec<ctl::Account> {
    use navigator_cookies::{ACCOUNT_PASSKEY, ACCOUNT_PASSWORD};
    v.into_iter()
        .filter_map(|(k, name)| {
            let kind = match k {
                ACCOUNT_PASSKEY => ctl::AccountKind::Passkey,
                ACCOUNT_PASSWORD => ctl::AccountKind::Password,
                _ => return None,
            };
            ctl::clean(&name, ctl::ACCOUNT_NAME_MAX).then_some(ctl::Account { kind, name })
        })
        .take(ctl::ACCOUNTS_MAX)
        .collect()
}

/// Pages kept converted, most recently used last (Back is instant).
const CACHE: usize = 16;
/// Addresses whose page record this run knows (for link checks and the
/// strip once a page has left the cache).
const KNOWN: usize = 4096;
/// The page record's format.
const RECORD: &str = "atrium-navigator-page/1";
/// How long a ticket stays redeemable.
const TICKET_LIFE: Duration = Duration::from_secs(60);
/// How long a converter has past its own deadline before its input is
/// closed, and then before it is killed (a kill leaks its jail).
const GRACE: Duration = Duration::from_secs(10);

struct Config {
    converter: Option<worker::Spec>,
    prerender: String,
    fetchd: Option<worker::Spec>,
    fixtures: Option<PathBuf>,
    unconfined: bool,
    deadline_s: u64,
    chrome: Option<PathBuf>,
    /// The UI navigatord starts as its worker (`NAVIGATORD_UI`, e.g.
    /// `jail:org.atrium.navigator.ui`; navigator-ui.md §2.3a J3): its
    /// stdin is its control channel, and navigatord ends with it.
    ui: Option<worker::Spec>,
    /// Where the UI's trail store is kept (`NAVIGATORD_TRAILS`).
    trails: Option<PathBuf>,
    /// limend's socket (`NAVIGATORD_LIMEN`): the UI's limend connection is
    /// opened here when it asks, as its frescod one is.
    limen: Option<PathBuf>,
    store: Option<PathBuf>,
    /// arcad's socket and Arca's courier launcher's: with both, a page
    /// whose site knows you is fetched through Arca's couriers.
    arca: Option<(PathBuf, String)>,
    /// Where Save writes a file itself (`NAVIGATORD_DOWNLOADS`, a
    /// development run's); otherwise portcullisd saves it into the
    /// Downloads of our persona (`SaveDownload`, personas.md §4.1).
    downloads: Option<PathBuf>,
    /// Monstrator, the link opener, that hand-offs go to with their press
    /// (`NAVIGATORD_MONSTRATOR`, default its socket; opening-links.md §7).
    monstrator: PathBuf,
    /// opifexd's socket (`NAVIGATORD_OPIFEX`, default its own): the
    /// open-an-app prompt asks it who publishes an `atrium-app:` link's app
    /// and what it asks for (navigator-ui.md §6.4). Read-only; no network.
    opifex: PathBuf,
    /// Schemes handed on (`NAVIGATORD_SCHEMES`, comma-separated, e.g.
    /// `mailto,tel`); `atrium-app` always is (navigator.md §2.2).
    schemes: SchemeAllowlist,
    /// frescod's activation socket (`NAVIGATORD_ACTIVATION`): Save and the
    /// hand-offs act only on a press spent there (`press.rs`). Required
    /// unless `--unconfined-for-testing`.
    activation: Option<PathBuf>,
    /// The captive portal's sign-in window (atrium-net.md §8.1 S3): set
    /// when portcullisd started us in the reserved `portal` persona. Every
    /// page then comes through its fetcher, and its forms may be sent.
    portal: Option<Arc<portal::Portal>>,
    /// A corporate VPN's sign-in window (vpn.md §9.3): set when
    /// portcullisd started us in the reserved `vpn` persona. Every page
    /// then comes through Arca's couriers in the profile's own jar, and its
    /// forms may be sent.
    vpn: Option<Arc<vpn::SignIn>>,
}

impl Config {
    /// Whole pages live on a press (navigator-islands.md I6.1), while it is
    /// built: `NAVIGATORD_LIVE_PAGES=1`, in a Navigator with a UI, the
    /// compositor's press guard and Limen; never in a portal or VPN window.
    fn live_pages_enabled(&self) -> bool {
        self.ui.is_some() && self.portal.is_none() && self.vpn.is_none() && self.activation.is_some()
            && self.limen.is_some() && settings::get("NAVIGATORD_LIVE_PAGES").as_deref() == Some("1")
    }

    fn inbox_enabled(&self) -> bool {
        self.ui.is_some() && self.portal.is_none() && self.vpn.is_none() && self.activation.is_some()
            && self.limen.is_some() && self.arca.is_some()
            && settings::get("NAVIGATORD_INBOX_FIXTURE").as_deref() == Some(ctl::INBOX_FIXTURE_URL)
    }

    fn from_env(unconfined: bool) -> Result<Self, String> {
        let var = settings::get;
        let mut c = Config {
            converter: var("NAVIGATORD_CONVERTER").and_then(|v| worker::Spec::parse(&v)),
            prerender: var("NAVIGATORD_PRERENDER").unwrap_or_else(|| "prerender".into()),
            fetchd: var("NAVIGATORD_FETCHD").and_then(|v| worker::Spec::parse(&v)),
            fixtures: var("NAVIGATORD_FIXTURES").map(PathBuf::from),
            unconfined,
            deadline_s: var("NAVIGATORD_DEADLINE").and_then(|v| v.parse().ok()).unwrap_or(30),
            chrome: var("FRESCOD_CHROME_SOCK").map(PathBuf::from),
            limen: var("NAVIGATORD_LIMEN").map(PathBuf::from),
            ui: var("NAVIGATORD_UI").and_then(|v| worker::Spec::parse(&v)),
            trails: var("NAVIGATORD_TRAILS").map(PathBuf::from),
            store: var("NAVIGATORD_STORE").map(PathBuf::from),
            arca: var("NAVIGATORD_ARCA").zip(var("NAVIGATORD_COURIERD")).map(|(a, c)| (PathBuf::from(a), c)),
            downloads: var("NAVIGATORD_DOWNLOADS").map(PathBuf::from),
            monstrator: var("NAVIGATORD_MONSTRATOR").map(PathBuf::from).unwrap_or_else(|| monstrator::SOCKET.into()),
            activation: var("NAVIGATORD_ACTIVATION").map(PathBuf::from),
            opifex: var("NAVIGATORD_OPIFEX").map(PathBuf::from).unwrap_or_else(|| OPIFEX_SOCKET.into()),
            portal: None,
            vpn: None,
            schemes: {
                let mut a = SchemeAllowlist::default();
                for sch in std::iter::once("atrium-app".to_string()).chain(var("NAVIGATORD_SCHEMES").unwrap_or_default().split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from)) {
                    a.allow(&sch).map_err(|e| format!("NAVIGATORD_SCHEMES: {e}"))?;
                }
                a
            },
        };
        // A test run's sign-in window, without portcullisd to ask.
        if let Some(v) = var("NAVIGATORD_VPN_SIGN_IN") {
            if !unconfined {
                return Err("NAVIGATORD_VPN_SIGN_IN is for tests (--unconfined-for-testing)".into());
            }
            c.vpn = Some(Arc::new(vpn::SignIn::parse(&v).ok_or("NAVIGATORD_VPN_SIGN_IN: <profile> <https start> <https final>")?));
        }
        if !unconfined {
            if c.activation.is_none() {
                return Err("no NAVIGATORD_ACTIVATION: refusing to save or hand on without frescod's word on the press".into());
            }
            if c.converter.is_none() {
                return Err("no NAVIGATORD_CONVERTER: refusing to run page JavaScript unconfined".into());
            }
            if c.fixtures.is_some() {
                return Err("NAVIGATORD_FIXTURES is for tests (--unconfined-for-testing)".into());
            }
        }
        if c.fixtures.is_none() && c.fetchd.is_none() {
            return Err("no NAVIGATORD_FETCHD (and no fixtures): pages have nowhere to come from".into());
        }
        Ok(c)
    }
}

/// Whose a conversion is (navigator-ui.md §2.4, §6.2): the persona's
/// jar it was opened in, and whether the site knew you (its requests
/// carried the jar's cookies). Only an anonymous Personal page is shared
/// and written; anything cookied or Unrecorded is that jar's alone and
/// kept in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Ctx {
    jar: Jar,
    knows: bool,
}

impl Ctx {
    const ANON: Ctx = Ctx { jar: Jar::Personal, knows: false };
    fn written(&self) -> bool { *self == Ctx::ANON }
}

/// A converted page.
struct Page {
    vpn_document: Option<Arc<Vec<u8>>>,
    url: String,
    recording: Vec<u8>,
    title: String,
    insecure: bool,
    /// Its page record's address in the store (empty: not stored).
    saved: String,
    ctx: Ctx,
    /// The images its recording names, as the converter fetched and
    /// verified them (content address, encoded bytes), within
    /// `ctl::images_ok`. Never decoded here: the page child does that.
    images: Arc<Vec<(String, Vec<u8>)>>,
    /// What each of its consent controls wrote when the converter pressed
    /// it (`atrium-consent:<n>`, legacy-web §5.4.1e), kept for the reader's
    /// own press. Memory only: a page from the store has none.
    consents: Arc<Vec<Vec<String>>>,
}

/// What the store holds for a page besides its recording: which address
/// that recording is the page at. A UI can only name records navigatord
/// wrote, and a record is only taken for the address it names.
#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    format: String,
    url: String,
    recording: String,
    /// (content address, store address) of each image, in order.
    #[serde(default)]
    images: Vec<(String, String)>,
    /// What each of its consent controls writes (`atrium-consent:<n>`,
    /// legacy-web §5.4.1e), so a page from the store can still be
    /// answered. The page's own script text, held as the recording is.
    #[serde(default)]
    consents: Vec<Vec<String>>,
}

struct State {
    cache: Vec<Arc<Page>>,
    tickets: HashMap<Ticket, (Arc<Page>, Instant)>,
    store: Box<dyn Store + Send>,
    /// Pages never written: each Unrecorded jar's, and Personal pages
    /// opened with the site knowing you (navigator-ui.md §2.4).
    memory: HashMap<Jar, MemoryStore>,
    /// Page address → its latest record address.
    known: HashMap<String, String>,
}

impl Default for State {
    fn default() -> Self {
        State { cache: vec![], tickets: HashMap::new(), store: Box::new(MemoryStore::default()), memory: HashMap::new(), known: HashMap::new() }
    }
}

impl State {
    /// Put a converted page in the store; returns it with its record's
    /// address (or as it was, if the store refused: it still shows).
    fn stored(&mut self, mut p: Page) -> Page {
        if p.vpn_document.is_some() { return p; }
        let written = p.ctx.written();
        let store: &mut dyn Store = if written { &mut *self.store } else { self.memory.entry(p.ctx.jar).or_default() };
        let put = store.put(&p.recording).and_then(|rec| {
            let images = p.images.iter().map(|(a, b)| store.put(b).map(|s| (a.clone(), s))).collect::<Result<Vec<_>, _>>()?;
            let r = Record { format: RECORD.into(), url: p.url.clone(), recording: rec, images, consents: (*p.consents).clone() };
            store.put(&serde_json::to_vec(&r).expect("encodes"))
        });
        match put {
            Ok(a) => {
                if written {
                    if self.known.len() >= KNOWN {
                        self.known.clear();
                    }
                    self.known.insert(p.url.clone(), a.clone());
                }
                p.saved = a;
            }
            Err(e) => eprintln!("navigatord: store: {e}"),
        }
        p
    }

    /// The page `saved` names, if it is the page at `url`.
    fn reopen(&self, url: &str, saved: &str, ctx: Ctx) -> Result<Page, String> {
        if !ctl::is_address(saved) {
            return Err("not a store address".into());
        }
        let store: &dyn Store = if ctx.written() {
            &*self.store
        } else {
            self.memory.get(&ctx.jar).ok_or("not in this jar")?
        };
        let b = store.get(saved)?;
        let r: Record = serde_json::from_slice(&b).map_err(|e| format!("not a page record: {e}"))?;
        if r.format != RECORD {
            return Err(format!("a {:?} record", r.format));
        }
        if r.url != url {
            return Err("the record is for another address".into());
        }
        let recording = store.get(&r.recording)?;
        let mut images = vec![];
        for (a, at) in &r.images {
            if !ctl::is_address(at) {
                return Err("an image's store address is malformed".into());
            }
            images.push((a.clone(), store.get(at)?));
        }
        let lens: Vec<(String, u64)> = images.iter().map(|(a, b)| (a.clone(), b.len() as u64)).collect();
        if !ctl::images_ok(&lens) {
            return Err("the record's images are out of bounds".into());
        }
        if !consents_ok(&r.consents) {
            return Err("the record's consents are out of bounds".into());
        }
        Ok(Page { vpn_document: None, title: title(&recording), insecure: url.starts_with("http:"), url: r.url, recording, saved: saved.into(), ctx, images: Arc::new(images), consents: Arc::new(r.consents) })
    }

    /// The page at `url` this UI may act on: the cache, else the store.
    fn page(&mut self, url: &str) -> Option<Arc<Page>> {
        if let Some(p) = self.cached(url, None) {
            return Some(p);
        }
        let saved = self.known.get(url)?.clone();
        let p = Arc::new(self.reopen(url, &saved, Ctx::ANON).ok()?);
        self.keep(p.clone(), url);
        Some(p)
    }

    /// The cached page at `url` converted in `ctx` (any, for `None`: the
    /// most recent, for link checks and the strip).
    fn cached(&mut self, url: &str, ctx: Option<Ctx>) -> Option<Arc<Page>> {
        let i = self.cache.iter().rposition(|p| p.url == url && ctx.is_none_or(|c| c == p.ctx))?;
        let p = self.cache.remove(i);
        self.cache.push(p.clone());
        Some(p)
    }

    fn keep(&mut self, p: Arc<Page>, asked: &str) {
        self.cache.retain(|q| !(q.url == p.url && q.ctx == p.ctx));
        self.cache.push(p.clone());
        if asked != p.url && p.vpn_document.is_none() {
            // A redirect: the asked address finds it too.
            self.cache.push(Arc::new(Page { vpn_document: p.vpn_document.clone(), url: asked.into(), recording: p.recording.clone(), title: p.title.clone(), insecure: p.insecure, saved: p.saved.clone(), ctx: p.ctx, images: p.images.clone(), consents: p.consents.clone() }));
        }
        while self.cache.len() > CACHE {
            self.cache.remove(0);
        }
    }

    /// Clearing history (navigator-ui.md §5.5): the page records named go,
    /// with each recording and image no other record in the same store
    /// names, and their pages leave the cache. Only objects that are page
    /// records are taken. Returns (records, objects) removed.
    fn clear(&mut self, records: &[String]) -> (usize, usize) {
        let mut urls = HashSet::new();
        let mut gone = HashSet::new();
        let (mut nr, mut no) = (0, 0);
        let stores = std::iter::once(&mut *self.store as &mut dyn Store).chain(self.memory.values_mut().map(|m| m as &mut dyn Store));
        for store in stores {
            let targets: Vec<(String, Record)> = records
                .iter()
                .filter(|a| ctl::is_address(a))
                .filter_map(|a| record_of(&*store, a).map(|r| (a.clone(), r)))
                .collect();
            if targets.is_empty() {
                continue;
            }
            let named: HashSet<&str> = targets.iter().map(|(a, _)| a.as_str()).collect();
            // What every other record here still names stays.
            let mut kept = HashSet::new();
            for (a, len) in store.list().unwrap_or_default() {
                if len <= RECORD_SCAN_MAX && !named.contains(a.as_str()) {
                    if let Some(r) = record_of(&*store, &a) {
                        kept.insert(r.recording);
                        kept.extend(r.images.into_iter().map(|(_, at)| at));
                    }
                }
            }
            for (a, r) in &targets {
                let objects = std::iter::once(&r.recording).chain(r.images.iter().map(|(_, at)| at));
                for o in objects.filter(|o| ctl::is_address(o) && !kept.contains(*o) && !named.contains(o.as_str())) {
                    if store.remove(o).is_ok() {
                        no += 1;
                    }
                }
                if store.remove(a).is_ok() {
                    nr += 1;
                }
                urls.insert(r.url.clone());
                gone.insert(a.clone());
            }
        }
        self.cache.retain(|p| !urls.contains(&p.url) && !gone.contains(&p.saved));
        self.tickets.retain(|_, (p, _)| !urls.contains(&p.url));
        self.known.retain(|u, a| !urls.contains(u) && !gone.contains(a));
        (nr, no)
    }

    /// An Unrecorded jar closed, or a site was forgotten in a jar: its
    /// private pages go (`site` `None`: all of the jar's).
    fn forget(&mut self, jar: Jar, site: Option<&str>) {
        let gone = |p: &Page| p.ctx.jar == jar && (p.ctx.knows || site.is_none()) && site.is_none_or(|s| site_of(&p.url).as_deref() == Some(s));
        self.cache.retain(|p| !gone(p));
        self.tickets.retain(|_, (p, _)| !gone(p));
        if site.is_none() {
            self.memory.remove(&jar);
        }
    }

    fn ticket(&mut self, p: Arc<Page>) -> Ticket {
        let now = Instant::now();
        self.tickets.retain(|_, (_, t)| now.duration_since(*t) < TICKET_LIFE);
        let mut r = [0u8; 12];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut r)).expect("/dev/urandom");
        // Our uid first: the page child redeems it at our socket, not
        // another instance's (personas.md §3).
        let t = ctl::ticket_for(unsafe { libc::getuid() }, r);
        self.tickets.insert(t, (p, now));
        t
    }
}

/// The page record at `a` in `store`, if that is what it holds.
fn record_of(store: &dyn Store, a: &str) -> Option<Record> {
    let b = store.get(a).ok()?;
    let r: Record = serde_json::from_slice(&b).ok()?;
    (r.format == RECORD).then_some(r)
}

/// Objects larger than this are not read as page records when clearing
/// (a record names at most `IMAGES_MAX` images: well under it).
const RECORD_SCAN_MAX: u64 = 256 << 10;

// ---------------------------------------------------------------------------
// Fetching and converting.
// ---------------------------------------------------------------------------

/// The broker's cookie state and its clients (navigator-cookies), shared
/// by every conversion.
type Cookies = Arc<Mutex<Broker<ArcadSocket, Launcher>>>;

/// The site (registrable domain) of a page, as Arca partitions go.
fn site_of(url: &str) -> Option<String> {
    navigator_cookies::partition_of(url)
}

fn jar_id(jar: Jar) -> Option<u64> {
    match jar {
        Jar::Personal => None,
        Jar::Unrecorded(j) => Some(j),
    }
}

static VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

enum Upstream {
    /// A fetcher, and its jail's portcullisd connection when it is one
    /// (dropped after the fetcher, so its input closes first).
    Fetchd(FetchdFetcher, #[allow(dead_code)] Option<worker::Worker>),
    Fixtures(PathBuf),
    /// A page whose site knows you: its https requests through Arca's
    /// couriers with the broker's tickets; the rest the plain way.
    /// In a sign-in window, the first ask may be a form's POST of these
    /// bytes, from a page on this host.
    Cookied { cookies: Cookies, view: u64, host: String, navigated: bool, form: Option<FormSend>, plain: Option<Box<Upstream>> },
    /// The sign-in window's fetcher; the first navigation is a form's POST
    /// of these bytes when there are any.
    Portal(Arc<portal::Portal>, Option<Vec<u8>>),
}

impl Upstream {
    fn reply(&mut self, ask: Ask) -> Reply {
        if let Upstream::Cookied { cookies, view, host, navigated, form, plain } = self {
            let url = ask.url().to_string();
            // The first ask is the page itself (the reader's navigation, or
            // the form they sent); the converter's are its subresources.
            let f = match form.take() {
                Some((body, from, secret)) if !*navigated => Fetch { secret, ..Fetch::form(&url, &from, body) },
                _ if *navigated => Fetch::subresource(&url, host),
                _ => Fetch::navigation(&url, None, true),
            };
            *navigated = true;
            let got = cookies.lock().unwrap().fetch(*view, &f);
            return match got {
                Ok(r) => {
                    let body = match ask {
                        Ask::Peek { start, len, .. } => r.body[start.min(r.body.len())..(start + len).min(r.body.len())].to_vec(),
                        Ask::Fetch(_) => r.body,
                    };
                    Reply::Ok { status: r.status, rest: format!("0 {} {}", body.len(), r.url), body }
                }
                Err(e) if e.plain_instead() => match plain {
                    Some(fetcher) => fetcher.reply(ask),
                    None => Reply::Err(format!("through Arca: {e:?}")),
                },
                Err(e) => Reply::Err(format!("through Arca: {e:?}")),
            };
        }
        match self {
            Upstream::Cookied { .. } => unreachable!(),
            Upstream::Fetchd(f, _) => match ask {
                Ask::Fetch(u) => f.exchange(u).unwrap_or_else(Reply::Err),
                Ask::Peek { url, start, len } => f.exchange_peek(url, start, len).unwrap_or_else(Reply::Err),
            },
            Upstream::Portal(p, _) => match ask {
                Ask::Fetch(u) => p.fetch(u, None, None),
                Ask::Peek { url, start, len } => p.fetch(url, Some((start, len)), None),
            },
            Upstream::Fixtures(dir) => {
                let url = ask.url();
                match fixture(dir, url) {
                    Some(b) => {
                        let b = match ask {
                            Ask::Peek { start, len, .. } => b[start.min(b.len())..(start + len).min(b.len())].to_vec(),
                            Ask::Fetch(_) => b,
                        };
                        Reply::Ok { status: 200, rest: format!("0 {} {url}", b.len()), body: b }
                    }
                    None => Reply::Ok { status: 404, rest: format!("0 0 {url}"), body: vec![] },
                }
            }
        }
    }
}

impl Upstream {
    /// The page itself (a navigation): its reply and, fetched the plain
    /// way, the chain it went through (legacy-web §5.4.1e). A cookied
    /// view's chain is the broker's, and reports none.
    fn navigate(&mut self, url: &str) -> (Reply, Vec<navigator_cookies::Hop>) {
        let hop = |(status, set_cookie, url): (u16, bool, String)| navigator_cookies::Hop { url, status, set_cookie };
        match self {
            Upstream::Fetchd(f, _) => match f.exchange_navigation(url) {
                Ok((r, hops)) => (r, hops.into_iter().map(hop).collect()),
                Err(e) => (Reply::Err(e), vec![]),
            },
            // A stand-in chain beside a fixture: `<page>.chain`, one hop a
            // line, `<status> <0|1> <url>`; the last hop's status answers.
            Upstream::Fixtures(dir) => match fixture_chain(dir, url) {
                Some(hops) => {
                    let last = hops.last().map_or(404, |h| h.status);
                    let at = hops.last().map_or(url.to_string(), |h| h.url.clone());
                    (Reply::Ok { status: last, rest: format!("{} 0 {at}", hops.len().saturating_sub(1)), body: vec![] }, hops)
                }
                None => (self.reply(Ask::Fetch(url)), vec![]),
            },
            Upstream::Cookied { .. } => (self.reply(Ask::Fetch(url)), vec![]),
            Upstream::Portal(p, form) => (p.fetch(url, None, form.take()), vec![]),
        }
    }
}

/// A fixture's stand-in redirect chain (`<page>.chain`), bounded.
fn fixture_chain(dir: &Path, url: &str) -> Option<Vec<navigator_cookies::Hop>> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    if host.contains("..") || u.path().contains("..") {
        return None;
    }
    let p = dir.join(host).join(format!("{}.chain", u.path().trim_start_matches('/')));
    let text = std::fs::read_to_string(p).ok()?;
    let hops: Vec<_> = text
        .lines()
        .take(16)
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(3, ' ').collect();
            Some(navigator_cookies::Hop { status: f.first()?.parse().ok()?, set_cookie: *f.get(1)? == "1", url: f.get(2)?.to_string() })
        })
        .collect();
    (!hops.is_empty()).then_some(hops)
}

/// A fixture page: `<dir>/<host>/<path>`, `index.html` for a directory.
fn fixture(dir: &Path, url: &str) -> Option<Vec<u8>> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    if !matches!(u.scheme(), "https" | "http") || host.contains("..") {
        return None;
    }
    let mut p = dir.join(host);
    for seg in u.path_segments()? {
        if seg == ".." || seg.contains('/') {
            return None;
        }
        if !seg.is_empty() {
            p.push(seg);
        }
    }
    if p.is_dir() {
        p.push("index.html");
    }
    std::fs::read(p).ok()
}

static INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn instance() -> String {
    format!("{}{:05}", std::process::id(), INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn upstream(cfg: &Config, inst: &str) -> Result<Upstream, String> {
    if let Some(p) = &cfg.portal {
        return Ok(Upstream::Portal(p.clone(), None));
    }
    if let Some(d) = &cfg.fixtures {
        return Ok(Upstream::Fixtures(d.clone()));
    }
    match cfg.fetchd.as_ref().ok_or("no fetcher")? {
        worker::Spec::Command(p, a) => {
            let a: Vec<String> = a.iter().map(|x| x.replace("{instance}", inst)).collect();
            FetchdFetcher::spawn(p, &a).map(|f| Upstream::Fetchd(f, None)).map_err(|e| format!("fetcher: {e}"))
        }
        spec => {
            let mut w = worker::Worker::start(spec, &format!("f{inst}"), false).map_err(|e| format!("fetcher: {e}"))?;
            let (i, o) = (w.stdin.take().expect("piped"), w.stdout.take().expect("piped"));
            Ok(Upstream::Fetchd(FetchdFetcher::over(i, o, ""), Some(w)))
        }
    }
}

/// Fetch and convert `url`; through Arca as the jar's, with `cookies`.
/// What an address turned out to be.
enum Got {
    Page(Page),
    /// Not a document (files.rs): kept for the person to save, never
    /// converted.
    File { url: String, what: &'static str, ext: &'static str, body: Vec<u8> },
    /// Fetched without cookies, the page failed the site's own cookie
    /// check (`navigator_cookies::site_needs_cookies`): the site needs to
    /// know you to show it. Carries the failure as it would be told.
    NeedsCookies(String),
}

fn convert(cfg: &Config, url: &str, ctx: Ctx, cookies: Option<(Cookies, u64)>) -> Result<Got, String> {
    convert_with(cfg, url, ctx, cookies, None)
}

/// [`convert`]; with `form`, the page is the answer to a form's POST of
/// those bytes to `url`, from a page on that host (the sign-in windows
/// only: the portal's through its fetcher, a VPN's through Arca's couriers).
/// A form to POST: its encoded fields, the host of the page it came from,
/// and where in the body a password typed into Arca's field goes (Arca's
/// handle; the courier puts the value there).
type FormSend = (Vec<u8>, String, Option<(usize, [u8; 16])>);

fn convert_with(cfg: &Config, url: &str, ctx: Ctx, cookies: Option<(Cookies, u64)>, form: Option<FormSend>) -> Result<Got, String> {
    let inst = instance();
    let jar = cookies.clone();
    let mut up = if let Some((cookies, view)) = cookies {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default();
        // A VPN sign-in has exactly one network path, through its profile's couriers.
        let plain = if cfg.vpn.is_some() { None } else { Some(Box::new(upstream(cfg, &inst)?)) };
        Upstream::Cookied { cookies, view, host, navigated: false, form, plain }
    } else {
        if cfg.vpn.is_some() {
            return Err("the VPN sign-in needs its courier jar".into());
        }
        let mut up = upstream(cfg, &inst)?;
        if let Upstream::Portal(_, body) = &mut up {
            if form.as_ref().is_some_and(|f| f.2.is_some()) {
                return Err("a password from Arca's field goes only through Arca's couriers".into());
            }
            *body = form.map(|(bytes, _, _)| bytes);
        } else if form.is_some() {
            return Err("a form is sent only from a sign-in window".into());
        }
        up
    };
    let mut budget = FetchBudget::new(FetchBudget::DEFAULT);
    let mut hops = vec![];
    let (base, doc) = match budget.admit(url, |u| {
        let (r, h) = up.navigate(u);
        hops = h;
        r
    }) {
        // A portal may answer its sign-in page as 511 (RFC 6585 §6).
        Reply::Ok { status, rest, body } if (200..300).contains(&status) || (status == 511 && cfg.portal.is_some()) => {
            let fin = rest.splitn(3, ' ').nth(2).filter(|f| !f.is_empty()).unwrap_or(url).to_string();
            (fin, body)
        }
        Reply::Ok { status, .. } if navigator_cookies::site_needs_cookies(&hops) => return Ok(Got::NeedsCookies(format!("the site answered {status}"))),
        Reply::Ok { status, .. } => return Err(format!("the site answered {status}")),
        Reply::Err(e) if navigator_cookies::site_needs_cookies(&hops) => return Ok(Got::NeedsCookies(e)),
        Reply::Err(e) => return Err(e),
    };
    let vpn_worker = cfg.vpn.is_some() && !cfg.unconfined;
    if vpn_worker && (doc.is_empty() || doc.len() > 2 * 1024 * 1024) {
        return Err("VPN sign-in document outside the worker size bound".into());
    }
    if let Some((what, ext)) = files::kind(&doc) {
        return Ok(Got::File { url: base, what, ext, body: doc });
    }
    // ★ A cookied view's scripts start with its script-readable cookies
    // (legacy-web §5.4.1e step 6); what they write stays in the converter.
    let script_cookies = match &jar {
        Some((c, view)) => match c.lock().unwrap().script_cookies(*view, &base) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("navigatord: no document.cookie for {base}: {e:?}");
                Some(String::new())
            }
        },
        None => None,
    };
    let job = Job {
        base: Some(base.clone()),
        cookies: script_cookies,
        net: true,
        emit: true,
        blobs: true,
        require_confined: !cfg.unconfined,
        deadline_s: cfg.deadline_s,
        len: doc.len(),
        ..Job::default()
    };
    let header = job.header()?;
    // Serve mode, no arguments, when there is no converter jail (testing).
    let spec = cfg.converter.clone().unwrap_or_else(|| worker::Spec::Command(cfg.prerender.clone(), vec![]));
    let mut ch = worker::Worker::start(&spec, &format!("conv{inst}"), true).map_err(|e| format!("converter: {e}"))?;
    let stdin: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(ch.stdin.take()));
    let stdout = ch.stdout.take().expect("piped");
    let mut stderr = ch.stderr.take().expect("captured");
    // Drained while it runs: a chatty child must not block on a full pipe.
    let err_t = std::thread::spawn(move || {
        let mut v = vec![];
        let _ = stderr.read_to_end(&mut v);
        v
    });
    // The deadline is watched beside the relay (the fetcher stays on this
    // thread): past it the converter's input is closed, then it is killed.
    let deadline = Duration::from_secs(cfg.deadline_s);
    let stdin_w = stdin.clone();
    let watch = std::thread::spawn(move || -> bool {
        let start = Instant::now();
        loop {
            match ch.try_wait() {
                Ok(true) => return true,
                Ok(false) if start.elapsed() > deadline + GRACE => {
                    drop(stdin_w.lock().unwrap().take());
                    let t = Instant::now();
                    while t.elapsed() < GRACE {
                        if let Ok(true) = ch.try_wait() {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    ch.kill();
                    return false;
                }
                Ok(false) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => return false,
            }
        }
    });
    let mut images: Vec<(String, Vec<u8>)> = vec![];
    let outcome = (|| -> Result<serve::Outcome, String> {
        {
            let mut g = stdin.lock().unwrap();
            let w = g.as_mut().ok_or("the converter is gone")?;
            w.write_all(header.as_bytes()).and_then(|_| w.write_all(&doc)).and_then(|_| w.flush())
                .map_err(|e| format!("could not hand the document to the converter: {e}"))?;
        }
        let mut out = std::io::BufReader::new(stdout);
        // Images the recording names, already checked against their
        // addresses by the driver; kept within the page's bounds, the rest
        // dropped (the page shows their boxes empty).
        let r = serve::drive(&mut out, &stdin, &mut |ask| budget.admit(ask.url(), |_| up.reply(ask)).to_frame(), &mut |a, b| {
            let total: u64 = images.iter().map(|(_, x): &(String, Vec<u8>)| x.len() as u64).sum();
            if images.len() < ctl::IMAGES_MAX && total + b.len() as u64 <= ctl::IMAGE_BYTES_MAX
                && ctl::is_image_address(a) && !images.iter().any(|(x, _)| x == a) {
                images.push((a.to_string(), b.to_vec()));
            }
        });
        drop(stdin.lock().unwrap().take());
        r
    })();
    let finished = watch.join().unwrap_or(false);
    let err = err_t.join().unwrap_or_default();
    if !finished {
        return Err(format!("the conversion timed out at {} s", cfg.deadline_s));
    }
    let o = outcome.map_err(|e| format!("converter protocol: {e}"))?;
    let recording = o.recording.or(o.first).ok_or_else(|| {
        let tail: String = String::from_utf8_lossy(&err).lines().rev().take(3).collect::<Vec<_>>().join(" | ");
        format!("the converter sent no recording ({tail})")
    })?;
    if recording.len() as u64 > ctl::RECORDING_MAX {
        return Err("the recording is over its bound".into());
    }
    let vpn_document = if vpn_worker {
        use limen::sign_in::{VpnDocument, VPN_JS_LOGIN, VPN_JS_PATH, VPN_JS_URL};
        let mut script = Vec::new();
        let declared = navigator_backend::links::attr_spelling("src", VPN_JS_PATH);
        if base.split('?').next() == Some(VPN_JS_LOGIN) && recording.windows(declared.len()).any(|w| w == declared) {
            let (broker, view) = jar.as_ref().ok_or("missing VPN profile jar")?;
            let ask = Fetch::subresource(VPN_JS_URL, "sso.crypta.test");
            let r = broker.lock().unwrap().fetch_once(*view, &ask).map_err(|e| format!("VPN script: {e:?}"))?;
            let mut types = r.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("content-type"));
            let javascript = types.next().is_some_and(|(_, v)| v.split(';').next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/javascript"))) && types.next().is_none();
            if r.status != 200 || r.url != VPN_JS_URL || r.redirects != 0 || !javascript || r.body.is_empty() || r.body.len() > 65536 {
                return Err("unexpected VPN script response".into());
            }
            script = r.body;
        }
        Some(Arc::new(VpnDocument { html: &doc, script: &script }.encode().map_err(|e| e.to_string())?))
    } else { None };
    let insecure = base.starts_with("http:");
    let consents = consents_of(&o.report);
    Ok(Got::Page(Page { vpn_document, title: title(&recording), url: base, recording, insecure, saved: String::new(), ctx, images: Arc::new(images), consents: Arc::new(consents) }))
}

/// The converter's `CW <n> <line>` report lines: what each consent control
/// wrote, bounded as the converter bounds them.
fn consents_of(report: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![];
    for l in report.lines() {
        let mut f = l.splitn(3, '\t');
        let (Some("CW"), Some(n), Some(line)) = (f.next(), f.next(), f.next()) else { continue };
        let Ok(n) = n.parse::<usize>() else { continue };
        if n >= CONSENTS_MAX || n > out.len() || line.len() > 4096 || line.chars().any(char::is_control) {
            continue;
        }
        if n == out.len() {
            out.push(vec![]);
        }
        if out[n].len() < 16 {
            out[n].push(line.to_string());
        }
    }
    out
}

/// Within the converter's bounds: at most [`CONSENTS_MAX`] controls, 16
/// lines each, 4 KiB a line, no control characters.
fn consents_ok(c: &[Vec<String>]) -> bool {
    c.len() <= CONSENTS_MAX && c.iter().all(|ls| ls.len() <= 16 && ls.iter().all(|l| l.len() <= 4096 && !l.chars().any(char::is_control)))
}

/// The most consent controls a page keeps (the converter's own bound).
const CONSENTS_MAX: usize = 4;
/// A consent control's mark in a recording (the converter's).
const CONSENT_SCHEME: &str = "atrium-consent:";

/// The reader pressed a consent control (navigator-ui.md §6.2, legacy-web
/// §5.4.1e): what the converter saw it write goes to Arca in the jar, as
/// the page's own script cookies. Only this press writes; the converter's
/// own press of it never left the converter.
fn consent(page: &Page, href: &str, jar: Jar, cookies: Option<&Cookies>) -> LinkAction {
    let blocked = |why: &str| LinkAction::Blocked { why: why.into() };
    let Some(n) = href.strip_prefix(CONSENT_SCHEME).and_then(|n| n.parse::<usize>().ok()) else { return blocked("not a consent control") };
    if !page.recording.windows(href.len()).any(|w| w == href.as_bytes()) {
        return blocked("the page does not carry this control");
    }
    let Some(lines) = page.consents.get(n) else { return blocked("this page's answer is not held here; reload it and press again") };
    let Some(c) = cookies else { return blocked("Arca isn't running here") };
    let Ok(u) = url::Url::parse(&page.url) else { return blocked("not an address") };
    let (Some(host), Some(part)) = (u.host_str().map(str::to_ascii_lowercase), navigator_cookies::partition_in(jar_id(jar), &page.url)) else {
        return blocked("not an address");
    };
    if u.scheme() != "https" {
        return blocked("only an https page may keep cookies");
    }
    let port = u.port_or_known_default().unwrap_or(443);
    let path = if u.path().is_empty() { "/" } else { u.path() };
    let mut kept = 0;
    for l in lines.iter() {
        match c.lock().unwrap().arca.script_set_cookie(&host, port, &part, path, l) {
            Ok(()) => kept += 1,
            Err(e) => eprintln!("navigatord: {part}: a consent's cookie not kept: {e}"),
        }
    }
    eprintln!("navigatord: consent {n} pressed on {}: {kept} of {} cookies kept in {part}", page.url, lines.len());
    if kept == 0 {
        return blocked("Arca kept none of its cookies");
    }
    LinkAction::Consented { url: page.url.clone() }
}

/// The document's `<title>` from the recording's JSON text, bounded.
fn title(rec: &[u8]) -> String {
    let s = String::from_utf8_lossy(&rec[..rec.len().min(4 << 20)]);
    let lower = s.to_ascii_lowercase();
    let Some(a) = lower.find("<title") else { return String::new() };
    let Some(b) = lower[a..].find('>').map(|i| a + i + 1) else { return String::new() };
    let Some(c) = lower[b..].find("</title").map(|i| b + i) else { return String::new() };
    let t = s[b..c].replace("\\n", " ").replace("\\t", " ").replace("\\\"", "\"").replace("\\\\", "\\")
        .replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&");
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for ch in t.chars().filter(|c| !c.is_control()) {
        if out.len() + ch.len_utf8() > ctl::TITLE_MAX {
            break;
        }
        out.push(ch);
    }
    out
}

/// What frescod's strip shows for `url`: `scheme://host[:port]`, the host
/// in punycode, or nothing when it has a character the strip refuses.
fn origin(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?;
    let o = match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    };
    o.bytes().all(fresco_split::chrome::origin_char).then_some(o)
}

// ---------------------------------------------------------------------------
// The UI's connection.
// ---------------------------------------------------------------------------

/// The strip: the chrome channel, the UI's connection handle and window,
/// and which request the UI waits on (only its answer sets the origin).
#[derive(Default)]
struct Strip {
    chan: Option<(fresco_split::chan::Chan, u32)>,
    window: Option<u32>,
    window_token: Option<[u8; 16]>,
    latest: u32,
    widget_document: Option<Ticket>,
    // A live page can be reshown after its redemption ticket has been spent.
    widget_records: std::collections::VecDeque<(Ticket, String, String)>,
    /// The VPN sign-in documents delivered on this connection, by `Open`
    /// id, newest last: a page's security-key request takes its origin
    /// from here (vpn.md §9.3a).
    vpn_documents: std::collections::VecDeque<(u32, String)>,
}

/// The WebAuthn origin of a delivered VPN document: https only.
fn vpn_origin(url: &str) -> Option<sigillum_webauthn::Origin> {
    let url = url::Url::parse(url).ok().filter(|u| u.scheme() == "https")?;
    sigillum_webauthn::Origin::parse(&url.origin().ascii_serialization()).filter(|o| o.is_secure())
}

/// A VPN page's resource request (vpn.md "Scripts in the sign-in window"):
/// the address, checked against the delivered document's URL. Only that
/// document's own HTTPS origin, with no credentials or fragment in the
/// address; returns the document's host for the courier's initiator.
fn vpn_resource_target(document: &str, url: &str) -> Result<String, &'static str> {
    if url.len() > limen::sign_in::VPN_RESOURCE_URL_MAX || !ctl::clean(url, limen::sign_in::VPN_RESOURCE_URL_MAX) {
        return Err("the address is out of shape");
    }
    let doc = url::Url::parse(document).ok().filter(|u| u.scheme() == "https").ok_or("the page is not an HTTPS document")?;
    let target = url::Url::parse(url).map_err(|_| "not an address")?;
    if target.scheme() != "https" || !target.username().is_empty() || target.password().is_some() || target.fragment().is_some() {
        return Err("only a plain HTTPS address");
    }
    if target.origin() != doc.origin() {
        return Err("only the page's own origin");
    }
    if target.as_str() != url {
        return Err("the address is not in its normal spelling");
    }
    // The page process asks, and it is not trusted to say what a load is
    // for: only a script or stylesheet file goes out with the jar's
    // cookies, never an API path a page's fetch() names (I6.0 saw /api/me).
    let path = target.path().to_ascii_lowercase();
    if ![".js", ".mjs", ".css"].iter().any(|e| path.ends_with(e)) {
        return Err("only a .js, .mjs or .css file");
    }
    doc.host_str().map(str::to_ascii_lowercase).ok_or("the page has no host")
}

/// A refused resource's host and registrable site, for the log: bounded,
/// and only the host, never the path or query.
fn refused_target(url: &str) -> String {
    let parsed = url::Url::parse(url).ok().filter(|_| url.len() <= limen::sign_in::VPN_RESOURCE_URL_MAX);
    let Some(host) = parsed.as_ref().and_then(|u| u.host_str()) else { return "host unreadable".into() };
    let host: String = host.chars().filter(|c| c.is_ascii_graphic()).take(253).collect();
    let site = site_of(url).map(|s| s.chars().filter(|c| c.is_ascii_graphic()).take(253).collect::<String>()).unwrap_or_default();
    format!("host {host}, site {site}")
}

/// What a fetched VPN page resource is: a script or a stylesheet by its one
/// content type, within the size bound; anything else is refused.
fn vpn_resource_kind(status: u16, headers: &[(String, String)], body: &[u8]) -> Result<ctl::VpnResourceKind, &'static str> {
    if status != 200 {
        return Err("the site did not answer 200");
    }
    if body.len() > limen::sign_in::VPN_RESOURCE_MAX {
        return Err("the resource is over its bound");
    }
    let mut types = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    let (Some((_, value)), None) = (types.next(), types.next()) else { return Err("not one content type") };
    let mime = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match mime.as_str() {
        "text/javascript" | "application/javascript" | "application/x-javascript" | "text/ecmascript" | "application/ecmascript" => Ok(ctl::VpnResourceKind::Script),
        "text/css" => Ok(ctl::VpnResourceKind::Stylesheet),
        _ => Err("neither a script nor a stylesheet"),
    }
}

/// The connection's one security-key ceremony, retired when it goes.
struct Ceremony(Arc<Mutex<Option<credential_get::Pending>>>);

impl Drop for Ceremony {
    fn drop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

impl Strip {
    fn set(&self, origin: &str) {
        let (Some((c, handle)), Some(window)) = (&self.chan, self.window) else { return };
        use fresco_split::chrome::ChromeMsg;
        if let Err(e) = c.send_raw(&ChromeMsg::SetOrigin { handle: *handle, window, origin: origin.into() }.encode(), &[]) {
            eprintln!("navigatord: chrome: {e}");
        }
    }
}

fn open_chrome(path: &Path) -> std::io::Result<(fresco_split::chan::Chan, u32, std::os::fd::OwnedFd)> {
    use fresco_split::chan::{self, Raw};
    use fresco_split::chrome::ChromeMsg;
    let c = chan::connect(path)?;
    c.send_raw(&ChromeMsg::OpenClient.encode(), &[])?;
    match c.recv_raw()? {
        Raw::Rec(rec, mut fds) if fds.len() == 1 => match ChromeMsg::decode(&rec) {
            Ok(ChromeMsg::Opened { handle }) => Ok((c, handle, fds.pop().unwrap())),
            other => Err(std::io::Error::other(format!("chrome: unexpected {other:?}"))),
        },
        _ => Err(std::io::Error::other("chrome: no connection")),
    }
}

/// Links other apps open in the Navigator (opening-links.md §5): the
/// UIs to tell, newest last, and links that came before any UI did.
struct Incoming {
    uis: Vec<std::sync::Weak<Mutex<UnixStream>>>,
    /// Each with the persona it was sent from, if another.
    held: Vec<(String, Option<String>)>,
}

static INCOMING: Mutex<Incoming> = Mutex::new(Incoming { uis: Vec::new(), held: Vec::new() });

/// At most this many links wait for a UI.
const HELD_MAX: usize = 16;

/// A link from another app: to the newest UI, else held for the first.
fn incoming_link(url: String, from: Option<String>) {
    let mut g = INCOMING.lock().unwrap_or_else(|e| e.into_inner());
    g.uis.retain(|w| w.strong_count() > 0);
    match g.uis.last().and_then(std::sync::Weak::upgrade) {
        Some(w) => send(&w, &Event::Incoming { url, from }),
        None if g.held.len() < HELD_MAX => g.held.push((url, from)),
        None => eprintln!("navigatord: a link from another app dropped: {HELD_MAX} already wait for the window"),
    }
}

/// A UI past its Hello: it gets the links from now on, and those held.
fn ui_ready(w: &Arc<Mutex<UnixStream>>) {
    let mut g = INCOMING.lock().unwrap_or_else(|e| e.into_inner());
    g.uis.push(Arc::downgrade(w));
    for (url, from) in std::mem::take(&mut g.held) {
        send(w, &Event::Incoming { url, from });
    }
}

/// Whether a link from another app is one the Navigator opens: a web
/// address, nothing else (D5), and one the Where field would take.
fn web_link(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://"))
        && url.len() <= atrium_open::MAX_LINK
        && url.bytes().all(|b| b.is_ascii_graphic())
}

/// The document channel portcullisd made for the Navigator: links that
/// other apps open here (`[[links]] schemes = ["http", "https"]`). Each
/// goes to the UI, which opens it in a trail of its own.
fn document_channel() {
    std::thread::spawn(|| loop {
        let mut ch = match atrium_open::Channel::connect() {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                eprintln!("navigatord: the document channel: {e}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        eprintln!("navigatord: taking links from other apps");
        loop {
            match ch.recv_any(None) {
                Ok(Some(atrium_open::Message::Link(url))) if web_link(&url) => {
                    eprintln!("navigatord: a link from another app: a new trail");
                    incoming_link(url, None);
                }
                Ok(Some(atrium_open::Message::Crossed { url, from })) if web_link(&url) => {
                    eprintln!("navigatord: a link the person sent from {from}: a new trail");
                    incoming_link(url, Some(from));
                }
                Ok(Some(atrium_open::Message::Crossed { .. })) => eprintln!("navigatord: a link from another persona refused: not a web address"),
                Ok(Some(atrium_open::Message::Link(_))) => eprintln!("navigatord: a link from another app refused: not a web address"),
                Ok(Some(atrium_open::Message::File(_))) => eprintln!("navigatord: a file on the document channel: the Navigator opens none"),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("navigatord: the document channel: {e}");
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

fn send(w: &Mutex<UnixStream>, e: &Event) {
    let mut s = w.lock().unwrap();
    if let Err(err) = ctl::write_frame(&mut *s, e) {
        eprintln!("navigatord: UI: {err}");
    }
}

fn serve_ui(cfg: Arc<Config>, state: Arc<Mutex<State>>, cookies: Option<Cookies>, s: UnixStream) {
    let w = Arc::new(Mutex::new(s.try_clone().expect("clone")));
    let strip = Arc::new(Mutex::new(Strip::default()));
    // Files waiting for Save, and links decided as hand-offs, on this
    // connection only.
    let waiting = Arc::new(Mutex::new(files::Waiting::default()));
    let mut handed: Vec<String> = Vec::new();
    // Save and hand-offs: the press, spent with frescod as a press on a
    // window of the UI's connection: ours when we opened it over the chrome
    // channel (navigator-ui.md §2.3a), else the UI's own.
    let presenter = Arc::new(Mutex::new(press::peer_uid(&s)));
    let presenter_s = presenter.clone();
    let press_strip = strip.clone();
    let mut widget: Option<widget::Widget> = None;
    // The page running live (I6.1), and the page on screen: (the address
    // the UI showed, the page's own address as delivered).
    let mut live: Option<live_page::Live> = None;
    let mut shown: Option<(String, String)> = None;
    // The page the UI opened last, in the foreground: a first open never
    // sends Show, so Run live must accept it too (I6.1 gate, 2026-11-25).
    let mut opened: Option<String> = None;
    let mut presses = cfg.activation.as_ref().map(press::Frescod::new);
    let mut spend = move |p: ctl::Press| -> Result<(), String> {
        let ui_uid = *presenter_s.lock().unwrap();
        match (&mut presses, ui_uid) {
            (Some(f), Some(uid)) => {
                let window = press_strip.lock().unwrap().window_token.ok_or("the UI has no window")?;
                f.spend(p, uid, window)
            }
            (Some(_), None) => Err("the UI's uid is unknown".into()),
            // Testing only (Config refuses this otherwise).
            (None, _) => Ok(()),
        }
    };
    let (mut trail_buf, mut trail_over): (Option<String>, bool) = (None, false);
    let ceremony = Ceremony(Arc::new(Mutex::new(None)));
    let mut r = s;
    loop {
        let req: Request = match ctl::read_frame(&mut r) {
            Ok(Some(q)) => q,
            Ok(None) => return,
            Err(e) => {
                eprintln!("navigatord: UI: {e}");
                return;
            }
        };
        let wants_tier = matches!(req, Request::OpenWithRecordingTier { .. });
        let wants_islands = wants_tier || matches!(req, Request::OpenWithIslands { .. });
        match req {
            Request::Hello { chrome, limen, measure, trails } => {
                let opened = match (&cfg.chrome, chrome) {
                    (Some(p), true) => match open_chrome(p) {
                        Ok(o) => Some(o),
                        Err(e) => {
                            eprintln!("navigatord: chrome channel {}: {e}", p.display());
                            None
                        }
                    },
                    _ => None,
                };
                match opened {
                    Some((c, handle, fd)) => {
                        strip.lock().unwrap().chan = Some((c, handle));
                        *presenter.lock().unwrap() = Some(unsafe { libc::geteuid() });
                        let s = w.lock().unwrap();
                        let ok = ctl::write_frame(&mut &*s, &Event::Chrome { fd: true }).and_then(|_| ctl::send_fd(&s, fd.as_raw_fd()));
                        if let Err(e) = ok {
                            eprintln!("navigatord: UI: {e}");
                            return;
                        }
                    }
                    None => send(&w, &Event::Chrome { fd: false }),
                }
                // limend, on the UI's behalf: the parent limend sees is then
                // the owner of the UI's windows (us), whoever runs the UI.
                if limen {
                    let opened = cfg.limen.as_ref().map(|p| limen::client::navigator_ui_channel(p).map_err(|e| eprintln!("navigatord: limend {}: {e}", p.display())));
                    match opened {
                        Some(Ok(l)) => {
                            let s = w.lock().unwrap();
                            let ok = ctl::write_frame(&mut &*s, &Event::Limen { fd: true }).and_then(|_| ctl::send_fd(&s, l.as_raw_fd()));
                            if let Err(e) = ok {
                                eprintln!("navigatord: UI: {e}");
                                return;
                            }
                        }
                        _ => send(&w, &Event::Limen { fd: false }),
                    }
                }
                // A second frescod connection, for measuring text: a UI in
                // its jail reaches frescod only through us.
                if measure {
                    let opened = cfg.chrome.as_ref().and_then(|p| open_chrome(p).map_err(|e| eprintln!("navigatord: measure connection: {e}")).ok());
                    match opened {
                        Some((_chan, _handle, fd)) => {
                            let s = w.lock().unwrap();
                            let ok = ctl::write_frame(&mut &*s, &Event::Measure { fd: true }).and_then(|_| ctl::send_fd(&s, fd.as_raw_fd()));
                            if let Err(e) = ok {
                                eprintln!("navigatord: UI: {e}");
                                return;
                            }
                        }
                        None => send(&w, &Event::Measure { fd: false }),
                    }
                }
                if trails {
                    let text = cfg.trails.as_ref().and_then(|p| read_trails(p));
                    match &text {
                        Some(t) => {
                            let ps = ctl::parts(t);
                            let n = ps.len();
                            for (i, p) in ps.into_iter().enumerate() {
                                send(&w, &Event::Trails { part: Some(p.to_string()), last: i + 1 == n });
                            }
                        }
                        None => send(&w, &Event::Trails { part: None, last: true }),
                    }
                }
                ui_ready(&w);
            }
            // The sign-in window keeps no trails.
            Request::SaveTrails { .. } if cfg.portal.is_some() => {}
            Request::SaveTrails { part, last } => {
                // Kept whole or not at all: a store over its bound is refused
                // rather than cut.
                if trail_buf.as_ref().is_some_and(|b: &String| b.len() + part.len() > ctl::TRAILS_MAX) {
                    trail_buf = None;
                    trail_over = true;
                } else if !trail_over {
                    trail_buf.get_or_insert_with(String::new).push_str(&part);
                }
                if last {
                    let why = match (trail_buf.take(), &cfg.trails) {
                        _ if std::mem::take(&mut trail_over) => Some(format!("the trail store is over {} bytes", ctl::TRAILS_MAX)),
                        (Some(t), Some(p)) => write_trails(p, &t).err().map(|e| format!("{}: {e}", p.display())),
                        (None, Some(p)) => write_trails(p, "").err().map(|e| format!("{}: {e}", p.display())),
                        (_, None) => Some("navigatord keeps no trail store (no NAVIGATORD_TRAILS)".into()),
                    };
                    if let Some(why) = why {
                        eprintln!("navigatord: trails not saved: {why}");
                        send(&w, &Event::TrailsNotSaved { why });
                    }
                }
            }
            Request::Window { window } => {
                let mut st = strip.lock().unwrap();
                if st.window_token.is_none() { st.window = Some(window); }
            }
            Request::RegisterWindow { window, token } => {
                let mut st = strip.lock().unwrap();
                if st.window_token.is_none() && token != [0; 16] {
                    st.window = Some(window);
                    st.window_token = Some(token);
                }
            }
            Request::Blank => {
                widget = None;
                live = None;
                shown = None;
                opened = None;
                let mut st = strip.lock().unwrap();
                st.latest = 0;
                st.widget_document = None;
                st.set("");
            }
            Request::Show { ref url } | Request::ShowDocument { ref url, .. } => {
                widget = None;
                let doc = match req { Request::ShowDocument { document, .. } => Some(document), _ => None };
                let p = state.lock().unwrap().page(&url);
                // Leaving the page ends its live engine (I6.1).
                shown = p.as_ref().map(|p| (url.clone(), p.url.clone()));
                opened = None;
                if live.as_ref().is_some_and(|l| shown.as_ref().is_none_or(|(_, page)| *page != l.url)) {
                    live = None;
                }
                let mut st = strip.lock().unwrap();
                st.latest = 0;
                st.widget_document = doc.filter(|t| st.widget_records.iter().any(|(d,u,_)| t == d && u == url));
                st.set(&p.and_then(|p| origin(&p.url)).unwrap_or_default());
            }
            Request::Forget { jar, url } => {
                let site = url.as_deref().and_then(site_of);
                if url.is_some() && site.is_none() {
                    continue;
                }
                state.lock().unwrap().forget(jar, site.as_deref());
                let Some(c) = &cookies else { continue };
                // The jar's partition for that site, or the whole jar's prefix.
                let (part, prefix) = match (jar, &url) {
                    (_, Some(u)) => (navigator_cookies::partition_in(jar_id(jar), u), false),
                    (Jar::Unrecorded(j), None) => (Some(navigator_cookies::jar_prefix(j)), true),
                    (Jar::Personal, None) => (None, false),
                };
                if let Some(part) = part {
                    match c.lock().unwrap().arca.forget(&part, prefix) {
                        Ok(n) => eprintln!("navigatord: forgot {part}{} ({n} cookie groups)", if prefix { "*" } else { "" }),
                        Err(e) => eprintln!("navigatord: forgetting {part}: {e}"),
                    }
                }
            }
            Request::SignOut { jar, url } => {
                let reply = |cookies: Option<u32>, why: Option<String>| send(&w, &Event::SignedOut { url: url.clone(), cookies, why });
                let Some(part) = ctl::clean(&url, ctl::URL_MAX).then(|| navigator_cookies::partition_in(jar_id(jar), &url)).flatten() else {
                    reply(None, Some("not an address".into()));
                    continue;
                };
                let Some(c) = &cookies else {
                    reply(None, Some("Arca isn't running here".into()));
                    continue;
                };
                let r = c.lock().unwrap().arca.sign_out(&part);
                match r {
                    Ok(n) => {
                        eprintln!("navigatord: signed out of {part} ({n} session cookies)");
                        reply(Some(n), None);
                    }
                    Err(e) => {
                        eprintln!("navigatord: signing out of {part}: {e}");
                        reply(None, Some(e));
                    }
                }
            }
            Request::EndSession { jar, url } => {
                let Some(part) = ctl::clean(&url, ctl::URL_MAX).then(|| navigator_cookies::partition_in(jar_id(jar), &url)).flatten() else { continue };
                let Some(c) = &cookies else { continue };
                let r = c.lock().unwrap().arca.sign_out(&part);
                match r {
                    Ok(n) => eprintln!("navigatord: the last page of {part} went: {n} session cookies ended"),
                    Err(e) => eprintln!("navigatord: ending the session of {part}: {e}"),
                }
            }
            Request::Credential { page, id, registering, options } => {
                let reply = |error: &str| send(&w, &Event::Credential { id, registering, json: None, error: Some(error.into()) });
                if cfg.vpn.is_none() || options.len() > ctl::CREDENTIAL_OPTIONS_MAX {
                    reply("NotAllowedError");
                    continue;
                }
                let origin = strip.lock().unwrap().vpn_documents.iter().rev().find(|(p, _)| *p == page).and_then(|(_, url)| vpn_origin(url));
                let Some(origin) = origin else {
                    eprintln!("navigatord: a security-key request for a page this window wasn't given");
                    reply("SecurityError");
                    continue;
                };
                let mut slot = ceremony.0.lock().unwrap();
                if slot.is_some() {
                    drop(slot);
                    reply("NotAllowedError");
                    continue;
                }
                match credential_get::Pending::start(id, &origin, &options, registering) {
                    Err(e) => {
                        drop(slot);
                        eprintln!("navigatord: the VPN page's security-key request was refused ({})", e.name());
                        reply(e.name());
                    }
                    Ok(pending) => {
                        eprintln!("navigatord: the VPN page asked for a security key ({}); Sigillum asks the person", if registering { "registering" } else { "signing in" });
                        *slot = Some(pending);
                        drop(slot);
                        let (slot, w) = (ceremony.0.clone(), w.clone());
                        std::thread::spawn(move || loop {
                            std::thread::sleep(Duration::from_millis(50));
                            let mut s = slot.lock().unwrap();
                            let Some(result) = s.as_ref().filter(|p| p.id == id).map(|p| p.poll()) else { return };
                            let Some(result) = result else { continue };
                            s.take();
                            drop(s);
                            let (json, error) = match result {
                                Ok(json) => (Some(json), None),
                                Err(e) => (None, Some(e.name().to_string())),
                            };
                            eprintln!("navigatord: the security-key request {}", if json.is_some() { "was answered" } else { "failed" });
                            send(&w, &Event::Credential { id, registering, json, error });
                            return;
                        });
                    }
                }
            }
            Request::CredentialAbort { id } => {
                ceremony.0.lock().unwrap().take_if(|p| p.id == id);
            }
            Request::VpnResource { page, id, url } => {
                // The refused host and its site, for the I6.0 measurement of
                // what whole pages reach (navigator-islands.md D12).
                let target = refused_target(&url);
                let refuse = |why: &str| {
                    eprintln!("navigatord: a VPN page's resource refused: {why} ({target})");
                    send(&w, &Event::VpnResource { id, kind: None, why: Some(why.into()) });
                };
                let (Some(v), Some(c)) = (cfg.vpn.clone(), cookies.clone()) else {
                    refuse("only a VPN sign-in window fetches a page's resources");
                    continue;
                };
                if !v.is_current() {
                    refuse("the VPN sign-in attempt ended");
                    continue;
                }
                let document = strip.lock().unwrap().vpn_documents.iter().rev().find(|(p, _)| *p == page).map(|(_, u)| u.clone());
                let Some(document) = document else {
                    refuse("a resource for a page this window wasn't given");
                    continue;
                };
                let host = match vpn_resource_target(&document, &url) {
                    Ok(h) => h,
                    Err(why) => {
                        refuse(why);
                        continue;
                    }
                };
                let w = w.clone();
                std::thread::spawn(move || {
                    let reply = |kind, why: Option<String>| send(&w, &Event::VpnResource { id, kind, why });
                    let fetched = {
                        let mut b = c.lock().unwrap();
                        b.show_sign_in(vpn::VIEW, &v.start_url, &v.profile)
                            .and_then(|_| b.fetch_once(vpn::VIEW, &Fetch::subresource(&url, &host)))
                    };
                    let r = match fetched {
                        Ok(r) => r,
                        Err(e) => {
                            eprintln!("navigatord: a VPN page's resource failed: {e:?}");
                            reply(None, Some(format!("{e:?}")));
                            return;
                        }
                    };
                    if r.url != url || r.redirects != 0 {
                        reply(None, Some("the resource was redirected".into()));
                        return;
                    }
                    let kind = match vpn_resource_kind(r.status, &r.headers, &r.body) {
                        Ok(k) => k,
                        Err(why) => {
                            eprintln!("navigatord: a VPN page's resource refused: {why}");
                            reply(None, Some(why.into()));
                            return;
                        }
                    };
                    eprintln!("navigatord: a VPN page's {kind:?} from its own origin ({} bytes)", r.body.len());
                    let result = limen::fdpass::sealed_file("vpn-sign-in-resource", &r.body).and_then(|fd| {
                        let socket = w.lock().unwrap();
                        ctl::write_frame(&mut &*socket, &Event::VpnResource { id, kind: Some(kind), why: None })?;
                        ctl::send_fd(&socket, std::os::fd::AsRawFd::as_raw_fd(&fd))
                    });
                    if let Err(e) = result {
                        eprintln!("navigatord: a VPN page's resource not handed over: {e}");
                    }
                });
            }
            Request::Accounts { url } => {
                let reply = |accounts: Vec<ctl::Account>, why: Option<String>| send(&w, &Event::Accounts { url: url.clone(), accounts, why });
                let Some(site) = ctl::clean(&url, ctl::URL_MAX).then(|| site_of(&url)).flatten() else {
                    reply(vec![], Some("not an address".into()));
                    continue;
                };
                let Some(c) = &cookies else {
                    reply(vec![], Some("Arca isn't running here".into()));
                    continue;
                };
                let r = c.lock().unwrap().arca.accounts(&site);
                match r {
                    Ok(v) => reply(accounts_of(v), None),
                    Err(e) => reply(vec![], Some(e)),
                }
            }
            Request::Clear { records } => {
                if records.len() > ctl::CLEAR_MAX {
                    continue;
                }
                let (r, o) = state.lock().unwrap().clear(&records);
                eprintln!("navigatord: cleared {r} page records and {o} objects from the store");
            }
            Request::Open { id, url, saved, jar, knows, fresh }
            | Request::OpenWithIslands { id, url, saved, jar, knows, fresh }
            | Request::OpenWithRecordingTier { id, url, saved, jar, knows, fresh } => {
                if !ctl::clean(&url, ctl::URL_MAX) {
                    send(&w, &Event::Failed { id, url: String::new(), why: "not an address".into() });
                    continue;
                }
                widget = None;
                // Opening another page leaves this one, and its live engine.
                live = None;
                shown = None;
                opened = Some(url.clone());
                {
                    let mut st = strip.lock().unwrap();
                    st.latest = id;
                    st.widget_document = None;
                }
                let (cfg, state, w, strip, cookies, waiting) = (cfg.clone(), state.clone(), w.clone(), strip.clone(), cookies.clone(), waiting.clone());
                std::thread::spawn(move || {
                    let t = Instant::now();
                    // The site knowing you needs Arca; without it the page
                    // opens anonymously, and the UI is told why.
                    let mut note = None;
                    let cookied = match (knows || cfg.vpn.is_some(), &cookies) {
                        // A VPN sign-in window's every page is in its jar.
                        (_, Some(c)) if cfg.vpn.is_some() => {
                            let v = cfg.vpn.as_ref().expect("vpn");
                            match c.lock().unwrap().show_sign_in(vpn::VIEW, &v.start_url, &v.profile) {
                                Ok(()) => Some((c.clone(), vpn::VIEW)),
                                Err(e) => {
                                    send(&w, &Event::Failed { id, url: url.clone(), why: format!("the sign-in's jar: {e:?}") });
                                    return;
                                }
                            }
                        }
                        (false, _) => None,
                        (true, None) => {
                            note = Some("this site can't remember you: Arca isn't running here".to_string());
                            None
                        }
                        (true, Some(c)) => {
                            let view = VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let mut b = c.lock().unwrap();
                            match b.show_in(view, &url, jar_id(jar)).and_then(|_| b.set_cookied(view, Why::Reader)) {
                                Ok(()) => Some((c.clone(), view)),
                                Err(e) => {
                                    note = Some(format!("this site can't remember you here ({e:?})"));
                                    None
                                }
                            }
                        }
                    };
                    let mut ctx = Ctx { jar, knows: cookied.is_some() };
                    let mut view = cookied.as_ref().map(|(_, v)| *v);
                    // The site asked (§6.2): set when its cookie check failed
                    // without cookies and the page opened again with them.
                    let mut asked = false;
                    let hit = if fresh { None } else { state.lock().unwrap().cached(&url, Some(ctx)) };
                    let saved = saved.filter(|_| !fresh);
                    let page = match hit {
                        Some(p) => Ok((p, Source::Cache)),
                        None => {
                            let back = saved.as_deref().map(|a| state.lock().unwrap().reopen(&url, a, ctx));
                            match back {
                                Some(Ok(p)) => {
                                    let p = Arc::new(p);
                                    state.lock().unwrap().keep(p.clone(), &url);
                                    Ok((p, Source::Store))
                                }
                                other => {
                                    if let Some(Err(e)) = other {
                                        eprintln!("navigatord: {url}: not reopened from the store ({e}); fetching");
                                    }
                                    // A SAML POST binding's start: its form, posted once
                                    // through the profile's couriers, as the gateway's page
                                    // would have (its script is never run).
                                    let start = cfg.vpn.as_ref().filter(|v| v.start_url == url).map(|v| (vpn::start_post(&v.start_url), v.final_url.clone()));
                                    let mut got = match start {
                                        Some((Ok(Some((target, body))), fin)) if !vpn::START_SENT.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                                            let from = url::Url::parse(&fin).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default();
                                            eprintln!("navigatord: the VPN sign-in starts with the gateway's SAML form, posted to {} ({} bytes)", origin(&target).unwrap_or_default(), body.len());
                                            convert_with(&cfg, &target, ctx, cookied, Some((body, from, None)))
                                        }
                                        Some((Ok(Some(_)), _)) => Err("the sign-in's request was sent once; start the sign-in again from Settings".into()),
                                        Some((Err(why), _)) => Err(why),
                                        _ => convert(&cfg, &url, ctx, cookied),
                                    };
                                    if let Ok(Got::NeedsCookies(failed)) = &got {
                                        // The backend's own rule (legacy-web §5.4.1e): the
                                        // view turns cookied, in this page's jar, and the
                                        // UI is told, so the change is never silent.
                                        let failed = failed.clone();
                                        got = match &cookies {
                                            None => Err(format!("{failed}: this site needs to know you (cookies) to show it, and Arca isn't running here")),
                                            Some(c) => {
                                                let v = VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                                let r = {
                                                    let mut b = c.lock().unwrap();
                                                    b.show_in(v, &url, jar_id(jar)).and_then(|_| b.set_cookied(v, Why::SiteNeedsIt))
                                                };
                                                match r {
                                                    Ok(()) => {
                                                        eprintln!("navigatord: {url}: the site's cookie check failed without cookies ({failed}); opening it with the site knowing you");
                                                        ctx = Ctx { jar, knows: true };
                                                        view = Some(v);
                                                        asked = true;
                                                        note = Some("this site needs to know you to show this page".into());
                                                        convert(&cfg, &url, ctx, Some((c.clone(), v)))
                                                    }
                                                    Err(e) => Err(format!("{failed}: this site needs to know you, and that can't be done here ({e:?})")),
                                                }
                                            }
                                        };
                                    }
                                    match got {
                                        Ok(Got::NeedsCookies(failed)) => Err(failed),
                                        Ok(Got::Page(p)) => {
                                            let mut st = state.lock().unwrap();
                                            let p = Arc::new(st.stored(p));
                                            st.keep(p.clone(), &url);
                                            Ok((p, Source::Fetched))
                                        }
                                        Ok(Got::File { url: at, what, ext, body }) => {
                                            if let (Some(v), Some(c)) = (view, &cookies) {
                                                c.lock().unwrap().close(v);
                                            }
                                            let name = files::name(&at, ext);
                                            let size = body.len() as u64;
                                            let origin = origin(&at).unwrap_or_default();
                                            let held = files::Held { url: at.clone(), name: name.clone(), bytes: body, since: Instant::now() };
                                            match waiting.lock().unwrap().hold(id, held) {
                                                Ok(()) => {
                                                    eprintln!("navigatord: {at} is a file ({what}, {size} bytes): asking");
                                                    send(&w, &Event::File { id, url: at, name, size, origin, kind: what.to_string() });
                                                }
                                                Err(why) => send(&w, &Event::Failed { id, url: at, why }),
                                            }
                                            return;
                                        }
                                        Err(e) => Err(e),
                                    }
                                }
                            }
                        }
                    };
                    if let (Some(v), Some(c)) = (view.filter(|&v| v != vpn::VIEW), &cookies) {
                        c.lock().unwrap().close(v);
                    }
                    let ms = t.elapsed().as_millis().min(u32::MAX as u128) as u32;
                    match page {
                        Ok((p, from)) => {
                            eprintln!("navigatord: opened {} in {ms} ms ({} bytes, {from:?}{})", p.url, p.recording.len(),
                                if ctx.knows { ", the site knowing you" } else { "" });
                            let ticket = state.lock().unwrap().ticket(p.clone());
                            {
                                let st = strip.lock().unwrap();
                                if st.latest == id {
                                    st.set(&origin(&p.url).unwrap_or_default());
                                }
                            }
                            if wants_islands {
                                let meta = serde_json::from_slice::<RecordingMetadata>(&p.recording).ok();
                                if wants_tier && meta.as_ref().is_some_and(|m| m.tier == Some(2)) {
                                    send(&w, &Event::ReplayedRecording { id });
                                }
                                let sources = meta.map_or_else(Vec::new, |m| m.island_sources(&p.url));
                                if ctx == Ctx::ANON && cfg.vpn.is_none() && cfg.portal.is_none() {
                                    if let Some((source, _)) = sources.iter().find(|(src, _)|
                                        widget::enabled_source(&p.url, src).is_some()) {
                                        let mut st = strip.lock().unwrap();
                                        if st.widget_records.len() >= 64 { st.widget_records.pop_front(); }
                                        st.widget_records.push_back((ticket, p.url.clone(), source.clone()));
                                        if st.latest == id { st.widget_document = Some(ticket); }
                                        drop(st);
                                        send(&w, &Event::RunnableWidget { id, source: source.clone() });
                                    }
                                }
                                if !sources.is_empty() { send(&w, &Event::IslandSources { id, sources }); }
                            }
                            if let Some(html) = &p.vpn_document {
                                {
                                    let mut st = strip.lock().unwrap();
                                    if st.vpn_documents.len() >= 16 { st.vpn_documents.pop_front(); }
                                    st.vpn_documents.push_back((id, p.url.clone()));
                                }
                                let result = limen::fdpass::sealed_file("vpn-sign-in-document", html).and_then(|fd| {
                                    let socket = w.lock().unwrap();
                                    ctl::write_frame(&mut &*socket, &Event::VpnDocument { id, url: p.url.clone() })?;
                                    ctl::send_fd(&socket, std::os::fd::AsRawFd::as_raw_fd(&fd))
                                });
                                if let Err(error) = result {
                                    send(&w, &Event::Failed { id, url: p.url.clone(), why: error.to_string() });
                                    return;
                                }
                            }
                            send(&w, &Event::Opened { id, url: p.url.clone(), title: p.title.clone(), insecure: p.insecure, ticket, ms, saved: p.saved.clone(), from, knows: ctx.knows, asked, note });
                            // The gateway's final page: the sign-in is over
                            // (its token, if it set one, is Arca's now).
                            if cfg.vpn.as_ref().is_some_and(|v| v.ends_at(&p.url)) {
                                eprintln!("navigatord: the VPN sign-in reached its final page; the window closes");
                                std::thread::spawn(|| {
                                    std::thread::sleep(vpn::LINGER);
                                    std::process::exit(0);
                                });
                            }
                        }
                        Err(why) => {
                            eprintln!("navigatord: {url}: {why}");
                            send(&w, &Event::Failed { id, url, why });
                        }
                    }
                });
            }
            Request::Activate { id, page, href, jar } => {
                let p = state.lock().unwrap().page(&page);
                let action = match p {
                    None => LinkAction::Blocked { why: "that page is not open".into() },
                    Some(p) if href.starts_with(CONSENT_SCHEME) => consent(&p, &href, jar, cookies.as_ref()),
                    Some(p) => activate(&p, &href, &cfg.schemes),
                };
                let action = match action {
                    LinkAction::Handoff { url, scheme, .. } if scheme == "atrium-app" => {
                        let app = app_id(&url).and_then(|id| app_about(&cfg.opifex, id));
                        LinkAction::Handoff { url, scheme, app }
                    }
                    a => a,
                };
                if let LinkAction::Handoff { url, .. } = &action {
                    if handed.len() >= 32 {
                        handed.remove(0);
                    }
                    handed.push(url.clone());
                }
                send(&w, &Event::Link { id, action });
            }
            Request::SubmitForm { id, page, jar, action, post, fields, press } => {
                let refuse = |why: String| {
                    eprintln!("navigatord: a form not sent: {why}");
                    send(&w, &Event::Link { id, action: LinkAction::Blocked { why } });
                };
                if cfg.portal.is_none() && cfg.vpn.is_none() {
                    refuse("only a sign-in window sends a page's form".into());
                    continue;
                }
                if cfg.vpn.as_ref().is_some_and(|v| !v.is_current()) {
                    refuse("the VPN sign-in attempt ended".into());
                    continue;
                }
                if !ctl::form_ok(&action, &fields) {
                    refuse("the form is out of shape".into());
                    continue;
                }
                let Some(p) = state.lock().unwrap().page(&page) else {
                    refuse("that page is not open".into());
                    continue;
                };
                let target = match form_target(&p, &action, &fields, &cfg.schemes) {
                    Ok(t) => t,
                    Err(why) => {
                        refuse(why);
                        continue;
                    }
                };
                if let Err(why) = spend(press) {
                    refuse(why);
                    continue;
                }
                // A password typed into Arca's field: only a VPN sign-in
                // window has one, and only in a POST through its couriers.
                let arca = fields.iter().position(|f| f.arca.is_some());
                if arca.is_some() && (cfg.vpn.is_none() || !post) {
                    refuse("a password from Arca's field goes only with a sign-in window's POST".into());
                    continue;
                }
                if arca.is_some() && !url::Url::parse(&p.url).ok().zip(url::Url::parse(&target).ok())
                    .is_some_and(|(page, target)| page.scheme() == "https" && page.origin() == target.origin()) {
                    refuse("a trusted password goes only to the HTTPS origin displaying its field".into());
                    continue;
                }
                // Values a VPN page's script set (vpn.md "Scripts in the
                // sign-in window"): only a VPN window's POST, and only to the
                // HTTPS origin of the page whose script set them.
                if fields.iter().any(|f| f.scripted) && (cfg.vpn.is_none() || !post || !same_https_origin(&p.url, &target)) {
                    refuse("a page script's form goes only from a VPN sign-in window to that page's HTTPS origin".into());
                    continue;
                }
                let secret_of = arca.and_then(|i| fields[i].arca);
                let pairs: Vec<(String, String)> = fields.into_iter().map(|f| (f.name, f.value)).collect();
                if !post {
                    match portal::get_target(&target, &pairs) {
                        Some(url) => {
                            eprintln!("navigatord: a form sent as a GET to {}", origin(&url).unwrap_or_default());
                            send(&w, &Event::Link { id, action: LinkAction::Navigate { insecure: url.starts_with("http:"), url } });
                        }
                        None => refuse("the form's address isn't one".into()),
                    }
                    continue;
                }
                // A POST: its answer converted as a page in the trail's jar,
                // then opened from the cache as the link it leads to.
                let (cfg, state, w, cookies) = (cfg.clone(), state.clone(), w.clone(), cookies.clone());
                let from = url::Url::parse(&p.url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)).unwrap_or_default();
                std::thread::spawn(move || {
                    // The fields in order; Arca's password, when there is one,
                    // goes in at its field's place, by the courier.
                    let (body, secret) = match (arca, secret_of) {
                        (Some(i), Some(h)) => {
                            let mut b = portal::encode(&pairs[..=i]);
                            let at = b.len();
                            if i + 1 < pairs.len() {
                                b.push('&');
                                b.push_str(&portal::encode(&pairs[i + 1..]));
                            }
                            (b.into_bytes(), Some((at, h)))
                        }
                        _ => (portal::encode(&pairs).into_bytes(), None),
                    };
                    // A VPN sign-in's form goes through Arca's couriers, in
                    // the profile's jar; the portal's through its fetcher.
                    let jarred = match (&cfg.vpn, &cookies) {
                        (Some(v), Some(c)) => match c.lock().unwrap().show_sign_in(vpn::VIEW, &v.start_url, &v.profile) {
                            Ok(()) => Some((c.clone(), vpn::VIEW)),
                            Err(e) => {
                                send(&w, &Event::Link { id, action: LinkAction::Blocked { why: format!("the sign-in's jar: {e:?}") } });
                                return;
                            }
                        },
                        _ => None,
                    };
                    let ctx = Ctx { jar, knows: jarred.is_some() };
                    eprintln!("navigatord: a form sent as a POST to {} ({} bytes)", origin(&target).unwrap_or_default(), body.len());
                    let action = match convert_with(&cfg, &target, ctx, jarred, Some((body, from, secret))) {
                        Ok(Got::Page(p)) => {
                            let mut st = state.lock().unwrap();
                            let p = Arc::new(st.stored(Page { ctx, ..p }));
                            st.keep(p.clone(), &p.url.clone());
                            LinkAction::Navigate { insecure: p.url.starts_with("http:"), url: p.url.clone() }
                        }
                        Ok(Got::File { .. }) => LinkAction::Blocked { why: "the network answered the form with a file".into() },
                        Ok(Got::NeedsCookies(why)) | Err(why) => LinkAction::Blocked { why: format!("the form wasn't taken: {why}") },
                    };
                    send(&w, &Event::Link { id, action });
                });
            }
            Request::StartWidget { document, source, rect, press } => {
                let grant = {
                    let st = strip.lock().unwrap();
                    st.widget_records.iter().find(|(d, _, src)| *d == document && src == &source)
                        .filter(|_| st.widget_document == Some(document))
                        .and_then(|(_, base, src)| st.window_token.zip(widget::enabled_source(base, src)))
                };
                let result = (|| {
                    let (window, source) = grant.ok_or("widget no longer belongs to the displayed recording")?;
                    widget::bounds(rect)?;
                    if cfg.activation.is_none() { return Err("no compositor guard".into()); }
                    spend(press)?;
                    widget = None;
                    let granted_origin = origin(source).ok_or("invalid island grant origin")?;
                    let on_attach = w.clone();
                    let on_unload = w.clone();
                    let on_media_ready = w.clone();
                    let writer = w.clone();
                    widget::Widget::start(cfg.limen.as_deref().ok_or("no Limen service")?, source, window, document, press, rect, cfg.clone(),
                        move || send(&on_attach, &Event::WidgetAttached { document, press, origin: granted_origin }),
                        move || send(&on_media_ready, &Event::WidgetMediaReady { document, start: press }),
                        move |query, pending| send(&on_unload, &Event::WidgetUnload { document, start: press, query, pending }),
                        move |failure| send(&writer, &match failure {
                            Some(failure) => Event::WidgetFailed { document, press, failure },
                            None => Event::WidgetStopped { document, press },
                        }))
                })();
                match result {
                    Ok(started) => { widget = Some(started); eprintln!("navigatord: widget started"); }
                    Err(why) => {
                        eprintln!("navigatord: widget start: {why}");
                        send(&w, &Event::WidgetFailed { document, press, failure: ctl::WidgetFailure::Start });
                    }
                }
            }
            Request::SeekWidgetChapter { document, start, seconds_bits } => {
                if let (Some(active), Some(seek)) = (widget.as_ref().filter(|w| w.document == document && w.press == start),
                    limen::web_island::ChapterSeek::new(f64::from_bits(seconds_bits))) {
                    if let Err(e) = active.seek(seek) { eprintln!("navigatord: chapter seek refused: {e}"); }
                }
            }
            Request::WidgetUnloadAnswer { document, start, query, leave, press } => {
                if let Some(active) = widget.as_ref().filter(|a| a.document == document && a.press == start) {
                    let accepted = leave && spend(press).is_ok();
                    if let Err(err) = active.answers.try_send(limen::web_island::UnloadAnswer { query, leave: accepted }) {
                        eprintln!("navigatord: widget unload answer: {err}");
                    }
                }
            }
            Request::PlaceWidget { document, rect } => {
                if let Some(active) = widget.as_mut().filter(|a| a.document == document) {
                    match rect {
                        None => widget = None,
                        Some(r) => if let Err(why) = active.place(r) {
                            let press = active.press;
                            widget = None;
                            eprintln!("navigatord: {why}");
                            send(&w, &Event::WidgetFailed { document, press, failure: ctl::WidgetFailure::Exit });
                        }
                    }
                }
            }
            Request::RunPageLive { url, rect, press } => {
                let page = shown.as_ref().filter(|(asked, page)| *asked == url || *page == url).map(|(_, page)| page.clone())
                    .or_else(|| opened.as_ref().filter(|o| **o == url).and_then(|_| state.lock().unwrap().page(&url)).map(|p| p.url.clone()));
                let window = strip.lock().unwrap().window_token;
                let result = (|| {
                    if !cfg.live_pages_enabled() { return Err("running a page live is off".to_string()); }
                    let page = page.ok_or("that page is no longer on screen")?;
                    let window = window.ok_or("no window to run it in")?;
                    widget::bounds(rect)?;
                    // A press frescod didn't count is asked again, not ended.
                    if let Err(why) = spend(press) { return Err(format!("\0{why}")); }
                    live = None;
                    widget = None;
                    let (on_start, on_end) = (w.clone(), w.clone());
                    let (start_url, end_url) = (page.clone(), page.clone());
                    live_page::Live::start(cfg.limen.as_deref().ok_or("no Limen service")?, window, page, press, rect, cfg.clone(),
                        move |origin| send(&on_start, &Event::LiveStarted { url: start_url, origin }),
                        move |why| send(&on_end, &Event::LiveEnded { url: end_url, why }))
                })();
                match result {
                    Ok(l) => live = Some(l),
                    Err(why) => match why.strip_prefix('\0') {
                        Some(why) => send(&w, &Event::PressRefused { why: why.into() }),
                        None => {
                            eprintln!("navigatord: run live: {why}");
                            send(&w, &Event::LiveEnded { url, why: Some(why) });
                        }
                    },
                }
            }
            Request::PlaceLive { url, rect } => {
                if let Some(active) = live.as_ref().filter(|l| l.url == url) {
                    match rect {
                        None => live = None,
                        Some(r) => if let Err(why) = active.place(r) {
                            live = None;
                            send(&w, &Event::LiveEnded { url, why: Some(why) });
                        }
                    }
                }
            }
            Request::RunLive { url, jar, press } => {
                if !cfg.inbox_enabled() || url != ctl::INBOX_FIXTURE_URL || jar != ctl::Jar::Personal {
                    send(&w, &Event::LivePageFailed { why: "Live mode is unavailable for this page or jar".into() });
                    continue;
                }
                if let Err(why) = spend(press) {
                    send(&w, &Event::PressRefused { why });
                    continue;
                }
                if let (Some(socket), Some((arca, launcher))) = (&cfg.limen, &cfg.arca) {
                    let broker = Broker::new(ArcadSocket { path: arca.clone() }, Launcher::new(launcher.clone()));
                    let w = w.clone();
                    inbox_fixture::start(socket.clone(), broker, move |why| send(&w, &Event::LivePageFailed { why }));
                }
            }
            Request::Save { id, press } => {
                if !waiting.lock().unwrap().holds(id) {
                    send(&w, &Event::NotSaved { id, why: "that file is no longer held".into() });
                    continue;
                }
                // A refused press leaves the file held: the person may press
                // again once the row is clear.
                if let Err(why) = spend(press) {
                    eprintln!("navigatord: save {id} refused: {why}");
                    send(&w, &Event::PressRefused { why });
                    continue;
                }
                let Some(h) = waiting.lock().unwrap().take(id) else { continue };
                match &cfg.downloads {
                    Some(dir) => match files::save(dir, &h) {
                        Ok(path) => {
                            eprintln!("navigatord: saved {} as {}", h.url, path.display());
                            send(&w, &Event::Saved { id, path: path.display().to_string() });
                        }
                        Err(why) => send(&w, &Event::NotSaved { id, why }),
                    },
                    // portcullisd writes it, off this thread: a big file
                    // takes a moment.
                    None => {
                        let w = w.clone();
                        std::thread::spawn(move || match worker::save_download(&h.name, &h.url, &h.bytes) {
                            Ok((name, place)) => {
                                eprintln!("navigatord: saved {} in {place} as “{name}”", h.url);
                                send(&w, &Event::Saved { id, path: format!("“{name}” in {place}") });
                            }
                            Err(why) => {
                                eprintln!("navigatord: {} not saved: {why}", h.url);
                                send(&w, &Event::NotSaved { id, why });
                            }
                        });
                    }
                }
            }
            Request::OpenFile { id, press } => {
                if !waiting.lock().unwrap().holds(id) {
                    send(&w, &Event::FileOpened { id, app: None, why: Some("that file is no longer held".into()) });
                    continue;
                }
                if let Err(why) = spend(press) {
                    eprintln!("navigatord: open {id} refused: {why}");
                    send(&w, &Event::PressRefused { why });
                    continue;
                }
                let Some(h) = waiting.lock().unwrap().take(id) else { continue };
                let w = w.clone();
                std::thread::spawn(move || match worker::open_download(&h.name, &h.bytes) {
                    Ok(app) => {
                        eprintln!("navigatord: opened {} in {app}", h.url);
                        send(&w, &Event::FileOpened { id, app: Some(app), why: None });
                    }
                    Err(why) => {
                        eprintln!("navigatord: {} not opened: {why}", h.url);
                        send(&w, &Event::FileOpened { id, app: None, why: Some(why) });
                    }
                });
            }
            Request::Discard { id } => {
                waiting.lock().unwrap().take(id);
            }
            Request::NewWindow { persona, press } => {
                let w = w.clone();
                std::thread::spawn(move || {
                    let r = worker::new_window_in(&persona, press);
                    match &r {
                        Ok(()) => eprintln!("navigatord: a new window in {persona}"),
                        Err(why) => eprintln!("navigatord: no new window in {persona}: {why}"),
                    }
                    send(&w, &Event::WindowIn { persona, ok: r.is_ok(), why: r.err() });
                });
            }
            Request::Hand { url, how: ctl::Hand::With, press } => {
                // Open with…: a page we hold, its address as we have it.
                if !state.lock().unwrap().cache.iter().any(|p| p.url == url) {
                    send(&w, &Event::Handed { url, ok: false, why: Some("that page is not open".into()) });
                    continue;
                }
                let (cfg, w) = (cfg.clone(), w.clone());
                std::thread::spawn(move || send(&w, &hand_how(&cfg, &url, press, true)));
            }
            Request::Hand { url, how, press } => {
                // The press goes on with the link: portcullisd spends it
                // (purpose LINK) for the app that asked, us (§2.3a, §7).
                if let Some(no) = handed_once(&mut handed, &url, how) {
                    send(&w, &no);
                    continue;
                }
                let (cfg, w) = (cfg.clone(), w.clone());
                std::thread::spawn(move || send(&w, &hand(&cfg, &url, press)));
            }
        }
    }
}

/// Take `url` as a hand-off once, if this connection was told it is one
/// (threat model §2.10): `None` to go on, else the refusal. `Try` waits for
/// the app catalog (opening-links.md §4.3).
fn handed_once(handed: &mut Vec<String>, url: &str, how: ctl::Hand) -> Option<Event> {
    let no = |why: &str| Some(Event::Handed { url: url.to_string(), ok: false, why: Some(why.to_string()) });
    let Some(i) = handed.iter().position(|u| u == url) else { return no("that link was not decided as a hand-off here") };
    handed.remove(i);
    if how == ctl::Hand::Try {
        return no("trying an app comes with the app catalog; Open opens an installed one");
    }
    None
}

/// Hand `url` to Monstrator with the person's press (opening-links.md §7).
fn hand(cfg: &Config, url: &str, press: ctl::Press) -> Event {
    hand_how(cfg, url, press, false)
}

/// [`hand`]; `with`: Monstrator asks in its sheet whatever the default
/// ("Open with…", another app or persona).
fn hand_how(cfg: &Config, url: &str, press: ctl::Press, with: bool) -> Event {
    let ev = |ok: bool, why: Option<String>| Event::Handed { url: url.to_string(), ok, why };
    if !cfg.monstrator.exists() {
        return ev(false, Some("nothing here takes hand-offs yet".into()));
    }
    let wait = std::time::Duration::from_secs(130);
    let r = if with { monstrator::open_link_with(&cfg.monstrator, url, press, wait) } else { monstrator::open_link(&cfg.monstrator, url, press, wait) };
    match r {
        Ok(Ok(())) => {
            eprintln!("navigatord: a hand-off went to Monstrator and was opened");
            ev(true, None)
        }
        Ok(Err(why)) => {
            eprintln!("navigatord: a hand-off was not opened: {why}");
            ev(false, Some(why))
        }
        Err(e) => ev(false, Some(format!("the link opener: {e}"))),
    }
}

/// opifexd's socket (opifex proto::SOCKET).
const OPIFEX_SOCKET: &str = "/atrium/sockets/opifex/opifex.sock";

/// The app an `atrium-app://<id>[/…]` link names.
fn app_id(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("atrium-app:")?.trim_start_matches("//");
    let id = rest.split(['/', '?', '#']).next()?;
    (!id.is_empty() && id.len() <= 255 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))).then_some(id)
}

/// What the package catalog says of app `id` (opifexd's `Describe`, which
/// anyone may ask; opifex.md §7.1), for the open-an-app prompt. None when
/// opifexd isn't there, doesn't answer within a second, or doesn't know
/// the id (a side-loaded app): the prompt then names only the id.
fn app_about(sock: &Path, id: &str) -> Option<AppAbout> {
    use std::io::{BufRead, BufReader};
    let mut s = UnixStream::connect(sock).ok()?;
    let t = Some(std::time::Duration::from_secs(1));
    s.set_read_timeout(t).ok()?;
    s.set_write_timeout(t).ok()?;
    let mut ask = serde_json::to_vec(&serde_json::json!({ "op": "describe", "id": id })).ok()?;
    ask.push(b'\n');
    s.write_all(&ask).ok()?;
    let mut line = String::new();
    BufReader::new(s.take(64 * 1024)).read_line(&mut line).ok()?;
    about_of(&serde_json::from_str(&line).ok()?)
}

/// opifexd's `Described` answer, as the prompt shows it.
fn about_of(v: &serde_json::Value) -> Option<AppAbout> {
    if v.get("answer")?.as_str()? != "described" {
        return None;
    }
    let a = v.get("app")?;
    let s = |k: &str| a.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let third_party = a.get("third_party").and_then(|x| x.as_bool()).unwrap_or(false);
    let domains: Vec<String> = a.get("publisher_domains").and_then(|x| x.as_array()).map(|d| d.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let name = s("publisher_name");
    let publisher = match (name.is_empty(), domains.is_empty()) {
        (false, false) => format!("{name} ({})", domains.join(", ")),
        (false, true) => name,
        (true, _) if !third_party => "Atrium".into(),
        (true, false) => domains.join(", "),
        (true, true) => match s("publisher") { p if p.is_empty() => "an unnamed publisher".into(), p => p },
    };
    let capabilities = a.get("capabilities").and_then(|x| x.as_array()).map(|c| c.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    Some(AppAbout { name: s("name"), publisher, capabilities, installed: a.get("installed").is_some_and(|x| !x.is_null()), third_party })
}

/// What activating `href` in `page` does: only a link the recording
/// carries (threat model §2.10), decided by the backend's link rules.
/// Where a form of `page` goes: its `action` (empty: the page itself)
/// resolved as a link would be, an http or https address. The action and
/// every name, and every value the person didn't type, must be spelled in
/// the page's recording, as a link's href must (`activate`): the UI can only
/// send what the page holds, plus what the person typed.
/// Both addresses are HTTPS and of one origin.
fn same_https_origin(page: &str, target: &str) -> bool {
    url::Url::parse(page).ok().zip(url::Url::parse(target).ok())
        .is_some_and(|(page, target)| page.scheme() == "https" && page.origin() == target.origin())
}

fn form_target(page: &Page, action: &str, fields: &[ctl::FormField], schemes: &SchemeAllowlist) -> Result<String, String> {
    use navigator_backend::links;
    let has = |n: &[u8]| page.recording.windows(n.len()).any(|w| w == n);
    if !action.is_empty() && !has(&links::attr_spelling("action", action)) {
        return Err("the page does not carry this form".into());
    }
    for f in fields {
        if !has(&links::attr_spelling("name", &f.name)) {
            return Err(format!("the page has no field {:?}", f.name));
        }
        // A script's value is the page's own and not in the document:
        // the caller sends it only to the page's HTTPS origin.
        if !f.typed && !f.scripted && !f.value.is_empty() && f.value != "on" && !has(&links::attr_spelling("value", &f.value)) {
            return Err(format!("{:?} isn't a value the page gives", f.name));
        }
    }
    let href = if action.is_empty() { page.url.as_str() } else { action };
    match links::decide(&page.url, href, schemes) {
        Ok(links::LinkAction::Navigate { url, .. }) => Ok(url),
        Ok(_) => Err("a form goes only to a web address".into()),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct RecordingMetadata<'a> {
    url: String,
    tier: Option<u8>,
    #[serde(borrow)]
    island_sources: Option<&'a serde_json::value::RawValue>,
}

impl RecordingMetadata<'_> {
    fn island_sources(self, page: &str) -> Vec<(String, String)> {
        // No HTML or script parsing in the broker. Only the converter writes
        // this top-level field. Old recordings contain no eligible sources.
        let Some(raw) = self.island_sources else { return vec![] };
        // A redirect alias in the cache must not rebase a frame onto another site.
        if self.url != page { return vec![]; }
        // Up to sixteen JSON-escaped source strings. Bound allocation before decoding.
        if raw.get().len() > 16 * (6 * 2048 + 4) { return vec![]; }
        let Ok(sources) = serde_json::from_str::<Vec<String>>(raw.get()) else { return vec![] };
        if sources.len() > 16 { return vec![]; }
        let Ok(base) = url::Url::parse(page) else { return vec![] };
        sources.into_iter().filter_map(|src| {
            if src.is_empty() || src.len() > 2048 || src.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') { return None; }
            let target = base.join(&src).ok()?;
            if !matches!(target.scheme(), "http" | "https") || !target.username().is_empty() || target.password().is_some() { return None; }
            let origin = origin(target.as_str())?;
            (origin.len() <= 512).then_some((src, origin))
        }).collect()
    }
}

fn activate(page: &Page, href: &str, schemes: &SchemeAllowlist) -> LinkAction {
    use navigator_backend::links;
    if href.len() > links::MAX_HREF {
        return LinkAction::Blocked { why: "the link is too long".into() };
    }
    let carried = links::recording_spellings(href).iter().any(|n| page.recording.windows(n.len()).any(|w| w == &n[..]));
    if !carried {
        return LinkAction::Blocked { why: "the page does not carry this link".into() };
    }
    match links::decide(&page.url, href, schemes) {
        Ok(links::LinkAction::Navigate { url, insecure }) => LinkAction::Navigate { url, insecure },
        Ok(links::LinkAction::InDocument { fragment }) => LinkAction::InDocument { fragment },
        Ok(links::LinkAction::Handoff { url, scheme }) => LinkAction::Handoff { url, scheme, app: None },
        Err(e) => LinkAction::Blocked { why: e.to_string() },
    }
}

// ---------------------------------------------------------------------------
// The page socket.
// ---------------------------------------------------------------------------

fn serve_page(state: Arc<Mutex<State>>, mut s: UnixStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut t = [0u8; 16];
    if s.read_exact(&mut t).is_err() {
        return;
    }
    let found = {
        let mut st = state.lock().unwrap();
        st.tickets.remove(&t).filter(|(_, at)| at.elapsed() < TICKET_LIFE).map(|(p, _)| p)
    };
    let Some(p) = found else {
        eprintln!("navigatord: a page child brought an unknown ticket");
        return;
    };
    if let Err(e) = ctl::write_page(&mut s, &p.url, &p.recording, &p.images) {
        eprintln!("navigatord: page child: {e}");
    }
}

/// The kept trail store, if there is one within its bound.
fn read_trails(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?;
    if m.len() as usize > ctl::TRAILS_MAX {
        eprintln!("navigatord: {} is over its bound; not sent", p.display());
        return None;
    }
    std::fs::read_to_string(p).ok().filter(|t| !t.is_empty())
}

/// Keep the UI's trail store at `p`: 0600 in a 0700 directory, written
/// whole and then renamed into place.
fn write_trails(p: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if let Some(d) = p.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d)?;
    }
    let tmp = p.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, p)
}

fn listen(path: &str) -> UnixListener {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).unwrap_or_else(|e| {
        eprintln!("navigatord: {path}: {e}");
        std::process::exit(1)
    });
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
    }
    l
}

fn main() {
    let unconfined = std::env::args().any(|a| a == "--unconfined-for-testing");
    let mut cfg = Config::from_env(unconfined).unwrap_or_else(|e| {
        eprintln!("navigatord: {e}");
        std::process::exit(2)
    });
    // Our persona (personas.md §3), from portcullisd by our uid; the
    // reserved `portal` one makes us the network's sign-in window
    // (atrium-net.md §8.1 S3).
    let persona = (!unconfined).then(worker::persona);
    if let Some(Ok((Some((name, _)), _))) = &persona {
        if name == portcullis_ipc::PORTAL_PERSONA {
            start_portal(&mut cfg);
        }
        if name == portcullis_ipc::VPN_PERSONA {
            start_vpn(&mut cfg);
        }
    }
    if cfg.vpn.is_some() {
        if !unconfined && !matches!(cfg.ui, Some(worker::Spec::Jail(_))) {
            eprintln!("navigatord: a VPN sign-in requires its jailed UI");
            std::process::exit(1);
        }
        // Nothing of the window is kept but the sign-in jar (vpn.rs).
        cfg.trails = None;
        cfg.store = None;
        cfg.downloads = None;
        if cfg.arca.is_none() {
            eprintln!("navigatord: no VPN sign-in window: it needs Arca (NAVIGATORD_ARCA, NAVIGATORD_COURIERD)");
            std::process::exit(1);
        }
    }
    let cfg = Arc::new(cfg);
    if unconfined {
        eprintln!("navigatord: UNCONFINED FOR TESTING: {}{}",
            if cfg.converter.is_none() { "the converter runs as an ordinary process; " } else { "" },
            if cfg.fixtures.is_some() { "pages come from fixtures" } else { "pages come from the fetcher" });
    }
    let mut state = State::default();
    if let Some(dir) = &cfg.store {
        match DirStore::new(dir) {
            Ok(d) => {
                eprintln!("navigatord: pages kept in {}", dir.display());
                state.store = Box::new(d);
            }
            Err(e) => {
                eprintln!("navigatord: NAVIGATORD_STORE {}: {e}", dir.display());
                std::process::exit(2)
            }
        }
    }
    let state = Arc::new(Mutex::new(state));
    // Arca, for sites that know you. An Unrecorded jar a crash left behind
    // is forgotten now: none outlives the navigatord that made it.
    let cookies: Option<Cookies> = cfg.arca.as_ref().filter(|_| cfg.portal.is_none()).map(|(a, c)| {
        let mut b = Broker::new(ArcadSocket { path: a.clone() }, Launcher::new(c.clone()));
        match b.arca.forget("u-", true) {
            Ok(n) => eprintln!("navigatord: Arca at {}; {n} Unrecorded cookie groups left over, forgotten", a.display()),
            Err(e) => eprintln!("navigatord: Arca at {}: could not forget Unrecorded jars: {e}", a.display()),
        }
        Arc::new(Mutex::new(b))
    });
    if cfg.vpn.is_none() {
        if let (Some(socket), Some(_)) = (&cfg.limen, &cookies) {
            if let Some((arca, launcher)) = &cfg.arca {
                // Dedicated view IDs and cookie state for the sheet provider.
                let broker = Broker::new(ArcadSocket { path: arca.clone() }, Launcher::new(launcher.clone()));
                mail_sheet::start(socket.clone(), Arc::new(Mutex::new(broker)));
            }
        }
    }
    // In a directory of its own: the page child's jail mounts that
    // directory (the `navigator-page` capability) and nothing else of ours.
    // Shared with every other navigatord (each person's, each persona's):
    // ours is named by our uid, which our tickets carry.
    let me = unsafe { libc::getuid() };
    let page_path = ctl::instance_path(&settings::path("NAVIGATORD_PAGE_SOCKET", ctl::PAGE_SOCKET), me);
    if let Some(dir) = Path::new(&page_path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let ctl_path = ctl::instance_path(&settings::path("NAVIGATORD_SOCKET", "/var/run/atrium/navigatord.sock"), me);
    let pages = listen(&page_path);
    let st = state.clone();
    std::thread::spawn(move || {
        for s in pages.incoming().flatten() {
            let st = st.clone();
            std::thread::spawn(move || serve_page(st, s));
        }
    });
    let ui = listen(&ctl_path);
    // With the UI our own worker, only our own uid may come here: in the
    // shared directory, another person's or another persona's Navigator
    // must not drive this one (personas.md §3).
    if cfg.ui.is_some() {
        use std::os::unix::fs::PermissionsExt;
        // The mode is the access control here: if it cannot be set, do not
        // serve.
        if let Err(e) = std::fs::set_permissions(&ctl_path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!("navigatord: chmod 0600 {ctl_path}: {e}; refusing to serve an open control socket");
            std::process::exit(1);
        }
    }
    eprintln!("navigatord: control on {ctl_path}, pages on {page_path}");
    if cfg.portal.is_some() {
        std::thread::spawn(watch_portal);
    }
    if let Some(sign_in) = cfg.vpn.clone() {
        std::thread::spawn(move || {
            let deadline = Instant::now() + vpn::LIFETIME;
            while Instant::now() < deadline {
                std::thread::sleep(vpn::POLL);
                if !sign_in.is_current() {
                    eprintln!("navigatord: the VPN sign-in attempt ended; the window closes");
                    std::process::exit(0);
                }
            }
            eprintln!("navigatord: the VPN sign-in window closes after {} minutes", vpn::LIFETIME.as_secs() / 60);
            std::process::exit(0);
        });
    }
    document_channel();
    // The UI as our worker: its control channel is one end of a pair, and
    // the Navigator ends when it does.
    if let Some(spec) = cfg.ui.clone() {
        let (ours, theirs) = UnixStream::pair().unwrap_or_else(|e| {
            eprintln!("navigatord: UI channel: {e}");
            std::process::exit(1)
        });
        let mut pairs = settings::ui_pairs();
        pairs.retain(|(k, _)| k != "NAVIGATOR_UI_INBOX_FIXTURE" && k != "NAVIGATOR_UI_LIVE_PAGES");
        if cfg.live_pages_enabled() {
            pairs.push(("NAVIGATOR_UI_LIVE_PAGES".into(), "1".into()));
        }
        if cfg.inbox_enabled() {
            pairs.push(("NAVIGATOR_UI_INBOX_FIXTURE".into(), ctl::INBOX_FIXTURE_URL.into()));
        }
        // The window's persona (personas.md §3), for the rail; frescod
        // draws the strip chip itself, from our uid.
        if let Some(p) = &cfg.portal {
            // The sign-in window: the network's name for the title, its
            // page in an Unrecorded trail, no persona and no other windows.
            pairs.retain(|(k, _)| !k.starts_with("NAVIGATOR_UI_PERSONA") && k != "NAVIGATOR_UI_OTHER_PERSONAS" && k != "NAVIGATOR_UI_OPEN");
            pairs.push(("NAVIGATOR_UI_PORTAL".into(), p.network.clone()));
            pairs.push(("NAVIGATOR_UI_OPEN".into(), p.start_url.clone()));
        } else if let Some(v) = &cfg.vpn {
            // A VPN's sign-in window: as the portal's, with its own words.
            pairs.retain(|(k, _)| !k.starts_with("NAVIGATOR_UI_PERSONA") && k != "NAVIGATOR_UI_OTHER_PERSONAS" && k != "NAVIGATOR_UI_OPEN");
            pairs.push(("NAVIGATOR_UI_PORTAL".into(), v.title()));
            pairs.push(("NAVIGATOR_UI_VPN".into(), "1".into()));
            pairs.push(("NAVIGATOR_UI_OPEN".into(), v.start_url.clone()));
        } else if let Some(persona) = persona {
            pairs.retain(|(k, _)| !k.starts_with("NAVIGATOR_UI_PERSONA") && k != "NAVIGATOR_UI_OTHER_PERSONAS");
            match persona {
                Ok((here, others)) => {
                    match here {
                        Some((name, colour)) => {
                            eprintln!("navigatord: persona {name} ({colour})");
                            pairs.push(("NAVIGATOR_UI_PERSONA".into(), name));
                            pairs.push(("NAVIGATOR_UI_PERSONA_COLOUR".into(), colour));
                        }
                        None => eprintln!("navigatord: persona Personal"),
                    }
                    // For the rail's New window menu.
                    if !others.is_empty() {
                        eprintln!("navigatord: other personas {}", others.join(", "));
                        pairs.push(("NAVIGATOR_UI_OTHER_PERSONAS".into(), others.join(",")));
                    }
                }
                Err(e) => eprintln!("navigatord: persona unknown, Personal shown: {e}"),
            }
        }
        if let Err(e) = ctl::write_frame(&mut &ours, &Event::Settings { pairs }) {
            eprintln!("navigatord: UI settings: {e}");
            std::process::exit(1);
        }
        let mut w = worker::Worker::start_on(&spec, &format!("ui{}", std::process::id()), theirs.into()).unwrap_or_else(|e| {
            eprintln!("navigatord: the UI did not start: {e}");
            std::process::exit(1)
        });
        eprintln!("navigatord: the UI started");
        let (cfg, state, cookies) = (cfg.clone(), state.clone(), cookies.clone());
        std::thread::spawn(move || serve_ui(cfg, state, cookies, ours));
        std::thread::spawn(move || loop {
            if w.try_wait().unwrap_or(true) {
                eprintln!("navigatord: the UI ended; so does the Navigator");
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(200));
        });
    }
    for s in ui.incoming().flatten() {
        let (cfg, state, cookies) = (cfg.clone(), state.clone(), cookies.clone());
        std::thread::spawn(move || serve_ui(cfg, state, cookies, s));
    }
}

/// Become a VPN's sign-in window: the profile and its addresses from
/// portcullisd. Without them there is nothing to show.
fn start_vpn(cfg: &mut Config) {
    match vpn::status() {
        Ok(v) => {
            eprintln!("navigatord: the sign-in window for VPN {} at {}", v.profile, origin(&v.start_url).unwrap_or_default());
            cfg.vpn = Some(Arc::new(v));
        }
        Err(why) => {
            eprintln!("navigatord: no VPN sign-in window: {why}");
            std::process::exit(1);
        }
    }
}

/// Become the sign-in window: the network's fetcher and start page, and
/// nothing of the person's (no trails kept, no Arca). Without them there is
/// nothing to show, and the window doesn't open.
fn start_portal(cfg: &mut Config) {
    match portal::Portal::open() {
        Ok(p) => {
            eprintln!("navigatord: the sign-in window for {} at {}", p.network, p.start_url);
            cfg.portal = Some(Arc::new(p));
            cfg.trails = None;
            cfg.store = None;
            cfg.arca = None;
            cfg.downloads = None;
        }
        Err(why) => {
            eprintln!("navigatord: no sign-in window: {why}");
            std::process::exit(1);
        }
    }
}

/// The sign-in window ends when atrium-net says the network is online, or
/// after its lifetime (atrium-net.md §8.1 P4); the UI and the fetcher end
/// with us.
fn watch_portal() {
    let start = Instant::now();
    loop {
        std::thread::sleep(portal::POLL);
        match portal::status() {
            Ok(st) if st.online => {
                eprintln!("navigatord: {} is online; the sign-in window closes", st.network);
                std::process::exit(0);
            }
            Ok(_) => {}
            Err(why) => eprintln!("navigatord: the network's state: {why}"),
        }
        if start.elapsed() >= portal::LIFETIME {
            eprintln!("navigatord: the sign-in window closes after {} minutes", portal::LIFETIME.as_secs() / 60);
            std::process::exit(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_links_come_in() {
        assert!(web_link("https://atlas.example/wiki"));
        assert!(web_link("HTTP://atlas.example/"));
        for bad in ["mailto:a@b", "javascript:alert(1)", "https://a b", "file:///etc/passwd", "httpx://a"] {
            assert!(!web_link(bad), "{bad}");
        }
    }

    fn page(url: &str, doc: &str) -> Page {
        let rec = format!(r#"{{"format":"atrium-navigator-recording/3","url":"{url}","document":"{doc}"}}"#);
        Page { vpn_document: None, url: url.into(), title: title(rec.as_bytes()), recording: rec.into_bytes(), insecure: false, saved: String::new(), ctx: Ctx::ANON, images: Default::default(), consents: Default::default() }
    }

    #[test]
    fn an_app_link_is_described_by_the_catalog() {
        assert_eq!(app_id("atrium-app://org.example.photos/x?y"), Some("org.example.photos"));
        assert_eq!(app_id("atrium-app:org.example.photos"), Some("org.example.photos"));
        assert_eq!(app_id("atrium-app://"), None);
        assert_eq!(app_id("atrium-app://a b"), None);
        let v = serde_json::json!({"answer": "described", "app": {"id": "org.example.photos", "name": "Photos",
            "publisher": "k1", "publisher_name": "Example Ltd", "publisher_domains": ["example.com"],
            "capabilities": ["network-read", "files"], "installed": null, "third_party": true}});
        assert_eq!(about_of(&v), Some(AppAbout { name: "Photos".into(), publisher: "Example Ltd (example.com)".into(),
            capabilities: vec!["network-read".into(), "files".into()], installed: false, third_party: true }));
        let v = serde_json::json!({"answer": "described", "app": {"name": "Write", "publisher": "", "capabilities": [], "installed": "3"}});
        assert_eq!(about_of(&v).map(|a| (a.publisher, a.installed)), Some(("Atrium".into(), true)));
        assert_eq!(about_of(&serde_json::json!({"answer": "error", "message": "x: not in the catalog"})), None);
        // No opifexd: no description, and no wait.
        assert_eq!(app_about(Path::new("/nonexistent/opifex.sock"), "org.example.photos"), None);
    }

    #[test]
    fn a_vpn_pages_security_key_origin_is_its_delivered_documents_https_origin() {
        let o = vpn_origin("https://idp.corp.example:8443/realms/x/login?y=1").expect("an https document");
        assert!(o.rp_id(Some("corp.example")).is_ok(), "a registrable suffix of the document's host");
        assert!(o.rp_id(Some("evil.example")).is_err(), "never another site");
        assert!(vpn_origin("http://idp.corp.example/").is_none(), "never plain http");
        assert!(vpn_origin("file:///etc/passwd").is_none());
        assert!(vpn_origin("not a url").is_none());
    }

    #[test]
    fn only_links_the_page_carries_are_followed() {
        let p = page("https://atlas.example/wiki/Atrium", r##"<html><head><title>Atrium & court</title></head><body><a href=\"/wiki/Impluvium\">pool</a><a href=\"#plan\">plan</a><a href=\"javascript:x()\">x</a></body></html>"##);
        assert_eq!(activate(&p, "/wiki/Impluvium", &SchemeAllowlist::default()), LinkAction::Navigate { url: "https://atlas.example/wiki/Impluvium".into(), insecure: false });
        assert_eq!(activate(&p, "#plan", &SchemeAllowlist::default()), LinkAction::InDocument { fragment: "plan".into() });
        assert!(matches!(activate(&p, "https://evil.example/", &SchemeAllowlist::default()), LinkAction::Blocked { .. }), "not in the page");
        assert!(matches!(activate(&p, "javascript:x()", &SchemeAllowlist::default()), LinkAction::Blocked { .. }), "a scheme no one allowed");
    }

    #[test]
    fn a_form_goes_only_where_the_page_says_with_what_it_holds() {
        let p = page("http://10.88.0.1/welcome", r#"<form method=\"post\" action=\"/login\"><input type=\"hidden\" name=\"t\" value=\"9f\"><input name=\"room\"><button name=\"go\" value=\"1\">Connect</button></form>"#);
        let f = |name: &str, value: &str, typed| ctl::FormField { name: name.into(), value: value.into(), typed, arca: None, scripted: false };
        let ok = [f("t", "9f", false), f("room", "4 12", true), f("go", "1", false)];
        let s = SchemeAllowlist::default();
        assert_eq!(form_target(&p, "/login", &ok, &s).as_deref(), Ok("http://10.88.0.1/login"));
        assert_eq!(form_target(&p, "", &ok, &s).as_deref(), Ok("http://10.88.0.1/welcome"), "no action: the page itself");
        assert!(form_target(&p, "http://evil.example/steal", &ok, &s).is_err(), "an action the page doesn't carry");
        assert!(form_target(&p, "/login", &[f("password", "x", true)], &s).is_err(), "a name the page doesn't have");
        assert!(form_target(&p, "/login", &[f("t", "forged", false)], &s).is_err(), "a page value the page doesn't give");
        assert!(form_target(&p, "/login", &[f("t", "typed is fine", true)], &s).is_ok());
    }

    #[test]
    fn a_page_scripts_value_needs_only_its_field_and_goes_only_to_its_https_origin() {
        let p = page("https://idp.example/realms/corp/login", r#"<form id=\"webauth\" method=\"post\" action=\"https://idp.example/realms/corp/login-actions/authenticate?x=1\"><input type=\"hidden\" name=\"signature\"></form>"#);
        let field = |name: &str, scripted| ctl::FormField { name: name.into(), value: "MEUCIQDx".into(), typed: false, arca: None, scripted };
        let s = SchemeAllowlist::default();
        let action = "https://idp.example/realms/corp/login-actions/authenticate?x=1";
        assert!(form_target(&p, action, &[field("signature", true)], &s).is_ok(), "a script's value need not be in the document");
        assert!(form_target(&p, action, &[field("signature", false)], &s).is_err(), "a page value must be");
        assert!(form_target(&p, action, &[field("password", true)], &s).is_err(), "only a field the page has");
        assert!(same_https_origin(&p.url, action));
        assert!(!same_https_origin(&p.url, "https://evil.example/x"));
        assert!(!same_https_origin("http://idp.example/", "http://idp.example/x"), "never plain http");
    }

    #[test]
    fn a_vpn_pages_resource_comes_only_from_its_documents_https_origin() {
        let doc = "https://idp.crypta.test:448/realms/corp/login-actions/authenticate?session_code=1";
        assert_eq!(vpn_resource_target(doc, "https://idp.crypta.test:448/resources/abc/login/keycloak/js/webauthnAuthenticate.js").as_deref(), Ok("idp.crypta.test"));
        assert!(vpn_resource_target(doc, "https://idp.crypta.test/resources/a.js").is_err(), "another port is another origin");
        assert!(vpn_resource_target(doc, "https://evil.example:448/a.js").is_err());
        assert!(vpn_resource_target(doc, "http://idp.crypta.test:448/a.js").is_err(), "never plain http");
        assert!(vpn_resource_target(doc, "https://u:p@idp.crypta.test:448/a.js").is_err(), "no credentials");
        assert!(vpn_resource_target(doc, "https://idp.crypta.test:448/a.js#x").is_err(), "no fragment");
        assert_eq!(refused_target("https://cdn.thirdparty.example/lib.js?k=secret"), "host cdn.thirdparty.example, site thirdparty.example");
        assert_eq!(refused_target("not an address"), "host unreadable");
        assert!(vpn_resource_target(doc, "https://IDP.crypta.test:448/a.js").is_err(), "only the normal spelling");
        assert!(vpn_resource_target(doc, "https://idp.crypta.test:448/realms/corp/api/me").is_err(), "a page's fetch() is not a script");
        assert!(vpn_resource_target(doc, "https://idp.crypta.test:448/a.json").is_err());
        assert!(vpn_resource_target(doc, "https://idp.crypta.test:448/r/v5/patternfly.min.css?v=1").is_ok(), "a query is fine");
        assert!(vpn_resource_target(doc, "https://idp.crypta.test:448/m.MJS").is_ok());
        assert!(vpn_resource_target("http://idp.crypta.test/", "http://idp.crypta.test/a.js").is_err());
        let long = format!("https://idp.crypta.test:448/{}", "a".repeat(limen::sign_in::VPN_RESOURCE_URL_MAX));
        assert!(vpn_resource_target(doc, &long).is_err());
    }

    #[test]
    fn a_vpn_pages_resource_is_one_script_or_stylesheet_within_its_bound() {
        let h = |v: &str| vec![("Content-Type".to_string(), v.to_string())];
        assert_eq!(vpn_resource_kind(200, &h("text/javascript; charset=utf-8"), b"x"), Ok(ctl::VpnResourceKind::Script));
        assert_eq!(vpn_resource_kind(200, &h("application/javascript"), b"x"), Ok(ctl::VpnResourceKind::Script));
        assert_eq!(vpn_resource_kind(200, &h("TEXT/CSS"), b"x"), Ok(ctl::VpnResourceKind::Stylesheet));
        assert!(vpn_resource_kind(200, &h("text/html"), b"x").is_err());
        assert!(vpn_resource_kind(404, &h("text/javascript"), b"x").is_err());
        assert!(vpn_resource_kind(200, &[], b"x").is_err(), "no content type");
        let two = vec![("content-type".to_string(), "text/css".to_string()), ("Content-Type".to_string(), "text/javascript".to_string())];
        assert!(vpn_resource_kind(200, &two, b"x").is_err(), "two content types");
        assert!(vpn_resource_kind(200, &h("text/javascript"), &vec![b' '; limen::sign_in::VPN_RESOURCE_MAX + 1]).is_err());
    }

    #[test]
    fn a_title_and_an_origin() {
        let p = page("https://a.example/", r#"<html><head><title>  A \"quoted\"\n title &amp; more </title></head></html>"#);
        assert_eq!(p.title, "A \"quoted\" title & more");
        assert_eq!(origin("https://Atlas.Example/wiki?x#y").as_deref(), Some("https://atlas.example"));
        assert_eq!(origin("http://old.example:8080/").as_deref(), Some("http://old.example:8080"));
        assert_eq!(origin("https://bücher.example/").as_deref(), Some("https://xn--bcher-kva.example"));
    }

    #[test]
    fn fixtures_map_addresses_to_files_and_refuse_escapes() {
        let d = std::env::temp_dir().join(format!("navigatord-fix-{}", std::process::id()));
        std::fs::create_dir_all(d.join("atlas.example/wiki")).unwrap();
        std::fs::write(d.join("atlas.example/index.html"), "home").unwrap();
        std::fs::write(d.join("atlas.example/wiki/Atrium"), "atrium").unwrap();
        assert_eq!(fixture(&d, "https://atlas.example/").as_deref(), Some(&b"home"[..]));
        assert_eq!(fixture(&d, "https://atlas.example/wiki/Atrium?x=1").as_deref(), Some(&b"atrium"[..]));
        assert_eq!(fixture(&d, "https://atlas.example/../etc/passwd"), None);
        assert_eq!(fixture(&d, "file:///etc/passwd"), None);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_consent_controls_writes_are_read_strictly() {
        let r = "Y\tReplayed\tx\nCW\t0\tconsent=yes; Path=/\nCW\t0\tb=1\nCW\t2\tskipped=1\nCW\t1\tc=2\nCW\tx\tbad=1\nCW\t9\tbig=1\n";
        assert_eq!(consents_of(r), vec![vec!["consent=yes; Path=/".to_string(), "b=1".into()], vec!["c=2".into()]]);
        let p = Page { consents: Arc::new(consents_of(r)), ..page("https://a.example/", "<a href=\"atrium-consent:0\">Accept</a>") };
        let no = |a: LinkAction| matches!(a, LinkAction::Blocked { .. });
        // Not carried, not held, or no Arca: refused before anything is written.
        assert!(no(consent(&p, "atrium-consent:1", Jar::Personal, None)));
        assert!(no(consent(&p, "atrium-consent:x", Jar::Personal, None)));
        assert!(no(consent(&p, "atrium-consent:0", Jar::Personal, None)));
    }

    #[test]
    fn a_failed_cookie_check_is_told_from_an_error() {
        let d = std::env::temp_dir().join(format!("navigatord-chain-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sd.example")).unwrap();
        // The site sets a cookie, checks for it, and fails without it.
        std::fs::write(d.join("sd.example/article.chain"), "302 1 https://sd.example/article\n302 0 https://auth.sd.example/check\n400 0 https://sd.example/article?x=1\n").unwrap();
        std::fs::write(d.join("sd.example/gone.chain"), "404 0 https://sd.example/gone\n").unwrap();
        let mut up = Upstream::Fixtures(d.clone());
        let (r, hops) = up.navigate("https://sd.example/article");
        assert!(matches!(r, Reply::Ok { status: 400, .. }));
        assert!(navigator_cookies::site_needs_cookies(&hops));
        let (r, hops) = up.navigate("https://sd.example/gone");
        assert!(matches!(r, Reply::Ok { status: 404, .. }) && !navigator_cookies::site_needs_cookies(&hops));
        // No chain beside it: the page as before, no hops.
        let (_, hops) = up.navigate("https://sd.example/other");
        assert!(hops.is_empty());
        assert_eq!(fixture_chain(&d, "https://sd.example/../../etc/x"), None);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_parked_page_reopens_from_the_store_only_for_its_address() {
        let mut st = State::default();
        let p = st.stored(page("https://a.example/", "x"));
        assert!(ctl::is_address(&p.saved));
        let back = st.reopen("https://a.example/", &p.saved, Ctx::ANON).unwrap();
        assert_eq!(back.recording, p.recording);
        // Its consent answers come back with it.
        let c = st.stored(Page { consents: Arc::new(vec![vec!["ok=1; Path=/".into()]]), ..page("https://cs.example/", "<a href=\"atrium-consent:0\">OK</a>") });
        assert_eq!(*st.reopen("https://cs.example/", &c.saved, Ctx::ANON).unwrap().consents, vec![vec!["ok=1; Path=/".to_string()]]);
        assert!(!consents_ok(&[vec!["a\nb".into()]]) && !consents_ok(&vec![vec![]; CONSENTS_MAX + 1]));
        assert_eq!(back.saved, p.saved);
        assert!(st.reopen("https://b.example/", &p.saved, Ctx::ANON).is_err(), "a record names its address");
        let rec = navigator_backend::session::digest(&p.recording);
        assert!(st.reopen("https://a.example/", &rec, Ctx::ANON).is_err(), "a recording is not a record");
        assert!(st.reopen("https://a.example/", "../x", Ctx::ANON).is_err());
        // An Unrecorded trail's page, or one the site knew you on, is
        // never in the written store, and is its jar's alone.
        let uj = Ctx { jar: Jar::Unrecorded(7), knows: false };
        let mut up = page("https://u.example/", "y");
        up.ctx = uj;
        let u = st.stored(up);
        assert!(st.reopen("https://u.example/", &u.saved, Ctx::ANON).is_err());
        assert!(st.reopen("https://u.example/", &u.saved, Ctx { jar: Jar::Unrecorded(8), knows: false }).is_err(), "another trail's jar");
        assert!(st.reopen("https://u.example/", &u.saved, uj).is_ok());
        assert!(st.page("https://u.example/").is_none(), "nor known for later");
        let kj = Ctx { jar: Jar::Personal, knows: true };
        let mut kp = page("https://k.example/", "mail");
        kp.ctx = kj;
        let k = st.stored(kp);
        assert!(st.reopen("https://k.example/", &k.saved, Ctx::ANON).is_err(), "a cookied page is not the shared one");
        assert!(st.reopen("https://k.example/", &k.saved, kj).is_ok());
        // The Unrecorded trail closes: its pages go.
        st.forget(Jar::Unrecorded(7), None);
        assert!(st.reopen("https://u.example/", &u.saved, uj).is_err());
        // Once out of the cache, a known page is still one links are checked on.
        assert!(st.cached("https://a.example/", None).is_none());
        assert!(st.page("https://a.example/").is_some());
        assert!(st.page("https://c.example/").is_none());
    }

    #[test]
    fn a_ticket_redeems_once() {
        let mut st = State::default();
        let p = Arc::new(page("https://a.example/", "x"));
        st.keep(p.clone(), "https://a.example/");
        let t = st.ticket(p);
        assert!(st.tickets.remove(&t).is_some());
        assert!(st.tickets.remove(&t).is_none());
        assert!(st.cached("https://a.example/", Some(Ctx::ANON)).is_some());
        assert!(st.cached("https://a.example/", Some(Ctx { jar: Jar::Personal, knows: true })).is_none(), "the anonymous page is not the cookied one");
    }

    #[test]
    fn clearing_takes_records_and_what_only_they_name() {
        let mut st = State::default();
        let img = |b: &[u8]| (format!("blake3:{}", navigator_backend::session::digest(b)), b.to_vec());
        let mut a = page("https://a.example/", "x");
        a.images = Arc::new(vec![img(b"shared"), img(b"only-a")]);
        let a = st.stored(a);
        // The same page seen again: another record, the same recording.
        let mut b = page("https://b.example/", "y");
        b.images = Arc::new(vec![img(b"shared")]);
        let b = st.stored(b);
        let a2 = Page { url: "https://a.example/?again".into(), ..page("https://a.example/", "x") };
        let a2 = st.stored(a2);
        let objects = |st: &State| st.store.list().unwrap().len();
        let before = objects(&st);
        st.keep(Arc::new(page("https://a.example/", "x")), "https://a.example/");
        // A recording is not a record: naming one takes nothing.
        let rec = navigator_backend::session::digest(&a.recording);
        assert_eq!(st.clear(&[rec.clone(), "../x".into()]), (0, 0));
        assert_eq!(objects(&st), before);
        let (r, o) = st.clear(std::slice::from_ref(&a.saved));
        assert_eq!(r, 1);
        assert_eq!(o, 1, "only a's own image: the recording is a2's too, the shared image b's");
        assert!(st.reopen("https://a.example/", &a.saved, Ctx::ANON).is_err());
        assert!(st.cached("https://a.example/", None).is_none(), "out of the cache too");
        assert!(st.reopen("https://b.example/", &b.saved, Ctx::ANON).is_ok());
        assert!(st.reopen("https://a.example/?again", &a2.saved, Ctx::ANON).is_ok());
        assert_eq!(st.clear(&[b.saved.clone(), a2.saved.clone()]), (2, 3), "b's recording, the shared image, a's recording");
        assert_eq!(objects(&st), 0, "nothing left");
        // A private page's record is in its jar's memory store.
        let uj = Ctx { jar: Jar::Unrecorded(3), knows: false };
        let mut up = page("https://u.example/", "u");
        up.ctx = uj;
        let u = st.stored(up);
        assert_eq!(st.clear(std::slice::from_ref(&u.saved)), (1, 1));
        assert!(st.reopen("https://u.example/", &u.saved, uj).is_err());
    }
}


#[cfg(test)]
mod island_metadata_tests {
    use super::RecordingMetadata;

    #[test]
    fn only_top_level_recorded_web_sources_can_supply_the_label() {
        let base = "https://parent.example:8443/docs/";
        let doc = br#"{"url":"https://parent.example:8443/docs/","document":"https://mentioned.example/","other":{"island_sources":["https://nested.example/"]}}"#;
        fn island_sources(recording: &[u8], page: &str) -> Vec<(String, String)> {
            serde_json::from_slice::<RecordingMetadata>(recording).unwrap().island_sources(page)
        }
        assert!(island_sources(doc, base).is_empty());
        let rec = br#"{"url":"https://parent.example:8443/docs/","island_sources":["../widget","//frame.example/box","file:///etc","https://user@private.example/","broken path"]}"#;
        assert_eq!(island_sources(rec, base), vec![
            ("../widget".into(), "https://parent.example:8443".into()),
            ("//frame.example/box".into(), "https://frame.example".into()),
        ]);
        assert!(island_sources(rec, "https://alias.example/").is_empty());
        let over = serde_json::json!({"url": base, "island_sources": vec!["/frame"; 17]});
        assert!(island_sources(over.to_string().as_bytes(), base).is_empty());
    }
}
