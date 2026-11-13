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
use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use limen::doc_viewer::{ToChild, ToParent};
use navigator_control::{self as ctl, Event, LinkAction, Request, Source, Ticket};
use pergola::embed::{EmbedId, Embeds, ParentEvent, WindowToken};

use crate::shell::{LivePage, Out, Shell, PAGE_SLOT};
use crate::trail::PageId;

/// Live pages kept by default (`NAVIGATOR_UI_LIVE` overrides). Each is a
/// jail with its own layout; the broker's own bound is 16 sessions
/// (backend §4.4a), and the UI stays well inside it.
pub const BUDGET: usize = 6;
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
}

pub struct Live {
    w: UnixStream,
    rx: mpsc::Receiver<Event>,
    embeds: Option<Rc<RefCell<Embeds>>>,
    window: Option<WindowToken>,
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
}

fn send(w: &mut UnixStream, r: &Request) {
    if let Err(e) = ctl::write_frame(w, r) {
        eprintln!("navigator-ui: navigatord: {e}");
    }
}

fn ms(t: Instant) -> u128 { t.elapsed().as_millis() }

impl Live {
    /// Connect to navigatord at `path` and say hello. With `chrome`,
    /// navigatord opens the window's frescod connection over the chrome
    /// channel and hands it back (so it can name the page's origin in
    /// frescod's strip); it comes back here when it did.
    pub fn connect(path: &str, chrome: bool) -> io::Result<(Live, Option<OwnedFd>)> {
        let mut s = UnixStream::connect(path)?;
        ctl::write_frame(&mut s, &Request::Hello { chrome })?;
        let fd = match ctl::read_frame::<Event>(&mut s)? {
            Some(Event::Chrome { fd: true }) => Some(ctl::recv_fd(&s)?),
            Some(Event::Chrome { fd: false }) => None,
            other => return Err(io::Error::other(format!("navigatord said {other:?}"))),
        };
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
        let budget = std::env::var("NAVIGATOR_UI_LIVE").ok().and_then(|v| v.parse().ok()).filter(|&n| n >= 1).unwrap_or(BUDGET);
        Ok((
            Live {
                w: s,
                rx,
                embeds: None,
                window: None,
                children: vec![],
                shown: None,
                started: false,
                next: 1,
                activations: HashMap::new(),
                dark: false,
                budget,
            },
            fd,
        ))
    }

    /// The window navigatord names: its id on the connection it opened.
    pub fn window(&mut self, id: u32) {
        send(&mut self.w, &Request::Window { window: id });
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

    fn close(&mut self, i: usize) {
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
            let (embed, scroll, dark) = (c.embed, c.scroll, self.dark);
            self.to_child(embed, &ToChild::Open { ticket, dark, scroll });
        } else {
            c.pending = Some(ticket);
        }
    }

    /// A new child for the page `key`, in the slot, and navigatord asked
    /// to open it (the version it saved, if the page was parked).
    fn start(&mut self, s: &mut Shell, key: Key) {
        let (Some(e), Some(w)) = (self.embeds.clone(), self.window) else { return };
        let Some((url, saved, scroll)) = s.store.page_by_key_mut(key.0, key.1).map(|p| (p.url.clone(), p.saved.clone(), p.scroll)) else { return };
        let unrecorded = s.store.trail().unrecorded;
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
        });
        s.live = Some(LivePage::Loading(url.clone()));
        send(&mut self.w, &Request::Open { id, url, saved, unrecorded });
    }

    /// Park the least recently seen pages past the budget.
    fn park(&mut self, s: &Shell) {
        let parent = self.shown.and_then(|(t, p)| (s.store.trail().id == t).then(|| s.store.trail().pages.get(p).and_then(|q| q.parent)).flatten().map(|q| (t, q)));
        while self.live() > self.budget {
            let pick = self
                .children
                .iter()
                .enumerate()
                .filter(|(_, c)| c.parking.is_none() && Some(c.key) != self.shown && Some(c.key) != parent)
                .min_by_key(|(_, c)| c.seen)
                .map(|(i, _)| i);
            let Some(i) = pick else { break };
            let live = self.live();
            let c = &mut self.children[i];
            c.parking = Some(Instant::now());
            eprintln!("navigator-ui: parking {} ({live} live, budget {})", c.url, self.budget);
            if self.children[i].attached {
                let embed = self.children[i].embed;
                self.to_child(embed, &ToChild::Park);
            } else {
                self.close(i);
            }
        }
    }

    /// Bring the shell and navigatord up to date with each other and with
    /// the page children. Returns whether the shell changed.
    pub fn tick(&mut self, s: &mut Shell) -> bool {
        let mut changed = false;
        // navigatord's answers.
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            match ev {
                Event::Opened { id, url, title, insecure, ticket, ms: took, saved, from } => {
                    let Some(i) = self.children.iter().position(|c| c.asked == Some(id)) else { continue };
                    eprintln!("navigator-ui: opened {url} in {took} ms ({from:?})");
                    let c = &mut self.children[i];
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
                    self.close(i);
                    // Keep Failed on screen rather than trying again.
                    self.shown = shown;
                }
                Event::Link { id, action } => {
                    let beside = self.activations.remove(&id).unwrap_or(false);
                    match action {
                        LinkAction::Navigate { url, .. } => s.follow_link(&url, beside),
                        LinkAction::InDocument { fragment } => s.last_note = Some(format!("in-page link #{fragment} (scrolling to it comes later)")),
                        LinkAction::Handoff { url, scheme } => s.last_note = Some(format!("{scheme} link {url}: hand-off comes in step 6")),
                        LinkAction::Blocked { why } => s.last_note = Some(format!("link refused: {why}")),
                    }
                }
                Event::Chrome { .. } => {}
            }
        }
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
            let embed = match &ev {
                ParentEvent::Attached { embed } | ParentEvent::Message { embed, .. } | ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed } => *embed,
                _ => continue,
            };
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
                    Some(ToParent::Loaded { title, .. }) => {
                        let c = &mut self.children[i];
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
                                self.close(i);
                            }
                            // Shown again while it was being parked: keep it.
                            None => {}
                        }
                        changed = true;
                    }
                    Some(ToParent::Error { code }) => {
                        eprintln!("navigator-ui: a page child failed ({code})");
                        if on_screen {
                            let url = self.children[i].url.clone();
                            s.live = Some(LivePage::Failed { url, why: format!("the page could not be shown (code {code})") });
                            changed = true;
                        }
                    }
                    None => eprintln!("navigator-ui: dropped a malformed message from a page child"),
                },
                ParentEvent::ChildLost { .. } | ParentEvent::Detached { .. } => {
                    // A lost page on screen gets a new child next (from
                    // the store: it was saved when it opened).
                    eprintln!("navigator-ui: a page child went away ({})", self.children[i].url);
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
            self.close(i);
        }
        // Links the shell wants decided.
        for o in std::mem::take(&mut s.outbox) {
            match o {
                Out::Activate { page, href, beside } => {
                    let id = self.id();
                    self.activations.insert(id, beside);
                    send(&mut self.w, &Request::Activate { id, page, href });
                }
            }
        }
        // The page the current trail shows.
        let want = s.shown_key();
        if want != self.shown || !self.started {
            changed = true;
            self.started = true;
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
                        send(&mut self.w, &Request::Show { url: url.clone() });
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
        self.park(s);
        changed
    }
}
