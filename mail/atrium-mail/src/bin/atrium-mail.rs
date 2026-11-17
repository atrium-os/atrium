//! `atrium-mail`: Mail's window over the engine's store (mail.md §15.2).
//! M2 reads what sync has stored and shows it as plain text; writing,
//! HTML and the separate engine and sync processes come in later
//! milestones (§16).
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
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    embed: limen::proto::EmbedId,
    dark: bool,
}

/// Ask for a doc-viewer in the frame slot and hand it the document: its
/// pictures first (`Provide`), then the HTML (`Load`).
fn show(embeds: &std::rc::Rc<std::cell::RefCell<pergola::embed::Embeds>>, token: [u8; 16], doc: &mail_schema::Document, dark: bool) -> Result<limen::proto::EmbedId, String> {
    use limen::doc_viewer::{DocKind, ToChild};
    let e = embeds.borrow_mut().request("doc-viewer", token, atrium_mail::view::FRAME_SLOT).map_err(|e| format!("{e:?}"))?;
    let send = || -> Result<(), String> {
        for (id, bytes) in &doc.pictures {
            let f = anon(bytes).map_err(|e| e.to_string())?;
            embeds.borrow_mut().send_fd(e, ToChild::Provide { id: id.clone() }.encode(), f.as_fd()).map_err(|e| format!("{e:?}"))?;
        }
        let f = anon(doc.html.as_bytes()).map_err(|e| e.to_string())?;
        embeds.borrow_mut().send_fd(e, ToChild::Load { kind: DocKind::Html, base: String::new(), dark }.encode(), f.as_fd()).map_err(|e| format!("{e:?}"))
    };
    if let Err(err) = send() {
        let _ = embeds.borrow_mut().detach_embed(e);
        return Err(err);
    }
    Ok(e)
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
    // A message the viewer couldn't be asked for, not asked again.
    let mut failed: Option<u64> = None;
    let dirty = Arc::new(AtomicBool::new(true));
    let view = MailView::new(engine, dirty.clone(), (win.width, win.height), now);
    let mail = view.mail.clone();
    let mut app = App::new_with_flag(view, dirty);
    let trace = std::env::var_os("MAIL_TRACE").is_some();
    let mut last = String::new();
    let mut refreshed = Instant::now();
    loop {
        // Mail hears j/k and the arrows before the list does, so a move of
        // the cursor doesn't also scroll the list.
        let first = |ev: &fresco_client::Event| pergola::input::translate(ev).is_some_and(|e| mail.key(&e));
        match win.step_with(&mut app, Some(Duration::from_millis(250)), first) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("atrium-mail: frescod: {e}");
                break;
            }
        }
        mail.set_size(win.width, win.height);
        if let Some(embeds) = &embeds {
            let want = mail.framed();
            if want.as_ref().map(|w| w.0) != frame.as_ref().map(|f| f.msg) {
                if let Some(f) = frame.take() {
                    let _ = embeds.borrow_mut().detach_embed(f.embed);
                }
                if let (Some((msg, doc, dark)), Some(token)) = (want.as_ref(), win.embed_token()) {
                    if failed != Some(*msg) {
                        match show(embeds, token, &doc.0, *dark) {
                            Ok(embed) => frame = Some(Frame { msg: *msg, embed, dark: *dark }),
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
