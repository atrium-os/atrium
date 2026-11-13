//! One live page (navigator-ui.md §14 step 2): navigatord opens pages and
//! decides links; the page itself is a doc-viewer child (Limen L6) bound
//! into the window's page slot.
//!
//! The UI never holds a page's bytes (§1). navigatord answers an `Open`
//! with a one-shot ticket; the UI passes it to the child, which redeems it
//! on navigatord's page socket. A press on a link in the child comes back
//! as the href the page spells, and goes to navigatord, which checks the
//! page carries it and says what it does; only then does the trail move.
//!
//! Back and Forward move along the trail as before: whenever the page the
//! current trail shows changes, the UI asks navigatord to open it (a page
//! it converted recently comes from its cache).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use limen::doc_viewer::{ToChild, ToParent};
use navigator_control::{self as ctl, Event, LinkAction, Request, Ticket};
use pergola::embed::{EmbedId, Embeds, ParentEvent, WindowToken};

use crate::shell::{LivePage, Out, Shell, PAGE_SLOT};

pub struct Live {
    w: UnixStream,
    rx: mpsc::Receiver<Event>,
    embeds: Option<Rc<RefCell<Embeds>>>,
    embed: Option<EmbedId>,
    window: Option<WindowToken>,
    attached: bool,
    next: u32,
    /// The latest `Open`: its id and address.
    asked: Option<(u32, String)>,
    /// The address last asked for (`None`: nothing, Start).
    requested: Option<Option<String>>,
    /// A ticket waiting for the child to attach.
    pending: Option<Ticket>,
    /// `Activate` ids, and whether the press asked to open beside.
    activations: HashMap<u32, bool>,
    pub dark: bool,
}

fn send(w: &mut UnixStream, r: &Request) {
    if let Err(e) = ctl::write_frame(w, r) {
        eprintln!("navigator-ui: navigatord: {e}");
    }
}

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
        Ok((
            Live { w: s, rx, embeds: None, embed: None, window: None, attached: false, next: 1, asked: None, requested: None, pending: None, activations: HashMap::new(), dark: false },
            fd,
        ))
    }

    /// The window navigatord names: its id on the connection it opened.
    pub fn window(&mut self, id: u32) {
        send(&mut self.w, &Request::Window { window: id });
    }

    /// Ask limend for the page child in the window's page slot.
    pub fn embed(&mut self, embeds: Rc<RefCell<Embeds>>, window: WindowToken) {
        self.window = Some(window);
        self.embeds = Some(embeds);
        self.request_child();
    }

    fn request_child(&mut self) {
        let (Some(e), Some(w)) = (&self.embeds, self.window) else { return };
        match e.borrow_mut().request("doc-viewer", w, PAGE_SLOT) {
            Ok(id) => {
                eprintln!("navigator-ui: asked limend for the page child (embed {id})");
                self.embed = Some(id);
                self.attached = false;
            }
            Err(err) => eprintln!("navigator-ui: limend: {err:?}"),
        }
    }

    fn to_child(&mut self, m: &ToChild) {
        let (Some(e), Some(id)) = (&self.embeds, self.embed) else { return };
        if let Err(err) = e.borrow_mut().send(id, m.encode()) {
            eprintln!("navigator-ui: limend: {err:?}");
        }
    }

    fn give(&mut self, ticket: Ticket) {
        if self.attached {
            let dark = self.dark;
            self.to_child(&ToChild::Open { ticket, dark });
        } else {
            self.pending = Some(ticket);
        }
    }

    /// Bring the shell and navigatord up to date with each other and with
    /// the page child. Returns whether the shell changed.
    pub fn tick(&mut self, s: &mut Shell) -> bool {
        let mut changed = false;
        // navigatord's answers.
        while let Ok(ev) = self.rx.try_recv() {
            changed = true;
            match ev {
                Event::Opened { id, url, title, insecure, ticket, ms } => {
                    let Some((aid, asked)) = self.asked.clone().filter(|(a, _)| *a == id) else { continue };
                    let _ = aid;
                    eprintln!("navigator-ui: opened {url} in {ms} ms");
                    s.opened(&asked, &url, &title, insecure);
                    self.requested = Some(Some(url));
                    self.give(ticket);
                }
                Event::Failed { id, url, why } => {
                    if self.asked.as_ref().is_some_and(|(a, _)| *a == id) {
                        eprintln!("navigator-ui: {url}: {why}");
                        s.live = Some(LivePage::Failed { url, why });
                    }
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
        // The page child.
        loop {
            let ev = match &self.embeds {
                Some(e) => e.borrow_mut().poll(Duration::from_millis(1)),
                None => break,
            };
            match ev {
                Ok(Some(ParentEvent::Attached { embed })) if Some(embed) == self.embed => {
                    eprintln!("navigator-ui: page child attached");
                    self.attached = true;
                    if let Some(t) = self.pending.take() {
                        self.give(t);
                    }
                }
                Ok(Some(ParentEvent::Message { embed, bytes })) if Some(embed) == self.embed => match ToParent::decode(&bytes) {
                    Some(ToParent::LinkActivated { url }) => {
                        s.link_pressed(&url);
                        changed = true;
                    }
                    Some(ToParent::LinkHover { url }) => {
                        s.hover_hint = url;
                        changed = true;
                    }
                    Some(ToParent::Loaded { title, .. }) => eprintln!("navigator-ui: the page child laid out \"{title}\""),
                    Some(ToParent::Error { code }) => {
                        eprintln!("navigator-ui: the page child failed ({code})");
                        if let Some(u) = s.shown_url() {
                            s.live = Some(LivePage::Failed { url: u, why: format!("the page could not be shown (code {code})") });
                            changed = true;
                        }
                    }
                    None => eprintln!("navigator-ui: dropped a malformed message from the page child"),
                },
                Ok(Some(ParentEvent::ChildLost { embed })) if Some(embed) == self.embed => {
                    // A new child for the same page: ask for it and open again.
                    eprintln!("navigator-ui: the page child was lost; starting another");
                    self.embed = None;
                    self.request_child();
                    self.requested = None;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => {
                    eprintln!("navigator-ui: limend: {e:?}");
                    break;
                }
            }
        }
        // Links the shell wants decided.
        for o in std::mem::take(&mut s.outbox) {
            match o {
                Out::Activate { page, href, beside } => {
                    let id = self.next;
                    self.next += 1;
                    self.activations.insert(id, beside);
                    send(&mut self.w, &Request::Activate { id, page, href });
                }
            }
        }
        // The page the current trail shows.
        let want = s.shown_url();
        if self.requested.as_ref() != Some(&want) {
            changed = true;
            self.requested = Some(want.clone());
            match want {
                Some(url) => {
                    let id = self.next;
                    self.next += 1;
                    self.asked = Some((id, url.clone()));
                    s.live = Some(LivePage::Loading(url.clone()));
                    send(&mut self.w, &Request::Open { id, url });
                }
                None => {
                    self.asked = None;
                    send(&mut self.w, &Request::Blank);
                }
            }
        }
        changed
    }
}
