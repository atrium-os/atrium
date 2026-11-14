//! scrinium: the file manager's window, and the system file picker
//! (docs/spec/scrinium.md).
//!
//!   scrinium [--no-window] [--no-picker]
//!
//! At start it asks dispensatord for every USB stick in its own jail
//! (`ATTACH_ALL`, capability `removable-files`) and offers Limen's `picker`
//! role (it is the catalogue's provider, `org.atrium.scrinium`). The window
//! is the file manager; each pick an app asks for gets a window of its own,
//! bound into the app's slot by limend, on its own thread.
//!
//! Environment (defaults in brackets):
//!   FRESCO_SOCKET           [/tmp/frescod.sock]
//!   LIMEN_SOCKET            [/var/run/atrium/limend.sock]
//!   SCRINIUM_MEDIA_SOCKET   [/atrium/sockets/removable-media/removable-media.sock]
//!   SCRINIUM_MEDIA          [/media] where the sticks are in the jail
//!   SCRINIUM_PLACES         [none] more places: `Label=/path;Other=/path`
//!   SCRINIUM_SIZE           [960x600] the window
//!   SCRINIUM_SECS           [unset] exit after this many seconds (VM checks)
//!   SCRINIUM_TRACE          [unset] log what is shown after each change
//!   SCRINIUM_OPEN           [unset] open this place id (`da0s1`) and path at start

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use limen::client::Child;
use limen::picker::{Filter, ToChild, ToParent};
use limen::proto::{ChildEvent, EmbedId};
use pergola::input::translate;
use pergola::text_edit::TextEdit;
use pergola::widgets::ScrollState;
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, Window, WindowDesc};
use scrinium::browser::{Browser, Mode, Mods, Outcome, Place};
use scrinium::places;
use scrinium::view::{on_key, with, ScriniumView};

fn env(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// The places now, shared with the picker threads, and a counter that
/// moves when they change.
#[derive(Default)]
struct Places {
    list: Vec<Place>,
    gen: u64,
}

type SharedPlaces = Arc<Mutex<Places>>;

fn size_of(s: &str, d: (u32, u32)) -> (u32, u32) {
    s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or(d)
}

/// One window, its app and its state: the file manager's or a picker's.
struct Pane {
    win: Window,
    app: App<ScriniumView>,
    state: Mutable<Browser>,
    field: Mutable<TextEdit>,
    size: Mutable<(f32, f32)>,
    mods: Arc<Mutex<Mods>>,
    gen: u64,
    trace: Option<String>,
}

impl Pane {
    fn new(win: Window, mode: Mode) -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        let state = Mutable::with_dirty(Browser::new(mode), dirty.clone());
        let field = Mutable::with_dirty(TextEdit::new(""), dirty.clone());
        let scroll = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
        let view = ScriniumView::new(state.clone(), field.clone(), scroll, size.clone());
        let mods = view.mods.clone();
        let app = App::new_with_flag(view, dirty);
        Pane { win, app, state, field, size, mods, gen: u64::MAX, trace: std::env::var_os("SCRINIUM_TRACE").map(|_| String::new()) }
    }

    /// One turn: an event (or `wait`), then the places if they changed.
    /// `false` once the window is closed.
    fn step(&mut self, wait: Duration, places: &SharedPlaces, who: &str) -> bool {
        match self.win.step(&mut self.app, Some(wait)) {
            Ok(StepResult::Closed) => return false,
            Ok(StepResult::Event(ev)) => {
                use fresco_client::Event as E;
                match &ev {
                    E::PointerButton { pressed: true, modifiers, .. } => {
                        *self.mods.lock().unwrap() = Mods { shift: modifiers & 0x22 != 0, ctrl: modifiers & 0x11 != 0 };
                    }
                    E::Key { .. } => {
                        if let Some(k) = translate(&ev) {
                            on_key(&self.state, &self.field, &k);
                        }
                    }
                    _ => {}
                }
            }
            Ok(StepResult::Timeout) => {}
            Err(e) => {
                eprintln!("scrinium: {who}: frescod: {e}");
                return false;
            }
        }
        if self.size.get() != (self.win.width, self.win.height) {
            self.size.set((self.win.width, self.win.height));
        }
        let p = places.lock().unwrap();
        if p.gen != self.gen {
            self.gen = p.gen;
            let list = p.list.clone();
            drop(p);
            with(&self.state, |b| b.set_places(list));
        }
        if self.trace.is_some() {
            let b = self.state.get_cloned();
            let line = format!(
                "place={} path=/{} entries=[{}] selected=[{}] notice={}",
                b.place().map_or("-", |p| p.id.as_str()),
                b.path.join("/"),
                b.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(","),
                b.selected_names().join(","),
                b.notice.as_ref().map_or("-".to_string(), |n| n.text.clone()),
            );
            if self.trace.as_deref() != Some(&line) {
                eprintln!("scrinium: {who}: {line}");
                self.trace = Some(line);
            }
        }
        true
    }
}

// -- the picker ----------------------------------------------------------------

/// From the limend thread to a picker's thread.
enum ToPane {
    Open(Filter),
    Failed(String),
    Close,
}

/// From a picker's thread to the limend thread.
enum FromPane {
    Ready(EmbedId, [u8; 16]),
    Picked(EmbedId, Outcome),
    Gone(EmbedId),
}

/// A picker's window, on its own thread: frescod keeps it hidden until
/// limend binds it into the app's slot.
fn picker_pane(embed: EmbedId, app_id: String, fresco: String, places: SharedPlaces, rx: Receiver<ToPane>, tx: Sender<FromPane>) {
    let who = format!("picker {embed}");
    let desc = WindowDesc {
        hints: fresco_protocol::WindowHints { embed_pending: true, ..Default::default() },
        ..WindowDesc::new("Choose", SizeSpec::Fixed(640, 420))
    };
    let win = match fresco_client::Connection::connect(&fresco).and_then(|c| Window::open_on(c, &desc)) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("scrinium: {who}: window: {e}");
            let _ = tx.send(FromPane::Gone(embed));
            return;
        }
    };
    let mut pane = Pane::new(win, Mode::Picker { filter: Filter::file(), app: app_id });
    let mut ready = false;
    let mut sent = false;
    loop {
        if !pane.step(Duration::from_millis(30), &places, &who) {
            break;
        }
        if !ready {
            if let Some(token) = pane.win.embed_token() {
                ready = true;
                let _ = tx.send(FromPane::Ready(embed, token));
            }
        }
        match rx.try_recv() {
            Ok(ToPane::Open(f)) => {
                let list = places.lock().unwrap().list.clone();
                with(&pane.state, |b| {
                    b.mode = match &b.mode {
                        Mode::Picker { app, .. } => Mode::Picker { filter: f, app: app.clone() },
                        m => m.clone(),
                    };
                    b.set_places(list);
                });
            }
            Ok(ToPane::Failed(why)) => {
                sent = false;
                with(&pane.state, |b| b.pick_failed(&why));
            }
            Ok(ToPane::Close) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if !sent {
            if let Some(o) = pane.state.get_cloned().outcome {
                sent = true;
                let _ = tx.send(FromPane::Picked(embed, o));
            }
        }
    }
    let _ = tx.send(FromPane::Gone(embed));
}

struct Embed {
    to_pane: Sender<ToPane>,
    parent_uid: u32,
    write: bool,
}

/// Hand the pick over: files as descriptors, a folder as a grant into the
/// app's jail (dispensatord finds the jail from the app's uid).
fn hand_over(child: &mut Child, embed: EmbedId, e: &Embed, o: Outcome, media_sock: &std::path::Path) -> Result<(), String> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;
    match o {
        Outcome::Cancelled => {
            let _ = child.emit(embed, ToParent::Cancelled.encode());
            Ok(())
        }
        Outcome::Files(files) => {
            // Open every one first: all or nothing.
            let mut open = Vec::new();
            for (name, path) in &files {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(e.write)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)
                    .map_err(|err| format!("Couldn't open “{name}”: {}", scrinium::browser::err_text(&err)))?;
                let m = f.metadata().map_err(|err| err.to_string())?;
                if !m.is_file() {
                    return Err(format!("“{name}” is not a file"));
                }
                open.push((name.clone(), m.len(), f));
            }
            for (name, size, f) in &open {
                let msg = ToParent::File { name: name.clone(), size: *size, write: e.write };
                child.emit_fd(embed, msg.encode(), f.as_fd()).map_err(|err| format!("limend refused the file ({err:?})"))?;
                eprintln!("scrinium: picker {embed}: handed over {name}{}", if e.write { " (read-write)" } else { "" });
            }
            let _ = child.emit(embed, ToParent::Done { files: open.len() as u32 }.encode());
            Ok(())
        }
        Outcome::Folder { part, path, name } => {
            let p = atrium_media::Picked { part, path: path.clone(), app_uid: e.parent_uid, read_only: !e.write };
            match atrium_media::client::picked(media_sock, &p) {
                Ok(at) => {
                    eprintln!("scrinium: picker {embed}: folder {path} at {at} in the app's jail");
                    let _ = child.emit(embed, ToParent::Folder { at, name, read_only: !e.write }.encode());
                    let _ = child.emit(embed, ToParent::Done { files: 0 }.encode());
                    Ok(())
                }
                Err((_, why)) => Err(format!("Couldn't hand the folder over: {why}")),
            }
        }
    }
}

/// The picker service: limend's `picker` provider, one pane per embed.
fn serve_picker(limen_sock: String, fresco: String, places: SharedPlaces, media_sock: std::path::PathBuf) {
    std::thread::spawn(move || loop {
        let mut child = match Child::provider(&limen_sock, "picker") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("scrinium: limend at {limen_sock}: {e:?}; again in 5 s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        eprintln!("scrinium: providing picker");
        let (tx, rx) = mpsc::channel::<FromPane>();
        let mut embeds: HashMap<EmbedId, Embed> = HashMap::new();
        loop {
            let ev = match child.poll(Duration::from_millis(20)) {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("scrinium: limend went ({e:?})");
                    break;
                }
            };
            match ev {
                Some(ChildEvent::Attach { embed, role, parent_app, parent_uid }) if role == "picker" => {
                    eprintln!("scrinium: picker {embed} for {parent_app} (uid {parent_uid})");
                    let (ptx, prx) = mpsc::channel();
                    embeds.insert(embed, Embed { to_pane: ptx, parent_uid, write: false });
                    let (f, pl, t) = (fresco.clone(), places.clone(), tx.clone());
                    std::thread::spawn(move || picker_pane(embed, parent_app, f, pl, prx, t));
                }
                Some(ChildEvent::Message { embed, bytes }) => {
                    if let (Some(e), Some(ToChild::Open(f))) = (embeds.get_mut(&embed), ToChild::decode(&bytes)) {
                        e.write = f.write;
                        let _ = e.to_pane.send(ToPane::Open(f));
                    }
                }
                Some(ChildEvent::Detaching { embed }) => {
                    if let Some(e) = embeds.remove(&embed) {
                        let _ = e.to_pane.send(ToPane::Close);
                    }
                }
                _ => {}
            }
            while let Ok(m) = rx.try_recv() {
                match m {
                    FromPane::Ready(embed, token) => {
                        if let Err(e) = child.ready(embed, token) {
                            eprintln!("scrinium: picker {embed}: ready: {e:?}");
                        }
                    }
                    FromPane::Picked(embed, o) => {
                        let Some(e) = embeds.get(&embed) else { continue };
                        if let Err(why) = hand_over(&mut child, embed, e, o, &media_sock) {
                            eprintln!("scrinium: picker {embed}: {why}");
                            let _ = e.to_pane.send(ToPane::Failed(why));
                        }
                    }
                    FromPane::Gone(embed) => {
                        // The window went first (frescod closed it): the app
                        // hears it was cancelled.
                        if embeds.remove(&embed).is_some() {
                            let _ = child.emit(embed, ToParent::Cancelled.encode());
                        }
                    }
                }
            }
        }
        for (_, e) in embeds.drain() {
            let _ = e.to_pane.send(ToPane::Close);
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let window = !args.iter().any(|a| a == "--no-window");
    let picker = !args.iter().any(|a| a == "--no-picker");
    let fresco = env("FRESCO_SOCKET", "/tmp/frescod.sock");
    let media_sock = places::media_socket();

    // Every stick, in this jail, for as long as it runs.
    if media_sock.exists() {
        match atrium_media::client::attach_all(&media_sock) {
            Ok(m) => eprintln!("scrinium: sticks: {m}"),
            Err((st, m)) => eprintln!("scrinium: no standing grant of the sticks: {m} (status {st})"),
        }
    }
    let configured = places::configured();
    let shared: SharedPlaces = Arc::new(Mutex::new(Places { list: configured.clone(), gen: 1 }));
    let (ptx, prx) = mpsc::channel();
    places::watch(ptx);
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            for sticks in prx {
                let mut all = places::distinct(sticks);
                all.extend(configured.iter().cloned());
                let mut p = shared.lock().unwrap();
                if p.list != all {
                    p.list = all;
                    p.gen += 1;
                }
            }
        });
    }
    if picker {
        if !window {
            if let Ok(m) = pergola::text::WireMeasurer::connect(&fresco) {
                pergola::text::install_measurer(Box::new(m));
            }
        }
        serve_picker(env("LIMEN_SOCKET", limen::client::DEFAULT_SOCKET), fresco.clone(), shared.clone(), media_sock.clone());
    }
    let until = std::env::var("SCRINIUM_SECS").ok().and_then(|s| s.parse::<u64>().ok()).map(|s| Instant::now() + Duration::from_secs(s));
    if !window {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if until.is_some_and(|u| Instant::now() >= u) {
                return;
            }
        }
    }

    let (w, h) = size_of(&env("SCRINIUM_SIZE", ""), (960, 600));
    let win = match Window::open(&WindowDesc::new("Scrinium", SizeSpec::Fixed(w, h))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("scrinium: frescod at {fresco}: {e}");
            std::process::exit(1);
        }
    };
    let mut pane = Pane::new(win, Mode::Manager);
    let mut opened = std::env::var("SCRINIUM_OPEN").ok();
    if let Err(e) = pane.win.paint(&mut pane.app) {
        eprintln!("scrinium: first paint: {e}");
    }
    loop {
        if !pane.step(Duration::from_millis(100), &shared, "window") {
            break;
        }
        // SCRINIUM_OPEN=da0s1/Docs: open there once the place is in.
        if let Some(o) = opened.clone() {
            let (id, rest) = o.split_once('/').unwrap_or((o.as_str(), ""));
            let b = pane.state.get_cloned();
            if let Some(i) = b.places.iter().position(|p| p.id == id) {
                opened = None;
                with(&pane.state, |b| {
                    b.open_place(i);
                    for n in rest.split('/').filter(|n| !n.is_empty()) {
                        b.enter(n);
                    }
                });
            }
        }
        if let Some(part) = pane.state.get_cloned().want_eject {
            let msg = match atrium_media::client::eject(&media_sock, &part) {
                Ok(m) => (format!("Ejected: {}", if m.is_empty() { "safe to unplug".into() } else { m }), false),
                Err((atrium_media::status::BUSY, _)) => ("A file on it is still open: close it, then eject".into(), true),
                Err((_, m)) => (format!("Couldn't eject: {m}"), true),
            };
            with(&pane.state, |b| {
                b.want_eject = None;
                b.notice = Some(scrinium::browser::Notice { text: msg.0, error: msg.1 });
            });
        }
        if until.is_some_and(|u| Instant::now() >= u) {
            break;
        }
    }
}
