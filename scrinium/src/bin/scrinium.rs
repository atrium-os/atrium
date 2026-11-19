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
//!   SCRINIUM_ARCA_SOCKET    [/atrium/sockets/arca/arcad.sock] saved network shares
//!   SCRINIUM_PERSONA        [personal] whose window this is: the sticks it
//!                           seals are this persona's (tessera-encryption.md §7)
//!
//! "Connect to server" (network-shares.md §5.2): dispensatord learns the
//! server's key in a jail of its own, the user accepts it, types the
//! password into Arca's field (a Limen `credential` slot), Scrinium has
//! Arca save that entry for exactly this server and key, and dispensatord
//! starts the share server, which gets the password from Arca itself.

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
use scrinium::view::{take_key, with, ScriniumView};

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
    cred: pergola::widgets::CredentialField,
    cred_state: Mutable<pergola::widgets::CredentialState>,
    rcode: pergola::widgets::CredentialField,
    rcode_state: Mutable<pergola::widgets::CredentialState>,
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
        let (cred, cred_state) = (view.cred.clone(), view.cred_state.clone());
        let (rcode, rcode_state) = (view.rcode.clone(), view.rcode_state.clone());
        let app = App::new_with_flag(view, dirty);
        Pane { cred, cred_state, rcode, rcode_state, win, app, state, field, size, mods, gen: u64::MAX, trace: std::env::var_os("SCRINIUM_TRACE").map(|_| String::new()) }
    }

    /// One turn: an event (or `wait`), then the places if they changed.
    /// `false` once the window is closed.
    fn step(&mut self, wait: Duration, places: &SharedPlaces, who: &str) -> bool {
        // The browser hears a key before the app does: an arrow that moved
        // the cursor must not also scroll the list (take_key). A key for
        // one of our pop-ups (an open menu, which takes the keys) is the
        // menu's alone: an arrow there must not also move the file
        // selection under it.
        let (id, state, field) = (self.win.id(), &self.state, &self.field);
        let first = |ev: &fresco_client::Event| match ev {
            fresco_client::Event::Key { window_id, hid_usage, pressed, activation, .. } if *window_id == id => {
                let handled = translate(ev).is_some_and(|k| take_key(state, field, &k));
                // take_key can choose a file even when it lets Enter continue
                // through the window's normal dispatch.
                if *pressed && matches!(*hid_usage, 0x28 | 0x58) {
                    with(state, |browser| {
                        if let Some(Outcome::Workbook { activation: token, .. }) = browser.outcome.as_mut() {
                            *token = *activation;
                        }
                    });
                }
                handled
            }
            _ => false,
        };
        match self.win.step_with(&mut self.app, Some(wait), first) {
            Ok(StepResult::Closed) => return false,
            Ok(StepResult::Event(ev)) => {
                if let fresco_client::Event::PointerButton { pressed: true, modifiers, .. } = &ev {
                    *self.mods.lock().unwrap() = Mods { shift: modifiers & 0x22 != 0, ctrl: modifiers & 0x11 != 0 };
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
                "place={} path=/{} entries=[{}] selected=[{}] notice={} connect={} servers=[{}] places=[{}] unreadable={} look={} open={} seals=[{}] stick={} names=[{}]",
                b.place().map_or("-", |p| p.id.as_str()),
                b.path.join("/"),
                b.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(","),
                b.selected_names().join(","),
                b.notice.as_ref().map_or("-".to_string(), |n| n.text.clone()),
                connect_trace(&b),
                b.saved.iter().map(|s| s.addr.text()).collect::<Vec<_>>().join(","),
                b.places.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().join(","),
                b.unreadable.as_deref().unwrap_or("-"),
                look_trace(&b),
                open_trace(&b),
                seals_trace(&b),
                stick_trace(&b),
                b.places.iter().filter(|p| p.seal != Default::default()).map(|p| format!("{}={}", p.id, p.label)).collect::<Vec<_>>().join(","),
            );
            if self.trace.as_deref() != Some(&line) {
                eprintln!("scrinium: {who}: {line}");
                self.trace = Some(line);
            }
        }
        true
    }
}

/// Quick look, for the trace: `NAME:text:LINES[+]`, `NAME:embed:ROLE`,
/// `NAME:info[:WHY]` or `NAME:reading`.
fn look_trace(b: &scrinium::browser::Browser) -> String {
    use scrinium::browser::LookBody;
    let Some(l) = &b.look else { return "-".into() };
    match &l.body {
        None => format!("{}:reading", l.name),
        Some(LookBody::Text { text, cut, .. }) => format!("{}:text:{}{}", l.name, text.lines().count(), if *cut { "+" } else { "" }),
        Some(LookBody::Embed { role, .. }) => format!("{}:embed:{role}", l.name),
        Some(LookBody::Info { why, .. }) => format!("{}:info{}", l.name, why.as_ref().map_or(String::new(), |w| format!(":{w}"))),
    }
}

/// The open sheet, for the trace: `choose:APP,APP` or `question:MIME|text`.
fn open_trace(b: &scrinium::browser::Browser) -> String {
    use scrinium::browser::OpenSheet;
    match &b.open_sheet {
        None => "-".into(),
        Some(OpenSheet::Choose { apps, always, .. }) => format!("choose:{}{}", apps.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(","), if *always { ":always" } else { "" }),
        Some(OpenSheet::Question { actual, text, .. }) => format!("question:{}{}", actual.as_ref().map_or("-", |a| a.0.as_str()), if *text { "|text" } else { "" }),
    }
}

/// The Connect sheet's step, or the key-changed stop, for the trace.
/// Sealed sticks, for the trace: `PART:PERSONA[+locked][+recovery][+older]`.
fn seals_trace(b: &scrinium::browser::Browser) -> String {
    b.places
        .iter()
        .filter(|p| p.seal != Default::default())
        .map(|p| {
            let s = &p.seal;
            format!("{}:{}{}{}{}", p.id, if s.persona.is_empty() { "-" } else { &s.persona }, if s.locked { "+locked" } else { "" }, if s.recovery { "+recovery" } else { "" }, if s.older { "+older" } else { "" })
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A stick's sheet, for the trace: `ASK[:busy|:armed][(error)]`.
fn stick_trace(b: &scrinium::browser::Browser) -> String {
    use scrinium::browser::StickAsk;
    let Some(sh) = &b.stick_sheet else { return "-".into() };
    let ask = match &sh.ask {
        StickAsk::Format { persona, recovery } => format!("format:{persona}{}", if *recovery { "+code" } else { "" }),
        StickAsk::Code { .. } => "code-shown".into(),
        StickAsk::Locked => "locked".into(),
        StickAsk::Recovery { keep } => format!("recovery:{}", if *keep { "keep" } else { "once" }),
        StickAsk::Older => "older".into(),
        StickAsk::MakeCurrent => "make-current".into(),
        StickAsk::Forget { recovery } => format!("forget{}", if *recovery { "+code" } else { "" }),
        StickAsk::RemoveCode => "remove-code".into(),
    };
    let state = if sh.busy { ":busy" } else if sh.armed.is_some() { ":armed" } else { "" };
    match &sh.error {
        Some(e) => format!("{ask}{state}({e})"),
        None => format!("{ask}{state}"),
    }
}

fn connect_trace(b: &scrinium::browser::Browser) -> String {
    use scrinium::connect::Step;
    if let Some(k) = &b.key_changed {
        return format!("key-changed:{}", k.now);
    }
    let Some(f) = &b.connect else { return "-".into() };
    let step = match &f.step {
        Step::Address => "address".to_string(),
        Step::Checking(_) => "checking".into(),
        Step::Key { fingerprint, .. } => format!("key:{fingerprint}"),
        Step::Password { filled, .. } => format!("password:{}", if f.armed.is_some() { "armed" } else if *filled { "filled" } else { "empty" }),
        Step::Connecting(_) => "connecting".into(),
    };
    match &f.error {
        Some(e) => format!("{step}({e})"),
        None => step,
    }
}

// -- the picker ----------------------------------------------------------------

/// From the limend thread to a picker's thread.
enum ToPane {
    Open(Filter),
    /// The asking app's persona (limen `ParentPersona`; `None`: Personal).
    Persona(Option<String>),
    Failed(String),
    Close,
}

/// From a picker's thread to the limend thread.
enum FromPane {
    Ready(EmbedId, [u8; 16]),
    Picked(EmbedId, Outcome),
    Gone(EmbedId),
    Published(EmbedId, std::io::Result<(std::fs::File, String, [u8; 16])>),
    Committed(EmbedId, std::io::Result<()>),
}

/// A picker's window, on its own thread: frescod keeps it hidden until
/// limend binds it into the app's slot.
fn picker_pane(embed: EmbedId, app_id: String, fresco: String, places: SharedPlaces, rx: Receiver<ToPane>, tx: Sender<FromPane>) {
    let who = format!("picker {embed}");
    let desc = WindowDesc {
        hints: fresco_protocol::WindowHints { embed_pending: true, ..Default::default() },
        ..WindowDesc::new("Choose", SizeSpec::Fixed(640, 440))
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
    let mut opened = false;
    let mut ready = false;
    let mut sent = false;
    loop {
        if !pane.step(Duration::from_millis(30), &places, &who) {
            break;
        }
        if opened && !ready {
            if let Some(token) = pane.win.embed_token() {
                ready = true;
                let _ = tx.send(FromPane::Ready(embed, token));
            }
        }
        match rx.try_recv() {
            Ok(ToPane::Open(f)) => {
                opened = true;
                let list = places.lock().unwrap().list.clone();
                with(&pane.state, |b| {
                    b.mode = match &b.mode {
                        Mode::Picker { app, .. } => Mode::Picker { filter: f, app: app.clone() },
                        m => m.clone(),
                    };
                    b.set_places(list);
                });
            }
            Ok(ToPane::Persona(p)) => {
                let list = places.lock().unwrap().list.clone();
                with(&pane.state, |b| {
                    b.persona = p.unwrap_or_else(|| "personal".into());
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
    sheets: bool,
    write: bool,
    staged: Option<std::fs::File>,
    requested: bool,
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
        Outcome::Workbook { name, path, write, activation } => {
            use std::os::unix::fs::FileExt;
            if !e.sheets {
                return Err("Only Sheets can open a workbook this way".into());
            }
            let open = |write| std::fs::OpenOptions::new().read(true).write(write)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path);
            let (file, write) = if write {
                open(true).map(|f| (f, true)).or_else(|_| open(false).map(|f| (f, false)))
            } else {
                open(false).map(|f| (f, false))
            }.map_err(|err| format!("Couldn't open “{name}”: {}", scrinium::browser::err_text(&err)))?;
            if !file.metadata().map_err(|err| err.to_string())?.is_file() {
                return Err(format!("“{name}” is not a file"));
            }
            let mut head = [0; atrium_types::HEAD];
            let n = file.read_at(&mut head, 0).map_err(|err| err.to_string())?;
            let bytes = &head[..n];
            let detected = atrium_types::detect(&name, bytes);
            let ty = detected.known().ok_or_else(|| detected.question().unwrap_or_else(|| "Unknown workbook format".into()))?;
            let mime = if ty.mime == "text/plain" { "text/csv" } else { ty.mime };
            let write = write && mime != "application/vnd.ms-excel";
            scrinium::open::open_workbook(e.parent_uid, mime, &name, write, activation, &file, &path)?;
            child.emit(embed, ToParent::Done { files: 0 }.encode()).map_err(|err| format!("{err:?}"))?;
            Ok(())
        }
        Outcome::Save { dir, name, origin } => {
            let (f, saved) = scrinium::fs::create_new(&dir, &name).map_err(|err| format!("Couldn't save “{name}”: {}", scrinium::browser::err_text(&err)))?;
            if !origin.is_empty() {
                if let Err(err) = scrinium::fs::mark_origin(&f, &origin) {
                    eprintln!("scrinium: picker {embed}: {saved}: origin not recorded ({err})");
                }
            }
            let msg = ToParent::File { name: saved.clone(), size: 0, write: true };
            child.emit_fd(embed, msg.encode(), f.as_fd()).map_err(|err| format!("limend refused the file ({err:?})"))?;
            let _ = child.emit(embed, ToParent::Done { files: 1 }.encode());
            eprintln!("scrinium: picker {embed}: a new file {saved} handed over to write");
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
fn workbook_failure(message: String) -> ToParent {
    let mut why = String::new();
    for c in message.chars().filter(|c| !c.is_control()) {
        if why.len() + c.len_utf8() > limen::picker::MAX_WHY { break; }
        why.push(c);
    }
    ToParent::Failed { why }
}

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
                    embeds.insert(embed, Embed { to_pane: ptx, parent_uid, sheets: parent_app == "org.atrium.sheets", write: false, staged: None, requested: false });
                    let (f, pl, t) = (fresco.clone(), places.clone(), tx.clone());
                    std::thread::spawn(move || picker_pane(embed, parent_app, f, pl, prx, t));
                }
                Some(ChildEvent::ParentPersona { embed, persona }) => {
                    if let Some(e) = embeds.get(&embed) {
                        eprintln!("scrinium: picker {embed} is for {}", persona.as_deref().unwrap_or("personal"));
                        let _ = e.to_pane.send(ToPane::Persona(persona));
                    }
                }
                Some(ChildEvent::MessageFd { embed, bytes }) => {
                    let fd = child.take_fd();
                    if let Some(ToChild::Commit(token)) = ToChild::decode(&bytes) {
                        let Some(e) = embeds.get_mut(&embed) else { continue };
                        if !e.sheets || e.requested || fd.is_none() {
                            let _ = child.emit(embed, ToParent::Failed { why: "Invalid workbook commit".into() }.encode());
                            continue;
                        }
                        e.requested = true;
                        let source = std::fs::File::from(fd.unwrap());
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            let result = scrinium::workbook::commit(token, &source);
                            let _ = tx.send(FromPane::Committed(embed, result));
                        });
                        continue;
                    }
                    if let (Some(e), Some(ToChild::Open(f)), Some(fd)) = (embeds.get_mut(&embed), ToChild::decode(&bytes), fd) {
                        if e.requested || !e.sheets || f.kind != limen::picker::Kind::Save || !f.origin.is_empty() {
                            let _ = child.emit(embed, ToParent::Failed { why: "Invalid workbook save request".into() }.encode());
                            continue;
                        }
                        e.requested = true;
                        e.staged = Some(fd.into());
                        let _ = e.to_pane.send(ToPane::Open(f));
                    }
                }
                Some(ChildEvent::Message { embed, bytes }) => {
                    if let (Some(e), Some(ToChild::Open(f))) = (embeds.get_mut(&embed), ToChild::decode(&bytes)) {
                        if e.requested { continue; }
                        e.requested = true;
                        if f.kind == limen::picker::Kind::Workbook && !e.sheets {
                            let _ = child.emit(embed, ToParent::Failed { why: "Only Sheets can open a workbook this way".into() }.encode());
                            continue;
                        }
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
                        let Some(e) = embeds.get_mut(&embed) else { continue };
                        if let (Outcome::Save { dir, name, .. }, Some(file)) = (&o, e.staged.take()) {
                            let (dir, name, tx) = (dir.clone(), name.clone(), tx.clone());
                            std::thread::spawn(move || {
                                let result = scrinium::fs::publish_workbook(&dir, &name, &file).and_then(|(file, name)| {
                                    let token = scrinium::workbook::grant(&dir.join(&name), &file)?;
                                    Ok((file, name, token))
                                });
                                let _ = tx.send(FromPane::Published(embed, result));
                            });
                            continue;
                        }
                        if let Err(why) = hand_over(&mut child, embed, e, o, &media_sock) {
                            eprintln!("scrinium: picker {embed}: {why}");
                            let _ = e.to_pane.send(ToPane::Failed(why));
                        }
                    }
                    FromPane::Committed(embed, result) => {
                        if embeds.contains_key(&embed) {
                            let reply = match result {
                                Ok(()) => ToParent::Committed,
                                Err(err) => workbook_failure(err.to_string()),
                            };
                            let _ = child.emit(embed, reply.encode());
                        }
                    }
                    FromPane::Published(embed, result) => {
                        use std::os::fd::AsFd;
                        if !embeds.contains_key(&embed) { continue; }
                        let result = result.and_then(|(file, name, commit)| {
                            let msg = ToParent::Published { name, size: file.metadata()?.len(), commit };
                            child.emit_fd(embed, msg.encode(), file.as_fd()).map_err(|err| std::io::Error::other(format!("{err:?}")))
                        });
                        let reply = match result {
                            Ok(()) => ToParent::Done { files: 1 },
                            Err(e) => workbook_failure(format!("Couldn't save workbook: {e}")),
                        };
                        let _ = child.emit(embed, reply.encode());
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

// -- network shares ---------------------------------------------------------

enum Job {
    Learned(Result<String, String>),
    /// The share's part, and the item to forget at disconnect (not remembered).
    Connected(Result<String, String>, Option<scrinium::shares::Saved>),
    Saved(Vec<scrinium::shares::Saved>),
    /// A saved server asked for its key again, before connecting.
    Rechecked(scrinium::shares::Saved, Result<String, String>),
    /// A share's size and free space (its server answers over the network,
    /// so it is asked off the window's thread).
    Space(String, Option<(u64, u64)>),
    /// dispensatord's answer to a sealed stick's sheet.
    Media(Result<String, String>),
    /// A file to open, what its bytes say it is, and whether they are text.
    Typed(scrinium::browser::OpenReq, std::io::Result<(atrium_types::Detected, bool)>),
    /// The apps that open its type.
    Apps(scrinium::browser::OpenReq, Result<Vec<scrinium::browser::Choice>, String>),
    /// How handing it to its app went.
    Handed(scrinium::browser::OpenReq, Result<String, String>),
    Installed(scrinium::browser::OpenReq, Result<scrinium::install::Outcome, String>),
    /// Quick look's read: what to show, and the file for a viewer.
    Looked(std::path::PathBuf, std::io::Result<(scrinium::browser::LookBody, Option<std::fs::File>)>),
    LookPrint(std::path::PathBuf, String, std::io::Result<Vec<u8>>),
    /// A share's "connect at login", as dispensatord answered.
    /// `true`: the user asked to change it (a failure is shown).
    AtLogin(String, Result<bool, String>, bool),
}

/// How often a share's free space is asked again: also how a share that
/// lost its server finds out it is back (its server connects again on the
/// next request, S4), so more often while it is away.
const SHARE_SPACE_EVERY: Duration = Duration::from_secs(30);
const SHARE_OFFLINE_EVERY: Duration = Duration::from_secs(5);

/// One of Arca's fields (a Limen `credential` slot): asked for while
/// `want`, let go after. `code`: the recovery-code field, else the
/// password field.
fn arca_slot(embeds: &std::rc::Rc<std::cell::RefCell<pergola::embed::Embeds>>, pane: &mut Pane, want: bool, cur: &mut Option<EmbedId>, code: bool) {
    let (field, state) = if code { (&pane.rcode, &pane.rcode_state) } else { (&pane.cred, &pane.cred_state) };
    match (want, *cur) {
        (true, None) => {
            if let Some(token) = pane.win.embed_token() {
                let asked = embeds.borrow_mut().request("credential", token, field.slot_key());
                match asked {
                    Ok(e) => {
                        let _ = embeds.borrow_mut().send(e, field.configure().encode());
                        *cur = Some(e);
                    }
                    Err(e) => eprintln!("scrinium: Arca's {} field: {e:?}", if code { "recovery-code" } else { "password" }),
                }
            }
        }
        (false, Some(e)) => {
            let _ = embeds.borrow_mut().detach_embed(e);
            *cur = None;
            state.set(pergola::widgets::CredentialState::default());
        }
        _ => {}
    }
}

struct LookViewer {
    path: std::path::PathBuf,
    embed: EmbedId,
    pdf: Option<std::fs::File>,
}

/// Embeds and background requests for the manager's window.
struct Network {
    media: std::path::PathBuf,
    arca: std::path::PathBuf,
    embeds: Option<std::rc::Rc<std::cell::RefCell<pergola::embed::Embeds>>>,
    /// Arca's password field, while the form shows it.
    cred_embed: Option<EmbedId>,
    /// Arca's recovery-code field, while a stick's sheet asks for it.
    rcode_embed: Option<EmbedId>,
    /// Quick look's viewer (opening-files.md §7), for this file.
    look_embed: Option<LookViewer>,
    printing: Option<pergola::print::Printing>,
    text_pdf_pending: bool,
    look_text_file: Option<(std::path::PathBuf, std::fs::File)>,
    /// The file read for the viewer, until it is handed over.
    look_file: Option<(std::path::PathBuf, std::fs::File)>,
    tx: Sender<Job>,
    rx: Receiver<Job>,
    /// Shares connected without "remember": their item goes at disconnect.
    /// By the share's label (`user@host:path`), which stays when a lock
    /// and unlock bring the share back under another part (S4).
    forget: HashMap<String, scrinium::shares::Saved>,
    /// Shares' (size, free) by part, and when each was asked.
    space: HashMap<String, (u64, u64)>,
    asked: HashMap<String, std::time::Instant>,
    /// Shares whose server didn't answer the last time.
    offline: std::collections::HashSet<String>,
    /// Shares whose "connect at login" was asked (the answer is in the
    /// browser's state).
    asked_login: std::collections::HashSet<String>,
}

impl Network {
    fn new(pane: &mut Pane, media: std::path::PathBuf) -> Self {
        let embeds = match pergola::embed::Embeds::connect() {
            Ok(e) => {
                pane.win.set_slot_binder(pergola::embed::Embeds::binder(&e));
                Some(e)
            }
            Err(e) => {
                eprintln!("scrinium: limend: {e:?}: no password field for servers");
                None
            }
        };
        let (tx, rx) = mpsc::channel();
        let n = Network { media, arca: scrinium::shares::arca_socket(), embeds, cred_embed: None, rcode_embed: None, look_embed: None, printing: None, text_pdf_pending: false, look_text_file: None, look_file: None, tx, rx, forget: HashMap::new(), space: HashMap::new(), asked: HashMap::new(), offline: Default::default(), asked_login: Default::default() };
        n.refresh_saved();
        n
    }

    fn print_look(&mut self, pane: &Pane, b: &mut Browser, path: &std::path::Path) {
        use std::os::fd::AsFd;
        use pergola::print::Printing;

        let Some(look) = b.look.as_ref().filter(|look| look.path == path && look.can_print()) else { return };
        if self.printing.is_some() || self.text_pdf_pending {
            return;
        }
        if !look.is_pdf() {
            let file = self.look_text_file.as_ref().filter(|(current, _)| current == path).map(|(_, file)| file.try_clone());
            let Some(Ok(file)) = file else {
                b.notice = Some(scrinium::browser::Notice { text: "The text preview isn't ready to print.".into(), error: true });
                return;
            };
            let (path, name, tx) = (path.to_path_buf(), look.name.clone(), self.tx.clone());
            self.text_pdf_pending = true;
            b.notice = Some(scrinium::browser::Notice { text: "Preparing the text to print…".into(), error: false });
            std::thread::spawn(move || {
                let paper = pergola::print::Paper::A4;
                let pdf = scrinium::fs::read_print_text(file).and_then(|text| {
                    officina_pdf::plain(&name, &[officina_pdf::Plain::Text(&text)], paper.w_pt, paper.h_pt)
                });
                let _ = tx.send(Job::LookPrint(path, name, pdf));
            });
            return;
        }
        let pdf = self.look_embed.as_ref().filter(|viewer| viewer.path == path).and_then(|viewer| viewer.pdf.as_ref());
        let (Some(embeds), Some(token), Some(pdf)) = (&self.embeds, pane.win.embed_token(), pdf) else {
            b.notice = Some(scrinium::browser::Notice { text: "The PDF preview isn't ready to print.".into(), error: true });
            return;
        };
        // Send the same read-only file that was handed to the current viewer.
        match Printing::start(embeds, token, scrinium::view::PRINT_SLOT, pdf.as_fd(), &look.name, false, None) {
            Ok(printing) => self.printing = Some(printing),
            Err(error) => b.notice = Some(scrinium::browser::Notice { text: format!("Couldn't open the print sheet: {error:?}"), error: true }),
        }
    }

    /// Quick look's viewer: the pane's embed for the file it shows, asked
    /// for when a file for one has been read, let go when the pane moves
    /// on or closes. The viewer gets the file narrowed to reading.
    fn look_viewer(&mut self, pane: &mut Pane, b: &mut Browser) -> bool {
        use scrinium::browser::LookBody;
        let Some(embeds) = self.embeds.clone() else {
            if let Some((p, _)) = self.look_file.take() {
                b.looked(&p, Ok(LookBody::Info { words: look_words(b), why: Some("Quick look needs Limen, which isn't running".into()) }));
                return true;
            }
            return false;
        };
        let want = b.look.as_ref().filter(|l| matches!(l.body, Some(LookBody::Embed { .. }))).map(|l| l.path.clone());
        if let Some(viewer) = &self.look_embed {
            if want.as_ref() != Some(&viewer.path) {
                let _ = embeds.borrow_mut().detach_embed(viewer.embed);
                self.look_embed = None;
            }
        }
        let Some((path, f)) = self.look_file.take() else { return false };
        if want.as_ref() != Some(&path) || self.look_embed.is_some() {
            return false;
        }
        let Some(LookBody::Embed { role, mime, .. }) = b.look.as_ref().and_then(|l| l.body.clone()) else { return false };
        let Some(token) = pane.win.embed_token() else {
            self.look_file = Some((path, f));
            return false;
        };
        let msg = if role == "doc-viewer" {
            let kind = if mime.starts_with("image/") { limen::doc_viewer::DocKind::Picture } else { limen::doc_viewer::DocKind::Html };
            limen::doc_viewer::ToChild::Load { kind, base: String::new(), dark: false }.encode()
        } else if role == "pdf-viewer" {
            limen::pdf_viewer::ToChild::Load { dark: false }.encode()
        } else {
            limen::media_player::ToChild::Load { source: limen::media_player::Source::Fd, mime, growing: false }.encode()
        };
        use std::os::fd::AsFd;
        // One borrow at a time: the request's borrow must end before the send's.
        let asked = embeds.borrow_mut().request(role, token, scrinium::view::LOOK_SLOT);
        let r = match asked {
            Ok(e) => {
                let sent = embeds.borrow_mut().send_fd(e, msg, f.as_fd());
                match sent {
                    Ok(()) => Ok(e),
                    Err(err) => {
                        let _ = embeds.borrow_mut().detach_embed(e);
                        Err(err)
                    }
                }
            }
            Err(err) => Err(err),
        };
        match r {
            Ok(e) => {
                eprintln!("scrinium: quick look: {} in {role}", path.display());
                self.look_embed = Some(LookViewer { path, embed: e, pdf: (role == "pdf-viewer").then_some(f) });
            }
            Err(e) => {
                let why = match e {
                    limen::client::Error::Refused(limen::proto::refusal::NO_PROVIDER) => "No app is set to show these".to_string(),
                    limen::client::Error::Refused(limen::proto::refusal::LAUNCH_FAILED) => "The viewer didn't start".to_string(),
                    limen::client::Error::Refused(n) => format!("The viewer was refused (Limen {n})"),
                    limen::client::Error::Io(e) => format!("Limen: {e}"),
                };
                eprintln!("scrinium: quick look: {role}: {why}");
                b.looked(&path, Ok(LookBody::Info { words: look_words(b), why: Some(why) }));
            }
        }
        true
    }

    fn refresh_saved(&self) {
        let (arca, tx) = (self.arca.clone(), self.tx.clone());
        std::thread::spawn(move || match scrinium::shares::list(&arca) {
            Ok(l) => {
                let _ = tx.send(Job::Saved(l));
            }
            Err(e) => eprintln!("scrinium: saved servers: {e}"),
        });
    }

    fn disconnected(&mut self, label: &str) {
        if let Some(s) = self.forget.remove(label) {
            if let Err(e) = scrinium::shares::forget(&self.arca, &s) {
                eprintln!("scrinium: forget {}: {e}", s.addr.host);
            }
        }
    }

    fn connect_job(&self, addr: scrinium::connect::Address, key: [u8; 32], save: Option<([u8; 16], bool)>, at_login: bool) {
        let (media, arca, tx) = (self.media.clone(), self.arca.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let mut forget = None;
            if let Some((handle, remember)) = save {
                match scrinium::shares::save(&arca, &handle, &addr, &key) {
                    Ok(item) if !remember => forget = Some(scrinium::shares::Saved { addr: addr.clone(), key, item }),
                    Ok(_) => {}
                    Err(e) => {
                        let _ = tx.send(Job::Connected(Err(e), None));
                        return;
                    }
                }
            }
            let c = atrium_media::Connect {
                proto: addr.proto.media(),
                host: addr.host.clone(),
                port: addr.port,
                path: addr.path.clone(),
                user: addr.user.clone(),
                fingerprint: scrinium::connect::identity_of(addr.proto, &key),
                read_only: false,
                at_login,
            };
            let r = atrium_media::client::connect(&media, &c).map_err(|(_, m)| m);
            let _ = tx.send(Job::Connected(r, forget));
        });
    }

    /// One turn; the part of a share that just connected, to open it.
    fn step(&mut self, pane: &mut Pane) -> Option<String> {
        use scrinium::connect::{Step, Want};
        let mut b = pane.state.get_cloned();
        let mut changed = false;
        let mut open = None;
        // A press on Connect that no handle followed.
        if let Some(f) = &mut b.connect {
            changed |= f.tick(std::time::Instant::now());
        }
        changed |= b.stick_tick(std::time::Instant::now());
        // What the form asks for.
        if let Some(want) = b.connect.as_mut().and_then(|f| f.take_want()) {
            changed = true;
            match want {
                Want::Learn(a) => {
                    let (media, tx) = (self.media.clone(), self.tx.clone());
                    std::thread::spawn(move || {
                        let r = atrium_media::client::learn(&media, &atrium_media::Learn { proto: a.proto.media(), host: a.host.clone(), port: a.port }).map_err(|(_, m)| m);
                        let _ = tx.send(Job::Learned(r));
                    });
                }
                Want::Connect { addr, fingerprint, remember, at_login } => {
                    let handle = pane.cred_state.get_cloned().handle;
                    match (handle, scrinium::connect::key_for(&fingerprint)) {
                        (Some(h), Some(key)) => self.connect_job(addr, key, Some((h, remember)), at_login),
                        _ => {
                            if let Some(f) = &mut b.connect {
                                f.failed("Type the password again".into());
                            }
                        }
                    }
                }
            }
        }
        if let Some(i) = b.want_reconnect.take() {
            changed = true;
            if let Some(s) = b.saved.get(i).cloned() {
                // Ask the server who it is first: a changed key stops here,
                // on its own sheet (the share server's pin would also stop
                // it, but only after the share seemed to connect).
                b.notice = Some(scrinium::browser::Notice { text: format!("Asking {} who it is…", s.addr.host), error: false });
                let (media, tx) = (self.media.clone(), self.tx.clone());
                std::thread::spawn(move || {
                    let r = atrium_media::client::learn(&media, &atrium_media::Learn { proto: s.addr.proto.media(), host: s.addr.host.clone(), port: s.addr.port }).map_err(|(_, m)| m);
                    let _ = tx.send(Job::Rechecked(s, r));
                });
            }
        }
        if let Some(s) = b.want_forget.take() {
            changed = true;
            match scrinium::shares::forget(&self.arca, &s) {
                Ok(()) => b.notice = Some(scrinium::browser::Notice { text: format!("Forgot {}", s.addr.host), error: false }),
                Err(e) => b.notice = Some(scrinium::browser::Notice { text: format!("Couldn't forget {}: {e}", s.addr.host), error: true }),
            }
            self.refresh_saved();
        }
        // Arca's field: asked for when the form gets to the password, let go after.
        let wants_field = matches!(b.connect.as_ref().map(|f| &f.step), Some(Step::Password { .. }));
        // Arca's recovery-code field: while a stick's sheet asks for the code.
        let wants_code = matches!(b.stick_sheet.as_ref().map(|s| &s.ask), Some(scrinium::browser::StickAsk::Recovery { .. }));
        if let Some(embeds) = self.embeds.clone() {
            arca_slot(&embeds, pane, wants_field, &mut self.cred_embed, false);
            arca_slot(&embeds, pane, wants_code, &mut self.rcode_embed, true);
            loop {
                // Not zero: a zero read timeout is refused (EINVAL), and
                // that lost limend, so the password field never came.
                let ev = embeds.borrow_mut().poll(Duration::from_millis(1));
                if let (Some(printing), Ok(Some(event))) = (&mut self.printing, &ev) {
                    if let Some(step) = printing.handle(&embeds, event) {
                        use pergola::print::{Outcome, Step};
                        if let Step::Done(outcome) = step {
                            self.printing = None;
                            b.notice = match outcome {
                                Outcome::Queued => Some(scrinium::browser::Notice { text: "Sent to the printer.".into(), error: false }),
                                Outcome::Saved(name) => Some(scrinium::browser::Notice { text: format!("Saved “{name}”."), error: false }),
                                Outcome::Failed(why) => Some(scrinium::browser::Notice { text: why, error: true }),
                                Outcome::Cancelled => None,
                            };
                            changed = true;
                        }
                        continue;
                    }
                }
                match ev {
                    Ok(Some(limen::proto::ParentEvent::Message { embed, bytes })) if Some(embed) == self.cred_embed => {
                        if let Some(m) = limen::credential::ToParent::decode(&bytes) {
                            pane.cred.apply(m);
                            let s = pane.cred_state.get_cloned();
                            if let Some(f) = &mut b.connect {
                                // A handle is what counts: a typed value brings only that (`Filled`
                                // comes with a press on Arca's chip).
                                f.field_filled(s.handle.is_some());
                                changed = true;
                            }
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::Message { embed, bytes })) if Some(embed) == self.rcode_embed => {
                        if let Some(m) = limen::credential::ToParent::decode(&bytes) {
                            pane.rcode.apply(m);
                            if let Some(h) = pane.rcode_state.get_cloned().handle {
                                b.code_handle(h);
                            }
                            changed = true;
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed, .. })) if Some(embed) == self.cred_embed => self.cred_embed = None,
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed, .. })) if Some(embed) == self.rcode_embed => self.rcode_embed = None,
                    Ok(Some(limen::proto::ParentEvent::Message { embed, bytes })) => {
                        if let Some(viewer) = self.look_embed.as_ref().filter(|viewer| viewer.embed == embed && viewer.pdf.is_some()) {
                            if limen::pdf_viewer::ToParent::decode(&bytes) == Some(limen::pdf_viewer::ToParent::PrintAsked) {
                                let path = viewer.path.clone();
                                self.print_look(pane, &mut b, &path);
                                changed = true;
                            }
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed, .. })) if self.look_embed.as_ref().is_some_and(|viewer| viewer.embed == embed) => {
                        let viewer = self.look_embed.take().unwrap();
                        let words = look_words(&b);
                        b.looked(&viewer.path, Ok(scrinium::browser::LookBody::Info { words, why: Some("The viewer stopped".into()) }));
                        changed = true;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("scrinium: limend: {e:?}");
                        self.embeds = None;
                        self.printing = None;
                        break;
                    }
                }
            }
        }
        // A share that went takes what was known of it along: another
        // may come under its name.
        let here_ids: std::collections::HashSet<String> = b.places.iter().filter(|p| p.network).map(|p| p.id.clone()).collect();
        let here = &here_ids;
        self.offline.retain(|k| here.contains(k));
        self.asked.retain(|k, _| here.contains(k));
        self.space.retain(|k, _| here.contains(k));
        self.asked_login.retain(|k| here.contains(k));
        // Each share's "connect at login", asked once; set when the status
        // line asks.
        b.at_login.retain(|k, _| here_ids.contains(k));
        let unasked: Vec<String> = here_ids.iter().filter(|k| !b.at_login.contains_key(*k) && !self.asked_login.contains(*k)).cloned().collect();
        for part in unasked.into_iter().map(|p| (p, None)).chain(b.want_at_login.take().map(|(p, on)| (p, Some(on)))) {
            changed = true;
            self.asked_login.insert(part.0.clone());
            let (media, tx) = (self.media.clone(), self.tx.clone());
            std::thread::spawn(move || {
                let r = atrium_media::client::at_login(&media, &atrium_media::AtLogin { part: part.0.clone(), on: part.1 }).map(|m| m == "on").map_err(|(_, m)| m);
                let _ = tx.send(Job::AtLogin(part.0, r, part.1.is_some()));
            });
        }
        // A sealed stick's sheet: dispensatord, off this thread (arcad may
        // wait for the person to verify).
        if let Some(ask) = b.want_media.take() {
            use scrinium::browser::MediaAsk;
            changed = true;
            let (media, tx) = (self.media.clone(), self.tx.clone());
            std::thread::spawn(move || {
                let eject = |part: String| atrium_media::Eject { part };
                let r = match ask {
                    MediaAsk::Format(f) => atrium_media::client::format(&media, &f),
                    MediaAsk::Adopt { part, persona, keep, handle } => atrium_media::client::adopt(&media, &atrium_media::Adopt { part, persona, keep, code: String::new(), handle }),
                    MediaAsk::OpenOlder(part) => atrium_media::client::open_older(&media, &eject(part)),
                    MediaAsk::MakeCurrent(part) => atrium_media::client::make_current(&media, &eject(part)),
                    MediaAsk::Forget(part) => atrium_media::client::forget(&media, &eject(part)),
                    MediaAsk::RemoveCode(part) => atrium_media::client::remove_code(&media, &eject(part)),
                };
                let _ = tx.send(Job::Media(r.map_err(|(_, m)| m)));
            });
        }
        // A file being opened: each step off this thread.
        if let Some(step) = b.want_open.take() {
            use scrinium::browser::OpenStep;
            changed = true;
            let tx = self.tx.clone();
            std::thread::spawn(move || match step {
                OpenStep::Type(req) => {
                    let r = scrinium::fs::head(&req.path).map(|h| (atrium_types::detect(&req.name, &h), atrium_types::is_text(&h)));
                    let _ = tx.send(Job::Typed(req, r));
                }
                OpenStep::Apps(req) => {
                    let r = scrinium::open::apps(req.mime.as_deref().unwrap_or_default());
                    let _ = tx.send(Job::Apps(req, r));
                }
                OpenStep::Hand(mut req) => {
                    let r = scrinium::fs::open_for_app(&req.path, req.write).map_err(|e| scrinium::browser::err_text(&e)).and_then(|(f, w)| {
                        req.write = w;
                        let (app, mime) = (req.app.clone().unwrap_or_default(), req.mime.clone().unwrap_or_default());
                        scrinium::open::open(&app, &mime, &req.name, w, req.always, req.token, &f, &req.path)
                    });
                    let _ = tx.send(Job::Handed(req, r));
                }
                OpenStep::Install(req) => {
                    let r = scrinium::fs::open_for_app(&req.path, false).map_err(|e| scrinium::browser::err_text(&e))
                        .and_then(|(f, _)| scrinium::install::side_load(&f));
                    let _ = tx.send(Job::Installed(req, r));
                }
            });
        }
        // Quick look: read off this thread; a viewer gets the file.
        if self.look_text_file.as_ref().is_some_and(|(path, _)| b.look.as_ref().is_none_or(|look| look.path != *path)) {
            self.look_text_file = None;
        }
        if let Some(path) = b.want_look.take() {
            changed = true;
            self.look_file = None;
            self.look_text_file = None;
            let name = b.look.as_ref().map(|l| l.name.clone()).unwrap_or_default();
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let r = scrinium::fs::look(&path, &name);
                let _ = tx.send(Job::Looked(path, r));
            });
        }
        changed |= self.look_viewer(pane, &mut b);
        if let Some(path) = b.want_look_print.take() {
            self.print_look(pane, &mut b, &path);
            changed = true;
        }
        // Shares' sizes: asked now and then, each on its own thread.
        for p in b.places.iter().filter(|p| p.network) {
            let every = if self.offline.contains(&p.id) { SHARE_OFFLINE_EVERY } else { SHARE_SPACE_EVERY };
            if self.asked.get(&p.id).is_some_and(|t| t.elapsed() < every) {
                continue;
            }
            self.asked.insert(p.id.clone(), std::time::Instant::now());
            let (id, root, tx) = (p.id.clone(), p.root.clone(), self.tx.clone());
            std::thread::spawn(move || {
                let r = scrinium::places::statvfs(&root).map(|(size, free, _)| (size, free));
                let _ = tx.send(Job::Space(id, r));
            });
        }
        // What came back.
        while let Ok(j) = self.rx.try_recv() {
            changed = true;
            match j {
                Job::Learned(r) => {
                    if let Some(f) = &mut b.connect {
                        f.learned(r);
                    }
                }
                Job::Connected(Ok(part), forget) => {
                    if let Some(s) = forget {
                        self.forget.insert(format!("{}@{}:{}", s.addr.user, s.addr.host, s.addr.path), s);
                    }
                    b.connect = None;
                    b.notice = Some(scrinium::browser::Notice { text: "Connected".into(), error: false });
                    open = Some(part);
                    self.refresh_saved();
                }
                Job::Connected(Err(e), _) => match &mut b.connect {
                    Some(f) => f.failed(e),
                    None => b.notice = Some(scrinium::browser::Notice { text: format!("Couldn't connect: {e}"), error: true }),
                },
                Job::Saved(l) => b.saved = l,
                Job::Typed(req, r) => b.typed(req, r),
                Job::Apps(req, r) => b.apps(req, r),
                Job::Handed(req, r) => b.handed(&req, r),
                Job::Installed(req, r) => b.installed(&req, r),
                Job::Looked(path, r) => match r {
                    Ok((body, file)) => {
                        let text = matches!(body, scrinium::browser::LookBody::Text { .. });
                        b.looked(&path, Ok(body));
                        if let (Some(f), true) = (file, b.look.as_ref().is_some_and(|l| l.path == path)) {
                            if text {
                                self.look_text_file = Some((path, f));
                            } else {
                                self.look_file = Some((path, f));
                            }
                        }
                    }
                    Err(e) => b.looked(&path, Err(e)),
                },
                Job::LookPrint(path, name, pdf) => {
                    self.text_pdf_pending = false;
                    if b.look.as_ref().is_some_and(|look| look.path == path && look.can_print()) {
                        let result = pdf.map_err(|err| err.to_string()).and_then(|pdf| {
                            let (Some(embeds), Some(token)) = (&self.embeds, pane.win.embed_token()) else {
                                return Err("The print sheet isn't available.".into());
                            };
                            pergola::print::Printing::start_bytes(embeds, token, scrinium::view::PRINT_SLOT, &pdf, &name, false, Some(pergola::print::Paper::A4))
                                .map_err(|err| format!("Couldn't open the print sheet: {err:?}"))
                        });
                        match result {
                            Ok(printing) => { self.printing = Some(printing); b.notice = None; }
                            Err(error) => b.notice = Some(scrinium::browser::Notice { text: error, error: true }),
                        }
                    } else if b.notice.as_ref().is_some_and(|n| n.text == "Preparing the text to print…") {
                        b.notice = None;
                    }
                }
                Job::Media(r) => {
                    // A code that didn't open it: type it again.
                    if r.is_err() {
                        pane.rcode_state.set(pergola::widgets::CredentialState::default());
                    }
                    b.media_done(r);
                }
                Job::AtLogin(part, r, set) => match r {
                    Ok(on) => {
                        b.at_login.insert(part, on);
                    }
                    Err(e) if !set => eprintln!("scrinium: {part}: connect at login: {e}"),
                    Err(e) => b.notice = Some(scrinium::browser::Notice { text: format!("Couldn't change “connect at login”: {e}"), error: true }),
                },
                Job::Space(part, r) => match r {
                    Some(sf) => {
                        // Back: show the open folder again if it couldn't be read.
                        if self.offline.remove(&part) && b.place().is_some_and(|p| p.id == part) && b.unreadable.is_some() {
                            b.reload();
                        }
                        self.space.insert(part, sf);
                    }
                    None => {
                        // The server can't be reached (its requests fail at
                        // once, EIO) or the share just went.
                        self.offline.insert(part.clone());
                        self.space.remove(&part);
                    }
                },
                Job::Rechecked(s, Ok(fp)) => match scrinium::connect::recheck(s.addr.proto, &s.key, &fp) {
                    scrinium::connect::Recheck::Same => {
                        b.notice = Some(scrinium::browser::Notice { text: format!("Connecting to {}…", s.addr.host), error: false });
                        self.connect_job(s.addr, s.key, None, false);
                    }
                    scrinium::connect::Recheck::Changed { now } => {
                        eprintln!("scrinium: {} shows another key ({now}): stopped", s.addr.host);
                        b.notice = None;
                        b.key_changed = Some(scrinium::browser::KeyChanged { saved: s, now });
                    }
                    scrinium::connect::Recheck::Odd => b.notice = Some(scrinium::browser::Notice { text: format!("{} gave no key", s.addr.host), error: true }),
                },
                Job::Rechecked(s, Err(e)) => b.notice = Some(scrinium::browser::Notice { text: format!("Couldn't reach {}: {e}", s.addr.host), error: true }),
            }
        }
        for p in b.places.iter_mut().filter(|p| p.network) {
            let (size, free) = self.space.get(&p.id).copied().unwrap_or((0, 0));
            let offline = self.offline.contains(&p.id);
            if (p.size, p.free, p.offline) != (size, free, offline) {
                (p.size, p.free, p.offline) = (size, free, offline);
                changed = true;
            }
        }
        if changed {
            pane.state.set(b);
        }
        open
    }
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
    // The person's folders first, their workspaces, then the folders it was given.
    let mut configured = places::home();
    configured.extend(places::workspaces());
    configured.extend(places::personas());
    configured.extend(places::configured());
    let shared: SharedPlaces = Arc::new(Mutex::new(Places { list: configured.clone(), gen: 1 }));
    let (ptx, prx) = mpsc::channel();
    places::watch(ptx);
    {
        let shared = shared.clone();
        std::thread::spawn(move || {
            for sticks in prx {
                // The person's own folders first (places.rs), then the sticks.
                let mut all = configured.clone();
                all.extend(places::distinct(sticks));
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
    if let Some(p) = std::env::var("SCRINIUM_PERSONA").ok().filter(|p| !p.is_empty() && p.len() <= 32 && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')) {
        with(&pane.state, |b| b.persona = p);
    }
    let mut opened = std::env::var("SCRINIUM_OPEN").ok();
    let mut net = Network::new(&mut pane, media_sock.clone());
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
        if let Some(part) = net.step(&mut pane) {
            opened = Some(part);
        }
        if let Some(part) = pane.state.get_cloned().want_eject {
            let label = pane.state.get_cloned().places.iter().find(|p| p.id == part).map(|p| p.label.clone()).unwrap_or_default();
            let msg = match atrium_media::client::eject(&media_sock, &part) {
                Ok(_) if part.starts_with("share-") => {
                    net.disconnected(&label);
                    ("Disconnected".to_string(), false)
                }
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

/// The words for the file in the quick look pane.
fn look_words(b: &Browser) -> String {
    match b.look.as_ref().and_then(|l| l.body.as_ref()) {
        Some(scrinium::browser::LookBody::Embed { words, .. } | scrinium::browser::LookBody::Text { words, .. } | scrinium::browser::LookBody::Info { words, .. }) => words.clone(),
        None => String::new(),
    }
}
