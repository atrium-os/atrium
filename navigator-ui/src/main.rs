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

/// Started by navigatord as its worker (navigator-ui.md §2.3a J3), our
/// stdin is the control channel and there is no environment of its: the
/// channel's first frame is our `NAVIGATOR_UI_*` settings.
fn worker_channel() -> Option<std::os::unix::net::UnixStream> {
    if std::env::var_os("NAVIGATORD_SOCKET").is_some() {
        return None;
    }
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(0, &mut st) } != 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFSOCK {
        return None;
    }
    use std::os::fd::FromRawFd;
    let mut s = unsafe { std::os::unix::net::UnixStream::from_raw_fd(libc::dup(0)) };
    match navigator_control::read_frame::<navigator_control::Event>(&mut s) {
        Ok(Some(navigator_control::Event::Settings { pairs })) => {
            for (k, v) in pairs.into_iter().filter(|(k, _)| k.starts_with("NAVIGATOR_UI_")) {
                // Before any thread starts.
                unsafe { std::env::set_var(k, v) };
            }
            Some(s)
        }
        other => die(format_args!("navigatord's channel began with {other:?}, not our settings")),
    }
}

fn main() -> std::io::Result<()> {
    let channel = worker_channel();
    // navigatord passes on only NAVIGATOR_UI_* settings (on the channel, so
    // after `worker_channel`): this one turns on Pergola's log of each line
    // of text sent to frescod.
    if std::env::var_os("NAVIGATOR_UI_WIRE_LOG").is_some() {
        std::env::set_var("PERGOLA_WIRE_LOG", "1");
    }
    if let Some(p) = navigator_ui::shell::window_persona() {
        eprintln!("navigator-ui: window persona {p}");
    }
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

    // The sign-in window says whose page it shows in its own title.
    let title = navigator_ui::shell::window_portal().map_or("Navigator".to_string(), |n| format!("Sign in to {n}"));
    let desc = WindowDesc::new(&title, SizeSpec::Fixed(w, h));
    // Live: navigatord first, since it may open the window's connection.
    let mut live = None;
    let mut limen_fd: Option<std::os::fd::OwnedFd> = None;
    // The trail store navigatord keeps for us, when we are its worker.
    let mut kept_trails: Option<Option<String>> = None;
    let hello = match (&channel, std::env::var("NAVIGATORD_SOCKET").ok()) {
        _ if scene.is_some() => None,
        (Some(c), _) => Some(Live::on(c.try_clone()?, true, true).unwrap_or_else(|e| die(format_args!("navigatord: {e}")))),
        (None, Some(path)) => Some(Live::connect(&path, true).unwrap_or_else(|e| die(format_args!("navigatord at {path}: {e}")))),
        (None, None) => None,
    };
    let mut window = match hello {
        Some((l, h)) => {
            live = Some(l);
            limen_fd = h.limen;
            if channel.is_some() {
                kept_trails = Some(h.trails);
            }
            match h.frescod {
                Some(fd) => {
                    // Text is measured on a connection of its own, as
                    // Window::open does: navigatord's second one, else ours.
                    let m = match h.measure {
                        Some(m) => fresco_client::Connection::wrap(std::os::unix::net::UnixStream::from(m)).map(pergola::text::WireMeasurer::on),
                        None => pergola::text::WireMeasurer::connect(&std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into())),
                    };
                    match m {
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
            let store = match &kept_trails {
                Some(t) => restore_kept(t.as_deref()),
                None => restore(&trails_path),
            };
            let mut s = Shell::new(store, window.width, window.height);
            s.live = Some(navigator_ui::shell::LivePage::Loading(String::new()));
            if s.portal.is_some() {
                // The sign-in window: Unrecorded, whatever was kept.
                s.store = navigator_ui::trail::TrailStore::empty();
                s.store.persona = navigator_ui::trail::Persona::Unrecorded;
                s.store.new_trail(true);
            }
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
            // limend over navigatord's connection when it opened one for us.
            let embeds = match limen_fd.take() {
                Some(fd) => aqueduct::Connection::wrap(std::os::unix::net::UnixStream::from(fd))
                    .map(|c| pergola::embed::Embeds::wrap(limen::client::Parent::wrap(c)))
                    .unwrap_or_else(|e| die(format_args!("limend via navigatord: {e}"))),
                None => pergola::embed::Embeds::connect().unwrap_or_else(|e| die(format_args!("limend: {e:?}"))),
            };
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
            let token = window.embed_token().unwrap();
            l.window(window.id(), token);
            l.embed(embeds, token);
        }
        None => window.set_slot_binder(Box::new(LogBinder)),
    }

    // The trail store goes to disk (§4.5) in live mode: at most once a
    // second, and only when its records changed.
    let sink = match kept_trails {
        Some(t) => Sink::Navigatord(t.unwrap_or_default()),
        None => Sink::File(trails_path),
    };
    let mut saver = live.as_mut().map(|l| Saver::new(sink, &state.get_cloned().store, l));
    // The prompt row, guarded by frescod (§6.4): what it was last told.
    let mut guarded: Option<Option<[i32; 4]>> = None;
    loop {
        match window.step(&mut app, Some(TICK))? {
            StepResult::Event(raw) => event(&raw, window.id(), &state),
            StepResult::Closed => {
                if let (Some(sv), Some(l)) = (&mut saver, &mut live) {
                    sv.save(&state.get_cloned().store, true, l);
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
            // F6 to the page: frescod hands it the keys, if this window has them.
            let to_page = std::mem::take(&mut s.focus_page);
            // frescod watches the prompt row for other windows over it;
            // navigatord spends an answer's press only if it saw it clear.
            // The sign-in window's form, when no prompt shows, is guarded
            // the same way: its button's press is spent only if seen clear.
            let g = s.geo();
            let row = g.prompt.or_else(|| g.form.map(|mut form| {
                if let Some(i) = s.form.as_ref().and_then(|p| p.arca_field()) {
                    if let Some((_, field)) = s.form_layout().0.into_iter().find(|(index, _)| *index == i) {
                        form = pergola::Rect::new(form.x(), form.y(), form.w(), field.y() - form.y());
                    }
                }
                form
            })).map(|r| [r.x().ceil() as i32, r.y().ceil() as i32, r.w().floor() as i32 - 1, r.h().floor() as i32 - 1]);
            if changed || to_page {
                state.set(s);
            }
            if guarded != Some(row) {
                guarded = Some(row);
                let id = window.id();
                if let Err(e) = window.connection().window_guard(id, row) {
                    eprintln!("navigator-ui: frescod: guard: {e}");
                }
            }
            if to_page {
                let id = window.id();
                match window.connection().window_focus_embed(id) {
                    Ok(()) => eprintln!("navigator-ui: F6: the page has the keys"),
                    Err(e) => eprintln!("navigator-ui: frescod: {e}"),
                }
            }
        }
        if let (Some(sv), Some(l)) = (&mut saver, &mut live) {
            sv.save(&state.get_cloned().store, false, l);
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

/// The kept store navigatord sent, as [`restore`] reads one from disk.
fn restore_kept(text: Option<&str>) -> TrailStore {
    let mut store = match text.map(|t| disk::decode(t, &|u| navigator_ui::shell::live_spec(u).facts)) {
        Some(Ok((s, dropped))) => {
            for d in dropped {
                eprintln!("navigator-ui: trail store: dropped {d}");
            }
            eprintln!("navigator-ui: trail store: {} trails, {} put away, kept by navigatord", s.trails.len(), s.earlier.len());
            s
        }
        Some(Err(e)) => {
            eprintln!("navigator-ui: trail store from navigatord: {e}; started afresh");
            let mut s = TrailStore::empty();
            s.new_trail(false);
            s
        }
        None => {
            eprintln!("navigator-ui: trail store: none yet kept by navigatord");
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

/// Where the trail store goes: a file of ours, or navigatord, which keeps
/// it for a UI in its jail (the text it last sent us).
enum Sink {
    File(std::path::PathBuf),
    Navigatord(String),
}

struct Saver {
    sink: Sink,
    last: String,
    at: Instant,
}

impl Saver {
    fn new(sink: Sink, store: &TrailStore, live: &mut Live) -> Self {
        // Unchanged from what was kept is not written, except that a store
        // read with records dropped or expired is written back as it now is.
        let last = match &sink {
            Sink::File(p) => std::fs::read_to_string(p).unwrap_or_default(),
            Sink::Navigatord(t) => t.clone(),
        };
        let mut sv = Saver { sink, last, at: Instant::now() - Duration::from_secs(2) };
        sv.save(store, true, live);
        sv
    }

    fn save(&mut self, store: &TrailStore, now: bool, live: &mut Live) {
        if !now && self.at.elapsed() < Duration::from_secs(1) {
            return;
        }
        let text = disk::encode(store);
        if text == self.last {
            return;
        }
        self.at = Instant::now();
        match &self.sink {
            Sink::File(p) => match disk::save(p, &text) {
                Ok(()) => self.last = text,
                Err(e) => eprintln!("navigator-ui: trail store: {}: {e}", p.display()),
            },
            Sink::Navigatord(_) => {
                live.save_trails(&text);
                self.last = text;
            }
        }
    }
}

/// The window's raw events the shell handles itself: its size, the
/// keyboard map and the Where field's typing, and presses (which close
/// the field and the popover when they land outside them).
fn event(raw: &fresco_client::Event, main: u32, state: &Mutable<Shell>) {
    if let fresco_client::Event::Resized { window_id, width, height, .. } = raw {
        // A pop-up's window (Aa, the menu) has its own size: only the
        // main window's is the shell's.
        if *window_id != main {
            return;
        }
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
            } else if s.rail_find_focus {
                s.rail_find_key(*key, *modifiers, chars);
            } else if s.rail_focus {
                s.rail_key(*key);
            } else if *key == pergola::Key::Escape {
                s.escape();
            } else if s.history.is_some() {
                s.history_key(*key, *modifiers, chars);
            } else if s.find.is_some() {
                s.find_key(*key, *modifiers, chars);
            } else if s.form.is_some() {
                s.form_key(*key, *modifiers, chars);
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
