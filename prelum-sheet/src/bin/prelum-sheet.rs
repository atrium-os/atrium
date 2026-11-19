//! atrium-prelum-sheet: the print sheet (docs/spec/printing.md §5.2), Limen's
//! `print` provider, running as `_prelum-sheet`.
//!
//! One process serves every sheet: a thread per open sheet, each with its
//! own window on frescod (a sheet slot over the app's window) and its own
//! connections to prelumd, which act for the app's person (`FOR`, with the
//! parent uid limend named). The sheet never reads the document: previews
//! and Save as PDF's rewrite come from a render jail through prelumd, and
//! the PDF goes to prelumd as the descriptor the app sent.
//!
//! Print counts only a real press: frescod's `PROMPT` on a press of the
//! button itself, uncovered and still for 500 ms (the guard is the button).
//!
//! - `LIMEN_SOCKET`, `FRESCO_SOCKET` as for any app.
//! - `PRELUM_SOCKET`: prelumd (default /atrium/sockets/print/print.sock).
//! - `PRELUM_SHEET_ACTIVATION`: frescod's activation socket (default
//!   /atrium/sockets/fresco-activation/fresco-activation.sock).
//! - `PRELUM_SHEET_NEARBY`: seconds to listen for printers nearby (default 4,
//!   0: don't look).

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use limen::client::{Child, Parent};
use limen::credential::{self, Handle};
use limen::print::{ToChild, ToParent, Why};
use limen::proto::{ChildEvent, EmbedId, ParentEvent, WindowToken};
use pergola::{App, Mutable, SizeSpec, StepResult, Window, WindowDesc};
use prelum_proto::print::{client::Client, Add, Layout, Nearby, PreviewReq, Previews, Print, Printer};
use prelum_sheet::sheet::{Action, Dest, PinChoice, Sheet, Tone};
use prelum_sheet::view::{self, Asked, SheetView};

const PRELUM: &str = "/atrium/sockets/print/print.sock";
const ACTIVATION: &str = "/atrium/sockets/fresco-activation/fresco-activation.sock";
/// A sheet nobody answers closes after this.
const SHEET_FOR: Duration = Duration::from_secs(600);
/// Settings settle this long before a new preview is asked for.
const SETTLE: Duration = Duration::from_millis(300);
/// Previews asked for at a time.
const BATCH: u32 = 8;
const LONGEST: u16 = 360;

fn env(k: &str, d: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.into())
}

/// From the provider's loop to a sheet.
enum ToPane {
    Print { title: String, hint: Option<limen::print::Paper>, can_relayout: bool, fd: OwnedFd },
    Document(OwnedFd),
    Close,
}

/// From a sheet to the provider's loop.
enum FromPane {
    Ready(EmbedId, WindowToken),
    Emit(EmbedId, ToParent),
    EmitFd(EmbedId, ToParent, std::fs::File),
    /// The last message: the embed is over.
    End(EmbedId, ToParent),
    /// The window went first.
    Gone(EmbedId),
}

fn main() {
    let limen_sock = env("LIMEN_SOCKET", limen::client::DEFAULT_SOCKET);
    let fresco = env("FRESCO_SOCKET", &fresco_client::default_socket_path().to_string_lossy());
    let prelum = PathBuf::from(env("PRELUM_SOCKET", PRELUM));
    loop {
        let mut child = match Child::provider(&limen_sock, "print") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("prelum-sheet: limend at {limen_sock}: {e:?}; again in 5 s");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        eprintln!("prelum-sheet: providing print");
        let (tx, rx) = mpsc::channel::<FromPane>();
        let mut panes: HashMap<EmbedId, Sender<ToPane>> = HashMap::new();
        loop {
            let ev = match child.poll(Duration::from_millis(20)) {
                Ok(ev) => ev,
                Err(e) => {
                    eprintln!("prelum-sheet: limend went ({e:?})");
                    break;
                }
            };
            match ev {
                Some(ChildEvent::Attach { embed, role, parent_app, parent_uid }) if role == "print" => {
                    eprintln!("prelum-sheet: sheet {embed} for {parent_app} (uid {parent_uid})");
                    let (ptx, prx) = mpsc::channel();
                    panes.insert(embed, ptx);
                    let (f, p, t) = (fresco.clone(), prelum.clone(), tx.clone());
                    std::thread::spawn(move || pane(embed, parent_uid, f, p, prx, t));
                }
                Some(ChildEvent::MessageFd { embed, bytes }) => {
                    let fd = child.take_fd();
                    let (Some(p), Some(fd)) = (panes.get(&embed), fd) else { continue };
                    let _ = p.send(match ToChild::decode(&bytes) {
                        Some(ToChild::Print { title, page_size_hint, can_relayout }) => ToPane::Print { title, hint: page_size_hint, can_relayout, fd },
                        Some(ToChild::Document) => ToPane::Document(fd),
                        None => continue,
                    });
                }
                Some(ChildEvent::Detaching { embed }) => {
                    if let Some(p) = panes.remove(&embed) {
                        let _ = p.send(ToPane::Close);
                    }
                }
                _ => {}
            }
            while let Ok(m) = rx.try_recv() {
                match m {
                    FromPane::Ready(embed, token) => {
                        if let Err(e) = child.ready(embed, token) {
                            eprintln!("prelum-sheet: sheet {embed}: ready: {e:?}");
                        }
                    }
                    FromPane::Emit(embed, msg) if panes.contains_key(&embed) => {
                        let _ = child.emit(embed, msg.encode());
                    }
                    FromPane::EmitFd(embed, msg, f) if panes.contains_key(&embed) => {
                        if let Err(e) = child.emit_fd(embed, msg.encode(), f.as_fd()) {
                            eprintln!("prelum-sheet: sheet {embed}: limend refused the PDF ({e:?})");
                        }
                    }
                    FromPane::End(embed, msg) => {
                        if panes.remove(&embed).is_some() {
                            eprintln!("prelum-sheet: sheet {embed}: {msg:?}");
                            let _ = child.emit(embed, msg.encode());
                        }
                    }
                    FromPane::Gone(embed) => {
                        if panes.remove(&embed).is_some() {
                            let _ = child.emit(embed, ToParent::Cancelled.encode());
                        }
                    }
                    _ => {}
                }
            }
        }
        for (_, p) in panes.drain() {
            let _ = p.send(ToPane::Close);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// prelumd, acting for the app's person.
fn prelumd(path: &PathBuf, app_uid: u32) -> Result<Client, String> {
    let mut c = Client::connect(path).map_err(|e| format!("The print service isn't running ({e})"))?;
    c.act_for(app_uid).map_err(|e| e.message)?;
    Ok(c)
}

/// What the sheet's two helpers do. Lists and Nearby on one connection (the
/// nearby scan takes seconds), previews and jobs on the other.
enum Job {
    Preview { req: PreviewReq, fd: OwnedFd, gen: u64 },
    Print { action: Action, fd: OwnedFd },
    PrintPin { print: Print, fd: OwnedFd, handle: Handle },
}

enum Done {
    Printers(Vec<Printer>),
    Nearby(Vec<Nearby>),
    Preview { gen: u64, r: Result<Previews, String> },
    Printed(Result<(), String>),
    Saved(Result<(std::fs::File, String), String>),
    /// prelumd can't be reached or refused the sheet.
    Broken(String),
}

fn lister(path: PathBuf, app_uid: u32, nearby_secs: u8, out: Sender<Done>, stop: Arc<AtomicBool>) {
    let mut c = match prelumd(&path, app_uid) {
        Ok(c) => c,
        Err(e) => {
            let _ = out.send(Done::Broken(e));
            return;
        }
    };
    let mut last_nearby = None::<Instant>;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match c.printers() {
            Ok(v) => {
                if out.send(Done::Printers(v)).is_err() {
                    return;
                }
            }
            Err(e) => {
                let _ = out.send(Done::Broken(e.message));
                return;
            }
        }
        if nearby_secs > 0 && last_nearby.is_none_or(|t| t.elapsed() > Duration::from_secs(30)) {
            last_nearby = Some(Instant::now());
            if let Ok(v) = c.nearby(nearby_secs) {
                if out.send(Done::Nearby(v)).is_err() {
                    return;
                }
            }
        }
        // The saved printers' state, every few seconds while the sheet is open.
        for _ in 0..50 {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn doer(path: PathBuf, app_uid: u32, jobs: Receiver<Job>, out: Sender<Done>) {
    let mut c = match prelumd(&path, app_uid) {
        Ok(c) => c,
        Err(e) => {
            let _ = out.send(Done::Broken(e));
            return;
        }
    };
    let mut queue: std::collections::VecDeque<Job> = Default::default();
    loop {
        if queue.is_empty() {
            match jobs.recv() {
                Ok(j) => queue.push_back(j),
                Err(_) => return,
            }
        }
        // Only the newest preview matters; jobs keep their order.
        while let Ok(n) = jobs.try_recv() {
            if matches!(n, Job::Preview { .. }) {
                queue.retain(|q| !matches!(q, Job::Preview { .. }));
            }
            queue.push_back(n);
        }
        let Some(j) = queue.pop_front() else { continue };
        let done = match j {
            Job::Preview { req, fd, gen } => Done::Preview { gen, r: c.preview(&req, &std::fs::File::from(fd)).map_err(|e| e.message) },
            Job::PrintPin { print, fd, handle } => Done::Printed(c.print_pin(&print, handle, &std::fs::File::from(fd)).map(|_| ()).map_err(|e| e.message)),
            Job::Print { action: Action::Print(p), fd } => Done::Printed(c.print(&p, &std::fs::File::from(fd)).map(|_| ()).map_err(|e| e.message)),
            Job::Print { action: Action::AddThenPrint { uri, name, mut print }, fd } => Done::Printed(
                c.add(&Add { uri, name })
                    .and_then(|added| {
                        print.printer = added.id;
                        c.print(&print, &std::fs::File::from(fd))
                    })
                    .map(|_| ())
                    .map_err(|e| e.message),
            ),
            Job::Print { action: Action::Save { layout, name }, fd } => Done::Saved(save(&mut c, &layout, fd).map(|f| (f, name))),
        };
        if out.send(done).is_err() {
            return;
        }
    }
}

fn save(c: &mut Client, l: &Layout, fd: OwnedFd) -> Result<std::fs::File, String> {
    c.save_pdf(l, &std::fs::File::from(fd)).map(|(_, f)| f).map_err(|e| e.message)
}

/// Spend `token`, a press on Print, as `PROMPT`: frescod counts it only if
/// the guard (the button) was seen whole, uncovered and still before it,
/// in the focused window.
fn counted(token: Option<[u8; 16]>) -> Result<(), String> {
    use fresco_protocol::activation::{op, purpose, Consume, ConsumeResult};
    let Some(token) = token.filter(|t| *t != fresco_protocol::activation::NONE) else {
        return Err("no press".into());
    };
    let path = env("PRELUM_SHEET_ACTIVATION", ACTIVATION);
    let ask = || -> std::io::Result<ConsumeResult> {
        let bad = |e: String| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
        let mut c = aqueduct::Connection::connect(&path)?;
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        let payload = postcard::to_stdvec(&Consume { token, presenter_uid: me, purpose: purpose::PROMPT }).map_err(|e| bad(e.to_string()))?;
        c.send_message(aqueduct::classes::CLASS_DISPLAY, op::CONSUME, aqueduct::flag::RESPONSE_EXPECTED, &payload)?;
        c.set_read_timeout(Some(Duration::from_millis(500)))?;
        let m = c.recv_message()?;
        postcard::from_bytes(&m.payload).map_err(|e| bad(e.to_string()))
    };
    match ask() {
        Ok(ConsumeResult::Ok { .. }) => Ok(()),
        Ok(r) => Err(format!("{r:?}")),
        Err(e) => Err(format!("frescod's activation socket: {e}")),
    }
}

/// Why the document couldn't be taken, from the first preview's refusal.
fn why(msg: &str) -> Option<Why> {
    let m = msg.to_ascii_lowercase();
    if m.contains("empty") || m.contains("no pages") {
        Some(Why::Empty)
    } else if m.contains("too large") {
        Some(Why::TooLarge)
    } else if m.contains("can't be read") || m.contains("could not be read") || m.contains("not a file") {
        Some(Why::NotPdf)
    } else {
        None
    }
}

struct PrinterCredentialPrompt {
    parent: Parent,
    embed: EmbedId,
    printer: u32,
    scale: f32,
    config: Option<credential::ToChild>,
    pending: Option<(Print, OwnedFd, Instant)>,
    saving_login_since: Option<Instant>,
}

impl PrinterCredentialPrompt {
    fn open(path: &PathBuf, app_uid: u32, printer: u32, window: WindowToken, scale: f32, login: bool) -> Result<Self, String> {
        let mut client = prelumd(path, app_uid)?;
        let binding = if login { client.login_binding(printer) } else { client.pin_binding(printer) }
            .map_err(|e| e.message)?;
        let prelum_proto::print::PinBinding { user, persona, key, uuid } = binding;
        let config = if login { credential::ToChild::ConfigurePrintLogin { user, persona, key, uuid } }
            else { credential::ToChild::ConfigurePrintPin { user, persona, key, uuid } };
        let mut parent = Parent::connect().map_err(|e| format!("Cannot open the Arca field ({e:?})"))?;
        let embed = parent.request("credential", window).map_err(|e| format!("Cannot open the Arca field ({e:?})"))?;
        let mut pin = Self { parent, embed, printer, scale, config: Some(config), pending: None, saving_login_since: None };
        pin.place(scale)?;
        Ok(pin)
    }

    fn place(&mut self, scale: f32) -> Result<(), String> {
        let r = view::pin_slot();
        self.parent.place(self.embed, Some([r.x() * scale, r.y() * scale, r.w() * scale, r.h() * scale]))
            .map_err(|e| format!("Cannot place the Arca field ({e:?})"))?;
        self.scale = scale;
        Ok(())
    }

    fn poll(&mut self, scale: f32) -> Result<Option<credential::ToParent>, String> {
        if self.scale != scale {
            self.place(scale)?;
        }
        match self.parent.poll(Duration::from_millis(1)).map_err(|_| "The Arca field disconnected")? {
            Some(ParentEvent::Attached { embed }) if embed == self.embed => {
                if let Some(config) = self.config.take() {
                    self.parent.send(embed, config.encode()).map_err(|_| "Cannot configure the Arca field")?;
                }
                Ok(None)
            }
            Some(ParentEvent::Message { embed, bytes }) if embed == self.embed => Ok(credential::ToParent::decode(&bytes)),
            Some(ParentEvent::ChildLost { embed } | ParentEvent::Detached { embed }) if embed == self.embed => {
                Err("The Arca field closed. Open it again.".into())
            }
            _ => Ok(None),
        }
    }
}

fn pane(embed: EmbedId, app_uid: u32, fresco: String, prelum: PathBuf, rx: Receiver<ToPane>, tx: Sender<FromPane>) {
    let who = format!("sheet {embed}");
    let gone = |tx: &Sender<FromPane>| {
        let _ = tx.send(FromPane::Gone(embed));
    };
    // The document first: nothing to show without it.
    let (title, hint, can_relayout, mut doc) = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(ToPane::Print { title, hint, can_relayout, fd }) => (title, hint, can_relayout, fd),
        _ => {
            eprintln!("prelum-sheet: {who}: no document came");
            return gone(&tx);
        }
    };
    let desc = WindowDesc {
        hints: fresco_protocol::WindowHints { embed_pending: true, ..Default::default() },
        ..WindowDesc::new("Print", SizeSpec::Fixed(view::W as u32, view::H as u32))
    };
    let mut win = match fresco_client::Connection::connect(&fresco).and_then(|c| Window::open_on(c, &desc)) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("prelum-sheet: {who}: window: {e}");
            return gone(&tx);
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let state = Mutable::with_dirty(Sheet::new(&title, can_relayout, hint), dirty.clone());
    let v = SheetView::new(state.clone());
    let mut app = App::new_with_flag(v.clone(), dirty);
    let _ = win.paint(&mut app);

    let (done_tx, done_rx) = mpsc::channel::<Done>();
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let stop = Arc::new(AtomicBool::new(false));
    let nearby: u8 = env("PRELUM_SHEET_NEARBY", "4").parse().unwrap_or(4).min(10);
    {
        let (p, d, s) = (prelum.clone(), done_tx.clone(), stop.clone());
        std::thread::spawn(move || lister(p, app_uid, nearby, d, s));
        let (p, d) = (prelum.clone(), done_tx.clone());
        std::thread::spawn(move || doer(p, app_uid, job_rx, d));
    }

    let set = |f: &dyn Fn(&mut Sheet)| {
        let mut s = state.get_cloned();
        f(&mut s);
        state.set(s);
    };
    let mut ready = false;
    let mut pin: Option<PrinterCredentialPrompt> = None;
    let mut guarded = (0.0f32, 0.0f32);
    // Previews: what was last asked for, and when settings last changed.
    let mut gen = 0u64;
    let mut asked_for: Option<(Layout, u32, u64)> = None;
    let mut changed_at = Instant::now();
    let mut last_layout: Option<Layout> = None;
    let mut doc_gen = 0u64;
    let mut first_preview = true;
    let mut relayout_sent: Option<ToParent> = None;
    let until = Instant::now() + SHEET_FOR;
    let end = |tx: &Sender<FromPane>, stop: &AtomicBool, m: ToParent| {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(FromPane::End(embed, m));
    };
    loop {
        // Escape is Cancel.
        let mut escape = false;
        let stepped = win.step_with(&mut app, Some(Duration::from_millis(30)), |ev| {
            let esc = matches!(ev, fresco_client::Event::Key { hid_usage: 0x29, pressed: true, .. });
            escape |= esc;
            esc
        });
        match stepped {
            Ok(StepResult::Closed) | Err(_) => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                return gone(&tx);
            }
            _ => {}
        }
        if escape {
            eprintln!("prelum-sheet: {who}: Escape");
            return end(&tx, &stop, ToParent::Cancelled);
        }
        if Instant::now() > until {
            return end(&tx, &stop, ToParent::Cancelled);
        }
        if !ready {
            if let Some(token) = win.embed_token() {
                ready = true;
                let _ = tx.send(FromPane::Ready(embed, token));
            }
        }
        // The guard: the Print button, in the window's pixels.
        let k = win.scale();
        if guarded != (k, win.width) {
            guarded = (k, win.width);
            let b = view::print_button();
            let r = [(b.x() * k).ceil() as i32, (b.y() * k).ceil() as i32, (b.w() * k).floor() as i32, (b.h() * k).floor() as i32];
            let id = win.id();
            if let Err(e) = win.connection().window_guard(id, Some(r)) {
                eprintln!("prelum-sheet: {who}: guard: {e}");
            }
        }
        match rx.try_recv() {
            Ok(ToPane::Close) | Err(mpsc::TryRecvError::Disconnected) => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            Ok(ToPane::Document(fd)) => {
                doc = fd;
                doc_gen += 1;
                changed_at = Instant::now() - SETTLE;
            }
            Ok(ToPane::Print { .. }) | Err(mpsc::TryRecvError::Empty) => {}
        }
        // The range typed so far.
        let typed = v.range.get_cloned().text().to_string();
        if state.get_cloned().range != typed {
            set(&|s| s.range = typed.clone());
        }
        let s = state.get_cloned();
        // An app that lays itself out: a new paper or orientation goes to it.
        if let Some(r) = s.relayout() {
            if relayout_sent.as_ref() != Some(&r) {
                // The first one only if the app's own paper isn't it.
                let theirs = hint.map(|p| ToParent::Relayout { paper: p, orientation: limen::print::Orientation::Portrait, margins_pt: 36 });
                if relayout_sent.is_some() || theirs.as_ref() != Some(&r) {
                    let _ = tx.send(FromPane::Emit(embed, r.clone()));
                }
                relayout_sent = Some(r);
            }
        }
        let layout = s.layout();
        if last_layout.as_ref() != Some(&layout) {
            last_layout = Some(layout.clone());
            changed_at = Instant::now();
        }
        // The sheets around the one shown, once settings settle.
        let first = ((s.showing.max(1) - 1) / BATCH) * BATCH + 1;
        let want = (layout.clone(), first, doc_gen);
        if asked_for.as_ref() != Some(&want) && changed_at.elapsed() >= SETTLE {
            match doc.try_clone() {
                Ok(fd) => {
                    gen += 1;
                    let _ = job_tx.send(Job::Preview { req: PreviewReq { layout: layout.clone(), first, count: BATCH as u8, longest: LONGEST }, fd, gen });
                    asked_for = Some(want);
                }
                Err(e) => eprintln!("prelum-sheet: {who}: {e}"),
            }
        }

        // What prelumd said.
        while let Ok(d) = done_rx.try_recv() {
            match d {
                Done::Printers(p) => set(&|s| s.set_printers(p.clone())),
                Done::Nearby(n) => set(&|s| s.set_nearby(n.clone())),
                Done::Preview { gen: g, r } if g == gen => match r {
                    Ok(p) => {
                        if first_preview {
                            eprintln!("prelum-sheet: {who}: previews: {} of {} sheets", p.images.len(), p.sheets);
                        }
                        first_preview = false;
                        let pics: Vec<(u32, pergola::node::Image)> = p.images.iter().filter_map(|i| view::picture(i.w, i.h, &i.rgb).map(|im| (i.sheet, im))).collect();
                        v.pictures.set(pics);
                        set(&|s| {
                            s.sheets = p.sheets;
                            s.showing = s.showing.clamp(1, p.sheets.max(1));
                            s.images = p.images.clone();
                            s.preview_note = None;
                        });
                    }
                    Err(e) => {
                        if first_preview {
                            if let Some(w) = why(&e) {
                                eprintln!("prelum-sheet: {who}: {e}");
                                return end(&tx, &stop, ToParent::Failed(w));
                            }
                        }
                        v.pictures.set(Vec::new());
                        set(&|s| s.preview_note = Some(format!("No preview: {e}")));
                    }
                },
                Done::Preview { .. } => {}
                Done::Printed(Ok(())) => {
                    eprintln!("prelum-sheet: {who}: queued");
                    return end(&tx, &stop, ToParent::Queued);
                }
                Done::Printed(Err(e)) | Done::Saved(Err(e)) => set(&|s| {
                    s.working = None;
                    s.notice = Some((e.clone(), Tone::Error));
                }),
                Done::Saved(Ok((file, name))) => {
                    let _ = tx.send(FromPane::EmitFd(embed, ToParent::SavePdf { name }, file));
                    eprintln!("prelum-sheet: {who}: saved as PDF, to the app");
                    return end(&tx, &stop, ToParent::Saved);
                }
                Done::Broken(e) => set(&|s| {
                    s.listed = true;
                    s.notice = Some((e.clone(), Tone::Error));
                }),
            }
        }

        let selected = state.get_cloned();
        if selected.pin_printer.is_some() && !selected.pin_requested() && selected.working.is_none() {
            pin = None;
            set(&|s| s.pin_printer = None);
        }
        if let Some(prompt) = &mut pin {
            let result = prompt.poll(k);
            match result {
                Ok(Some(credential::ToParent::PrintLoginSaved)) if prompt.saving_login_since.is_some() => {
                    pin = None;
                    v.login_account.set(pergola::text_edit::TextEdit::new(""));
                    set(&|s| {
                        s.login_printer = None;
                        s.working = None;
                        s.notice = Some(("Printer login saved.".into(), Tone::Quiet));
                    });
                }
                Ok(Some(credential::ToParent::Cleared)) if prompt.saving_login_since.take().is_some() => {
                    set(&|s| {
                        s.working = None;
                        s.notice = Some(("Arca could not save this login. Check the username, password and lock state.".into(), Tone::Error));
                    });
                }
                Ok(Some(credential::ToParent::Handle { handle })) => {
                    if let Some((print, fd, _)) = prompt.pending.take() {
                        let _ = job_tx.send(Job::PrintPin { print, fd, handle });
                    }
                }
                Ok(Some(credential::ToParent::Cleared)) if prompt.pending.take().is_some() => {
                    set(&|s| {
                        s.working = None;
                        let message = match s.pin_choice {
                            PinChoice::Saved => "No saved PIN is available. Unlock Arca or choose another PIN option.",
                            PinChoice::Save => "Arca could not save this PIN. Enter digits and check that Arca is unlocked.",
                            PinChoice::Once => "Enter a PIN in Arca and try again.",
                        };
                        s.notice = Some((message.into(), Tone::Warning));
                    });
                }
                Err(message) => {
                    pin = None;
                    set(&|s| {
                        s.working = None;
                        s.notice = Some((message.clone(), Tone::Error));
                    });
                }
                _ if prompt.pending.as_ref().is_some_and(|(_, _, at)| at.elapsed() > Duration::from_secs(8))
                    || prompt.saving_login_since.is_some_and(|at| at.elapsed() > Duration::from_secs(8)) => {
                    pin = None;
                    set(&|s| {
                        s.working = None;
                        s.notice = Some(("Arca did not answer. Close the credential field and try again.".into(), Tone::Error));
                    });
                }
                _ => {}
            }
        }

        // What the person did.
        let asked = v.asked.get_cloned();
        if asked != Asked::default() {
            v.asked.set(Asked::default());
            if asked.cancel {
                if state.get_cloned().login_printer.is_some() {
                    pin = None;
                    v.login_account.set(pergola::text_edit::TextEdit::new(""));
                    set(&|s| { s.login_printer = None; s.working = None; s.notice = None; });
                    continue;
                }
                return end(&tx, &stop, ToParent::Cancelled);
            }
            if asked.open_login && state.get_cloned().working.is_none() {
                if let (Dest::Saved(printer), Some(window)) = (state.get_cloned().dest(), win.embed_token()) {
                    match PrinterCredentialPrompt::open(&prelum, app_uid, printer.id, window, k, true) {
                        Ok(prompt) => {
                            pin = Some(prompt);
                            set(&|s| { s.login_printer = Some(printer.id); s.notice = None; });
                        }
                        Err(message) => set(&|s| s.notice = Some((message.clone(), Tone::Error))),
                    }
                }
            }
            if asked.toggle_pin && state.get_cloned().working.is_none() {
                if state.get_cloned().pin_requested() {
                    pin = None;
                    set(&|s| { s.pin_printer = None; s.notice = None; });
                } else if let (Dest::Saved(printer), Some(window)) = (state.get_cloned().dest(), win.embed_token()) {
                    match PrinterCredentialPrompt::open(&prelum, app_uid, printer.id, window, k, false) {
                        Ok(prompt) => {
                            pin = Some(prompt);
                            set(&|s| { s.pin_printer = Some(printer.id); s.pin_choice = PinChoice::Once; s.notice = None; });
                        }
                        Err(message) => set(&|s| s.notice = Some((message.clone(), Tone::Error))),
                    }
                }
            }
            if let Some(token) = asked.print {
                let s = state.get_cloned();
                if s.working.is_some() {
                    continue;
                }
                if s.login_printer.is_some() {
                    let account = v.login_account.get_cloned().text().to_owned();
                    if account.is_empty() || account.len() > 512 || account.contains(':') || account.chars().any(char::is_control) {
                        set(&|s| s.notice = Some(("Enter a valid printer username.".into(), Tone::Warning)));
                        continue;
                    }
                    if counted(token).is_err() {
                        set(&|s| s.notice = Some(("That press didn’t count. Press Save login again.".into(), Tone::Warning)));
                        continue;
                    }
                    if let Some(prompt) = pin.as_mut().filter(|p| p.config.is_none()) {
                        if prompt.parent.send(prompt.embed, credential::ToChild::SavePrintLogin { account }.encode()).is_ok() {
                            prompt.saving_login_since = Some(Instant::now());
                            set(&|s| { s.notice = None; s.working = Some("Saving in Arca…"); });
                            continue;
                        }
                    }
                    set(&|s| s.notice = Some(("The Arca field is unavailable. Go back and open Printer login again.".into(), Tone::Error)));
                    continue;
                }
                match s.action() {
                    Err(e) => set(&|s| s.notice = Some((e.clone(), Tone::Error))),
                    Ok(action) => match counted(token) {
                        Err(why) => {
                            eprintln!("prelum-sheet: {who}: a press on Print not counted: {why}");
                            set(&|s| s.notice = Some(("That press didn't count: the sheet was covered or had just moved. Press again.".into(), Tone::Warning)));
                        }
                        Ok(()) => match doc.try_clone() {
                            Ok(fd) => {
                                if s.pin_requested() {
                                    let request = match s.pin_choice {
                                        PinChoice::Once => credential::ToChild::SubmitPrintPin,
                                        PinChoice::Save => credential::ToChild::SavePrintPin,
                                        PinChoice::Saved => credential::ToChild::UseSavedPrintPin,
                                    };
                                    if let (Some(prompt), Action::Print(print)) = (&mut pin, &action) {
                                        if prompt.printer == print.printer && prompt.config.is_none()
                                            && prompt.parent.send(prompt.embed, request.encode()).is_ok()
                                        {
                                            prompt.pending = Some((print.clone(), fd, Instant::now()));
                                            set(&|s| { s.notice = None; s.working = Some("Waiting for Arca…"); });
                                            continue;
                                        }
                                    }
                                    set(&|s| s.notice = Some(("The Arca PIN field is unavailable. Go back and open the PIN field again.".into(), Tone::Error)));
                                    continue;
                                }
                                let saving = matches!(action, Action::Save { .. });
                                set(&|s| {
                                    s.notice = None;
                                    s.working = Some(if saving { "Preparing the PDF…" } else { "Sending…" });
                                });
                                let _ = job_tx.send(Job::Print { action, fd });
                            }
                            Err(e) => set(&|s| s.notice = Some((e.to_string(), Tone::Error))),
                        },
                    },
                }
            }
        }
    }
}
