//! navigator-ui: the Navigator's window on a live frescod.
//!
//!   FRESCO_SOCKET=… navigator-ui [--scene main|prompt|start|live|stop] [--dark|--light]
//!
//! Build step 1 (docs/spec/navigator-ui.md §14): the stand-in trail store,
//! the page as a placeholder slot. `--scene` opens on one of §5.8's frames
//! (default: main, with the status bar off as D11 has it). Without
//! `--dark`/`--light` the window follows the system appearance.
//! `NAVIGATOR_UI_SECS=n` exits after n seconds (for VM screenshots);
//! `NAVIGATOR_UI_SIZE=WxH` sets the window's size (default 1280x696, the
//! 720-line screen under frescod's 24 px origin strip).
//!
//! Step 2 (`navigator_ui::live`): with `NAVIGATORD_SOCKET`, the window
//! opens on a fresh trail on Start; pages come from navigatord and are
//! drawn by a doc-viewer child that limend (`LIMEN_SOCKET`) binds into the
//! page slot. `NAVIGATOR_UI_OPEN=url` opens one at start.
//!
//! Step 3 (`navigator_ui::disk`): in live mode the trail store is read at
//! start and written back as it changes, at `NAVIGATOR_TRAILS` or
//! ~/.local/share/atrium/navigator/trails.json (§4.5).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use navigator_ui::fake::Scene;
use navigator_ui::{disk, keys};
use navigator_ui::live::Live;
use navigator_ui::shell::{Shell, PAGE_SLOT};
use navigator_ui::trail::TrailStore;
use navigator_ui::ui::{now_ms, ShellView};

use pergola::theme::Semantic;
use pergola::window::StepResult;
use pergola::{App, Event, Mutable, SizeSpec, Window, WindowDesc};

/// How long a step waits for frescod before navigatord and limend are
/// looked at (live mode).
const TICK: Duration = Duration::from_millis(30);

/// Where the page slot is, for the log, until Limen binds it (step 2).
struct LogBinder;

impl pergola::slot::SlotBinder for LogBinder {
    fn place(&mut self, window_id: u32, key: pergola::slot::SlotKey, rect: Option<[f32; 4]>) {
        if key == PAGE_SLOT {
            eprintln!("navigator-ui: page slot of window {window_id} -> {rect:?}");
        }
    }
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("navigator-ui: {m}");
    std::process::exit(1)
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut scene = None;
    let mut theme = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scene" => {
                i += 1;
                scene = args.get(i).and_then(|s| Scene::parse(s));
                if scene.is_none() {
                    eprintln!("navigator-ui: --scene main|prompt|start|live|stop");
                    std::process::exit(2);
                }
            }
            "--dark" => theme = Some(Semantic::DARK),
            "--light" => theme = Some(Semantic::LIGHT),
            a => {
                eprintln!("navigator-ui: unknown argument {a}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let (w, h) = std::env::var("NAVIGATOR_UI_SIZE").ok()
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
        .unwrap_or((1280u32, 696u32));
    if let Some(secs) = std::env::var("NAVIGATOR_UI_SECS").ok().and_then(|s| s.parse::<u64>().ok()) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            std::process::exit(0);
        });
    }

    let desc = WindowDesc::new("Navigator", SizeSpec::Fixed(w, h));
    // Live: navigatord first, since it may open the window's connection.
    let mut live = None;
    let mut window = match std::env::var("NAVIGATORD_SOCKET").ok().filter(|_| scene.is_none()) {
        Some(path) => {
            let (l, fd) = Live::connect(&path, true).unwrap_or_else(|e| die(format_args!("navigatord at {path}: {e}")));
            live = Some(l);
            match fd {
                Some(fd) => {
                    // Text is measured on a connection of its own, as
                    // Window::open does.
                    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
                    match pergola::text::WireMeasurer::connect(&sock) {
                        Ok(m) => pergola::text::install_measurer(Box::new(m)),
                        Err(e) => eprintln!("navigator-ui: no text measurer ({e}); using estimates"),
                    }
                    let conn = fresco_client::Connection::wrap(std::os::unix::net::UnixStream::from(fd))?;
                    Window::open_on(conn, &desc)?
                }
                None => {
                    eprintln!("navigator-ui: no chrome channel: the strip will not name the page");
                    Window::open(&desc)?
                }
            }
        }
        None => Window::open(&desc)?,
    };
    window.follow_appearance(theme.is_none());
    let trails_path = disk::path();
    let dirty = Arc::new(AtomicBool::new(true));
    let mut shell = match (scene, &live) {
        (Some(sc), _) => Shell::scene(sc, window.width, window.height),
        (None, Some(_)) => {
            let store = restore(&trails_path);
            let mut s = Shell::new(store, window.width, window.height);
            s.live = Some(navigator_ui::shell::LivePage::Loading(String::new()));
            if let Ok(u) = std::env::var("NAVIGATOR_UI_OPEN") {
                if s.store.trail().page().is_some() {
                    s.store.new_trail(false);
                }
                s.enter(&u);
            } else if s.store.trail().page().is_none() {
                s.begin_edit();
            }
            s
        }
        (None, None) => Shell::new(navigator_ui::fake::store(), window.width, window.height),
    };
    shell.w = window.width;
    shell.h = window.height;
    let state = Mutable::with_dirty(shell, dirty.clone());
    let mut app = App::new_with_flag(ShellView { state: state.clone() }, dirty);
    if let Some(t) = theme {
        app = app.with_theme(t);
    }

    match &mut live {
        Some(l) => {
            l.dark = theme == Some(Semantic::DARK);
            l.window(window.id());
            let embeds = pergola::embed::Embeds::connect().unwrap_or_else(|e| die(format_args!("limend: {e:?}")));
            window.set_slot_binder(pergola::embed::Embeds::binder(&embeds));
            // frescod names the window with an embed token; limend wants it.
            let until = std::time::Instant::now() + Duration::from_secs(10);
            while window.embed_token().is_none() {
                if std::time::Instant::now() > until {
                    die("no embed token from frescod");
                }
                if let StepResult::Closed = window.step(&mut app, Some(Duration::from_millis(100)))? {
                    return Ok(());
                }
            }
            l.embed(embeds, window.embed_token().unwrap());
        }
        None => window.set_slot_binder(Box::new(LogBinder)),
    }

    // The trail store goes to disk (§4.5) in live mode: at most once a
    // second, and only when its records changed.
    let mut saver = live.as_ref().map(|_| Saver::new(trails_path, &state.get_cloned().store));
    loop {
        match window.step(&mut app, Some(TICK))? {
            StepResult::Event(raw) => event(&raw, &state),
            StepResult::Closed => {
                if let Some(sv) = &mut saver {
                    sv.save(&state.get_cloned().store, true);
                }
                return Ok(());
            }
            StepResult::Timeout => {}
        }
        {
            // The prompt row's 500 ms (§6.4) starts again when it moves or
            // something covers it.
            let mut s = state.get_cloned();
            let mut changed = s.settle(now_ms());
            if let Some(l) = &mut live {
                if l.tick(&mut s) {
                    navigator_ui::ui::trace(&mut s);
                    changed = true;
                }
            }
            if changed {
                state.set(s);
            }
        }
        if let Some(sv) = &mut saver {
            sv.save(&state.get_cloned().store, false);
        }
    }
}

/// The store from disk, Earlier past its 30 days dropped; an empty one
/// with a fresh trail when there is none (or it cannot be read).
fn restore(p: &std::path::Path) -> TrailStore {
    let mut store = match disk::load(p, &|u| navigator_ui::shell::live_spec(u).facts) {
        Ok(Some((s, dropped))) => {
            for d in dropped {
                eprintln!("navigator-ui: trail store: dropped {d}");
            }
            eprintln!("navigator-ui: trail store: {} trails, {} put away, from {}", s.trails.len(), s.earlier.len(), p.display());
            s
        }
        Ok(None) => {
            let mut s = TrailStore::empty();
            s.new_trail(false);
            s
        }
        Err(e) => {
            // Kept aside rather than overwritten, so nothing is lost to a bug.
            let aside = p.with_extension("json.unreadable");
            let _ = std::fs::rename(p, &aside);
            eprintln!("navigator-ui: trail store: {e}; moved it to {} and started afresh", aside.display());
            let mut s = TrailStore::empty();
            s.new_trail(false);
            s
        }
    };
    let gone = store.expire(navigator_ui::clock::now_s());
    if gone > 0 {
        eprintln!("navigator-ui: trail store: {gone} put-away trails past 30 days forgotten");
    }
    store
}

struct Saver {
    path: std::path::PathBuf,
    last: String,
    at: Instant,
}

impl Saver {
    fn new(path: std::path::PathBuf, store: &TrailStore) -> Self {
        // Unchanged from disk is not written, except that a store read
        // with records dropped or expired is written back as it now is.
        let last = std::fs::read_to_string(&path).unwrap_or_default();
        let mut sv = Saver { path, last, at: Instant::now() - Duration::from_secs(2) };
        sv.save(store, true);
        sv
    }

    fn save(&mut self, store: &TrailStore, now: bool) {
        if !now && self.at.elapsed() < Duration::from_secs(1) {
            return;
        }
        let text = disk::encode(store);
        if text == self.last {
            return;
        }
        self.at = Instant::now();
        match disk::save(&self.path, &text) {
            Ok(()) => self.last = text,
            Err(e) => eprintln!("navigator-ui: trail store: {}: {e}", self.path.display()),
        }
    }
}

/// The window's raw events the shell handles itself: its size, the
/// keyboard map and the Where field's typing, and presses (which close
/// the field and the popover when they land outside them).
fn event(raw: &fresco_client::Event, state: &Mutable<Shell>) {
    if let fresco_client::Event::Resized { width, height, .. } = raw {
        let mut s = state.get_cloned();
        s.w = *width as f32;
        s.h = *height as f32;
        state.set(s);
        return;
    }
    let Some(ev) = pergola::input::translate(raw) else { return };
    match &ev {
        Event::Key { modifiers, kind, key, chars } => {
            let mut s = state.get_cloned();
            s.mods = *modifiers;
            let now = now_ms();
            if let Some(a) = keys::action(&ev) {
                s.apply(a, now);
            } else if !kind.pressed() {
            } else if s.editing.is_some() {
                s.edit_key(*key, *modifiers, chars, now);
            } else if *key == pergola::Key::Escape {
                s.escape();
            } else if s.history.is_some() {
                s.history_key(*key, *modifiers, chars);
            } else if s.find.is_some() {
                s.find_key(*key, *modifiers, chars);
            }
            if kind.pressed() {
                navigator_ui::ui::trace(&mut s);
            }
            state.set(s);
        }
        Event::PointerDown { at } => {
            let mut s = state.get_cloned();
            s.press_at(at.x, at.y);
            state.set(s);
        }
        _ => {}
    }
}
