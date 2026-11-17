//! `atrium-mail`: Mail's window over the engine's store (mail.md §15.2).
//! It reads what sync has stored; HTML bodies are drawn by a jailed
//! doc-viewer in a slot, and remote pictures, only when asked, come
//! through Mail's own Nuntius fetcher (§7.4; the manifest holds
//! `net-fetch`). Each message being written has a compose window of its
//! own (§9.1); `mailto:` links from other apps arrive on the document
//! channel and open one. The separate engine and sync processes come in
//! later milestones (§16).
//!
//! Environment:
//!   MAIL_STORE        [$HOME/.local/share/atrium-mail] the engine's store
//!   MAIL_PARSE_JAIL   [mail-parse-jail beside this program] the parse jail
//!   MAIL_TRACE        print the window's state after each change (VM gates)

use std::fs::File;
use std::io::{Seek, Write};
use std::os::fd::{AsFd, FromRawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use atrium_mail::compose::{ComposeView, Composer, Done};
use atrium_mail::view::FileAct;
use atrium_mail::MailView;
use mail_engine::{Engine, Parser};
use mail_store::Store;
use pergola::window::StepResult;
use pergola::{App, SizeSpec, Window, WindowDesc};

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Bytes as an anonymous file for the doc-viewer, read from the start.
fn anon(bytes: &[u8]) -> std::io::Result<File> {
    let fd = unsafe { libc::memfd_create(c"mail".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut f = unsafe { File::from_raw_fd(fd) };
    f.write_all(bytes)?;
    f.rewind()?;
    Ok(f)
}

/// The doc-viewer showing a message's HTML body.
struct Frame {
    msg: u64,
    doc: atrium_mail::view::Doc,
    embed: limen::proto::EmbedId,
    dark: bool,
}

/// Remote pictures (§7.4) are fetched on a thread of their own, through a
/// fetcher started the first time they are asked for: (message, what to
/// fetch) in, (message, what came whole) out.
struct Loader {
    jobs: mpsc::Sender<(u64, Vec<(String, String)>)>,
    done: mpsc::Receiver<(u64, Vec<(String, Vec<u8>)>)>,
}

fn loader() -> Loader {
    let (jobs, rx) = mpsc::channel::<(u64, Vec<(String, String)>)>();
    let (tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        let mut fetcher: Option<(nuntius::Client, nuntius::launch::FetcherGuard)> = None;
        for (msg, want) in rx {
            if fetcher.as_ref().is_some_and(|(_, g)| g.exited()) {
                fetcher = None;
            }
            if fetcher.is_none() {
                match nuntius::launch::open() {
                    Ok(f) => fetcher = Some(f.into_parts()),
                    Err(e) => eprintln!("atrium-mail: no fetcher for pictures: {e}"),
                }
            }
            let got = match fetcher.as_mut() {
                Some((c, _)) => mail_engine::pictures::fetch(c, &want),
                None => Vec::new(),
            };
            if tx.send((msg, got)).is_err() {
                break;
            }
        }
    });
    Loader { jobs, done }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Posts to Praeco (§12) from a thread of their own (praecod may be slow or
/// absent; Mail never waits on it). Remembers which post was for which
/// message, to withdraw it when read elsewhere, and replaces the last
/// "while you were away" summary rather than stacking another.
fn notifications() -> mpsc::Sender<mail_engine::notify::Post> {
    use mail_engine::notify::Post;
    use praeco::proto::urgency;
    let (tx, rx) = mpsc::channel::<Post>();
    std::thread::spawn(move || {
        let sock = std::path::PathBuf::from(std::env::var("PRAECO_SOCKET").unwrap_or_else(|_| "/var/run/atrium/praecod.sock".into()));
        let mut by_msg: std::collections::HashMap<u64, u64> = Default::default();
        let mut summary: Option<u64> = None;
        let mut said = false;
        for p in rx {
            let r = match p {
                Post::One { msg, title, body } => praeco::client::post(&sock, urgency::NORMAL, &title, &body).map(|id| {
                    by_msg.insert(msg, id);
                }),
                Post::Group { title, body } => praeco::client::post(&sock, urgency::NORMAL, &title, &body).map(|_| ()),
                Post::Review { title, body } => praeco::client::post(&sock, urgency::LOW, &title, &body).map(|_| ()),
                Post::Summary { title, body } => {
                    if let Some(old) = summary.take() {
                        let _ = praeco::client::withdraw(&sock, old);
                    }
                    praeco::client::post(&sock, urgency::LOW, &title, &body).map(|id| summary = Some(id))
                }
                Post::Withdraw { msg } => match by_msg.remove(&msg) {
                    Some(id) => praeco::client::withdraw(&sock, id),
                    None => Ok(()),
                },
            };
            if let Err(e) = r {
                if !said {
                    eprintln!("atrium-mail: notifications: {e}");
                    said = true;
                }
            }
        }
    });
    tx
}

/// Hand the doc-viewer pictures by resource id (before `Load`, for its
/// first layout; after, it lays out again).
fn provide(embeds: &std::rc::Rc<std::cell::RefCell<pergola::embed::Embeds>>, e: limen::proto::EmbedId, pictures: &[(String, Vec<u8>)]) -> Result<(), String> {
    use limen::doc_viewer::ToChild;
    for (id, bytes) in pictures {
        let f = anon(bytes).map_err(|e| e.to_string())?;
        embeds.borrow_mut().send_fd(e, ToChild::Provide { id: id.clone() }.encode(), f.as_fd()).map_err(|e| format!("{e:?}"))?;
    }
    Ok(())
}

/// Ask for a doc-viewer in the frame slot and hand it the document: its
/// pictures first (`Provide`: the message's own, and remote ones already
/// loaded), then the HTML (`Load`).
fn show(embeds: &std::rc::Rc<std::cell::RefCell<pergola::embed::Embeds>>, token: [u8; 16], slot: pergola::slot::SlotKey, doc: &mail_schema::Document, remote: &[(String, Vec<u8>)], dark: bool) -> Result<limen::proto::EmbedId, String> {
    use limen::doc_viewer::{DocKind, ToChild};
    let e = embeds.borrow_mut().request("doc-viewer", token, slot).map_err(|e| format!("{e:?}"))?;
    let send = || -> Result<(), String> {
        provide(embeds, e, &doc.pictures)?;
        provide(embeds, e, remote)?;
        let f = anon(doc.html.as_bytes()).map_err(|e| e.to_string())?;
        embeds.borrow_mut().send_fd(e, ToChild::Load { kind: DocKind::Html, base: String::new(), dark }.encode(), f.as_fd()).map_err(|e| format!("{e:?}"))
    };
    if let Err(err) = send() {
        let _ = embeds.borrow_mut().detach_embed(e);
        return Err(err);
    }
    Ok(e)
}

/// Quick look (§8.1): the attachment in a viewer in the look slot, handed
/// over as a sealed read-only memory file (never a file on disk).
fn look(embeds: &std::rc::Rc<std::cell::RefCell<pergola::embed::Embeds>>, engine: &Engine, token: [u8; 16], msg: u64, f: &atrium_mail::files::File, dark: bool) -> Result<limen::proto::EmbedId, String> {
    let atrium_mail::files::Look::Viewer { role, mime } = f.look else { return Err("no viewer for this".into()) };
    let a = engine.attachment(msg, &f.which).map_err(|e| format!("couldn't read it: {e}"))?;
    let fd = atrium_mail::files::sealed(&a.bytes).map_err(|e| e.to_string())?;
    drop(a);
    let load = match role {
        "doc-viewer" => {
            let kind = if mime.starts_with("image/") { limen::doc_viewer::DocKind::Picture } else { limen::doc_viewer::DocKind::Html };
            limen::doc_viewer::ToChild::Load { kind, base: String::new(), dark }.encode()
        }
        "pdf-viewer" => limen::pdf_viewer::ToChild::Load { dark }.encode(),
        _ => limen::media_player::ToChild::Load { source: limen::media_player::Source::Fd, mime: mime.into(), growing: false }.encode(),
    };
    let asked = embeds.borrow_mut().request(role, token, atrium_mail::view::LOOK_SLOT);
    let e = asked.map_err(|e| match e {
        limen::client::Error::Refused(limen::proto::refusal::NO_PROVIDER) => "No app is set to show these".to_string(),
        limen::client::Error::Refused(limen::proto::refusal::LAUNCH_FAILED) => "The viewer didn't start".to_string(),
        limen::client::Error::Refused(n) => format!("The viewer was refused (Limen {n})"),
        limen::client::Error::Io(e) => format!("Limen: {e}"),
    })?;
    let sent = embeds.borrow_mut().send_fd(e, load, fd.as_fd());
    if let Err(err) = sent {
        let _ = embeds.borrow_mut().detach_embed(e);
        return Err(format!("{err:?}"));
    }
    Ok(e)
}

/// A compose window and its state.
struct ComposeWin {
    win: Window,
    app: App<ComposeView>,
    c: Composer,
    size: pergola::Mutable<(f32, f32)>,
    /// The doc-viewer showing the original under a reply or forward.
    quote: Option<limen::proto::EmbedId>,
    /// The original couldn't be shown; not asked again.
    quote_failed: bool,
    /// The system picker, while files are attached.
    attach: Option<limen::proto::EmbedId>,
}

/// Open a compose window for `c` (`draft`: the stored draft it continues).
fn compose_window(engine: &Rc<Engine>, embeds: Option<&Rc<std::cell::RefCell<pergola::embed::Embeds>>>, c: mail_engine::compose::Compose, draft: Option<u64>) -> std::io::Result<ComposeWin> {
    let title = if c.draft.subject.trim().is_empty() { "New message".to_string() } else { c.draft.subject.clone() };
    let mut win = Window::open(&WindowDesc::new(&title, SizeSpec::Fixed(760, 640)))?;
    if let Some(e) = embeds {
        win.set_slot_binder(pergola::embed::Embeds::binder(e));
    }
    let dirty = Arc::new(AtomicBool::new(true));
    let k = Composer::new(engine.clone(), c, draft, dirty.clone(), now);
    let view = ComposeView::new(k.clone(), (win.width, win.height));
    let size = view.size.clone();
    Ok(ComposeWin { win, app: App::new_with_flag(view, dirty), c: k, size, quote: None, quote_failed: false, attach: None })
}

/// The drafts whose windows were open, so a crash or a restart opens them
/// again (`open-drafts` in the store's folder, one id per line).
fn remember_open(store_dir: &std::path::Path, wins: &[ComposeWin]) {
    let ids: Vec<String> = wins.iter().filter_map(|w| w.c.ui.get_cloned().draft_id).map(|i| i.to_string()).collect();
    let _ = std::fs::write(store_dir.join("open-drafts"), ids.join("\n"));
}

/// `mailto:` links from Monstrator, read on a thread (§9.1).
fn links() -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        let mut ch = match atrium_open::Channel::connect() {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                eprintln!("atrium-mail: the document channel: {e}");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        loop {
            match ch.recv_any(None) {
                Ok(Some(atrium_open::Message::Link(url))) if url.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")) => {
                    if tx.send(url).is_err() {
                        return;
                    }
                }
                Ok(Some(atrium_open::Message::Link(_))) => eprintln!("atrium-mail: a link refused: not a mailto: link"),
                Ok(Some(atrium_open::Message::File(_))) => eprintln!("atrium-mail: a file on the document channel: Mail opens none"),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("atrium-mail: the document channel: {e}");
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    });
    rx
}

fn main() {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let store_dir = std::env::var_os("MAIL_STORE").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share/atrium-mail"));
    let jail = std::env::var_os("MAIL_PARSE_JAIL").map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("mail-parse-jail"))).unwrap_or_else(|| PathBuf::from("mail-parse-jail"))
    });
    let store = match Store::open(&store_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("atrium-mail: store {}: {e}", store_dir.display());
            std::process::exit(1);
        }
    };
    let engine = Rc::new(Engine::new(store, Parser { program: jail }));
    // A send Mail was in the middle of when it stopped may have gone: it
    // waits for the person instead of going twice.
    match engine.recover_sends() {
        Ok(0) => {}
        Ok(n) => eprintln!("atrium-mail: {n} message(s) may not have been sent; they wait in the outbox"),
        Err(e) => eprintln!("atrium-mail: outbox: {e}"),
    }
    let loads = loader();
    let incoming = links();

    let mut win = match Window::open(&WindowDesc::new("Mail", SizeSpec::Fixed(1280, 800))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("atrium-mail: frescod: {e}");
            std::process::exit(1);
        }
    };
    // HTML bodies are drawn by a jailed doc-viewer in a slot (§7.3); without
    // limend Mail still reads plain text.
    let embeds = match pergola::embed::Embeds::connect() {
        Ok(e) => {
            win.set_slot_binder(pergola::embed::Embeds::binder(&e));
            Some(e)
        }
        Err(e) => {
            eprintln!("atrium-mail: limend: {e:?}; HTML bodies won't show");
            None
        }
    };
    let mut frame: Option<Frame> = None;
    // The quick look viewer: which attachment it shows.
    let mut looked: Option<(u64, mail_schema::Which, limen::proto::EmbedId)> = None;
    // What became of Open and Save, from their threads.
    let (file_done_tx, file_done) = mpsc::channel::<String>();
    // One-press unsubscribes (§11.3): each a POST through a fetcher of its
    // own, on a thread; (message, what came of it) back.
    let (unsub_tx, unsub_done) = mpsc::channel::<(u64, Result<(), String>)>();
    // The picker up for a Save: its embed, and what it saves.
    let mut saving: Option<(limen::proto::EmbedId, u64, atrium_mail::files::File)> = None;
    // A message the viewer couldn't be asked for, not asked again.
    let mut failed: Option<u64> = None;
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine.clone(), dirty.clone(), (win.width, win.height), now);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    let trace = std::env::var_os("MAIL_TRACE").is_some();
    let mut last = String::new();
    let mut refreshed = Instant::now();
    let mut composes: Vec<ComposeWin> = Vec::new();
    // Drafts whose windows were open last time open again.
    let reopen: Vec<u64> = std::fs::read_to_string(store_dir.join("open-drafts")).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect();
    for (account, _) in engine.store.accounts().unwrap_or_default() {
        for (id, d) in engine.drafts(account).unwrap_or_default() {
            if reopen.contains(&id) {
                mail.open_draft(account, id, d);
            }
        }
    }
    let mut notifier = mail_engine::notify::Notifier::new();
    let notes = notifications();
    let mut ticked = Instant::now();
    loop {
        // Mail hears j/k and the arrows before the list does, so a move of
        // the cursor doesn't also scroll the list.
        let first = |ev: &fresco_client::Event| {
            // A key's activation token is its press (Enter on the link bar).
            let activation = match ev {
                fresco_client::Event::Key { pressed: true, activation, .. } if *activation != [0u8; 16] => Some(*activation),
                _ => None,
            };
            pergola::input::translate(ev).is_some_and(|e| mail.key(&e, activation))
        };
        let wait = if composes.is_empty() { 250 } else { 16 };
        match win.step_with(&mut app, Some(Duration::from_millis(wait)), first) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("atrium-mail: frescod: {e}");
                break;
            }
        }
        mail.set_size(win.width, win.height);
        mail.poll_link();
        mail.tick();
        // Compose windows: links from other apps, replies, Undo of a send.
        while let Ok(url) = incoming.try_recv() {
            mail.mailto(&url);
        }
        let asked = mail.take_composes();
        let opened = !asked.is_empty();
        for (c, draft) in asked {
            match compose_window(&engine, embeds.as_ref(), c, draft) {
                Ok(w) => composes.push(w),
                Err(e) => eprintln!("atrium-mail: a compose window: {e}"),
            }
        }
        let save = ticked.elapsed() > Duration::from_secs(1);
        if save {
            ticked = Instant::now();
        }
        let before = composes.len();
        composes.retain_mut(|cw| {
            let c = cw.c.clone();
            let key = |ev: &fresco_client::Event| {
                let activation = match ev {
                    fresco_client::Event::Key { pressed: true, activation, .. } if *activation != [0u8; 16] => Some(*activation),
                    _ => None,
                };
                pergola::input::translate(ev).is_some_and(|e| c.key(&e, activation))
            };
            match cw.win.step_with(&mut cw.app, Some(Duration::ZERO), key) {
                Ok(StepResult::Closed) => cw.c.close(),
                Ok(_) => {}
                Err(e) => {
                    eprintln!("atrium-mail: a compose window: {e}");
                    cw.c.close();
                }
            }
            cw.size.set((cw.win.width, cw.win.height));
            if save {
                cw.c.tick();
            }
            // Attach…: the system picker over the window, files to read.
            if cw.c.take_attach_ask() && cw.attach.is_none() {
                match (&embeds, cw.win.embed_token()) {
                    (Some(e), Some(token)) => {
                        let filter = limen::picker::Filter { multiple: true, title: "Attach files".into(), ..limen::picker::Filter::file() };
                        let asked = e.borrow_mut().request("picker", token, atrium_mail::compose::ATTACH_SLOT);
                        match asked.and_then(|id| e.borrow_mut().send(id, limen::picker::ToChild::Open(filter).encode()).map(|_| id)) {
                            Ok(id) => {
                                cw.attach = Some(id);
                                cw.c.set_attaching(true);
                            }
                            Err(err) => cw.c.ui.set(atrium_mail::compose::ComposeUi { error: Some(format!("The picker didn't open: {err:?}")), ..cw.c.ui.get_cloned() }),
                        }
                    }
                    _ => cw.c.ui.set(atrium_mail::compose::ComposeUi { error: Some("Attaching needs the system picker, which isn't running".into()), ..cw.c.ui.get_cloned() }),
                }
            }
            // The original under a reply, in a jailed doc-viewer.
            if let Some(embeds) = &embeds {
                let original = cw.c.original.borrow().clone();
                match (&original, cw.quote) {
                    (Some(doc), None) if !cw.quote_failed => {
                        if let Some(token) = cw.win.embed_token() {
                            match show(embeds, token, atrium_mail::compose::QUOTE_SLOT, doc, &[], mail.is_dark()) {
                                Ok(e) => cw.quote = Some(e),
                                Err(e) => {
                                    eprintln!("atrium-mail: the quoted message: {e}");
                                    cw.quote_failed = true;
                                }
                            }
                        }
                    }
                    (None, Some(e)) => {
                        let _ = embeds.borrow_mut().detach_embed(e);
                        cw.quote = None;
                    }
                    _ => {}
                }
                if cw.c.done().is_some() {
                    if let Some(e) = cw.quote.take() {
                        let _ = embeds.borrow_mut().detach_embed(e);
                    }
                    if let Some(e) = cw.attach.take() {
                        let _ = embeds.borrow_mut().detach_embed(e);
                    }
                }
            }
            match cw.c.done() {
                None => true,
                Some(Done::Sent { send, label }) => {
                    mail.sent(send, label);
                    false
                }
                Some(Done::Discarded | Done::Kept) => false,
            }
        });
        if opened || save || composes.len() != before {
            remember_open(&store_dir, &composes);
        }
        // Pictures asked for go to the fetch thread; what came back is kept
        // and, when its message is in the viewer, handed to it.
        for msg in mail.take_loads() {
            let want = engine.pictures_to_load(msg).unwrap_or_default();
            if loads.jobs.send((msg, want)).is_err() {
                mail.pictures_loaded(msg, Vec::new());
            }
        }
        while let Ok((msg, got)) = loads.done.try_recv() {
            let ids: Vec<String> = got.iter().map(|g| g.0.clone()).collect();
            mail.pictures_loaded(msg, got);
            if let (Some(embeds), Some(f)) = (&embeds, frame.as_ref()) {
                let same = mail.framed().is_some_and(|(m, d, _)| m == msg && d == f.doc);
                if f.msg == msg && same {
                    let kept: Vec<_> = engine.pictures(msg).unwrap_or_default().into_iter().filter(|(id, _)| ids.contains(id)).collect();
                    if let Err(e) = provide(embeds, f.embed, &kept) {
                        eprintln!("atrium-mail: doc-viewer: {e}");
                    }
                }
            }
        }
        while let Ok(note) = file_done.try_recv() {
            mail.file_note(note);
        }
        for (msg, url) in mail.take_unsubscribes() {
            let tx = unsub_tx.clone();
            std::thread::spawn(move || {
                let r = match nuntius::launch::open() {
                    Ok(f) => {
                        let (mut c, _guard) = f.into_parts();
                        mail_engine::senders::post(&mut c, &url)
                    }
                    Err(e) => Err(format!("no fetcher ({e})")),
                };
                let _ = tx.send((msg, r));
            });
        }
        while let Ok((msg, r)) = unsub_done.try_recv() {
            mail.unsubscribed(msg, r);
        }
        for ask in mail.take_file_asks() {
            match ask.act {
                // The viewer follows `looking()` below.
                FileAct::Look => {}
                FileAct::Open { activation } => match engine.attachment(ask.msg, &ask.file.which).map_err(|e| e.to_string()).and_then(|a| atrium_mail::files::sealed(&a.bytes).map_err(|e| e.to_string())) {
                    // The hand-over may start an app: off the window's thread.
                    Ok(fd) => {
                        mail.file_note(format!("Opening {}…", ask.file.name));
                        let (tx, name) = (file_done_tx.clone(), ask.file.name.clone());
                        std::thread::spawn(move || {
                            let r = atrium_mail::files::open_in_app(&name, activation, &fd);
                            let _ = tx.send(match r {
                                Ok(app) => format!("Opened {name} in {app}"),
                                Err(why) => format!("Couldn't open {name}: {why}"),
                            });
                        });
                    }
                    Err(e) => mail.file_note(format!("Couldn't open {}: {e}", ask.file.name)),
                },
                FileAct::Save if saving.is_some() => mail.file_note("Already saving a file".into()),
                FileAct::Save => match (&embeds, win.embed_token()) {
                    (Some(e), Some(token)) => {
                        let origin = engine.origin(ask.msg).unwrap_or_else(|_| "mail".into());
                        let filter = limen::picker::Filter::save(&ask.file.name, &origin);
                        let asked = e.borrow_mut().request("picker", token, atrium_mail::view::PICK_SLOT);
                        match asked.and_then(|id| e.borrow_mut().send(id, limen::picker::ToChild::Open(filter).encode()).map(|_| id)) {
                            Ok(id) => {
                                saving = Some((id, ask.msg, ask.file.clone()));
                                mail.set_saving(true);
                            }
                            Err(err) => mail.file_note(format!("The picker didn't open: {err:?}")),
                        }
                    }
                    _ => mail.file_note("Saving needs the system picker, which isn't running".into()),
                },
            }
        }
        if let Some(embeds) = &embeds {
            let want = mail.looking();
            if want.as_ref().map(|(m, f)| (*m, &f.which)) != looked.as_ref().map(|(m, w, _)| (*m, w)) {
                if let Some((_, _, e)) = looked.take() {
                    let _ = embeds.borrow_mut().detach_embed(e);
                }
                if let (Some((msg, f)), Some(token)) = (want.as_ref(), win.embed_token()) {
                    match look(embeds, &engine, token, *msg, f, mail.is_dark()) {
                        Ok(e) => looked = Some((*msg, f.which.clone(), e)),
                        Err(why) => {
                            eprintln!("atrium-mail: quick look: {why}");
                            mail.look_failed(&why);
                        }
                    }
                }
            }
        } else if mail.looking().is_some() {
            mail.look_failed("Quick look needs Limen, which isn't running");
        }
        if let Some(embeds) = &embeds {
            let want = mail.framed();
            // A new viewer for another message, or for a document converted again.
            if want.as_ref().map(|w| (w.0, &w.1)) != frame.as_ref().map(|f| (f.msg, &f.doc)) {
                if let Some(f) = frame.take() {
                    let _ = embeds.borrow_mut().detach_embed(f.embed);
                }
                if let (Some((msg, doc, dark)), Some(token)) = (want.as_ref(), win.embed_token()) {
                    if failed != Some(*msg) {
                        let remote = engine.pictures(*msg).unwrap_or_default();
                        match show(embeds, token, atrium_mail::view::FRAME_SLOT, &doc.0, &remote, *dark) {
                            Ok(embed) => frame = Some(Frame { msg: *msg, doc: doc.clone(), embed, dark: *dark }),
                            Err(e) => {
                                eprintln!("atrium-mail: doc-viewer: {e}");
                                failed = Some(*msg);
                            }
                        }
                    }
                }
            } else if let (Some(f), Some((_, _, dark))) = (frame.as_mut(), want.as_ref()) {
                if f.dark != *dark {
                    f.dark = *dark;
                    let _ = embeds.borrow_mut().send(f.embed, limen::doc_viewer::ToChild::SetTheme { dark: *dark }.encode());
                }
            }
            loop {
                let ev = embeds.borrow_mut().poll(Duration::from_millis(0));
                match ev {
                    Ok(Some(limen::proto::ParentEvent::Message { embed, bytes })) if frame.as_ref().is_some_and(|f| f.embed == embed) => {
                        match limen::doc_viewer::ToParent::decode(&bytes) {
                            Some(limen::doc_viewer::ToParent::LinkActivated { url }) => mail.link(&url),
                            Some(limen::doc_viewer::ToParent::Error { code }) => eprintln!("atrium-mail: doc-viewer error {code}"),
                            _ => {}
                        }
                    }
                    // Files picked for a compose window: read them in.
                    Ok(Some(limen::proto::ParentEvent::MessageFd { embed, bytes })) if composes.iter().any(|c| c.attach == Some(embed)) => {
                        let fd = embeds.borrow_mut().take_fd();
                        let cw = composes.iter().find(|c| c.attach == Some(embed)).unwrap();
                        if let (Some(limen::picker::ToParent::File { name, .. }), Some(fd)) = (limen::picker::ToParent::decode(&bytes), fd) {
                            use std::io::Read;
                            let mut buf = Vec::new();
                            let max = atrium_mail::compose::ATTACH_MAX as u64;
                            match File::from(fd).take(max + 1).read_to_end(&mut buf) {
                                Ok(n) if n as u64 > max => cw.c.ui.set(atrium_mail::compose::ComposeUi { error: Some(format!("“{name}” is too large to attach")), ..cw.c.ui.get_cloned() }),
                                Ok(_) => cw.c.attach(&name, buf),
                                Err(e) => cw.c.ui.set(atrium_mail::compose::ComposeUi { error: Some(format!("Couldn't read “{name}”: {e}")), ..cw.c.ui.get_cloned() }),
                            }
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::Message { embed, bytes })) if composes.iter().any(|c| c.attach == Some(embed)) => {
                        let end = matches!(limen::picker::ToParent::decode(&bytes), Some(limen::picker::ToParent::Done { .. } | limen::picker::ToParent::Cancelled | limen::picker::ToParent::Failed { .. }));
                        if end {
                            let _ = embeds.borrow_mut().detach_embed(embed);
                            if let Some(cw) = composes.iter_mut().find(|c| c.attach == Some(embed)) {
                                cw.attach = None;
                                cw.c.set_attaching(false);
                            }
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed })) if composes.iter().any(|c| c.attach == Some(embed)) => {
                        if let Some(cw) = composes.iter_mut().find(|c| c.attach == Some(embed)) {
                            cw.attach = None;
                            cw.c.set_attaching(false);
                        }
                    }
                    // The picker made the new file: write the attachment into it.
                    Ok(Some(limen::proto::ParentEvent::MessageFd { embed, bytes })) if saving.as_ref().is_some_and(|s| s.0 == embed) => {
                        let fd = embeds.borrow_mut().take_fd();
                        let (_, msg, f) = saving.as_ref().unwrap();
                        match (limen::picker::ToParent::decode(&bytes), fd) {
                            (Some(limen::picker::ToParent::File { name, write: true, .. }), Some(fd)) => {
                                let r = engine.attachment(*msg, &f.which).map_err(|e| e.to_string()).and_then(|a| {
                                    let mut out = File::from(fd);
                                    out.write_all(&a.bytes).and_then(|_| out.sync_all()).map_err(|e| e.to_string())
                                });
                                mail.file_note(match r {
                                    Ok(()) => format!("Saved {name}"),
                                    Err(why) => format!("Couldn't save {name}: {why}"),
                                });
                            }
                            _ => eprintln!("atrium-mail: an odd message with a descriptor from the picker"),
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::Message { embed, bytes })) if saving.as_ref().is_some_and(|s| s.0 == embed) => {
                        let end = match limen::picker::ToParent::decode(&bytes) {
                            Some(limen::picker::ToParent::Done { .. } | limen::picker::ToParent::Cancelled) => true,
                            Some(limen::picker::ToParent::Failed { why }) => {
                                mail.file_note(format!("The picker couldn't save it: {why}"));
                                true
                            }
                            _ => false,
                        };
                        if end {
                            let _ = embeds.borrow_mut().detach_embed(embed);
                            saving = None;
                            mail.set_saving(false);
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed })) if saving.as_ref().is_some_and(|s| s.0 == embed) => {
                        saving = None;
                        mail.set_saving(false);
                    }
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed })) if looked.as_ref().is_some_and(|l| l.2 == embed) => {
                        looked = None;
                        mail.look_failed("The viewer went away");
                    }
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed })) if frame.as_ref().is_some_and(|f| f.embed == embed) => {
                        eprintln!("atrium-mail: the doc-viewer went away");
                        failed = frame.take().map(|f| f.msg);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("atrium-mail: limend: {e:?}");
                        break;
                    }
                }
            }
        }
        // The index changes under the window as sync stores mail.
        if refreshed.elapsed() > Duration::from_secs(5) {
            refreshed = Instant::now();
            mail.refresh();
            // What to tell Praeco, posted from its own thread.
            for p in notifier.check(&engine, now_secs()) {
                let _ = notes.send(p);
            }
        }
        if trace {
            let ui = mail.ui.get_cloned();
            let t = format!(
                "places=[{}] folder={:?} count={} cursor={:?} reading={}",
                ui.places.iter().map(|p| format!("{}:{}", p.label, p.count)).collect::<Vec<_>>().join(","),
                ui.folder,
                ui.count,
                ui.cursor,
                ui.reading.as_ref().map(|r| format!("{:?} msgs={} open={}", r.subject, r.msgs.len(), r.msgs.iter().filter(|m| m.open).count())).unwrap_or("-".into()),
            );
            if t != last {
                eprintln!("atrium-mail: {t}");
                last = t;
            }
        }
    }
}
