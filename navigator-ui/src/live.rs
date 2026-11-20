//! Live and parked pages (navigator-ui.md §4.4; §14 steps 2 and 4).
//!
//! navigatord opens pages and decides links; each live page is a
//! doc-viewer child of its own (Limen L6), one jail per page. The child of
//! the page on screen is in the window's page slot; other live children
//! are kept attached and hidden, so going back to one is a swap, not a
//! load.
//!
//! The UI never holds a page's bytes (§1). navigatord answers an `Open`
//! with a one-shot ticket; the UI passes it to the child, which redeems it
//! on navigatord's page socket. A press on a link in the child comes back
//! as the href the page spells, and goes to navigatord, which checks the
//! page carries it and says what it does; only then does the trail move.
//!
//! **Parking.** At most `budget` children are live. Past it, the least
//! recently seen page that is neither on screen nor the parent of the page
//! on screen is parked: its child is asked where it was scrolled (`Park`,
//! answered by `Parked`), then closed; the page keeps that offset and the
//! store address navigatord gave for the version it showed. Showing a
//! parked page opens that version again from navigatord's store, scrolled
//! back to where it was.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use limen::doc_viewer::{Reading, ToChild, ToParent};
use navigator_control::{self as ctl, Event, Jar, LinkAction, Request, Source, Ticket};
use pergola::embed::{EmbedId, Embeds, ParentEvent, WindowToken};
use pergola::print::{Orientation, Outcome, Paper, Printing, Step};

use crate::shell::{LivePage, Out, Shell, PAGE_SLOT};
use crate::trail::PageId;

/// Live pages kept by default (`NAVIGATOR_UI_LIVE` overrides). Each is a
/// jail with its own layout; the broker's own bound is 16 sessions
/// (backend §4.4a), and the UI stays well inside it.
pub const BUDGET: usize = 6;
/// A live page not looked at for this long is parked: well inside the
/// broker's 30-minute idle timeout (backend §4.4b).
pub const IDLE_S: u64 = 20 * 60;
/// How long a child has to answer `Park` before it is closed anyway (the
/// page then comes back from the top).
const PARK_WAIT: Duration = Duration::from_secs(3);

/// A page by its trail's id and its id in the trail.
type Key = (u64, PageId);

struct Child {
    key: Key,
    url: String,
    embed: EmbedId,
    attached: bool,
    /// The `Open` this child waits on.
    asked: Option<u32>,
    /// A ticket waiting for the child to attach.
    pending: Option<Ticket>,
    /// Where to scroll the page when it opens.
    scroll: f32,
    /// When it was asked for, until the page is laid out (timing).
    since: Option<(Instant, Option<Source>)>,
    seen: Instant,
    parking: Option<Instant>,
    /// Whether it was opened with its site knowing you (§6.2): when the
    /// person changes that, the child no longer shows the page they asked
    /// for, and is closed.
    knows: bool,
    /// The reading choices the child was last told (§9.1).
    reading: Reading,
    /// Its page's sections, from `Loaded` (§7 "Jump to section").
    sections: Vec<(String, String)>,
    island_sources: Vec<(String, String)>,
    document: Option<Ticket>,
    runnable: Option<String>,
    selected_widget: Option<(u32, String, [f32; 4])>,
    /// Its page's plain forms (limen `Forms`), for the sign-in window's
    /// form panel.
    forms: Vec<limen::doc_viewer::Form>,
}

#[derive(Clone, Copy)]
struct ActiveWidget {
    page: Key,
    document: Ticket,
    press: ctl::Press,
    key: u32,
    rect: [u32; 4],
}

impl ActiveWidget {
    fn set_origin(self, shell: &mut Shell, origin: Option<String>) {
        if let Some(page) = shell.store.page_by_key_mut(self.page.0, self.page.1) {
            page.attached_origin = origin;
        }
    }
}

fn retire_widget(active: &mut Option<ActiveWidget>, document: Ticket, press: ctl::Press) -> Option<ActiveWidget> {
    active.take_if(|w| w.document == document && w.press == press)
}

pub struct Live {
    w: UnixStream,
    rx: mpsc::Receiver<Event>,
    embeds: Option<Rc<RefCell<Embeds>>>,
    window: Option<WindowToken>,
    widget: Option<ActiveWidget>,
    children: Vec<Child>,
    /// The page whose child is in the slot (`None`: Start).
    shown: Option<Key>,
    /// Whether the UI has said what it shows yet.
    started: bool,
    next: u32,
    /// `Activate` ids, and whether the press asked to open beside.
    activations: HashMap<u32, bool>,
    pub dark: bool,
    pub budget: usize,
    /// The Unrecorded jars of open trails: one that goes (its trail
    /// closed) is forgotten in navigatord and Arca.
    jars: HashSet<u64>,
    /// The sites that know you with a page in an open trail, by jar
    /// (`TrailStore::open_sessions`), as of the last tick.
    sessions: std::collections::BTreeSet<(Jar, String)>,
    /// The history view is open, the page's slot withdrawn.
    history: bool,
    /// Pages to open as their current version, not the stored one
    /// (Reload, "Fetch the current version").
    fresh: HashSet<Key>,
    /// A page not looked at for this long is parked (`NAVIGATOR_UI_IDLE_S`).
    idle: Duration,
    /// The print sheet, while it is open (printing.md §5.1).
    printing: Option<Printing>,
    /// The page child asked to lay its page out on paper: its embed, the
    /// title, and whether the sheet is already open (a new paper size).
    print_ask: Option<(EmbedId, String, bool)>,
    /// Arca's password field in a VPN sign-in form: its embed, for which
    /// form (page, forms sent), and when to ask again after a refusal.
    arca: Option<(EmbedId, ArcaFor)>,
    arca_next: Instant,
}

type ArcaFor = (Option<(u64, PageId)>, u32);

/// The paper a page is first laid out on for the sheet; it asks again for
/// another (`Step::Relayout`).
const PRINT_FIRST: (u32, u32, u32) = (595, 842, 36);
/// The most bytes of PDF a page child hands back.
const PRINT_BYTES_MAX: u64 = 64 << 20;

fn send(w: &mut UnixStream, r: &Request) {
    if let Err(e) = ctl::write_frame(w, r) {
        eprintln!("navigator-ui: navigatord: {e}");
    }
}

fn ms(t: Instant) -> u128 { t.elapsed().as_millis() }

/// A size as the prompt row says it.
pub fn human(n: u64) -> String {
    match n {
        0..=999 => format!("{n} bytes"),
        1000..=999_999 => format!("{:.0} KB", n as f64 / 1e3),
        _ => format!("{:.1} MB", n as f64 / 1e6),
    }
}

/// What navigatord sent with its answer to `Hello`.
pub struct Hello {
    /// The window's frescod connection, opened over the chrome channel.
    pub frescod: Option<OwnedFd>,
    /// A limend connection navigatord opened for us.
    pub limen: Option<OwnedFd>,
    /// A second frescod connection, for measuring text.
    pub measure: Option<OwnedFd>,
    /// The trail store navigatord keeps, when asked and there is one.
    pub trails: Option<String>,
}

impl Live {
    /// Hand navigatord the trail store to keep, in parts.
    pub fn save_trails(&mut self, text: &str) {
        let ps = ctl::parts(text);
        let n = ps.len();
        for (i, p) in ps.into_iter().enumerate() {
            send(&mut self.w, &Request::SaveTrails { part: p.to_string(), last: i + 1 == n });
        }
    }

    /// Connect to navigatord at `path` and say hello. With `chrome`,
    /// navigatord opens the window's frescod connection over the chrome
    /// channel and hands it back (so it can name the page's origin in
    /// frescod's strip); it comes back here when it did.
    ///
    /// The UI also asks for its limend connection: navigatord opens it when
    /// it is configured to (navigator-ui.md §2.3a), so limend's parent is the
    /// owner of the UI's windows; the second descriptor is that, if any.
    pub fn connect(path: &str, chrome: bool) -> io::Result<(Live, Hello)> {
        Self::on(UnixStream::connect(path)?, chrome, false)
    }

    /// Say hello on `s`, a connection to navigatord (or the channel it
    /// started us on). With `trails`, navigatord keeps the trail store
    /// and sends it back here (navigator-ui.md §2.3a J3).
    pub fn on(mut s: UnixStream, chrome: bool, trails: bool) -> io::Result<(Live, Hello)> {
        ctl::write_frame(&mut s, &Request::Hello { chrome, limen: true, measure: true, trails })?;
        let fd = match ctl::read_frame::<Event>(&mut s)? {
            Some(Event::Chrome { fd: true }) => Some(ctl::recv_fd(&s)?),
            Some(Event::Chrome { fd: false }) => None,
            other => return Err(io::Error::other(format!("navigatord said {other:?}"))),
        };
        let limen = match ctl::read_frame::<Event>(&mut s)? {
            Some(Event::Limen { fd: true }) => Some(ctl::recv_fd(&s)?),
            Some(Event::Limen { fd: false }) => None,
            other => return Err(io::Error::other(format!("navigatord said {other:?}"))),
        };
        let measure = match ctl::read_frame::<Event>(&mut s)? {
            Some(Event::Measure { fd: true }) => Some(ctl::recv_fd(&s)?),
            Some(Event::Measure { fd: false }) => None,
            other => return Err(io::Error::other(format!("navigatord said {other:?}"))),
        };
        let mut kept: Option<String> = None;
        if trails {
            loop {
                match ctl::read_frame::<Event>(&mut s)? {
                    Some(Event::Trails { part, last }) => {
                        if let Some(p) = part {
                            let k = kept.get_or_insert_with(String::new);
                            if k.len() + p.len() > ctl::TRAILS_MAX {
                                return Err(io::Error::other("the kept trail store is over its bound"));
                            }
                            k.push_str(&p);
                        }
                        if last {
                            break;
                        }
                    }
                    other => return Err(io::Error::other(format!("navigatord said {other:?}"))),
                }
            }
        }
        let (tx, rx) = mpsc::channel();
        let mut r = s.try_clone()?;
        std::thread::spawn(move || loop {
            match ctl::read_frame::<Event>(&mut r) {
                Ok(Some(e)) => {
                    if tx.send(e).is_err() {
                        return;
                    }
                }
                Ok(None) => {
                    eprintln!("navigator-ui: navigatord closed the connection");
                    return;
                }
                Err(e) => {
                    eprintln!("navigator-ui: navigatord: {e}");
                    return;
                }
            }
        });
        let idle = Duration::from_secs(std::env::var("NAVIGATOR_UI_IDLE_S").ok().and_then(|v| v.parse().ok()).filter(|&n| n >= 1).unwrap_or(IDLE_S));
        let budget = std::env::var("NAVIGATOR_UI_LIVE").ok().and_then(|v| v.parse().ok()).filter(|&n| n >= 1).unwrap_or(BUDGET);
        Ok((
            Live {
                w: s,
                rx,
                embeds: None,
                window: None,
                widget: None,
                children: vec![],
                shown: None,
                started: false,
                next: 1,
                activations: HashMap::new(),
                dark: false,
                budget,
                jars: HashSet::new(),
                sessions: Default::default(),
                history: false,
                fresh: HashSet::new(),
                idle,
                printing: None,
                print_ask: None,
                arca: None,
                arca_next: Instant::now(),
            },
            Hello { frescod: fd, limen, measure, trails: kept },
        ))
    }

    /// The window navigatord names: its id on the connection it opened.
    pub fn window(&mut self, id: u32, token: WindowToken) {
        send(&mut self.w, &Request::RegisterWindow { window: id, token });
    }

    /// limend, and the window's token for it.
    pub fn embed(&mut self, embeds: Rc<RefCell<Embeds>>, window: WindowToken) {
        self.window = Some(window);
        self.embeds = Some(embeds);
    }

    /// How many page children are live (not being parked).
    pub fn live(&self) -> usize {
        self.children.iter().filter(|c| c.parking.is_none()).count()
    }

    fn widget_bounds(&self, s: &Shell, page: Key, key: u32, source: Option<&str>) -> Option<(Ticket, [u32; 4])> {
        if s.history.is_some() || self.shown != Some(page) || s.shown_key() != Some(page) { return None; }
        let child = self.children.iter().find(|c| c.key == page)?;
        let (id, src, [x,y,w,h]) = child.selected_widget.as_ref()?;
        if *id != key || child.runnable.as_ref() != Some(src) || source.is_some_and(|v| v != src) { return None; }
        let slot = s.geo().page;
        let left = x.max(0.).ceil();
        let top = y.max(0.).ceil();
        let right = (x+w).min(slot.w()).floor();
        let bottom = (y+h).min(slot.h()).floor();
        if right <= left || bottom <= top { return None; }
        Some((child.document?, [(slot.x()+left).ceil() as u32, (slot.y()+top).ceil() as u32,
            (right-left) as u32, (bottom-top) as u32]))
    }

    fn id(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    fn to_child(&self, embed: EmbedId, m: &ToChild) {
        let Some(e) = &self.embeds else { return };
        if let Err(err) = e.borrow_mut().send(embed, m.encode()) {
            eprintln!("navigator-ui: limend: {err:?}");
        }
    }

    /// Ask the page shown to lay itself out on `w`×`h` points.
    fn ask_print(&mut self, s: &mut Shell, w: u32, h: u32, margin: u32, again: bool) {
        let shown = self.shown.and_then(|k| self.children.iter().find(|c| c.key == k && c.attached)).map(|c| c.embed);
        let Some(embed) = shown else {
            self.print_done(s, Outcome::Failed("the page isn't open any more".into()));
            return;
        };
        let title = s.store.trail().page().map(|p| p.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Page".into());
        let margin = margin.min(w.min(h) / 4);
        eprintln!("navigator-ui: print: the page laid out on {w}x{h} pt");
        self.to_child(embed, &ToChild::Print { w_pt: w, h_pt: h, margin_pt: margin });
        self.print_ask = Some((embed, title, again));
    }

    fn print_done(&mut self, s: &mut Shell, o: Outcome) {
        if let (Some(p), Some(e)) = (self.printing.take(), &self.embeds) {
            if matches!(o, Outcome::Failed(_)) {
                p.cancel(e);
            }
        }
        self.print_ask = None;
        s.printing = false;
        eprintln!("navigator-ui: print: {o:?}");
        s.last_note = match o {
            Outcome::Queued => Some("sent to the printer".into()),
            Outcome::Saved(name) => Some(format!("saved “{name}”")),
            Outcome::Cancelled => None,
            Outcome::Failed(why) => Some(format!("couldn't print: {why}")),
        };
    }

    /// The page child's PDF: the sheet opens with it, or takes it as its
    /// answer to a new paper size.
    fn printed(&mut self, s: &mut Shell, fd: Option<OwnedFd>, pages: u32, cut: bool) {
        use std::io::Read;
        let Some((_, title, again)) = self.print_ask.take() else { return };
        let mut pdf = Vec::new();
        let read = fd.ok_or_else(|| "no file came with it".to_string()).and_then(|fd| {
            std::fs::File::from(fd).take(PRINT_BYTES_MAX + 1).read_to_end(&mut pdf).map_err(|e| e.to_string())
        });
        let r = match (read, &self.embeds, self.window) {
            (Err(e), ..) => Err(e),
            (Ok(n), ..) if n as u64 > PRINT_BYTES_MAX => Err("the page is too long to print".into()),
            (Ok(_), Some(e), _) if again => match &mut self.printing {
                Some(p) => p.document_bytes(e, &pdf).map_err(|e| format!("{e:?}")),
                None => Ok(()),
            },
            (Ok(_), Some(e), Some(w)) => Printing::start_bytes(e, w, crate::shell::PRINT_SLOT, &pdf, &title, true, Some(Paper::A4)).map(|p| {
                self.printing = Some(p);
            }).map_err(|e| format!("the print sheet didn't open ({e:?})")),
            _ => Err("Limen isn't running".into()),
        };
        match r {
            Ok(()) => eprintln!("navigator-ui: print sheet {} ({pages} pages{}, {} bytes)", if again { "has the page again" } else { "open" }, if cut { ", cut short" } else { "" }, pdf.len()),
            Err(why) => self.print_done(s, Outcome::Failed(why)),
        }
    }

    fn close(&mut self, s: &mut Shell, i: usize) {
        s.widget_placement(self.children[i].key, None, None, crate::ui::now_ms());
        let c = self.children.remove(i);
        if self.shown == Some(c.key) {
            self.shown = None;
        }
        if let Some(e) = &self.embeds {
            if let Err(err) = e.borrow_mut().detach_embed(c.embed) {
                eprintln!("navigator-ui: limend: {err:?}");
            }
        }
    }

    fn give(&mut self, i: usize, ticket: Ticket) {
        let c = &mut self.children[i];
        if c.attached {
            let (embed, scroll, reading) = (c.embed, c.scroll, c.reading);
            self.to_child(embed, &ToChild::Open { ticket, reading, scroll });
        } else {
            c.pending = Some(ticket);
        }
    }

    /// Arca's field for the form panel's password (a VPN sign-in window):
    /// asked for while the form shows one, anew for each form sent, gone
    /// with the form. A press waiting for its handle goes at the deadline.
    fn arca_field(&mut self, s: &mut Shell) -> bool {
        s.arca_tick(Instant::now());
        let want: Option<ArcaFor> = s.form.as_ref().and_then(|f| f.arca_field().map(|_| (f.key, f.sent)));
        let Some(e) = self.embeds.clone() else { return false };
        let mut changed = false;
        if let Some((id, of)) = self.arca {
            if Some(of) != want {
                let _ = e.borrow_mut().detach_embed(id);
                self.arca = None;
                changed = true;
            }
        }
        if let (Some(of), None, Some(w)) = (want, self.arca, self.window) {
            if Instant::now() >= self.arca_next {
                self.arca_next = Instant::now() + Duration::from_secs(2);
                let asked = e.borrow_mut().request("credential", w, crate::shell::ARCA_SLOT);
                match asked {
                    Ok(id) => {
                        let c = limen::credential::ToChild::Configure { purpose: limen::credential::Purpose::CurrentPassword, username_field: false };
                        let _ = e.borrow_mut().send(id, c.encode());
                        self.arca = Some((id, of));
                        changed = true;
                    }
                    Err(err) => eprintln!("navigator-ui: Arca's password field: {err:?}"),
                }
            }
        }
        changed
    }

    /// A new child for the page `key`, in the slot, and navigatord asked
    /// to open it (the version it saved, if the page was parked).
    fn start(&mut self, s: &mut Shell, key: Key) {
        let (Some(e), Some(w)) = (self.embeds.clone(), self.window) else { return };
        let Some((url, saved, scroll)) = s.store.page_by_key_mut(key.0, key.1).map(|p| (p.url.clone(), p.saved.clone(), p.scroll)) else { return };
        let (jar, knows) = s.store.presence_of(key.0, &url).unwrap_or((Jar::Personal, false));
        if jar == Jar::Personal && !s.store.trail().anonymous && url == ctl::INBOX_FIXTURE_URL
            && std::env::var("NAVIGATOR_UI_INBOX_FIXTURE").ok().as_deref() == Some(ctl::INBOX_FIXTURE_URL)
        {
            let _ = e.borrow_mut().hide(PAGE_SLOT);
            self.fresh.remove(&key);
            send(&mut self.w, &Request::Blank);
            s.live = Some(LivePage::LiveOffer);
            s.ask_at(key, crate::trail::Prompt::RunLive, crate::ui::now_ms());
            return;
        }
        let embed = {
            let mut e = e.borrow_mut();
            if let Err(err) = e.hide(PAGE_SLOT) {
                eprintln!("navigator-ui: limend: {err:?}");
            }
            match e.request("doc-viewer", w, PAGE_SLOT) {
                Ok(id) => id,
                Err(err) => {
                    eprintln!("navigator-ui: limend: {err:?}");
                    s.live = Some(LivePage::Failed { url, why: "no page worker could be started".into() });
                    return;
                }
            }
        };
        let reading = s.store.reading_of(key.0, &url).for_child(self.dark);
        let id = self.id();
        eprintln!("navigator-ui: asked limend for a page child for {url} (embed {embed}){}", if saved.is_some() { ", bringing back a parked page" } else { "" });
        self.children.push(Child {
            key,
            url: url.clone(),
            embed,
            attached: false,
            asked: Some(id),
            pending: None,
            scroll,
            since: Some((Instant::now(), None)),
            seen: Instant::now(),
            parking: None,
            knows,
            reading,
            sections: Vec::new(),
            island_sources: Vec::new(),
            document: None, runnable: None, selected_widget: None,
            forms: Vec::new(),
        });
        s.live = Some(LivePage::Loading(url.clone()));
        let fresh = self.fresh.remove(&key);
        if fresh {
            eprintln!("navigator-ui: fetching the current version of {url}");
        }
        send(&mut self.w, &Request::OpenWithIslands { id, url, saved, jar, knows, fresh });
    }

    /// Park the least recently seen pages past the budget.
    /// Park the least recently seen pages past the budget, and any page
    /// not looked at for `idle` (§4.4: an idle expiry is a park, never a
    /// loss: the UI parks a page well before a broker's idle timeout).
    /// Never the page on screen or its parent.
    fn park(&mut self, s: &mut Shell) {
        let parent = self.shown.and_then(|(t, p)| (s.store.trail().id == t).then(|| s.store.trail().pages.get(p).and_then(|q| q.parent)).flatten().map(|q| (t, q)));
        let (shown, idle) = (self.shown, self.idle);
        let parkable = move |c: &Child| c.parking.is_none() && Some(c.key) != shown && Some(c.key) != parent;
        while self.live() > self.budget {
            let pick = self.children.iter().enumerate().filter(|(_, c)| parkable(c)).min_by_key(|(_, c)| c.seen).map(|(i, _)| i);
            let Some(i) = pick else { break };
            let why = format!("{} live, budget {}", self.live(), self.budget);
            self.park_one(s, i, &why);
        }
        while let Some(i) = self.children.iter().position(|c| parkable(c) && c.seen.elapsed() > idle) {
            let why = format!("not looked at for {} s", self.children[i].seen.elapsed().as_secs());
            self.park_one(s, i, &why);
        }
    }

    fn park_one(&mut self, s: &mut Shell, i: usize, why: &str) {
        let c = &mut self.children[i];
        c.parking = Some(Instant::now());
        eprintln!("navigator-ui: parking {} ({why})", c.url);
        if self.children[i].attached {
            let embed = self.children[i].embed;
            self.to_child(embed, &ToChild::Park);
        } else {
            self.close(s, i);
        }
    }

    /// Bring the shell and navigatord up to date with each other and with
    /// the page children. Returns whether the shell changed.
    pub fn tick(&mut self, s: &mut Shell) -> bool {
        let mut changed = false;
        // Stored versions only a forgotten trail named (§4.5).
        if !s.store.orphaned.is_empty() {
            let records = std::mem::take(&mut s.store.orphaned);
            eprintln!("navigator-ui: a forgotten trail: {} stored versions to take out", records.len());
            for chunk in records.chunks(navigator_control::CLEAR_MAX) {
                send(&mut self.w, &Request::Clear { records: chunk.to_vec() });
            }
        }
        // navigatord's answers.
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            match ev {
                Event::RunnableWidget { id, source } => {
                    if let Some(c) = self.children.iter_mut().find(|c| c.asked == Some(id)) { c.runnable = Some(source); }
                }
                Event::IslandSources { id, sources } => {
                    if let Some(child) = self.children.iter_mut().find(|c| c.asked == Some(id)) {
                        child.island_sources = sources;
                    }
                }
                Event::Opened { id, url, title, insecure, ticket, ms: took, saved, from, knows, asked, note } => {
                    let Some(i) = self.children.iter().position(|c| c.asked == Some(id)) else { continue };
                    eprintln!("navigator-ui: opened {url} in {took} ms ({from:?}{})", if knows { ", the site knowing you" } else { "" });
                    if let Some(n) = note {
                        s.last_note = Some(n);
                    }
                    // Asked for, and not carried: the control shows what is
                    // so (§6.2), and this child already matches it.
                    if self.children[i].knows && !knows {
                        let (t, u) = (self.children[i].key.0, self.children[i].url.clone());
                        s.store.unknow(t, &u);
                        self.children[i].knows = false;
                    }
                    // The site asked (§6.2): it knows you now in this jar.
                    if asked && knows && !self.children[i].knows {
                        let (t, u) = (self.children[i].key.0, url.clone());
                        eprintln!("navigator-ui: {} asked to know you; it does now in this trail's jar", crate::trail::site(&u));
                        s.store.know(t, &u);
                        self.children[i].knows = true;
                    }
                    let c = &mut self.children[i];
                    c.document = Some(ticket);
                    c.asked = None;
                    if let Some((_, f)) = &mut c.since {
                        *f = Some(from);
                    }
                    let (key, asked) = (c.key, std::mem::replace(&mut c.url, url.clone()));
                    s.opened(key, &asked, &url, &title, insecure, &saved);
                    if self.shown == Some(key) {
                        s.live = Some(LivePage::Shown(url));
                    }
                    self.give(i, ticket);
                }
                Event::Failed { id, url, why } => {
                    let Some(i) = self.children.iter().position(|c| c.asked == Some(id)) else { continue };
                    eprintln!("navigator-ui: {url}: {why}");
                    if self.shown == Some(self.children[i].key) {
                        s.live = Some(LivePage::Failed { url, why });
                    }
                    let shown = self.shown;
                    self.close(s, i);
                    // Keep Failed on screen rather than trying again.
                    self.shown = shown;
                }
                Event::Link { id, action } => {
                    let beside = self.activations.remove(&id).unwrap_or(false);
                    match action {
                        LinkAction::Navigate { url, .. } => s.follow_link(&url, beside),
                        // The page shown scrolls to it; the trail is unchanged.
                        LinkAction::InDocument { fragment } => {
                            if let Some(c) = self.shown.and_then(|k| self.children.iter().find(|c| c.key == k && c.attached)) {
                                eprintln!("navigator-ui: in-page link #{fragment}");
                                self.to_child(c.embed, &ToChild::ScrollTo { anchor: fragment });
                            }
                        }
                        LinkAction::Handoff { url, scheme, app } => s.handoff(&url, &scheme, app, crate::ui::now_ms()),
                        LinkAction::Blocked { why } => s.last_note = Some(format!("link refused: {why}")),
                        LinkAction::Consented { url } => {
                            eprintln!("navigator-ui: the reader's answer on {url} is kept; it opens again with the site knowing you");
                            s.consented(&url);
                        }
                    }
                }
                Event::File { id, url, name, size, origin, kind } => {
                    // A link to a file (§6.4): the page it was going to be
                    // is taken back, and the page it was on asks.
                    let Some(i) = self.children.iter().position(|c| c.asked == Some(id)) else {
                        send(&mut self.w, &Request::Discard { id });
                        continue;
                    };
                    let key = self.children[i].key;
                    eprintln!("navigator-ui: {url} is a file ({kind}, {size} bytes); asking");
                    self.close(s, i);
                    let prompt = crate::trail::Prompt::Download { id, file: name.clone(), size: human(size), origin, kind };
                    let now = crate::ui::now_ms();
                    let parent = s.store.trails.iter_mut().find(|t| t.id == key.0).and_then(|t| t.retract(key.1));
                    let asked = match parent {
                        Some(p) => s.ask_at((key.0, p), prompt, now),
                        // Typed, with nowhere to go back to: the page says
                        // what it is and asks.
                        None => {
                            if let Some(p) = s.store.page_by_key_mut(key.0, key.1) {
                                p.title = name.clone();
                            }
                            if self.shown == Some(key) {
                                s.live = Some(LivePage::File { url: url.clone(), name });
                            }
                            s.ask_at(key, prompt, now)
                        }
                    };
                    if !asked {
                        send(&mut self.w, &Request::Discard { id });
                    }
                    if self.shown == Some(key) && parent.is_some() {
                        self.shown = None;
                    }
                }
                Event::Saved { path, .. } => s.last_note = Some(format!("saved as {path}")),
                Event::NotSaved { why, .. } => s.last_note = Some(format!("not saved: {why}")),
                Event::FileOpened { app, why, .. } => {
                    let note = match (app, why) {
                        (Some(a), _) => format!("opened in {a}"),
                        (None, w) => format!("not opened: {}", w.unwrap_or_default()),
                    };
                    eprintln!("navigator-ui: download {note}");
                    s.last_note = Some(note);
                }
                Event::PressRefused { why } => s.press_refused(&why, crate::ui::now_ms()),
                Event::WindowIn { persona, ok, why } => {
                    let note = if ok { format!("a window in {persona}") } else { format!("no window in {persona}: {}", why.unwrap_or_default()) };
                    eprintln!("navigator-ui: {note}");
                    s.persona_open = false;
                    s.last_note = Some(note)
                }
                Event::Handed { url, ok, why } => {
                    let note = if ok { format!("handed {url} on") } else { format!("{url} not handed on: {}", why.unwrap_or_default()) };
                    eprintln!("navigator-ui: {note}");
                    s.last_note = Some(note)
                }
                Event::Incoming { url, from } => {
                    if s.incoming(&url, from.as_deref()) {
                        let anon = if s.store.trail().anonymous { ", anonymous" } else { "" };
                        match &from {
                            None => eprintln!("navigator-ui: a link from another app: a new trail{anon}"),
                            Some(f) => eprintln!("navigator-ui: a link sent from {f}: a new trail{anon}"),
                        }
                    }
                }
                Event::WidgetAttached { document, press, origin } => {
                    if let Some(active) = self.widget.filter(|w| w.document == document && w.press == press) {
                        active.set_origin(s, Some(origin));
                    }
                }
                Event::WidgetStopped { document, press } => {
                    if let Some(active) = retire_widget(&mut self.widget, document, press) {
                        active.set_origin(s, None);
                        s.last_note = Some("live widget stopped".into());
                    }
                }
                Event::WidgetFailed { document, press, failure } => {
                    if let Some(active) = retire_widget(&mut self.widget, document, press) {
                        active.set_origin(s, None);
                        if let Some(page) = self.shown.filter(|&p| self.widget_bounds(s, p, active.key, None)
                            .is_some_and(|(d, _)| d == document)) {
                            s.ask_at(page, crate::trail::Prompt::WidgetFailed { key: active.key, failure }, crate::ui::now_ms());
                        }
                    }
                }
                Event::LivePageFailed { why } => s.last_note = Some(why),
                Event::SignedOut { url, cookies, why } => {
                    let site = crate::trail::site(&url).to_string();
                    eprintln!("navigator-ui: signed out of {site} here: {}", match (cookies, &why) {
                        (Some(n), _) => format!("{n} session cookies ended"),
                        (None, w) => format!("not done ({})", w.clone().unwrap_or_default()),
                    });
                    s.last_note = Some(match cookies {
                        Some(_) => format!("signed out of {site} here"),
                        None => format!("couldn't sign out of {site} here: {}", why.unwrap_or_default()),
                    });
                }
                Event::Accounts { url, accounts, why } => {
                    eprintln!("navigator-ui: Arca holds {} accounts for {}{}", accounts.len(), crate::trail::site(&url), why.as_deref().map(|w| format!(" ({w})")).unwrap_or_default());
                    s.accounts_answer(&url, accounts, why);
                }
                Event::Chrome { .. } | Event::Limen { .. } | Event::Measure { .. } | Event::Trails { .. } | Event::Settings { .. } => {}
                Event::TrailsNotSaved { why } => eprintln!("navigator-ui: trail store not kept: {why}"),
            }
        }
        changed |= self.arca_field(s);
        // The page children.
        loop {
            let ev = match &self.embeds {
                Some(e) => e.borrow_mut().poll(Duration::from_millis(1)),
                None => break,
            };
            let ev = match ev {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("navigator-ui: limend: {e:?}");
                    break;
                }
            };
            // The print sheet's own events first.
            if let (Some(p), Some(e)) = (self.printing.as_mut(), self.embeds.clone()) {
                match p.handle(&e, &ev) {
                    None => {}
                    Some(Step::Pending) => continue,
                    Some(Step::Relayout { paper, orientation, margins_pt }) => {
                        let (w, h) = if orientation == Orientation::Landscape { (paper.h_pt, paper.w_pt) } else { (paper.w_pt, paper.h_pt) };
                        self.ask_print(s, w, h, margins_pt, true);
                        changed = true;
                        continue;
                    }
                    Some(Step::Done(o)) => {
                        self.print_done(s, o);
                        changed = true;
                        continue;
                    }
                }
            }
            let embed = match &ev {
                ParentEvent::Attached { embed } | ParentEvent::Message { embed, .. } | ParentEvent::MessageFd { embed, .. } | ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed } => *embed,
                _ => continue,
            };
            // A descriptor only a print asked for: any other is closed.
            let fd = match &ev {
                ParentEvent::MessageFd { .. } => self.embeds.as_ref().and_then(|e| e.borrow_mut().take_fd()),
                _ => None,
            };
            if self.arca.as_ref().is_some_and(|a| a.0 == embed) {
                match ev {
                    ParentEvent::Message { bytes, .. } => match limen::credential::ToParent::decode(&bytes) {
                        Some(limen::credential::ToParent::Handle { handle }) => s.arca_handle(Some(handle)),
                        Some(limen::credential::ToParent::Cleared) => s.arca_handle(None),
                        Some(limen::credential::ToParent::Filled { .. } | limen::credential::ToParent::PrintLoginSaved) => {}
                        None => eprintln!("navigator-ui: dropped a malformed message from Arca's field"),
                    },
                    ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. } => {
                        // What was typed went with Arca's side: a new field.
                        eprintln!("navigator-ui: Arca's field went away");
                        self.arca = None;
                        s.arca_handle(None);
                    }
                    _ => {}
                }
                changed = true;
                continue;
            }
            let Some(i) = self.children.iter().position(|c| c.embed == embed) else { continue };
            let on_screen = self.shown == Some(self.children[i].key);
            match ev {
                ParentEvent::Attached { .. } => {
                    eprintln!("navigator-ui: page child attached");
                    self.children[i].attached = true;
                    if let Some(t) = self.children[i].pending.take() {
                        self.give(i, t);
                    }
                }
                ParentEvent::MessageFd { bytes, .. } => match ToParent::decode(&bytes) {
                    Some(ToParent::Printed { pages, cut }) if self.print_ask.as_ref().is_some_and(|a| a.0 == embed) => {
                        self.printed(s, fd, pages, cut);
                        changed = true;
                    }
                    _ => eprintln!("navigator-ui: a page child sent a file nobody asked for; closed"),
                },
                ParentEvent::Message { bytes, .. } => match ToParent::decode(&bytes) {
                    Some(ToParent::LinkActivated { url }) if on_screen => {
                        s.link_pressed(&url);
                        changed = true;
                    }
                    Some(ToParent::LinkHover { url }) if on_screen => {
                        s.hover_hint = url;
                        changed = true;
                    }
                    Some(ToParent::LinkActivated { .. } | ToParent::LinkHover { .. }) => {}
                    Some(ToParent::IslandPressed { key, src, rect }) if on_screen => {
                        let child = &mut self.children[i];
                        let runnable = child.runnable.as_deref() == Some(&src);
                        child.selected_widget = Some((key, src.clone(), rect));
                        // A slow engine may still leave the viewer's placeholder exposed.
                        // Only offer Start once for the current slot while its request is live.
                        let starting = self.widget.is_some_and(|w| child.document == Some(w.document)
                            && w.key == key && runnable);
                        if !starting {
                            changed |= s.widget_pressed(child.key, &child.island_sources, &src, key, rect, runnable, crate::ui::now_ms());
                        }
                    }
                    Some(ToParent::IslandPressed { .. }) => {}
                    Some(ToParent::IslandPlacement { key, rect }) => {
                        let child = &mut self.children[i];
                        if let Some((id, _, bounds)) = &mut child.selected_widget {
                            if *id == key {
                                if let Some(r) = rect { *bounds = r; } else { child.selected_widget = None; }
                            }
                        }
                        changed |= s.widget_placement(child.key, Some(key), rect, crate::ui::now_ms());
                    }
                    Some(ToParent::Loaded { title, sections, .. }) => {
                        let c = &mut self.children[i];
                        c.sections = sections;
                        eprintln!("navigator-ui: the page child laid out \"{title}\"");
                        if let Some((t, from)) = c.since.take() {
                            let how = match from {
                                Some(Source::Store) => "brought back from the store",
                                Some(Source::Cache) => "opened from navigatord's cache",
                                _ => "fetched and opened",
                            };
                            eprintln!("navigator-ui: page {} {how} in {} ms (to laid out, at y={:.0})", c.url, ms(t), c.scroll);
                        }
                    }
                    Some(ToParent::Parked { scroll }) => {
                        let c = &self.children[i];
                        if let Some(p) = s.store.page_by_key_mut(c.key.0, c.key.1) {
                            p.scroll = scroll;
                        }
                        match c.parking {
                            Some(t) => {
                                eprintln!("navigator-ui: parked {} at y={scroll:.0} in {} ms", c.url, ms(t));
                                self.close(s, i);
                            }
                            // Shown again while it was being parked: keep it.
                            None => {}
                        }
                        changed = true;
                    }
                    Some(ToParent::Forms { forms }) => {
                        eprintln!("navigator-ui: the page has {} form(s)", forms.len());
                        self.children[i].forms = forms;
                        changed = true;
                    }
                    Some(ToParent::Found { count, current, .. }) => {
                        if on_screen {
                            eprintln!("navigator-ui: find: {count} matches, at {current}");
                            s.found(count, current);
                            changed = true;
                        }
                    }
                    Some(ToParent::Zoomed { zoom_64 }) => {
                        // The person zoomed in the page: remembered for its
                        // site, and the child is not told what it did.
                        self.children[i].reading.zoom_64 = zoom_64;
                        if on_screen {
                            eprintln!("navigator-ui: the page zoomed to {}%", zoom_64 as u32 * 100 / 64);
                            s.reading_change(|r| r.zoom_64 = zoom_64);
                            changed = true;
                        }
                    }
                    Some(ToParent::Key { key, chars, ctrl, shift, alt }) if on_screen => {
                        // Only what the chrome's own map has: the child
                        // can do no more than a press in the chrome.
                        let ev = crate::keys::from_child(key, &chars, ctrl, shift, alt);
                        if let Some(a) = crate::keys::action(&ev) {
                            eprintln!("navigator-ui: a key from the page: {a:?}");
                            match a {
                                crate::keys::Action::NextRegion | crate::keys::Action::PrevRegion => s.region_from_page(a == crate::keys::Action::NextRegion),
                                _ => s.apply(a, crate::ui::now_ms()),
                            }
                            changed = true;
                        } else if key == limen::doc_viewer::ChromeKey::Escape {
                            changed |= s.escape();
                        }
                    }
                    Some(ToParent::Key { .. }) => {}
                    Some(ToParent::Error { code }) if code == limen::doc_viewer::error::PRINT_FAILED && self.print_ask.as_ref().is_some_and(|a| a.0 == embed) => {
                        self.print_done(s, Outcome::Failed("the page couldn't be laid out on paper".into()));
                        changed = true;
                    }
                    Some(ToParent::Printed { .. }) => eprintln!("navigator-ui: a print came without its file"),
                    Some(ToParent::Error { code }) => {
                        eprintln!("navigator-ui: a page child failed ({code})");
                        if on_screen {
                            let url = self.children[i].url.clone();
                            s.live = Some(LivePage::Failed { url, why: format!("the page could not be shown (code {code})") });
                            changed = true;
                        }
                    }
                    Some(ToParent::Thumbnail { .. }) => eprintln!("navigator-ui: dropped an unsolicited thumbnail from a page child"),
                    None => eprintln!("navigator-ui: dropped a malformed message from a page child"),
                },
                ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. } => {
                    // A lost page on screen gets a new child next (from
                    // the store: it was saved when it opened).
                    eprintln!("navigator-ui: a page child went away ({})", self.children[i].url);
                    if self.print_ask.as_ref().is_some_and(|a| a.0 == embed) {
                        self.print_done(s, Outcome::Failed("the page closed".into()));
                    }
                    s.widget_placement(self.children[i].key, None, None, crate::ui::now_ms());
                    self.children.remove(i);
                    if on_screen {
                        self.shown = None;
                        self.started = false;
                    }
                }
                _ => {}
            }
        }
        // Children slow to answer `Park` are closed anyway.
        while let Some(i) = self.children.iter().position(|c| c.parking.is_some_and(|t| t.elapsed() > PARK_WAIT)) {
            eprintln!("navigator-ui: {} did not answer Park; closed", self.children[i].url);
            self.close(s, i);
        }
        // Links the shell wants decided. Taking them is a change: the
        // caller writes the shell back only when this returns true, and an
        // outbox left in it would be sent again next tick.
        changed |= !s.outbox.is_empty();
        for o in std::mem::take(&mut s.outbox) {
            match o {
                Out::Activate { page, href, beside, jar } => {
                    let id = self.id();
                    self.activations.insert(id, beside);
                    send(&mut self.w, &Request::Activate { id, page, href, jar });
                }
                Out::Forget { jar, url } => send(&mut self.w, &Request::Forget { jar, url: Some(url) }),
                Out::Moved { keys } => {
                    // The pages' children follow them to their new trail.
                    let to = |k: Key| keys.iter().find(|(o, _)| *o == k).map(|(_, n)| *n);
                    for c in &mut self.children {
                        if let Some(n) = to(c.key) {
                            c.key = n;
                        }
                    }
                    if let Some(n) = self.shown.and_then(to) {
                        self.shown = Some(n);
                    }
                    self.fresh = self.fresh.iter().map(|&k| to(k).unwrap_or(k)).collect();
                    eprintln!("navigator-ui: a branch of {} pages is its own trail now", keys.len());
                }
                Out::SignOut { jar, url } => send(&mut self.w, &Request::SignOut { jar, url }),
                Out::Accounts { url } => send(&mut self.w, &Request::Accounts { url }),
                Out::Save { id, press } => send(&mut self.w, &Request::Save { id, press }),
                Out::Discard { id } => send(&mut self.w, &Request::Discard { id }),
                Out::StartWidget { page, key, src, press } => {
                    if let Some((document, rect)) = self.widget_bounds(s, page, key, Some(&src)) {
                        send(&mut self.w, &Request::StartWidget { document, source: src, rect, press });
                        if let Some(old) = self.widget { old.set_origin(s, None); }
                        self.widget = Some(ActiveWidget { page, document, key, rect, press });
                    }
                }
                Out::RunLive { url, jar, press } => send(&mut self.w, &Request::RunLive { url, jar, press }),
                Out::OpenFile { id, press } => send(&mut self.w, &Request::OpenFile { id, press }),
                Out::SubmitForm { page, jar, action, post, fields, press } => {
                    let id = self.id();
                    self.activations.insert(id, false);
                    send(&mut self.w, &Request::SubmitForm { id, page, jar, action, post, fields, press });
                }
                Out::Hand { url, how, press } => send(&mut self.w, &Request::Hand { url, how, press }),
                Out::NewWindow { persona, press } => send(&mut self.w, &Request::NewWindow { persona, press }),
                Out::Reload { key } => {
                    // Its child closes; the page opens again, fetched.
                    if let Some(i) = self.children.iter().position(|c| c.key == key) {
                        self.close(s, i);
                    }
                    self.fresh.insert(key);
                    if s.shown_key() == Some(key) {
                        self.started = false;
                    }
                }
                Out::Clear { keys, records } => {
                    while let Some(i) = self.children.iter().position(|c| keys.contains(&c.key) && Some(c.key) != self.shown) {
                        self.close(s, i);
                    }
                    eprintln!("navigator-ui: history cleared: {} pages, {} stored versions to take out", keys.len(), records.len());
                    for chunk in records.chunks(navigator_control::CLEAR_MAX) {
                        send(&mut self.w, &Request::Clear { records: chunk.to_vec() });
                    }
                }
                Out::Find { query, step } => {
                    if let Some(c) = self.shown.and_then(|k| self.children.iter().find(|c| c.key == k && c.attached)) {
                        self.to_child(c.embed, &ToChild::Find { query, step });
                    }
                }
                Out::Print => {
                    let (w, h, m) = PRINT_FIRST;
                    self.ask_print(s, w, h, m, false);
                }
                Out::Jump { anchor } => {
                    if let Some(c) = self.shown.and_then(|k| self.children.iter().find(|c| c.key == k && c.attached)) {
                        eprintln!("navigator-ui: jump to section #{anchor}");
                        self.to_child(c.embed, &ToChild::ScrollTo { anchor });
                    }
                }
            }
        }
        // The page shown's forms, for the sign-in window's form panel.
        if s.portal.is_some() {
            let shown = self.shown.and_then(|k| self.children.iter().find(|c| c.key == k));
            let forms = shown.map(|c| c.forms.clone()).unwrap_or_default();
            let now = s.form.as_ref().map(|f| (f.key, f.form.clone()));
            let want = forms.first().map(|f| (s.shown_key(), f.clone()));
            if now != want {
                s.set_forms(forms);
                changed = true;
            }
        }
        // The page shown's sections, for the find bar.
        let secs = self.shown.and_then(|k| self.children.iter().find(|c| c.key == k)).map(|c| c.sections.clone()).unwrap_or_default();
        if s.sections != secs {
            s.set_sections(secs);
            changed = true;
        }
        // Unrecorded trails that closed: their jars are forgotten.
        let jars: HashSet<u64> = s.store.trails.iter().filter(|t| t.unrecorded).map(|t| t.jar).collect();
        for &j in self.jars.difference(&jars) {
            eprintln!("navigator-ui: an Unrecorded trail closed; forgetting its jar");
            send(&mut self.w, &Request::Forget { jar: Jar::Unrecorded(j), url: None });
        }
        // A site's last page in a jar went, and it still knows you there:
        // its session cookies end (Arca §8.6). A closed Unrecorded trail's
        // jar was forgotten whole just above.
        let sessions = s.store.open_sessions();
        for (jar, site) in self.sessions.difference(&sessions) {
            let gone = matches!(jar, Jar::Unrecorded(j) if !jars.contains(j));
            if !gone && s.store.knows_in(*jar, site) {
                eprintln!("navigator-ui: the last page of {site} in its jar went; ending its session");
                send(&mut self.w, &Request::EndSession { jar: *jar, url: format!("https://{site}/") });
            }
        }
        self.sessions = sessions;
        self.jars = jars;
        // Pages whose site the person let remember them, or forgot, since
        // they opened: closed, to open again as asked.
        while let Some(i) = self.children.iter().position(|c| {
            c.parking.is_none() && s.store.presence_of(c.key.0, &c.url).is_some_and(|(_, k)| k != c.knows)
        }) {
            eprintln!("navigator-ui: {} opens again: its site {} you now", self.children[i].url, if self.children[i].knows { "forgot" } else { "knows" });
            let on_screen = self.shown == Some(self.children[i].key);
            self.close(s, i);
            if on_screen {
                self.started = false;
            }
        }
        // The history view is drawn where the page is: the page's window
        // is withdrawn while it is open, and shown again after.
        if s.history.is_some() != self.history {
            self.history = s.history.is_some();
            changed = true;
            if self.history {
                if let Some(old) = self.shown { s.widget_placement(old, None, None, crate::ui::now_ms()); }
                if let Some(e) = &self.embeds {
                    let _ = e.borrow_mut().hide(PAGE_SLOT);
                }
                eprintln!("navigator-ui: history view open; the page is withdrawn");
            } else {
                self.started = false;
            }
        }
        // The page the current trail shows.
        let want = if self.history { self.shown } else { s.shown_key() };
        if want != self.shown || !self.started {
            changed = true;
            self.started = true;
            if let Some(old) = self.shown.filter(|k| Some(*k) != want) {
                s.widget_placement(old, None, None, crate::ui::now_ms());
            }
            self.shown = want;
            match want {
                None => {
                    if let Some(e) = &self.embeds {
                        let _ = e.borrow_mut().hide(PAGE_SLOT);
                    }
                    s.live = None;
                    send(&mut self.w, &Request::Blank);
                }
                Some(key) => match self.children.iter().position(|c| c.key == key) {
                    Some(i) => {
                        // Live already: a swap.
                        let t = Instant::now();
                        let c = &mut self.children[i];
                        c.parking = None;
                        let (embed, url, loading) = (c.embed, c.url.clone(), c.asked.is_some());
                        if let Some(e) = &self.embeds {
                            if let Err(err) = e.borrow_mut().show(PAGE_SLOT, embed) {
                                eprintln!("navigator-ui: limend: {err:?}");
                            }
                        }
                        match c.document {
                            Some(document) => send(&mut self.w, &Request::ShowDocument { url: url.clone(), document }),
                            None => send(&mut self.w, &Request::Show { url: url.clone() }),
                        }
                        eprintln!("navigator-ui: showed live page {url} in {} ms", ms(t));
                        s.live = Some(if loading { LivePage::Loading(url) } else { LivePage::Shown(url) });
                    }
                    None => self.start(s, key),
                },
            }
        }
        if let Some(key) = self.shown {
            if let Some(c) = self.children.iter_mut().find(|c| c.key == key) {
                c.seen = Instant::now();
            }
        }
        if let Some(active) = self.widget {
            let ActiveWidget { document, key, rect: old, .. } = active;
            let rect = self.shown.and_then(|p| self.widget_bounds(s, p, key, None))
                .filter(|(d, _)| *d == document).map(|(_, r)| r);
            if rect != Some(old) {
                send(&mut self.w, &Request::PlaceWidget { document, rect });
                if rect.is_none() { active.set_origin(s, None); changed = true; }
                self.widget = rect.map(|rect| ActiveWidget { rect, ..active });
            }
        }
        // Reading choices changed (Aa, or the system appearance): the live
        // children of that site lay out again.
        let mut tell = vec![];
        for c in self.children.iter_mut().filter(|c| c.parking.is_none()) {
            let want = s.store.reading_of(c.key.0, &c.url).for_child(self.dark);
            if want != c.reading {
                c.reading = want;
                if c.attached && c.pending.is_none() {
                    tell.push((c.embed, want));
                }
            }
        }
        for (embed, reading) in tell {
            eprintln!("navigator-ui: reading changed: text {} zoom {} {} {}", reading.text_64, reading.zoom_64, if reading.dark { "dark" } else { "light" }, if reading.comfortable { "comfortable" } else { "full" });
            self.to_child(embed, &ToChild::SetReading { reading });
        }
        self.park(s);
        changed
    }
}

#[cfg(test)]
mod widget_lifecycle_tests {
    use super::*;

    #[test]
    fn late_failure_cannot_retire_a_replacement_in_the_same_slot() {
        let mut active = Some(ActiveWidget { page: (1, 1), document: [1; 16], press: [3; 16], key: 9, rect: [0, 0, 20, 20] });
        assert!(retire_widget(&mut active, [1; 16], [2; 16]).is_none());
        assert!(retire_widget(&mut active, [0; 16], [3; 16]).is_none());
        assert_eq!(retire_widget(&mut active, [1; 16], [3; 16]).unwrap().key, 9);
        assert!(retire_widget(&mut active, [1; 16], [3; 16]).is_none());
    }
}
