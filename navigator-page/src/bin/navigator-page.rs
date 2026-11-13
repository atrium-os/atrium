//! navigator-page: the doc-viewer child (limen.md §2.1, L6).
//!
//! Started by limend through portcullisd, jailed: its stdin is the Limen
//! channel and the first descriptor on it an embed-only frescod
//! connection. It opens one window on that connection (shown only in the
//! parent's slot), waits for `Load` with the document's descriptor, lays
//! the document out with DocumentView and reports `Loaded`; a click on a
//! link sends `LinkActivated` with the link resolved against the page, a
//! pointer over one `LinkHover`. It never follows a link itself.
//!
//!   navigator-page                       # as a Limen child
//!                                        # (NAVIGATOR_PAGE_SOCKET: where
//!                                        # an `Open` ticket is redeemed)
//!   navigator-page --file rec.json [--html] [--dark]
//!                                        # development: its own window on
//!                                        # FRESCO_SOCKET, links to stderr

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::doc_viewer::{error, DocKind, ToChild, ToParent};
use limen::proto::{ChildEvent, EmbedId};
use navigator_page::{prepare, read_bounded, resolve, Prepared};
use pergola::geom::Rect;
use pergola::reactive::Mutable;
use pergola::view::{Ctx, View};
use pergola::widgets::scroll_view::ScrollState;
use pergola::window::StepResult;
use pergola::{App, Event, SizeSpec, Window, WindowDesc};
use pergola_document::{Doc, DocumentView};

/// navigatord's page socket, where a ticket is redeemed.
const PAGE_SOCKET: &str = "/var/run/atrium/navigator-page.sock";

/// How long a step waits for frescod before the Limen channel is looked at.
const TICK: Duration = Duration::from_millis(30);

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("navigator-page: {msg}");
    std::process::exit(1)
}

/// The page as laid out, and what it was laid out from.
struct Page {
    prepared: Rc<Prepared>,
    base: String,
    dark: bool,
    doc: Rc<RefCell<Doc>>,
    /// The size it was laid out at.
    size: (f32, f32),
}

#[derive(Clone)]
struct PageView {
    page: Mutable<Option<Rc<Page>>>,
    scroll: Mutable<ScrollState>,
    more: Mutable<u64>,
    size: Mutable<(f32, f32)>,
}

impl PageView {
    fn view(&self) -> Option<DocumentView> {
        let p = self.page.get_cloned()?;
        let (w, h) = self.size.get();
        Some(DocumentView::new(self.scroll.clone(), Rect::new(0.0, 0.0, w, h), p.doc.clone(), self.more.clone()))
    }
}

impl View for PageView {
    fn render(&self, ctx: &mut Ctx) {
        match self.view() {
            Some(v) => v.render(ctx),
            None => {
                // Nothing loaded yet: the page's canvas, white as on the web.
                let (w, h) = self.size.get();
                ctx.add(pergola::node::Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: pergola::Color::rgba(1.0, 1.0, 1.0, 1.0), radius: 0.0 });
            }
        }
    }
}

fn lay_out(prepared: Rc<Prepared>, base: String, dark: bool, size: (f32, f32)) -> Result<Page, String> {
    let doc = Doc::html(prepared.html.clone(), prepared.subs.clone(), size.0, size.1, dark, |_| None)?;
    Ok(Page { prepared, base, dark, doc: Rc::new(RefCell::new(doc)), size })
}

/// Where messages for the parent go.
trait Out {
    fn send(&mut self, m: ToParent);
}

struct LimenOut(limen::client::Child, EmbedId);

impl Out for LimenOut {
    fn send(&mut self, m: ToParent) {
        if let Err(e) = self.0.emit(self.1, m.encode()) {
            die(format_args!("limend: {e:?}"));
        }
    }
}

struct LogOut;

impl Out for LogOut {
    fn send(&mut self, m: ToParent) {
        eprintln!("navigator-page: -> {m:?}");
    }
}

struct Run {
    view: PageView,
    hover: Option<String>,
}

impl Run {
    fn load(&mut self, out: &mut dyn Out, kind: DocKind, base: String, dark: bool, scroll: f32, bytes: Result<Vec<u8>, u16>) {
        let prepared = match bytes.and_then(|b| prepare(kind, &b)) {
            Ok(p) => Rc::new(p),
            Err(code) => return out.send(ToParent::Error { code }),
        };
        let t = Instant::now();
        match lay_out(prepared.clone(), base, dark, self.view.size.get()) {
            Ok(p) => {
                // A parked page comes back where it was; the scroll view
                // clamps it to the page as laid out.
                let mut st = ScrollState::default();
                st.physics.y = scroll as f64;
                self.view.scroll.set(st);
                if scroll > 0.0 {
                    eprintln!("navigator-page: opened at y={scroll:.0}");
                }
                self.view.page.set(Some(Rc::new(p)));
                eprintln!("navigator-page: laid out in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
                out.send(ToParent::Loaded { title: prepared.title.clone(), anchors: prepared.anchors.clone() });
            }
            Err(e) => {
                eprintln!("navigator-page: layout: {e}");
                out.send(ToParent::Error { code: error::UNREADABLE });
            }
        }
    }

    /// Lay the page out again for a new size or appearance.
    fn relayout(&mut self, dark: Option<bool>) {
        let Some(p) = self.view.page.get_cloned() else { return };
        let size = self.view.size.get();
        let dark = dark.unwrap_or(p.dark);
        if p.size == size && p.dark == dark {
            return;
        }
        match lay_out(p.prepared.clone(), p.base.clone(), dark, size) {
            Ok(n) => self.view.page.set(Some(Rc::new(n))),
            Err(e) => eprintln!("navigator-page: layout: {e}"),
        }
    }

    /// The href under `at`, as the document spells it.
    fn href(&self, at: pergola::geom::Point) -> Option<String> {
        self.view.view()?.link_at(at)
    }

    /// The same, resolved against the page (for showing where it goes).
    fn link(&self, at: pergola::geom::Point) -> Option<String> {
        let href = self.href(at)?;
        let base = self.view.page.get_cloned()?.base.clone();
        resolve(&base, &href)
    }

    /// The window's own events: pointer over and on links, resizes.
    fn window_event(&mut self, out: &mut dyn Out, ev: &fresco_client::Event) {
        if let fresco_client::Event::Resized { width, height, .. } = ev {
            self.view.size.set((*width as f32, *height as f32));
            self.relayout(None);
            return;
        }
        match pergola::input::translate(ev) {
            Some(Event::PointerMove { at }) => {
                let h = self.link(at);
                if h != self.hover {
                    self.hover = h.clone();
                    out.send(ToParent::LinkHover { url: h });
                }
            }
            Some(Event::PointerDown { at }) => {
                // Unresolved: the parent checks the page carries it.
                if let Some(url) = self.href(at).filter(|h| !h.is_empty() && h.len() <= limen::doc_viewer::URL_MAX) {
                    out.send(ToParent::LinkActivated { url });
                }
            }
            _ => {}
        }
    }
}

fn new_app(size: (f32, f32)) -> (App<PageView>, PageView) {
    let dirty = Arc::new(AtomicBool::new(true));
    let view = PageView {
        page: Mutable::with_dirty(None, dirty.clone()),
        scroll: Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        more: Mutable::with_dirty(0, dirty.clone()),
        size: Mutable::with_dirty(size, dirty.clone()),
    };
    (App::new_with_flag(view.clone(), dirty), view)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--file") {
        let path = args.get(i + 1).unwrap_or_else(|| die("--file PATH"));
        let kind = if args.iter().any(|a| a == "--html") { DocKind::Html } else { DocKind::Recording };
        let dark = args.iter().any(|a| a == "--dark");
        return standalone(path, kind, dark);
    }
    embedded();
}

/// Development: the page in a window of its own.
fn standalone(path: &str, kind: DocKind, dark: bool) {
    let mut window = Window::open(&WindowDesc::new("Page", SizeSpec::Fixed(1000, 700))).unwrap_or_else(|e| die(e));
    let (mut app, view) = new_app((window.width, window.height));
    let mut run = Run { view, hover: None };
    let bytes = std::fs::File::open(path).map_err(|_| error::NO_DOCUMENT).and_then(read_bounded);
    let base = std::env::var("NAVIGATOR_PAGE_BASE").unwrap_or_else(|_| "https://a.test/".into());
    run.load(&mut LogOut, kind, base, dark, 0.0, bytes);
    loop {
        match window.step(&mut app, Some(TICK)) {
            Ok(StepResult::Event(ev)) => run.window_event(&mut LogOut, &ev),
            Ok(StepResult::Closed) => return,
            Ok(_) => {}
            Err(e) => die(format_args!("frescod: {e}")),
        }
    }
}

fn embedded() {
    let (mut child, fresco) = limen::client::Child::embedded().unwrap_or_else(|e| die(format_args!("no limend channel: {e:?}")));
    let embed = loop {
        match child.poll(Duration::from_secs(30)) {
            Ok(Some(ChildEvent::Attach { embed, role, .. })) if role == "doc-viewer" => break embed,
            Ok(Some(ChildEvent::Attach { role, .. })) => die(format_args!("asked to be {role}")),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => die("limend never attached us"),
        }
    };
    let conn = fresco_client::Connection::wrap(fresco).unwrap_or_else(|e| die(format_args!("frescod: {e}")));
    let mut desc = WindowDesc::new("page", SizeSpec::Fixed(800, 600));
    desc.hints = fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() };
    let mut window = Window::open_on(conn, &desc).unwrap_or_else(|e| die(format_args!("window: {e}")));
    window.follow_appearance(false);
    let (mut app, view) = new_app((window.width, window.height));
    let mut run = Run { view, hover: None };
    // Resizes that come before the token are the slot's size.
    let until = Instant::now() + Duration::from_secs(10);
    let token = loop {
        if let Some(t) = window.embed_token() {
            break t;
        }
        match window.step(&mut app, Some(Duration::from_millis(100))) {
            Ok(StepResult::Event(ev)) => run.window_event(&mut LogOut, &ev),
            Ok(StepResult::Closed) => return,
            Ok(_) => {}
            Err(e) => die(format_args!("frescod: {e}")),
        }
        if Instant::now() > until {
            die("no embed token from frescod");
        }
    };
    if child.ready(embed, token).is_err() {
        die("limend refused ready");
    }
    let mut out = LimenOut(child, embed);
    loop {
        // The parent's messages first: they're few and small.
        loop {
            match out.0.poll(Duration::from_millis(1)) {
                Ok(Some(ChildEvent::Message { bytes, .. })) => match ToChild::decode(&bytes) {
                    Some(ToChild::SetTheme { dark }) => run.relayout(Some(dark)),
                    Some(ToChild::ScrollTo { .. }) => {} // anchors scroll in a later step
                    Some(ToChild::Load { .. }) => out.send(ToParent::Error { code: error::NO_DOCUMENT }),
                    Some(ToChild::Park) => {
                        let y = run.view.scroll.get_cloned().physics.y as f32;
                        let y = if y.is_finite() { y.clamp(0.0, limen::doc_viewer::SCROLL_MAX) } else { 0.0 };
                        eprintln!("navigator-page: parked at y={y:.0}");
                        out.send(ToParent::Parked { scroll: y });
                    }
                    Some(ToChild::Open { ticket, dark, scroll }) => {
                        let path = std::env::var("NAVIGATOR_PAGE_SOCKET").unwrap_or_else(|_| PAGE_SOCKET.into());
                        match navigator_control::redeem(&path, &ticket) {
                            Ok((base, bytes)) => run.load(&mut out, DocKind::Recording, base, dark, scroll, Ok(bytes)),
                            Err(e) => {
                                eprintln!("navigator-page: redeem: {e}");
                                out.send(ToParent::Error { code: error::NO_DOCUMENT });
                            }
                        }
                    }
                    None => eprintln!("navigator-page: dropped a malformed message"),
                },
                Ok(Some(ChildEvent::MessageFd { bytes, .. })) => {
                    let fd = out.0.take_fd();
                    match ToChild::decode(&bytes) {
                        Some(ToChild::Load { kind, base, dark }) => {
                            let b = fd.map(std::fs::File::from).ok_or(error::NO_DOCUMENT).and_then(read_bounded);
                            run.load(&mut out, kind, base, dark, 0.0, b);
                        }
                        _ => eprintln!("navigator-page: dropped a descriptor"),
                    }
                }
                Ok(Some(ChildEvent::Detaching { .. })) => return,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => die(format_args!("limend: {e:?}")),
            }
        }
        match window.step(&mut app, Some(TICK)) {
            Ok(StepResult::Event(ev)) => run.window_event(&mut out, &ev),
            Ok(StepResult::Closed) => return,
            Ok(_) => {}
            Err(e) => die(format_args!("frescod: {e}")),
        }
    }
}
