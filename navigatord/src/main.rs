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
//!   (`NAVIGATORD_FETCHD`, e.g. `portcullis exec --daemon --instance
//!   f{instance} org.atrium.navigator.fetcher`) and converted in a one-shot
//!   jail (`NAVIGATORD_CONVERTER`, e.g. `portcullis exec --daemon
//!   --instance conv{instance} org.atrium.navigator.converter`), the fetches
//!   relayed under the per-conversion byte budget, as the prerender driver
//!   does (backend §4.7a).
//! - **The origin** of the page the UI shows is set in frescod's strip over
//!   the chrome channel (`FRESCOD_CHROME_SOCK`, F4d): on `Hello { chrome }`
//!   navigatord opens the UI's frescod connection there and hands it over,
//!   so the UI's window is one navigatord may name.
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

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use navigator_backend::store::{DirStore, MemoryStore, Store};
use navigator_control::{self as ctl, Event, LinkAction, PageHeader, Request, Source, Ticket};
use navigator_prerender::fetch::{FetchBudget, FetchdFetcher, Reply};
use navigator_prerender::serve::{self, Ask, Job};

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

fn words(v: &str) -> Option<(String, Vec<String>)> {
    let w: Vec<String> = v.split_whitespace().map(String::from).collect();
    let (p, a) = w.split_first()?;
    Some((p.clone(), a.to_vec()))
}

struct Config {
    converter: Option<(String, Vec<String>)>,
    prerender: String,
    fetchd: Option<(String, Vec<String>)>,
    fixtures: Option<PathBuf>,
    unconfined: bool,
    deadline_s: u64,
    chrome: Option<PathBuf>,
    store: Option<PathBuf>,
}

impl Config {
    fn from_env(unconfined: bool) -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let c = Config {
            converter: var("NAVIGATORD_CONVERTER").and_then(|v| words(&v)),
            prerender: var("NAVIGATORD_PRERENDER").unwrap_or_else(|| "prerender".into()),
            fetchd: var("NAVIGATORD_FETCHD").and_then(|v| words(&v)),
            fixtures: var("NAVIGATORD_FIXTURES").map(PathBuf::from),
            unconfined,
            deadline_s: var("NAVIGATORD_DEADLINE").and_then(|v| v.parse().ok()).unwrap_or(30),
            chrome: var("FRESCOD_CHROME_SOCK").map(PathBuf::from),
            store: var("NAVIGATORD_STORE").map(PathBuf::from),
        };
        if !unconfined {
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

/// A converted page.
struct Page {
    url: String,
    recording: Vec<u8>,
    title: String,
    insecure: bool,
    /// Its page record's address in the store (empty: not stored).
    saved: String,
}

/// What the store holds for a page besides its recording: which address
/// that recording is the page at. A UI can only name records navigatord
/// wrote, and a record is only taken for the address it names.
#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    format: String,
    url: String,
    recording: String,
}

struct State {
    cache: Vec<Arc<Page>>,
    tickets: HashMap<Ticket, (Arc<Page>, Instant)>,
    store: Box<dyn Store + Send>,
    /// Unrecorded trails' pages: never written (navigator-ui.md §2.4).
    memory: MemoryStore,
    /// Page address → its latest record address.
    known: HashMap<String, String>,
}

impl Default for State {
    fn default() -> Self {
        State { cache: vec![], tickets: HashMap::new(), store: Box::new(MemoryStore::default()), memory: MemoryStore::default(), known: HashMap::new() }
    }
}

impl State {
    /// Put a converted page in the store; returns it with its record's
    /// address (or as it was, if the store refused: it still shows).
    fn stored(&mut self, mut p: Page, unrecorded: bool) -> Page {
        let store: &mut dyn Store = if unrecorded { &mut self.memory } else { &mut *self.store };
        let put = store.put(&p.recording).and_then(|rec| {
            let r = Record { format: RECORD.into(), url: p.url.clone(), recording: rec };
            store.put(&serde_json::to_vec(&r).expect("encodes"))
        });
        match put {
            Ok(a) => {
                if !unrecorded {
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
    fn reopen(&self, url: &str, saved: &str, unrecorded: bool) -> Result<Page, String> {
        if !ctl::is_address(saved) {
            return Err("not a store address".into());
        }
        let store: &dyn Store = if unrecorded { &self.memory } else { &*self.store };
        let b = store.get(saved)?;
        let r: Record = serde_json::from_slice(&b).map_err(|e| format!("not a page record: {e}"))?;
        if r.format != RECORD {
            return Err(format!("a {:?} record", r.format));
        }
        if r.url != url {
            return Err("the record is for another address".into());
        }
        let recording = store.get(&r.recording)?;
        Ok(Page { title: title(&recording), insecure: url.starts_with("http:"), url: r.url, recording, saved: saved.into() })
    }

    /// The page at `url` this UI may act on: the cache, else the store.
    fn page(&mut self, url: &str) -> Option<Arc<Page>> {
        if let Some(p) = self.cached(url) {
            return Some(p);
        }
        let saved = self.known.get(url)?.clone();
        let p = Arc::new(self.reopen(url, &saved, false).ok()?);
        self.keep(p.clone(), url);
        Some(p)
    }

    fn cached(&mut self, url: &str) -> Option<Arc<Page>> {
        let i = self.cache.iter().position(|p| p.url == url)?;
        let p = self.cache.remove(i);
        self.cache.push(p.clone());
        Some(p)
    }

    fn keep(&mut self, p: Arc<Page>, asked: &str) {
        self.cache.retain(|q| q.url != p.url);
        self.cache.push(p.clone());
        if asked != p.url {
            // A redirect: the asked address finds it too.
            self.cache.push(Arc::new(Page { url: asked.into(), recording: p.recording.clone(), title: p.title.clone(), insecure: p.insecure, saved: p.saved.clone() }));
        }
        while self.cache.len() > CACHE {
            self.cache.remove(0);
        }
    }

    fn ticket(&mut self, p: Arc<Page>) -> Ticket {
        let now = Instant::now();
        self.tickets.retain(|_, (_, t)| now.duration_since(*t) < TICKET_LIFE);
        let mut t = [0u8; 16];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut t)).expect("/dev/urandom");
        self.tickets.insert(t, (p, now));
        t
    }
}

// ---------------------------------------------------------------------------
// Fetching and converting.
// ---------------------------------------------------------------------------

enum Upstream {
    Fetchd(FetchdFetcher),
    Fixtures(PathBuf),
}

impl Upstream {
    fn reply(&mut self, ask: Ask) -> Reply {
        match self {
            Upstream::Fetchd(f) => match ask {
                Ask::Fetch(u) => f.exchange(u).unwrap_or_else(Reply::Err),
                Ask::Peek { url, start, len } => f.exchange_peek(url, start, len).unwrap_or_else(Reply::Err),
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
    if let Some(d) = &cfg.fixtures {
        return Ok(Upstream::Fixtures(d.clone()));
    }
    let (p, a) = cfg.fetchd.as_ref().ok_or("no fetcher")?;
    let a: Vec<String> = a.iter().map(|x| x.replace("{instance}", inst)).collect();
    FetchdFetcher::spawn(p, &a).map(Upstream::Fetchd).map_err(|e| format!("fetcher: {e}"))
}

/// Fetch and convert `url`.
fn convert(cfg: &Config, url: &str) -> Result<Page, String> {
    let inst = instance();
    let mut up = upstream(cfg, &inst)?;
    let mut budget = FetchBudget::new(FetchBudget::DEFAULT);
    let (base, doc) = match budget.admit(url, |u| up.reply(Ask::Fetch(u))) {
        Reply::Ok { status, rest, body } if (200..300).contains(&status) => {
            let fin = rest.splitn(3, ' ').nth(2).filter(|f| !f.is_empty()).unwrap_or(url).to_string();
            (fin, body)
        }
        Reply::Ok { status, .. } => return Err(format!("the site answered {status}")),
        Reply::Err(e) => return Err(e),
    };
    let job = Job {
        base: Some(base.clone()),
        net: true,
        emit: true,
        require_confined: !cfg.unconfined,
        deadline_s: cfg.deadline_s,
        len: doc.len(),
        ..Job::default()
    };
    let header = job.header()?;
    let mut cmd = match &cfg.converter {
        Some((p, a)) => {
            let mut c = std::process::Command::new(p);
            c.args(a.iter().map(|x| x.replace("{instance}", &format!("conv{inst}"))));
            c
        }
        None => std::process::Command::new(&cfg.prerender), // serve mode: no arguments
    };
    use std::process::Stdio;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = cmd.spawn().map_err(|e| format!("converter: {e}"))?;
    let stdin: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(Some(Box::new(ch.stdin.take().expect("piped")))));
    let stdout = ch.stdout.take().expect("piped");
    let mut stderr = ch.stderr.take().expect("piped");
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
                Ok(Some(_)) => return true,
                Ok(None) if start.elapsed() > deadline + GRACE => {
                    drop(stdin_w.lock().unwrap().take());
                    let t = Instant::now();
                    while t.elapsed() < GRACE {
                        if let Ok(Some(_)) = ch.try_wait() {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    let _ = ch.kill();
                    let _ = ch.wait();
                    return false;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => return false,
            }
        }
    });
    let outcome = (|| -> Result<serve::Outcome, String> {
        {
            let mut g = stdin.lock().unwrap();
            let w = g.as_mut().ok_or("the converter is gone")?;
            w.write_all(header.as_bytes()).and_then(|_| w.write_all(&doc)).and_then(|_| w.flush())
                .map_err(|e| format!("could not hand the document to the converter: {e}"))?;
        }
        let mut out = std::io::BufReader::new(stdout);
        let r = serve::drive(&mut out, &stdin, &mut |ask| budget.admit(ask.url(), |_| up.reply(ask)).to_frame(), &mut |_, _| {});
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
    let insecure = base.starts_with("http:");
    Ok(Page { title: title(&recording), url: base, recording, insecure, saved: String::new() })
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
    latest: u32,
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

fn send(w: &Mutex<UnixStream>, e: &Event) {
    let mut s = w.lock().unwrap();
    if let Err(err) = ctl::write_frame(&mut *s, e) {
        eprintln!("navigatord: UI: {err}");
    }
}

fn serve_ui(cfg: Arc<Config>, state: Arc<Mutex<State>>, s: UnixStream) {
    let w = Arc::new(Mutex::new(s.try_clone().expect("clone")));
    let strip = Arc::new(Mutex::new(Strip::default()));
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
        match req {
            Request::Hello { chrome } => {
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
                        let s = w.lock().unwrap();
                        let ok = ctl::write_frame(&mut &*s, &Event::Chrome { fd: true }).and_then(|_| ctl::send_fd(&s, fd.as_raw_fd()));
                        if let Err(e) = ok {
                            eprintln!("navigatord: UI: {e}");
                            return;
                        }
                    }
                    None => send(&w, &Event::Chrome { fd: false }),
                }
            }
            Request::Window { window } => strip.lock().unwrap().window = Some(window),
            Request::Blank => {
                let mut st = strip.lock().unwrap();
                st.latest = 0;
                st.set("");
            }
            Request::Show { url } => {
                let p = state.lock().unwrap().page(&url);
                let mut st = strip.lock().unwrap();
                st.latest = 0;
                st.set(&p.and_then(|p| origin(&p.url)).unwrap_or_default());
            }
            Request::Open { id, url, saved, unrecorded } => {
                if !ctl::clean(&url, ctl::URL_MAX) {
                    send(&w, &Event::Failed { id, url: String::new(), why: "not an address".into() });
                    continue;
                }
                strip.lock().unwrap().latest = id;
                let (cfg, state, w, strip) = (cfg.clone(), state.clone(), w.clone(), strip.clone());
                std::thread::spawn(move || {
                    let t = Instant::now();
                    let hit = state.lock().unwrap().cached(&url);
                    let page = match hit {
                        Some(p) => Ok((p, Source::Cache)),
                        None => {
                            let back = saved.as_deref().map(|a| state.lock().unwrap().reopen(&url, a, unrecorded));
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
                                    convert(&cfg, &url).map(|p| {
                                        let mut st = state.lock().unwrap();
                                        let p = Arc::new(st.stored(p, unrecorded));
                                        st.keep(p.clone(), &url);
                                        (p, Source::Fetched)
                                    })
                                }
                            }
                        }
                    };
                    let ms = t.elapsed().as_millis().min(u32::MAX as u128) as u32;
                    match page {
                        Ok((p, from)) => {
                            eprintln!("navigatord: opened {} in {ms} ms ({} bytes, {from:?})", p.url, p.recording.len());
                            let ticket = state.lock().unwrap().ticket(p.clone());
                            {
                                let st = strip.lock().unwrap();
                                if st.latest == id {
                                    st.set(&origin(&p.url).unwrap_or_default());
                                }
                            }
                            send(&w, &Event::Opened { id, url: p.url.clone(), title: p.title.clone(), insecure: p.insecure, ticket, ms, saved: p.saved.clone(), from });
                        }
                        Err(why) => {
                            eprintln!("navigatord: {url}: {why}");
                            send(&w, &Event::Failed { id, url, why });
                        }
                    }
                });
            }
            Request::Activate { id, page, href } => {
                let p = state.lock().unwrap().page(&page);
                let action = match p {
                    None => LinkAction::Blocked { why: "that page is not open".into() },
                    Some(p) => activate(&p, &href),
                };
                send(&w, &Event::Link { id, action });
            }
        }
    }
}

/// What activating `href` in `page` does: only a link the recording
/// carries (threat model §2.10), decided by the backend's link rules.
fn activate(page: &Page, href: &str) -> LinkAction {
    use navigator_backend::links;
    if href.len() > links::MAX_HREF {
        return LinkAction::Blocked { why: "the link is too long".into() };
    }
    let carried = links::recording_spellings(href).iter().any(|n| page.recording.windows(n.len()).any(|w| w == &n[..]));
    if !carried {
        return LinkAction::Blocked { why: "the page does not carry this link".into() };
    }
    match links::decide(&page.url, href, &links::SchemeAllowlist::default()) {
        Ok(links::LinkAction::Navigate { url, insecure }) => LinkAction::Navigate { url, insecure },
        Ok(links::LinkAction::InDocument { fragment }) => LinkAction::InDocument { fragment },
        Ok(links::LinkAction::Handoff { url, scheme }) => LinkAction::Handoff { url, scheme },
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
    let h = PageHeader { base: p.url.clone(), len: p.recording.len() as u64 };
    if let Err(e) = ctl::write_frame(&mut s, &h).and_then(|_| s.write_all(&p.recording)) {
        eprintln!("navigatord: page child: {e}");
    }
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
    let cfg = Arc::new(Config::from_env(unconfined).unwrap_or_else(|e| {
        eprintln!("navigatord: {e}");
        std::process::exit(2)
    }));
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
    let page_path = std::env::var("NAVIGATORD_PAGE_SOCKET").unwrap_or_else(|_| "/var/run/atrium/navigator-page.sock".into());
    let ctl_path = std::env::var("NAVIGATORD_SOCKET").unwrap_or_else(|_| "/var/run/atrium/navigatord.sock".into());
    let pages = listen(&page_path);
    let st = state.clone();
    std::thread::spawn(move || {
        for s in pages.incoming().flatten() {
            let st = st.clone();
            std::thread::spawn(move || serve_page(st, s));
        }
    });
    let ui = listen(&ctl_path);
    eprintln!("navigatord: control on {ctl_path}, pages on {page_path}");
    for s in ui.incoming().flatten() {
        let (cfg, state) = (cfg.clone(), state.clone());
        std::thread::spawn(move || serve_ui(cfg, state, s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str, doc: &str) -> Page {
        let rec = format!(r#"{{"format":"atrium-navigator-recording/3","url":"{url}","document":"{doc}"}}"#);
        Page { url: url.into(), title: title(rec.as_bytes()), recording: rec.into_bytes(), insecure: false, saved: String::new() }
    }

    #[test]
    fn only_links_the_page_carries_are_followed() {
        let p = page("https://atlas.example/wiki/Atrium", r##"<html><head><title>Atrium & court</title></head><body><a href=\"/wiki/Impluvium\">pool</a><a href=\"#plan\">plan</a><a href=\"javascript:x()\">x</a></body></html>"##);
        assert_eq!(activate(&p, "/wiki/Impluvium"), LinkAction::Navigate { url: "https://atlas.example/wiki/Impluvium".into(), insecure: false });
        assert_eq!(activate(&p, "#plan"), LinkAction::InDocument { fragment: "plan".into() });
        assert!(matches!(activate(&p, "https://evil.example/"), LinkAction::Blocked { .. }), "not in the page");
        assert!(matches!(activate(&p, "javascript:x()"), LinkAction::Blocked { .. }), "a scheme no one allowed");
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
    fn a_parked_page_reopens_from_the_store_only_for_its_address() {
        let mut st = State::default();
        let p = st.stored(page("https://a.example/", "x"), false);
        assert!(ctl::is_address(&p.saved));
        let back = st.reopen("https://a.example/", &p.saved, false).unwrap();
        assert_eq!(back.recording, p.recording);
        assert_eq!(back.saved, p.saved);
        assert!(st.reopen("https://b.example/", &p.saved, false).is_err(), "a record names its address");
        let rec = navigator_backend::session::digest(&p.recording);
        assert!(st.reopen("https://a.example/", &rec, false).is_err(), "a recording is not a record");
        assert!(st.reopen("https://a.example/", "../x", false).is_err());
        // An Unrecorded trail's page is never in the written store.
        let u = st.stored(page("https://u.example/", "y"), true);
        assert!(st.reopen("https://u.example/", &u.saved, false).is_err());
        assert!(st.reopen("https://u.example/", &u.saved, true).is_ok());
        assert!(st.page("https://u.example/").is_none(), "nor known for later");
        // Once out of the cache, a known page is still one links are checked on.
        assert!(st.cached("https://a.example/").is_none());
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
        assert!(st.cached("https://a.example/").is_some());
    }
}
