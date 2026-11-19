//! atrium-pdf: the `pdf-viewer` child (folium.md §8), and later the app.
//!
//! As a Limen child it is started by limend through portcullisd, jailed:
//! its stdin is the Limen channel and the first descriptor on it an
//! embed-only frescod connection. It opens one window there, waits for
//! `Load` with the PDF's descriptor, and shows the pages in a column that
//! scrolls and zooms with Pergola's `ScrollView`. Pages are drawn as tiles
//! at the painted size by the engine's workers (`atrium_pdf::Engine`).
//!
//!   atrium-pdf                         # as a Limen child
//!   atrium-pdf --file doc.pdf [--dark] # development: its own window
//!   atrium-pdf --bench doc.pdf [WIDTH] # no window: time to open and
//!                                      # to draw the first screen

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_pdf::{Engine, Fit, Key, Layout, OpenError, TileCache, TILE, TILE_CACHE_BYTES};
use limen::pdf_viewer::{cut, error, ToChild, ToParent, TITLE_MAX};
use limen::proto::{ChildEvent, EmbedId};
use pergola::geom::{Axis, Rect};
use pergola::layout::FlexStyle;
use pergola::node::{Image, Node};
use pergola::reactive::Mutable;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, Draw, View};
use pergola::widgets::scroll_view::{ScrollState, ScrollView};
use pergola::window::StepResult;
use pergola::{App, Color, SizeSpec, TextStyle, Window, WindowDesc};

/// How long a step waits for frescod before the channel and the workers
/// are looked at.
const TICK: Duration = Duration::from_millis(16);

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("atrium-pdf: {msg}");
    std::process::exit(1)
}

/// What the window shows instead of pages.
#[derive(Clone, PartialEq)]
enum Note {
    None,
    Opening,
    Says(String),
}

/// The tiles the last render wanted, for the engine.
#[derive(Default)]
struct Wants {
    keys: Vec<Key>,
    changed: bool,
}

#[derive(Clone)]
struct PdfView {
    layout: Mutable<Option<Rc<Layout>>>,
    scroll: Mutable<ScrollState>,
    size: Mutable<(f32, f32)>,
    dark: Mutable<bool>,
    note: Mutable<Note>,
    /// Bumped when tiles arrive, so the window draws again.
    drawn: Mutable<u64>,
    /// Device pixels per window point.
    density: Rc<std::cell::Cell<f32>>,
    tiles: Rc<RefCell<TileCache<Image>>>,
    wants: Rc<RefCell<Wants>>,
    sizes: Rc<RefCell<Vec<(f64, f64)>>>,
    /// The print sheet is over the window (standalone only).
    printing: Mutable<bool>,
}

/// The print sheet's slot, over the whole window.
const PRINT_SLOT: pergola::slot::SlotKey = 0x5052_4e54;

/// Ctrl+P, pressed.
fn ctrl_p(ev: &fresco_client::Event) -> bool {
    matches!(ev, fresco_client::Event::Key { hid_usage: 0x13, pressed: true, modifiers, repeat: false, .. } if modifiers & 0b0001_0001 != 0 && modifiers & 0b1100_1100 == 0)
}

impl PdfView {
    fn canvas(&self) -> Color {
        if self.dark.get() { Color::rgba(0.16, 0.16, 0.17, 1.0) } else { Color::rgba(0.86, 0.86, 0.87, 1.0) }
    }
}

impl View for PdfView {
    fn render(&self, ctx: &mut Ctx) {
        self.render_pages(ctx);
        if self.printing.get() {
            let (w, h) = self.size.get();
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PRINT_SLOT);
        }
    }
}

impl PdfView {
    fn render_pages(&self, ctx: &mut Ctx) {
        let _ = self.drawn.get();
        let (w, h) = self.size.get();
        let canvas = self.canvas();
        let Some(layout) = self.layout.get_cloned() else {
            ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: canvas, radius: 0.0 });
            self.note_node(ctx, w, h);
            return;
        };
        let st = self.scroll.get();
        let zoom = st.zoom.settled.max(0.01) as f32;
        let ws = self.density.get();
        // What the window shows, in content points at zoom 1, and half a
        // screen more above and below to draw ahead.
        let (top, left) = (st.physics.y as f32, st.physics_x.y as f32);
        let (shown_w, shown_h) = (w / zoom, h / zoom);
        let (ay0, ay1) = (top - shown_h / 2.0, top + shown_h * 1.5);
        let dev = zoom * ws; // device px per content point
        let mut want = Vec::new();
        let mut items: Vec<(usize, [f32; 4], Vec<(Key, Image)>)> = Vec::new();
        {
            let mut tiles = self.tiles.borrow_mut();
            let sizes = self.sizes.borrow();
            for i in layout.visible(ay0, ay1) {
                let r = layout.rects[i];
                let on_screen = r[1] < top + shown_h && r[1] + r[3] > top;
                let scale = (layout.scale * dev) as f64;
                let area_y0 = ay0.max(r[1]) - r[1];
                let area_y1 = ay1.min(r[1] + r[3]) - r[1];
                let area_x0 = left.max(r[0]) - r[0];
                let area_x1 = (left + shown_w).min(r[0] + r[2]) - r[0];
                let area = [(area_x0 * dev) as f64, (area_y0 * dev) as f64, ((area_x1 - area_x0) * dev) as f64, ((area_y1 - area_y0) * dev) as f64];
                let keys = tiles_for(&sizes, i, scale, area);
                let mut shown = Vec::new();
                let mut missing = false;
                for k in &keys {
                    match tiles.get(k) {
                        Some(img) => shown.push((*k, img)),
                        None => missing = true,
                    }
                }
                if missing {
                    // Tiles at other scales stand in while these come.
                    let mut under = tiles.stand_ins(i as u32, keys.first().map_or(0, |k| k.scale_256));
                    under.extend(shown);
                    shown = under;
                }
                // Pages on screen first, then those just off it.
                let missing_keys = keys.into_iter().filter(|k| !tiles.contains(k));
                if on_screen {
                    want.splice(0..0, missing_keys);
                } else {
                    want.extend(missing_keys);
                }
                items.push((i, r, shown));
            }
        }
        {
            let mut w = self.wants.borrow_mut();
            if w.keys != want {
                w.keys = want;
                w.changed = true;
            }
        }
        let content = Draw(move |ctx: &mut Ctx| {
            ctx.key(1);
            let col = ctx.push(Node::stack(Axis::Vertical, Rect::new(0.0, 0.0, layout.width, layout.height), 0.0));
            ctx.set_flex(col, FlexStyle { free: true, ..FlexStyle::default() });
            for (i, r, shown) in &items {
                ctx.key(2 + (*i as u64) * 2);
                ctx.add(Node::Rect { rect: Rect::new(r[0], r[1], r[2], r[3]), fill: Color::rgba(1.0, 1.0, 1.0, 1.0), radius: 0.0 });
                for (k, img) in shown {
                    // A tile's place: device px of the page at its scale,
                    // back to content points.
                    let per = layout.scale / (k.scale_256 as f32 / 256.0);
                    let (iw, ih) = (img.data().width as f32, img.data().height as f32);
                    let x = r[0] + (k.col * TILE) as f32 * per;
                    let y = r[1] + (k.row * TILE) as f32 * per;
                    let (tw, th) = ((iw * per).min(r[0] + r[2] - x), (ih * per).min(r[1] + r[3] - y));
                    ctx.key(1 << 40 | (k.page as u64) << 20 ^ (k.scale_256 as u64) << 8 ^ (k.col as u64) << 4 ^ k.row as u64);
                    ctx.add(Node::Image { rect: Rect::new(x, y, tw, th), image: img.clone() });
                }
            }
            ctx.pop();
        });
        ScrollView::new(self.scroll.clone(), Rect::new(0.0, 0.0, w, h), content).backdrop(canvas).key(7).emit(ctx);
        self.note_node(ctx, w, h);
    }
}

impl PdfView {
    /// The note ("Opening…", why it can't be shown), always the same keyed
    /// node, empty once pages show, so it is changed rather than removed.
    fn note_node(&self, ctx: &mut Ctx, w: f32, h: f32) {
        let words = if self.layout.get_cloned().is_some() {
            String::new()
        } else {
            match self.note.get_cloned() {
                Note::Says(s) => s,
                Note::Opening => "Opening…".into(),
                Note::None => String::new(),
            }
        };
        let color = if self.dark.get() { Color::rgba(0.85, 0.85, 0.86, 1.0) } else { Color::rgba(0.25, 0.25, 0.27, 1.0) };
        ctx.key(9001);
        ctx.add(Node::Text {
            rect: Rect::new(24.0, h / 2.0 - 12.0, (w - 48.0).max(0.0), 24.0),
            content: words,
            style: TextStyle { family: font::SANS.into(), size: type_size::MD, weight: Weight::Regular, color },
        });
    }
}

/// `Engine::tiles_for` without the engine (the view holds only sizes).
fn tiles_for(sizes: &[(f64, f64)], page: usize, scale: f64, area: [f64; 4]) -> Vec<Key> {
    atrium_pdf::tiles_for(sizes, page, scale, area)
}

trait Out {
    fn send(&mut self, m: ToParent);
}

struct LogOut;
impl Out for LogOut {
    fn send(&mut self, m: ToParent) {
        eprintln!("atrium-pdf: to parent: {m:?}");
    }
}

struct LimenOut(limen::client::Child, EmbedId);
impl Out for LimenOut {
    fn send(&mut self, m: ToParent) {
        if let Err(e) = self.0.emit(self.1, m.encode()) {
            eprintln!("atrium-pdf: limend: {e:?}");
        }
    }
}

struct Run {
    view: PdfView,
    engine: Option<Engine>,
    fit: Fit,
    at_page: Option<usize>,
    opened: Option<Instant>,
    first_shown: bool,
}

impl Run {
    fn new(view: PdfView) -> Run {
        Run { view, engine: None, fit: Fit::Width, at_page: None, opened: None, first_shown: false }
    }

    fn load(&mut self, out: &mut dyn Out, file: Option<std::fs::File>) {
        self.engine = None;
        self.view.layout.set(None);
        self.view.note.set(Note::Opening);
        self.opened = Some(Instant::now());
        self.first_shown = false;
        let Some(f) = file else {
            self.view.note.set(Note::Says("No document came".into()));
            return out.send(ToParent::Error { code: error::NO_DOCUMENT });
        };
        match Engine::open(Box::new(f), b"") {
            Ok(e) => {
                let title = cut(&e.title(), TITLE_MAX);
                eprintln!("atrium-pdf: {} pages, opened in {:.0} ms", e.page_count(), self.opened.unwrap().elapsed().as_secs_f64() * 1e3);
                out.send(ToParent::Loaded { pages: e.page_count() as u32, title });
                *self.view.sizes.borrow_mut() = e.sizes().to_vec();
                *self.view.tiles.borrow_mut() = TileCache::new(TILE_CACHE_BYTES);
                self.engine = Some(e);
                self.view.note.set(Note::None);
                self.relayout();
            }
            Err(OpenError::NeedsPassword) => {
                // The password sheet is the app's (P3); until then, say so.
                self.view.note.set(Note::Says("This PDF is locked with a password".into()));
                out.send(ToParent::PasswordNeeded);
            }
            Err(OpenError::Unreadable(why)) => {
                eprintln!("atrium-pdf: {why}");
                self.view.note.set(Note::Says("This file can't be read as a PDF".into()));
                out.send(ToParent::Error { code: error::UNREADABLE });
            }
        }
    }

    /// Lay the pages out again for the window's size and the fit, keeping
    /// the page at the top where it was.
    fn relayout(&mut self) {
        let sizes = self.view.sizes.borrow().clone();
        if sizes.is_empty() {
            return;
        }
        let old = self.view.layout.get_cloned();
        let mut st = self.view.scroll.get();
        let anchor = old.as_ref().map(|l| {
            let y = st.physics.y as f32;
            let p = l.page_at(y);
            (p, (y - l.rects[p][1]) / l.rects[p][3].max(1.0))
        });
        let l = Layout::new(&sizes, self.fit, self.view.size.get());
        if let Some((p, f)) = anchor {
            st.physics.set_position((l.rects[p][1] + f * l.rects[p][3]) as f64);
            self.view.scroll.set(st);
        }
        if old.as_deref() != Some(&l) {
            self.view.layout.set(Some(Rc::new(l)));
        }
    }

    fn go_to(&mut self, page: usize) {
        if let Some(l) = self.view.layout.get_cloned() {
            if let Some(r) = l.rects.get(page) {
                let mut st = self.view.scroll.get();
                st.physics.set_position((r[1] - atrium_pdf::GAP / 2.0).max(0.0) as f64);
                self.view.scroll.set(st);
            }
        }
    }

    fn message(&mut self, out: &mut dyn Out, m: ToChild) {
        match m {
            ToChild::SetTheme { dark } => self.view.dark.set(dark),
            ToChild::GoTo { page } => self.go_to(page as usize),
            ToChild::SetZoom { fit } => {
                self.fit = fit;
                let mut st = self.view.scroll.get();
                st.zoom = Default::default();
                self.view.scroll.set(st);
                self.relayout();
            }
            // Finding text comes with the text layer (P3).
            ToChild::Find { .. } => out.send(ToParent::Found { count: 0, current: 0, page: None }),
            ToChild::ExportImages | ToChild::Load { .. } => out.send(ToParent::Error { code: error::NO_DOCUMENT }),
        }
    }

    /// Between window steps: hand the engine what the last render wanted,
    /// take what it drew, report the page at the top.
    fn tick(&mut self, out: &mut dyn Out) {
        let Some(e) = self.engine.as_mut() else { return };
        {
            let mut w = self.view.wants.borrow_mut();
            if w.changed {
                w.changed = false;
                e.want(&w.keys);
            }
        }
        let drawn = e.drawn();
        if !drawn.is_empty() {
            let mut tiles = self.view.tiles.borrow_mut();
            for t in drawn {
                let bytes = t.rgba.len();
                tiles.insert(t.key, Image::new(t.width, t.height, t.rgba), bytes);
            }
            drop(tiles);
            self.view.drawn.set(self.view.drawn.get().wrapping_add(1));
        }
        if !self.first_shown && !e.busy() && self.view.wants.borrow().keys.is_empty() && self.view.layout.get_cloned().is_some() {
            self.first_shown = true;
            if let Some(t) = self.opened {
                eprintln!("atrium-pdf: first screen in {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
            }
        }
        if let Some(l) = self.view.layout.get_cloned() {
            let p = l.page_at(self.view.scroll.get().physics.y as f32 + 1.0);
            if self.at_page != Some(p) {
                self.at_page = Some(p);
                out.send(ToParent::AtPage { page: p as u32 });
            }
        }
    }

    fn window_event(&mut self, ev: &fresco_client::Event) {
        if let fresco_client::Event::Resized { width, height, .. } = ev {
            self.view.size.set((*width as f32, *height as f32));
            self.relayout();
        }
    }
}

fn new_app(size: (f32, f32), density: f32) -> (App<PdfView>, PdfView) {
    let dirty = Arc::new(AtomicBool::new(true));
    let view = PdfView {
        layout: Mutable::with_dirty(None, dirty.clone()),
        scroll: Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        size: Mutable::with_dirty(size, dirty.clone()),
        dark: Mutable::with_dirty(false, dirty.clone()),
        note: Mutable::with_dirty(Note::Opening, dirty.clone()),
        drawn: Mutable::with_dirty(0, dirty.clone()),
        density: Rc::new(std::cell::Cell::new(density.max(0.5))),
        tiles: Rc::new(RefCell::new(TileCache::new(TILE_CACHE_BYTES))),
        wants: Rc::new(RefCell::new(Wants::default())),
        sizes: Rc::new(RefCell::new(Vec::new())),
        printing: Mutable::with_dirty(false, dirty.clone()),
    };
    (App::new_with_flag(view.clone(), dirty), view)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--bench") {
        let path = args.get(i + 1).unwrap_or_else(|| die("--bench FILE [WIDTH]"));
        let width: f32 = args.get(i + 2).and_then(|w| w.parse().ok()).unwrap_or(1000.0);
        return bench(path, width);
    }
    if let Some(i) = args.iter().position(|a| a == "--file") {
        let path = args.get(i + 1).unwrap_or_else(|| die("--file PATH"));
        return standalone(path, args.iter().any(|a| a == "--dark"));
    }
    embedded();
}

/// No window: open, lay out for a window `width` points wide (at 1x), and
/// draw what that window shows of the first page.
fn bench(path: &str, width: f32) {
    let t = Instant::now();
    let f = std::fs::File::open(path).unwrap_or_else(|e| die(e));
    let mut e = Engine::open(Box::new(f), b"").unwrap_or_else(|e| die(format_args!("{e:?}")));
    let opened = t.elapsed();
    let l = Layout::new(e.sizes(), Fit::Width, (width, width * 0.75));
    let r = l.rects[0];
    let keys = e.tiles_for(0, l.scale as f64, [0.0, 0.0, r[2] as f64, (width * 0.75 - r[1]).min(r[3]) as f64]);
    e.want(&keys);
    let mut n = 0;
    while e.busy() {
        n += e.wait(Duration::from_secs(5)).len();
    }
    println!(
        "{} pages; open {:.0} ms; first screen ({n} tiles) {:.0} ms",
        e.page_count(),
        opened.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3
    );
}

/// Development: the PDF in a window of its own.
fn standalone(path: &str, dark: bool) {
    let mut window = Window::open(&WindowDesc::new("PDF", SizeSpec::Fixed(1000, 760))).unwrap_or_else(|e| die(e));
    let (mut app, view) = new_app((window.width, window.height), window.scale());
    view.dark.set(dark);
    let mut run = Run::new(view);
    run.load(&mut LogOut, std::fs::File::open(path).ok());
    // Ctrl+P prints the file through the system's print sheet.
    let embeds = pergola::embed::Embeds::connect().map_err(|e| eprintln!("atrium-pdf: no limend ({e:?}); printing is off")).ok();
    if let Some(e) = &embeds {
        window.set_slot_binder(pergola::embed::Embeds::binder(e));
    }
    let title = std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut printing: Option<pergola::print::Printing> = None;
    loop {
        let mut asked = false;
        match window.step_with(&mut app, Some(TICK), |ev| {
            asked |= ctrl_p(ev);
            ctrl_p(ev)
        }) {
            Ok(StepResult::Event(ev)) => run.window_event(&ev),
            Ok(StepResult::Closed) => return,
            Ok(_) => {}
            Err(e) => die(format_args!("frescod: {e}")),
        }
        if let (true, None, Some(e), Some(token)) = (asked, &printing, &embeds, window.embed_token()) {
            let started = std::fs::File::open(path).map_err(|e| format!("{e}")).and_then(|f| {
                use std::os::fd::AsFd;
                pergola::print::Printing::start(e, token, PRINT_SLOT, f.as_fd(), &title, false, None).map_err(|e| format!("{e:?}"))
            });
            match started {
                Ok(p) => {
                    printing = Some(p);
                    run.view.printing.set(true);
                }
                Err(why) => eprintln!("atrium-pdf: print: the sheet didn't open: {why}"),
            }
        }
        if let (Some(p), Some(e)) = (printing.as_mut(), &embeds) {
            let ev = e.borrow_mut().poll(Duration::ZERO);
            match ev.as_ref().ok().and_then(|ev| ev.as_ref()).and_then(|ev| p.handle(e, ev)) {
                Some(pergola::print::Step::Done(o)) => {
                    eprintln!("atrium-pdf: print: {o:?}");
                    printing = None;
                    run.view.printing.set(false);
                }
                _ => {}
            }
        }
        run.tick(&mut LogOut);
    }
}

fn embedded() {
    let (mut child, fresco) = limen::client::Child::embedded().unwrap_or_else(|e| die(format_args!("no limend channel: {e:?}")));
    let (embed, sanitizer) = loop {
        match child.poll(Duration::from_secs(30)) {
            Ok(Some(ChildEvent::Attach { embed, role, .. })) if role == "pdf-viewer" || role == "pdf-sanitizer" => break (embed, role == "pdf-sanitizer"),
            Ok(Some(ChildEvent::Attach { role, .. })) => die(format_args!("asked to be {role}")),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => die("limend never attached us"),
        }
    };
    let conn = fresco_client::Connection::wrap(fresco).unwrap_or_else(|e| die(format_args!("frescod: {e}")));
    let mut desc = WindowDesc::new("pdf", SizeSpec::Fixed(800, 600));
    desc.hints = fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() };
    let mut window = Window::open_on(conn, &desc).unwrap_or_else(|e| die(format_args!("window: {e}")));
    window.follow_appearance(false);
    let (mut app, view) = new_app((window.width, window.height), window.scale());
    let mut run = Run::new(view);
    // Resizes that come before the token are the slot's size.
    let until = Instant::now() + Duration::from_secs(10);
    let token = loop {
        if let Some(t) = window.embed_token() {
            break t;
        }
        match window.step(&mut app, Some(Duration::from_millis(100))) {
            Ok(StepResult::Event(ev)) => run.window_event(&ev),
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
        loop {
            match out.0.poll(Duration::from_millis(1)) {
                Ok(Some(ChildEvent::Message { bytes, .. })) => match ToChild::decode(&bytes) {
                    Some(m) => run.message(&mut out, m),
                    None => eprintln!("atrium-pdf: dropped a malformed message"),
                },
                Ok(Some(ChildEvent::MessageFd { bytes, .. })) => {
                    let fd = out.0.take_fd();
                    match ToChild::decode(&bytes) {
                        Some(ToChild::ExportImages) if sanitizer => {
                            use std::os::fd::AsFd;
                            let pdf = fd.ok_or(folium_syntax::Error::Unreadable("missing PDF"))
                                .and_then(|fd| atrium_pdf::sanitized::export(Box::new(std::fs::File::from(fd))))
                                .and_then(|bytes| Ok(limen::fdpass::sealed_file("visual-copy.pdf", &bytes)?));
                            match pdf {
                                Ok(file) => { let _ = out.0.emit_fd(embed, ToParent::ImageCopy.encode(), file.as_fd()); }
                                Err(folium_syntax::Error::NeedsPassword) => out.send(ToParent::PasswordNeeded),
                                Err(_) => out.send(ToParent::Error { code: error::UNREADABLE }),
                            }
                        }
                        Some(ToChild::Load { dark }) if !sanitizer => {
                            run.view.dark.set(dark);
                            run.load(&mut out, fd.map(std::fs::File::from));
                        }
                        _ => eprintln!("atrium-pdf: dropped a descriptor"),
                    }
                }
                Ok(Some(ChildEvent::Detaching { .. })) => return,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => die(format_args!("limend: {e:?}")),
            }
        }
        let mut asked = false;
        match window.step_with(&mut app, Some(TICK), |ev| {
            asked |= ctrl_p(ev);
            ctrl_p(ev)
        }) {
            Ok(StepResult::Event(ev)) => run.window_event(&ev),
            Ok(StepResult::Closed) => return,
            Ok(_) => {}
            Err(e) => die(format_args!("frescod: {e}")),
        }
        // Ctrl+P: the parent prints the PDF it handed over.
        if asked && run.engine.is_some() {
            out.send(ToParent::PrintAsked);
        }
        run.tick(&mut out);
    }
}
