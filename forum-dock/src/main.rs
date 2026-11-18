//! forum-dock — Forum's launcher, an ordinary graphics-only app.
//!
//! It holds `app-launch` — the grant to ASK portcullisd for the installed-app
//! catalog and to request a launch — and `forum-control` (the WM intents). That
//! is all it is: the requester. portcullisd (the TCB) still does the verify →
//! allocate uid → register → jail dance, and the user's grants still authorize
//! it (docs/spec/forum.md §6).
//!
//! It draws two windows (§6.1): the dock (the wallpaper and the row of pinned
//! and running apps, behind everything) and, while it is up, the Applications
//! view (every installed app, type to filter), each on a thread of its own.
//!
//! Usage:
//!   forum-dock              # the dock
//!   forum-dock list         # list installed apps
//!   forum-dock launch <id> [--persona <name> | --ask]  # ask the TCB to launch app <id>
//!
//! Apps dir: FORUM_APPS_DIR (default /var/lib/atrium/apps). Pins: §6.1.

use std::collections::HashSet;
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forum_dock::apps::{self, Move};
use forum_dock::apps_view::{self, Action, AppsState, AppsView, Note};
use forum_dock::dock_view::{reserve_pt, DockAction, DockState, DockView};
use forum_dock::launch::{self, PreLaunch};
use forum_dock::persona_view::{self, Answer, PersonaView, Question};
use forum_dock::pins::{self, Pins};
use forum_dock::{AppEntry, APPS_DIR};

use fresco_protocol::{WindowHints, WmRole};
use pergola::window::{SizeSpec, StepResult, Window, WindowDesc};
use pergola::{App, Mutable};

fn apps_dir() -> PathBuf {
    std::env::var("FORUM_APPS_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(APPS_DIR))
}

// ── state shared by the dock's threads ───────────────────────────────────────

/// What both windows show. `version` moves on every change, so a window
/// redraws only when something it shows changed.
#[derive(Debug, Default)]
struct Shared {
    apps: Vec<AppEntry>,
    pins: Pins,
    running: HashSet<u32>,
    apps_open: bool,
    version: u64,
}

type SharedRef = Arc<Mutex<Shared>>;

impl Shared {
    fn bump(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    fn dock_state(&self) -> DockState {
        DockState { apps: self.apps.clone(), pins: self.pins.clone(), running: self.running.clone(), apps_open: self.apps_open }
    }
}

/// Fetch the catalog into `shared`. Says what happened: "nothing is
/// installed" and "I could not see any apps" look the same on screen.
fn refresh_catalog(shared: &SharedRef) {
    let (all, src) = forum_dock::catalog_resolved(&apps_dir());
    match (&src, all.is_empty()) {
        (forum_dock::CatalogSource::Unavailable, _) => eprintln!(
            "forum-dock: NO CATALOG — portcullisd is not reachable and {} is not readable \
             (a jailed dock needs the `app-launch` capability)",
            apps_dir().display()
        ),
        (_, true) => eprintln!("forum-dock: catalog from {src:?} is empty — no apps installed"),
        (_, false) => {}
    }
    let mut listed = apps::listed(&all);
    // The person's workspaces after the apps (homes.md §5).
    match launch::workspaces() {
        Ok(ws) => listed.extend(ws.iter().map(forum_dock::workspace_entry)),
        Err(e) => eprintln!("forum-dock: no workspaces listed: {e}"),
    }
    let mut s = shared.lock().unwrap();
    if s.apps != listed {
        eprintln!("forum-dock: {} app(s) from {src:?}, {} with their own icon", listed.len(), listed.iter().filter(|a| a.vector.is_some()).count());
        s.apps = listed;
        s.bump();
    }
}

// ── launching ────────────────────────────────────────────────────────────────

/// What a launch came to, for the Applications view.
#[derive(Debug)]
enum Report {
    /// Started, or its window was focused: the view's job is done.
    Done(String),
    /// Stays up, saying this.
    Note(String, String),
}

/// From the dock and the key thread to the Applications view's thread.
#[derive(Debug)]
enum ToApps {
    Toggle,
    Launched(Report),
}

/// Ask portcullisd to launch `id` on a thread of its own (the request stays
/// open while the app runs), and tell the Applications view how it went.
fn start_launch(id: String, name: String, to_apps: Sender<ToApps>) {
    std::thread::spawn(move || {
        // The app gets the dock's stdio: the session's log.
        let stdio = [io::stdin().as_raw_fd(), io::stdout().as_raw_fd(), io::stderr().as_raw_fd()];
        let persona = match choose_persona(&id, &name) {
            Ok(Some(p)) => p,
            Ok(None) => {
                let _ = to_apps.send(ToApps::Launched(Report::Done(id)));
                return;
            }
            Err(e) => {
                let _ = to_apps.send(ToApps::Launched(Report::Note(id, format!("{name} could not ask which persona: {e}"))));
                return;
            }
        };
        match launch::request_in(&id, persona.as_deref(), stdio) {
            PreLaunch::Started(run) => {
                eprintln!("forum-dock: launched '{id}'");
                let _ = to_apps.send(ToApps::Launched(Report::Done(id.clone())));
                match run.wait() {
                    Ok(code) => eprintln!("forum-dock: '{id}' exited (code {code:?})"),
                    Err(e) => eprintln!("forum-dock: '{id}': {e}"),
                }
            }
            PreLaunch::AlreadyRunning { jid, uid } => {
                eprintln!("forum-dock: '{id}' is already running (jid {jid})");
                match launch::focus_running(uid) {
                    Ok(Some(sid)) => eprintln!("forum-dock: asked the WM to focus its surface {sid}"),
                    Ok(None) => eprintln!("forum-dock: '{id}' has no window to focus"),
                    Err(e) => eprintln!("forum-dock: could not focus '{id}': {e}"),
                }
                let _ = to_apps.send(ToApps::Launched(Report::Done(id)));
            }
            PreLaunch::NeedsApproval(delta) => {
                eprintln!("forum-dock: '{id}' needs the user's approval: {}", delta.join("; "));
                let text = format!("{name} needs your approval before it can run: {}", delta.join(", "));
                let _ = to_apps.send(ToApps::Launched(Report::Note(id, text)));
            }
            PreLaunch::Failed(why) => {
                eprintln!("forum-dock: launch of '{id}': {why}");
                let _ = to_apps.send(ToApps::Launched(Report::Note(id, format!("{name} could not start: {why}"))));
            }
        }
    });
}

/// Which persona to open `id` in (personas.md §3), asked only when there is
/// a choice: `Ok(Some(None))` Personal or nothing to ask, `Ok(None)` the
/// person said no.
fn choose_persona(id: &str, name: &str) -> Result<Option<Option<String>>, String> {
    let (list, last, spans) = match launch::personas(id) {
        Ok(v) => v,
        // An older portcullisd, or none: the launch says what's wrong.
        Err(e) => {
            eprintln!("forum-dock: no persona list for '{id}' ({e}); Personal");
            return Ok(Some(None));
        }
    };
    let Some(q) = Question::new(name, list, last.as_deref(), spans) else { return Ok(Some(None)) };
    match ask_persona(q) {
        Ok(Answer::In(p)) => {
            eprintln!("forum-dock: '{id}' in persona {p}");
            Ok(Some(Some(p)))
        }
        Ok(Answer::Cancel) => {
            eprintln!("forum-dock: '{id}': no persona chosen, not opened");
            Ok(None)
        }
        Err(e) => {
            eprintln!("forum-dock: '{id}': the persona question failed: {e}");
            Err(e.to_string())
        }
    }
}

/// Ask which persona, in a small dialog of its own, until answered. Losing
/// focus is no. `FORUM_DOCK_PERSONA_AUTO=<name>` answers it once drawn (for
/// checks without a person).
fn ask_persona(q: Question) -> io::Result<Answer> {
    let (w, h) = persona_view::size(q.personas.len());
    let mut desc = WindowDesc::new(q.title(), SizeSpec::Fixed(w.ceil() as u32, h.ceil() as u32));
    desc.hints = WindowHints { role: Some(WmRole::Dialog), server_decorations: Some(false), ..Default::default() };
    let mut win = Window::open(&desc)?;
    let names: Vec<&str> = q.personas.iter().map(|p| p.name.as_str()).collect();
    eprintln!("forum-dock: persona question for {}: [{}], {} preselected, {:.0}x{:.0} pt", q.app_name, names.join(", "), q.personas[q.selected].name, w, h);
    let auto = std::env::var("FORUM_DOCK_PERSONA_AUTO").ok();
    let dirty = Arc::new(AtomicBool::new(true));
    let qm = Mutable::with_dirty(q, dirty.clone());
    let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
    let picks: std::rc::Rc<std::cell::RefCell<Vec<usize>>> = Default::default();
    let mut app = App::new_with_flag(PersonaView { q: qm.clone(), size: size.clone(), picks: picks.clone() }, dirty);
    win.paint(&mut app)?;
    if let Some(a) = auto {
        let mut q = qm.get_cloned();
        if let Some(i) = q.personas.iter().position(|p| p.name == a) {
            q.pick(i);
            qm.set(q);
        }
    }
    let mut had_focus = false;
    loop {
        if let Some(a) = qm.get_cloned().answer {
            return Ok(a);
        }
        let ev = match win.step(&mut app, Some(Duration::from_millis(100)))? {
            StepResult::Closed => return Ok(Answer::Cancel),
            StepResult::Event(ev) => Some(ev),
            StepResult::Timeout => None,
        };
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        use fresco_client::Event as E;
        match ev {
            Some(E::Key { hid_usage, pressed: true, modifiers, .. }) if modifiers & 0x88 == 0 => {
                let mut q = qm.get_cloned();
                q.key(hid_usage);
                qm.set(q);
            }
            Some(E::FocusChanged { window_id, gained, .. }) if window_id == win.id() => {
                if gained {
                    had_focus = true;
                } else if had_focus {
                    return Ok(Answer::Cancel);
                }
            }
            _ => {}
        }
        for i in std::mem::take(&mut *picks.borrow_mut()) {
            let mut q = qm.get_cloned();
            q.pick(i);
            qm.set(q);
        }
        win.paint(&mut app)?;
    }
}

fn name_of(shared: &SharedRef, id: &str) -> String {
    shared.lock().unwrap().apps.iter().find(|a| a.id == id).map(|a| a.name.clone()).unwrap_or_else(|| id.to_string())
}

fn toggle_pin(shared: &SharedRef, id: &str) {
    let mut s = shared.lock().unwrap();
    let apps = s.apps.clone();
    s.pins.toggle(&apps, id);
    s.bump();
    if let Some(p) = pins::path() {
        match s.pins.save(&p) {
            Ok(()) => eprintln!("forum-dock: pins saved to {}: {:?}", p.display(), s.pins.ids),
            Err(e) => eprintln!("forum-dock: could not save pins to {} ({e}); they hold until logout", p.display()),
        }
    }
}

// ── the Applications view (§6.1) ─────────────────────────────────────────────

const KEY_ENTER: u16 = 0x28;
const KEY_ESCAPE: u16 = 0x29;
const KEY_D: u16 = 0x07;
const KEY_PAGE_UP: u16 = 0x4B;
const KEY_PAGE_DOWN: u16 = 0x4E;
const KEY_RIGHT: u16 = 0x4F;
const KEY_LEFT: u16 = 0x50;
const KEY_DOWN: u16 = 0x51;
const KEY_UP: u16 = 0x52;
const KEY_KP_ENTER: u16 = 0x58;
/// How often the open view looks for launch results and Super+A.
const APPS_POLL: Duration = Duration::from_millis(50);
/// Wheel travel that turns a page, and the pause after a turn.
const WHEEL_PAGE: f32 = 60.0;
const WHEEL_REST: Duration = Duration::from_millis(350);

/// The view's thread: waits closed, opens on a toggle, runs until closed.
fn apps_thread(rx: Receiver<ToApps>, to_self: Sender<ToApps>, shared: SharedRef) {
    loop {
        match rx.recv() {
            Ok(ToApps::Toggle) => {
                if let Err(e) = run_apps_view(&rx, &to_self, &shared) {
                    eprintln!("forum-dock: Applications view: {e}");
                }
                let mut s = shared.lock().unwrap();
                s.apps_open = false;
                s.bump();
            }
            // A launch from the dock while the view is closed: logged already.
            Ok(ToApps::Launched(_)) => {}
            Err(_) => return,
        }
    }
}

fn run_apps_view(rx: &Receiver<ToApps>, to_self: &Sender<ToApps>, shared: &SharedRef) -> io::Result<()> {
    // New installs show up each time it opens.
    refresh_catalog(shared);
    let mut desc = WindowDesc::new("Applications", SizeSpec::FullScreen);
    desc.hints = WindowHints { role: Some(WmRole::Dialog), server_decorations: Some(false), ..Default::default() };
    let mut win = Window::open(&desc)?;
    {
        let mut s = shared.lock().unwrap();
        s.apps_open = true;
        s.bump();
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let (state0, mut seen) = {
        let s = shared.lock().unwrap();
        (AppsState { apps: s.apps.clone(), pins: s.pins.clone(), ..Default::default() }, s.version)
    };
    // Opened at the screen's size: that is also its minimum, so forum-wm's
    // dialog rule gives it the whole work area (forum.md §6.1).
    let mut view = AppsView::new(state0, (win.width, win.height), dirty.clone());
    view.min = (win.width, win.height);
    let (state, query, size, actions) = (view.state.clone(), view.query.clone(), view.size.clone(), view.actions.clone());
    let mut app = App::new_with_flag(view, dirty);
    {
        let l = apps_view::layout(win.width, win.height);
        eprintln!("forum-dock: Applications view open: {} app(s), {}x{} per page, {:.0}x{:.0} pt",
                  state.get_cloned().apps.len(), l.grid.cols, l.grid.rows, win.width, win.height);
    }
    let mut last_query = String::new();
    let mut wheel = 0.0f32;
    let mut wheel_rest = Instant::now();
    let mut had_focus = false;
    let mut trace = String::new();
    win.paint(&mut app)?;

    loop {
        let ev = match win.step(&mut app, Some(APPS_POLL))? {
            StepResult::Closed => return Ok(()),
            StepResult::Event(ev) => Some(ev),
            StepResult::Timeout => None,
        };
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        let lay = apps_view::layout(win.width, win.height);
        let matches = || -> Vec<usize> {
            let s = state.get_cloned();
            apps::filter(&s.apps, query.get_cloned().text())
        };
        let select = |m: Move| {
            let n = matches().len();
            let mut s = state.get_cloned();
            s.selected = apps::moved(s.selected, n, lay.grid, m);
            state.set(s);
        };
        let mut close = false;
        use fresco_client::Event as E;
        match ev {
            // Ctrl+D pins or unpins the selected app (the keyboard's pin button).
            Some(E::Key { hid_usage: KEY_D, pressed: true, modifiers, .. })
                if modifiers & 0x11 != 0 && modifiers & 0x88 == 0 => {
                let s = state.get_cloned();
                if let Some(&i) = matches().get(s.selected) {
                    actions.borrow_mut().push(Action::TogglePin(s.apps[i].id.clone()));
                }
            }
            Some(E::Key { hid_usage, pressed: true, modifiers, .. }) if modifiers & 0x88 == 0 => match hid_usage {
                KEY_ESCAPE => {
                    if query.get_cloned().text().is_empty() {
                        close = true;
                    } else {
                        query.set(pergola::text_edit::TextEdit::new(""));
                    }
                }
                KEY_ENTER | KEY_KP_ENTER => {
                    let s = state.get_cloned();
                    if let Some(&i) = matches().get(s.selected) {
                        actions.borrow_mut().push(Action::Launch(s.apps[i].id.clone()));
                    }
                }
                KEY_LEFT => select(Move::Left),
                KEY_RIGHT => select(Move::Right),
                KEY_UP => select(Move::Up),
                KEY_DOWN => select(Move::Down),
                KEY_PAGE_UP => select(Move::PageUp),
                KEY_PAGE_DOWN => select(Move::PageDown),
                _ => {}
            },
            // Another button on a tile selects it (its pin button shows),
            // without launching.
            Some(E::PointerButton { pressed: true, button, x, y, .. }) if button != 1 => {
                let n = matches().len();
                let page = lay.grid.page_of(state.get_cloned().selected.min(n.saturating_sub(1)));
                if let Some(i) = lay.item_at(page, n, x, y) {
                    let mut s = state.get_cloned();
                    s.selected = i;
                    state.set(s);
                }
            }
            Some(E::PointerScroll { dy, .. }) => {
                if wheel_rest.elapsed() >= WHEEL_REST {
                    wheel += dy;
                    if wheel.abs() >= WHEEL_PAGE {
                        select(if wheel > 0.0 { Move::PageDown } else { Move::PageUp });
                        wheel = 0.0;
                        wheel_rest = Instant::now();
                    }
                }
            }
            Some(E::FocusChanged { window_id, gained, .. }) if window_id == win.id() => {
                // Gone to something else: it isn't wanted any more.
                if gained {
                    had_focus = true;
                } else if had_focus {
                    close = true;
                }
            }
            _ => {}
        }

        // Typing: the best match is selected, and an old note goes.
        let q = query.get_cloned().text().to_string();
        if q != last_query {
            let mut s = state.get_cloned();
            s.selected = 0;
            s.note = None;
            // A name no workspace has offers New workspace.
            s.apps.retain(|a| !a.id.starts_with(forum_dock::NEW_WORKSPACE));
            if let Some(e) = forum_dock::new_workspace_entry(&s.apps, &q) {
                s.apps.push(e);
            }
            last_query = q;
            state.set(s);
        }

        for a in std::mem::take(&mut *actions.borrow_mut()) {
            match a {
                Action::Close => close = true,
                Action::TogglePin(id) => toggle_pin(shared, &id),
                Action::Console(id, token) => {
                    let name = name_of(shared, &id);
                    let tx = to_self.clone();
                    std::thread::spawn(move || {
                        // New workspace: made, then its terminal, on the same press.
                        let r = match id.strip_prefix(forum_dock::NEW_WORKSPACE) {
                            Some(ws) => launch::new_workspace(ws).and_then(|_| {
                                eprintln!("forum-dock: workspace {ws} made");
                                launch::console(&format!("{}{ws}", forum_dock::WORKSPACE), token)
                            }),
                            None => launch::console(&id, token),
                        };
                        match &r {
                            Ok(title) => eprintln!("forum-dock: console in '{id}': {title}"),
                            Err(e) => eprintln!("forum-dock: console in '{id}': {e}"),
                        }
                        if let Err(e) = r {
                            let _ = tx.send(ToApps::Launched(Report::Note(id, format!("No terminal for {name}: {e}"))));
                        }
                    });
                    // The terminal's window taking focus closes the view.
                }
                // Enter on a workspace: its terminal, on the key press.
                Action::Launch(id) if forum_dock::is_workspace(&id) => {
                    let token = pergola::clipboard::current_activation().unwrap_or([0; 16]);
                    actions.borrow_mut().push(Action::Console(id, token));
                }
                Action::Launch(id) => {
                    let mut s = state.get_cloned();
                    if s.launching.is_none() {
                        s.launching = Some(id.clone());
                        s.note = None;
                        state.set(s);
                        start_launch(id.clone(), name_of(shared, &id), to_self.clone());
                    }
                }
            }
        }

        loop {
            match rx.try_recv() {
                Ok(ToApps::Toggle) => close = true,
                Ok(ToApps::Launched(Report::Done(id))) => {
                    if state.get_cloned().launching.as_deref() == Some(id.as_str()) {
                        close = true;
                    }
                }
                Ok(ToApps::Launched(Report::Note(id, text))) => {
                    let mut s = state.get_cloned();
                    if s.launching.as_deref() == Some(id.as_str()) {
                        s.launching = None;
                    }
                    s.note = Some(Note { text, error: true });
                    state.set(s);
                }
                Err(_) => break,
            }
        }
        if close {
            eprintln!("forum-dock: Applications view closed");
            return Ok(());
        }

        {
            let s = state.get_cloned();
            let m = apps::filter(&s.apps, query.get_cloned().text());
            let line = format!(
                "query={:?} matches={} selected={} page={} note={}",
                query.get_cloned().text(),
                m.len(),
                m.get(s.selected).map_or("-", |&i| s.apps[i].id.as_str()),
                lay.grid.page_of(s.selected),
                s.note.as_ref().map_or("-", |n| n.text.as_str())
            );
            if line != trace {
                eprintln!("forum-dock: apps: {line}");
                trace = line;
            }
        }
        // Pins changed (here or in the dock).
        let fresh = {
            let s = shared.lock().unwrap();
            (s.version != seen).then(|| (s.version, s.apps.clone(), s.pins.clone()))
        };
        if let Some((v, apps_now, pins_now)) = fresh {
            seen = v;
            let mut s = state.get_cloned();
            if s.apps != apps_now || s.pins != pins_now {
                s.apps = apps_now;
                s.pins = pins_now;
                state.set(s);
            }
        }
        win.paint(&mut app)?;
    }
}

/// Wait for Super+A from forum-wm and toggle the view, for as long as the
/// dock lives. forum-wm going away is retried, slowly.
fn key_thread(to_apps: Sender<ToApps>) {
    let mut said = false;
    loop {
        match launch::wait_apps_key() {
            Ok(()) => {
                said = false;
                if to_apps.send(ToApps::Toggle).is_err() {
                    return;
                }
            }
            Err(e) => {
                if !said {
                    eprintln!("forum-dock: no Super+A from forum-wm ({e}); retrying");
                    said = true;
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

// ── the dock ─────────────────────────────────────────────────────────────────

/// How often the dock looks for a change it shows (the Applications view
/// opened or closed, a pin): a cheap version check, no drawing unless it moved.
const DOCK_CHECK: Duration = Duration::from_millis(300);
/// How often it asks forum-wm which apps have windows (as the bar does).
const RUNNING_POLL: Duration = Duration::from_secs(2);

fn poll_running(shared: &SharedRef) {
    let Ok(surfaces) = launch::surfaces() else { return };
    let running: HashSet<u32> = surfaces.iter().map(|s| s.owner_uid).filter(|&u| u != 0).collect();
    let unknown = {
        let mut s = shared.lock().unwrap();
        let known: HashSet<u32> = s.apps.iter().filter_map(|a| a.uid).collect();
        if s.running != running {
            s.running = running.clone();
            s.bump();
            let ids: Vec<&str> = s.apps.iter().filter(|a| a.uid.is_some_and(|u| running.contains(&u))).map(|a| a.id.as_str()).collect();
            eprintln!("forum-dock: running: [{}]", ids.join(", "));
        }
        // A uid no app has yet: an app ran for the first time and the
        // registry has its uid now.
        running.iter().any(|u| !known.contains(u))
    };
    if unknown {
        refresh_catalog(shared);
    }
}

fn render_dock() -> io::Result<()> {
    let pin_file = pins::path();
    let pins = Pins::load(pin_file.as_deref());
    match (&pin_file, pins.from_file) {
        (Some(p), true) => eprintln!("forum-dock: pins from {}: {:?}", p.display(), pins.ids),
        (p, _) => eprintln!("forum-dock: default pins (nothing at {})",
                            p.as_ref().map_or("-".into(), |p| p.display().to_string())),
    }
    let shared: SharedRef = Arc::new(Mutex::new(Shared { pins, ..Default::default() }));
    refresh_catalog(&shared);

    // The dock paints the full-screen wallpaper and the launcher row, so it is
    // the background surface: the WM places it behind everything, and the row
    // in the bottom strip it reserves stays visible.
    let mut desc = WindowDesc::new("forum-dock", SizeSpec::FullScreen);
    desc.hints = WindowHints { role: Some(WmRole::Background), ..Default::default() };
    let mut win = Window::open(&desc)?;

    let (to_apps, apps_rx) = channel();
    {
        let (shared, to_self) = (shared.clone(), to_apps.clone());
        std::thread::Builder::new().name("forum-dock-apps".into()).spawn(move || apps_thread(apps_rx, to_self, shared))?;
    }
    {
        let to_apps = to_apps.clone();
        std::thread::Builder::new().name("forum-dock-keys".into()).spawn(move || key_thread(to_apps))?;
    }

    let dirty = Arc::new(AtomicBool::new(true));
    let (state, mut seen) = {
        let s = shared.lock().unwrap();
        (Mutable::with_dirty(s.dock_state(), dirty.clone()), s.version)
    };
    let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
    let actions = Default::default();
    let view = DockView { state: state.clone(), size: size.clone(), actions: std::rc::Rc::clone(&actions) };
    let mut app = App::new_with_flag(view, dirty);
    let mut reserved = 0u32;
    let mut last_poll = Instant::now() - RUNNING_POLL;
    let mut apps_button = (0.0f32, 0.0f32);
    seen = seen.wrapping_sub(1); // log the button's place at the first check
    win.paint(&mut app)?;

    // ★ Stay up: the dock IS the desktop background, so exiting takes the
    // desktop with it.
    loop {
        if let StepResult::Closed = win.step(&mut app, Some(DOCK_CHECK))? {
            return Ok(());
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        // Tell the WM how tall the dock draws, so a maximized window ends
        // above it (resolution-independence.md §7.2); it grows with the text.
        let want = reserve_pt();
        if want != reserved {
            match launch::reserve_bottom(want) {
                Ok(()) => reserved = want,
                Err(e) => {
                    // Without forum-wm (a bare dev session) there is nobody to tell.
                    eprintln!("forum-dock: could not reserve {want} pt for the dock: {e}");
                    reserved = want;
                }
            }
        }
        for a in std::mem::take(&mut *actions.borrow_mut()) {
            match a {
                DockAction::ToggleApps => {
                    let _ = to_apps.send(ToApps::Toggle);
                }
                DockAction::Launch(id) => start_launch(id.clone(), name_of(&shared, &id), to_apps.clone()),
            }
        }
        if last_poll.elapsed() >= RUNNING_POLL {
            last_poll = Instant::now();
            poll_running(&shared);
        }
        let fresh = {
            let s = shared.lock().unwrap();
            (s.version != seen).then(|| (s.version, s.dock_state()))
        };
        if let Some((v, ds)) = fresh {
            seen = v;
            // Where the Applications button is, for checks that press it.
            let p = forum_dock::dock_view::panel(&ds.items(), win.width, win.height);
            let b = (p.x() + pergola::theme::size::em(forum_dock::dock_view::PAD + forum_dock::dock_view::TILE / 2.0), p.y() + p.h() / 2.0);
            if b != apps_button {
                apps_button = b;
                eprintln!("forum-dock: Applications button at {:.0},{:.0}", b.0, b.1);
            }
            state.set(ds);
            win.paint(&mut app)?;
        }
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => render_dock(),
        Some("list") => {
            let (apps, src) = forum_dock::catalog_resolved(&apps_dir());
            if apps.is_empty() {
                println!("forum-dock: no apps ({src:?}) — daemon unreachable or nothing installed");
            }
            for a in apps {
                let desc = a.description.unwrap_or_default();
                println!("  {:<28} {:<20} {}", a.id, a.name, desc);
            }
            Ok(())
        }
        Some("launch") => {
            let Some(id) = args.get(1) else {
                eprintln!("usage: forum-dock launch <app-id> [--persona <name> | --ask]");
                std::process::exit(2);
            };
            let asked;
            let persona = match (args.get(2).map(String::as_str), args.get(3)) {
                (Some("--persona"), Some(p)) => Some(p.as_str()),
                // As the dock would: ask which persona when there's a choice.
                (Some("--ask"), None) => match choose_persona(id, id) {
                    Ok(Some(p)) => {
                        asked = p;
                        asked.as_deref()
                    }
                    Ok(None) => return Ok(()),
                    Err(e) => {
                        eprintln!("forum-dock: {e}");
                        std::process::exit(1);
                    }
                },
                (None, _) => None,
                _ => {
                    eprintln!("usage: forum-dock launch <app-id> [--persona <name> | --ask]");
                    std::process::exit(2);
                }
            };
            let stdio = [io::stdin().as_raw_fd(), io::stdout().as_raw_fd(), io::stderr().as_raw_fd()];
            match launch::request_in(id, persona, stdio) {
                PreLaunch::Started(run) => match run.wait() {
                    Ok(code) => {
                        println!("forum-dock: '{id}' exited (code {code:?})");
                        Ok(())
                    }
                    Err(e) => {
                        eprintln!("forum-dock: '{id}' {e}");
                        std::process::exit(1);
                    }
                },
                PreLaunch::AlreadyRunning { jid, uid } => {
                    println!("forum-dock: '{id}' is already running (jid {jid})");
                    // Ack means the WM ACCEPTED the intent, not that focus moved.
                    match launch::focus_running(uid) {
                        Ok(Some(sid)) => println!("forum-dock: asked the WM to focus its surface {sid}"),
                        Ok(None) => println!("forum-dock: it has no surface to focus"),
                        Err(e) => eprintln!("forum-dock: could not focus it: {e}"),
                    }
                    Ok(())
                }
                PreLaunch::NeedsApproval(delta) => {
                    eprintln!("forum-dock: '{id}' needs the user's approval:");
                    for line in delta {
                        eprintln!("  - {line}");
                    }
                    std::process::exit(1);
                }
                PreLaunch::Failed(why) => {
                    eprintln!("forum-dock: launch of '{id}': {why}");
                    std::process::exit(1);
                }
            }
        }
        Some(other) => {
            eprintln!("forum-dock: unknown command '{other}' (try: list | launch <id>)");
            std::process::exit(2);
        }
    }
}
