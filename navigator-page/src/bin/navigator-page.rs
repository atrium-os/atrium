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

use limen::doc_viewer::{error, ChromeKey, DocKind, Reading, ToChild, ToParent, FOUND_MAX};
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
/// navigatord's page socket, as the `navigator-page` capability mounts it.
const PAGE_SOCKET: &str = "/atrium/sockets/navigator-page/navigator-page.sock";

/// The comfortable measure (navigator-ui.md §7): about 70 characters of
/// body text at 100%, in CSS px; it grows with the text size.
const COMFORT_PX: f32 = 680.0;

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
    reading: Reading,
    doc: Rc<RefCell<Doc>>,
    /// The window size it was laid out for.
    size: (f32, f32),
    /// The measure: its left edge and width in the window (the whole
    /// width unless the reader asked for a comfortable one).
    x0: f32,
    lw: f32,
}

#[derive(Clone)]
struct PageView {
    page: Mutable<Option<Rc<Page>>>,
    scroll: Mutable<ScrollState>,
    more: Mutable<u64>,
    size: Mutable<(f32, f32)>,
    /// The current find match, in document px (`[x, y, w, h]`).
    mark: Mutable<Option<[f32; 4]>>,
}

impl PageView {
    fn view(&self) -> Option<DocumentView> {
        let p = self.page.get_cloned()?;
        let (_, h) = self.size.get();
        Some(DocumentView::new(self.scroll.clone(), Rect::new(p.x0, 0.0, p.lw, h), p.doc.clone(), self.more.clone()))
    }

    /// Where a document rect is in the window now.
    fn on_screen(&self, r: [f32; 4]) -> Option<[f32; 4]> {
        let p = self.page.get_cloned()?;
        let st = self.scroll.get();
        let z = st.zoom.settled.max(0.01) as f32;
        Some([p.x0 + r[0] * z, (r[1] - st.physics.y as f32) * z, r[2] * z, r[3] * z])
    }
}

impl View for PageView {
    fn render(&self, ctx: &mut Ctx) {
        match self.view() {
            Some(v) => {
                // Either side of a comfortable measure: the page's canvas.
                if let Some(p) = self.page.get_cloned().filter(|p| p.lw < p.size.0) {
                    let (w, h) = self.size.get();
                    let c = if p.reading.dark { pergola::Color::rgba(0.11, 0.11, 0.12, 1.0) } else { pergola::Color::rgba(1.0, 1.0, 1.0, 1.0) };
                    ctx.add(pergola::node::Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: c, radius: 0.0 });
                }
                v.render(ctx);
                if let Some([x, y, w, h]) = self.mark.get().and_then(|m| self.on_screen(m)) {
                    ctx.add(pergola::node::Node::Rect { rect: Rect::new(x - 2.0, y - 1.0, w + 4.0, h + 2.0), fill: pergola::Color::rgba(1.0, 0.8, 0.1, 0.45), radius: 3.0 });
                }
            }
            None => {
                // Nothing loaded yet: the page's canvas, white as on the web.
                let (w, h) = self.size.get();
                ctx.add(pergola::node::Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: pergola::Color::rgba(1.0, 1.0, 1.0, 1.0), radius: 0.0 });
            }
        }
    }
}

/// The measure in a window `w` wide: all of it, or a comfortable line
/// centred in it.
fn measure(w: f32, r: &Reading) -> (f32, f32) {
    if !r.comfortable {
        return (0.0, w);
    }
    let lw = (COMFORT_PX * r.text_64 as f32 / 64.0 * r.zoom_64 as f32 / 64.0).min(w).max(1.0);
    (((w - lw) / 2.0).floor(), lw)
}

fn lay_out(prepared: Rc<Prepared>, base: String, reading: Reading, size: (f32, f32)) -> Result<Page, String> {
    let (x0, lw) = measure(size.0, &reading);
    let doc = Doc::html_scaled(prepared.html.clone(), prepared.subs.clone(), lw, size.1, reading.dark, reading.text_64 as u32, |_| None)?;
    Ok(Page { prepared, base, reading, doc: Rc::new(RefCell::new(doc)), size, x0, lw })
}

/// A find in progress: the query, the reading it was laid out for, the
/// matches (document px) and the current one.
struct Finding {
    query: String,
    reading: Reading,
    lw: f32,
    matches: Vec<[f32; 4]>,
    current: usize,
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
    finding: Option<Finding>,
}

impl Run {
    fn load(&mut self, out: &mut dyn Out, kind: DocKind, base: String, reading: Reading, scroll: f32, bytes: Result<Vec<u8>, u16>) {
        let prepared = match bytes.and_then(|b| prepare(kind, &b)) {
            Ok(p) => Rc::new(p),
            Err(code) => return out.send(ToParent::Error { code }),
        };
        let t = Instant::now();
        self.finding = None;
        self.view.mark.set(None);
        match lay_out(prepared.clone(), base, reading, self.view.size.get()) {
            Ok(p) => {
                // A parked page comes back where it was; the scroll view
                // clamps it to the page as laid out.
                let mut st = ScrollState::default();
                st.physics.y = scroll as f64;
                let z = reading.zoom_64 as f64 / 64.0;
                (st.zoom.live, st.zoom.settled) = (z, z);
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

    /// Lay the page out again for a new size or reading, keeping its
    /// place (the same document px from the top).
    fn relayout(&mut self, reading: Option<Reading>) {
        let Some(p) = self.view.page.get_cloned() else { return };
        let size = self.view.size.get();
        let reading = reading.unwrap_or(p.reading);
        if p.size == size && p.reading == reading {
            return;
        }
        if p.reading.zoom_64 != reading.zoom_64 {
            let mut st = self.view.scroll.get();
            let z = reading.zoom_64 as f64 / 64.0;
            (st.zoom.live, st.zoom.settled) = (z, z);
            self.view.scroll.set(st);
        }
        match lay_out(p.prepared.clone(), p.base.clone(), reading, size) {
            Ok(n) => {
                eprintln!("navigator-page: laid out again: text {}%, zoom {}%, {}{}", n.reading.text_64 as u32 * 100 / 64, n.reading.zoom_64 as u32 * 100 / 64,
                    if n.reading.dark { "dark" } else { "light" }, if n.reading.comfortable { ", comfortable width" } else { "" });
                self.view.page.set(Some(Rc::new(n)));
            }
            Err(e) => eprintln!("navigator-page: layout: {e}"),
        }
        self.view.mark.set(None);
    }

    /// A zoom the person made here (keys, a pinch) that has settled: the
    /// page is laid out for it, and the parent told so it can remember it.
    fn zoom_settled(&mut self, out: &mut dyn Out) {
        let Some(p) = self.view.page.get_cloned() else { return };
        let st = self.view.scroll.get();
        if (st.zoom.live - st.zoom.settled).abs() > 1e-6 {
            return;
        }
        let z = ((st.zoom.settled * 64.0).round() as u16).clamp(Reading::ZOOM_MIN, Reading::ZOOM_MAX);
        if z != p.reading.zoom_64 {
            self.relayout(Some(Reading { zoom_64: z, ..p.reading }));
            out.send(ToParent::Zoomed { zoom_64: z });
        }
    }

    /// Find (navigator-ui.md §7): count the matches, go to one, mark it
    /// and say where it is. The parent learns counts and a rect only.
    fn find(&mut self, out: &mut dyn Out, query: &str, step: i8) {
        let Some(p) = self.view.page.get_cloned() else { return };
        if query.is_empty() {
            self.finding = None;
            self.view.mark.set(None);
            return out.send(ToParent::Found { count: 0, current: 0, rect: None });
        }
        let st = self.view.scroll.get();
        let zoom = st.zoom.settled.max(0.01);
        let stale = self.finding.as_ref().is_none_or(|f| f.query != query || f.reading != p.reading || f.lw != p.lw);
        if stale {
            let t = Instant::now();
            let Ok(fonts) = navigator_render::fontset::FontSet::shared() else { return };
            let env = navigator_style::cascade::Env {
                dark: p.reading.dark,
                text_scale_64: p.reading.text_64 as u32,
                ..navigator_style::cascade::Env::zoomed(p.lw as f64, p.size.1 as f64, (zoom * 64.0).round() as u32)
            };
            let matches = navigator_render::find::find(&p.prepared.html, fonts, &env, &p.prepared.subs, query, FOUND_MAX as usize);
            eprintln!("navigator-page: find: {} matches in {:.0} ms", matches.len(), t.elapsed().as_secs_f64() * 1e3);
            self.finding = Some(Finding { query: query.to_string(), reading: p.reading, lw: p.lw, matches, current: usize::MAX });
        }
        let f = self.finding.as_mut().unwrap();
        let n = f.matches.len();
        if n == 0 {
            self.view.mark.set(None);
            return out.send(ToParent::Found { count: 0, current: 0, rect: None });
        }
        let top = st.physics.y as f32;
        f.current = match (step, f.current) {
            (_, usize::MAX) | (0, _) => f.matches.iter().position(|m| m[1] >= top).unwrap_or(0),
            (1, c) => (c + 1) % n,
            (_, c) => (c + n - 1) % n,
        };
        let m = f.matches[f.current];
        // Bring it into view, a third of the way down, unless it shows.
        let vh = p.size.1 as f64 / zoom;
        let mut st = st;
        if (m[1] as f64) < st.physics.y || (m[1] + m[3]) as f64 > st.physics.y + vh {
            st.physics.y = (m[1] as f64 - vh / 3.0).max(0.0);
            self.view.scroll.set(st);
        }
        self.view.mark.set(Some(m));
        let rect = self.view.on_screen(m);
        out.send(ToParent::Found { count: n as u32, current: f.current as u32 + 1, rect });
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
    /// A window event; returns whether the focus should go back to the
    /// parent (a chrome shortcut that opens a field there).
    fn window_event(&mut self, out: &mut dyn Out, ev: &fresco_client::Event) -> bool {
        if let fresco_client::Event::Resized { width, height, .. } = ev {
            self.view.size.set((*width as f32, *height as f32));
            self.relayout(None);
            return false;
        }
        match pergola::input::translate(ev) {
            Some(Event::PointerMove { at }) => {
                let h = self.link(at);
                if h != self.hover {
                    self.hover = h.clone();
                    out.send(ToParent::LinkHover { url: h });
                }
            }
            Some(Event::Key { kind: pergola::KeyEventKind::Down, key, modifiers: m, chars }) => {
                if let Some(k) = chrome_key(key, m, &chars) {
                    out.send(k);
                    // Find, the Where field, history, a new trail: what
                    // comes next is typed in the chrome.
                    return m.ctrl && !m.alt && matches!(chars.to_ascii_lowercase().as_str(), "f" | "l" | "h" | "t" | "n");
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
        false
    }
}

/// A press the page does not use that the chrome's keyboard map may
/// (navigator-ui.md §9): it goes to the parent. Ctrl-plus, Ctrl-minus and
/// Ctrl-0 stay here, for the ScrollView's zoom.
fn chrome_key(key: pergola::Key, m: pergola::Modifiers, chars: &str) -> Option<ToParent> {
    use pergola::Key;
    let k = match key {
        Key::Escape => ChromeKey::Escape,
        Key::Tab if m.ctrl => ChromeKey::Tab,
        Key::Left if m.alt => ChromeKey::Left,
        Key::Right if m.alt => ChromeKey::Right,
        Key::Char if (m.ctrl || m.alt) && !(m.ctrl && matches!(chars, "+" | "=" | "-" | "0")) => ChromeKey::Char,
        _ => return None,
    };
    let chars = if k == ChromeKey::Char { chars.chars().take(1).collect() } else { String::new() };
    let m = ToParent::Key { key: k, chars, ctrl: m.ctrl, shift: m.shift, alt: m.alt };
    ToParent::decode(&m.encode())
}

fn new_app(size: (f32, f32)) -> (App<PageView>, PageView) {
    let dirty = Arc::new(AtomicBool::new(true));
    let view = PageView {
        page: Mutable::with_dirty(None, dirty.clone()),
        scroll: Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        more: Mutable::with_dirty(0, dirty.clone()),
        size: Mutable::with_dirty(size, dirty.clone()),
        mark: Mutable::with_dirty(None, dirty.clone()),
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
    let mut run = Run { view, hover: None, finding: None };
    let bytes = std::fs::File::open(path).map_err(|_| error::NO_DOCUMENT).and_then(read_bounded);
    let base = std::env::var("NAVIGATOR_PAGE_BASE").unwrap_or_else(|_| "https://a.test/".into());
    run.load(&mut LogOut, kind, base, Reading::plain(dark), 0.0, bytes);
    loop {
        match window.step(&mut app, Some(TICK)) {
            Ok(StepResult::Event(ev)) => {
                run.window_event(&mut LogOut, &ev);
            }
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
    let mut run = Run { view, hover: None, finding: None };
    // Resizes that come before the token are the slot's size.
    let until = Instant::now() + Duration::from_secs(10);
    let token = loop {
        if let Some(t) = window.embed_token() {
            break t;
        }
        match window.step(&mut app, Some(Duration::from_millis(100))) {
            Ok(StepResult::Event(ev)) => {
                run.window_event(&mut LogOut, &ev);
            }
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
                    Some(ToChild::SetTheme { dark }) => {
                        let r = run.view.page.get_cloned().map_or(Reading::plain(dark), |p| Reading { dark, ..p.reading });
                        run.relayout(Some(r))
                    }
                    Some(ToChild::SetReading { reading }) => run.relayout(Some(reading)),
                    Some(ToChild::Find { query, step }) => run.find(&mut out, &query, step),
                    Some(ToChild::ScrollTo { .. }) => {} // anchors scroll in a later step
                    Some(ToChild::Load { .. }) => out.send(ToParent::Error { code: error::NO_DOCUMENT }),
                    Some(ToChild::Park) => {
                        let y = run.view.scroll.get_cloned().physics.y as f32;
                        let y = if y.is_finite() { y.clamp(0.0, limen::doc_viewer::SCROLL_MAX) } else { 0.0 };
                        eprintln!("navigator-page: parked at y={y:.0}");
                        out.send(ToParent::Parked { scroll: y });
                    }
                    Some(ToChild::Open { ticket, reading, scroll }) => {
                        let path = std::env::var("NAVIGATOR_PAGE_SOCKET").unwrap_or_else(|_| PAGE_SOCKET.into());
                        match navigator_control::redeem(&path, &ticket) {
                            Ok((base, bytes)) => run.load(&mut out, DocKind::Recording, base, reading, scroll, Ok(bytes)),
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
                            run.load(&mut out, kind, base, Reading::plain(dark), 0.0, b);
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
            Ok(StepResult::Event(ev)) => {
                if run.window_event(&mut out, &ev) {
                    let id = window.id();
                    if let Err(e) = window.connection().window_focus_parent(id) {
                        eprintln!("navigator-page: frescod: {e}");
                    }
                }
            }
            Ok(StepResult::Closed) => return,
            Ok(_) => {}
            Err(e) => die(format_args!("frescod: {e}")),
        }
        run.zoom_settled(&mut out);
    }
}
