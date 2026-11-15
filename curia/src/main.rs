//! Curia, the Settings app (docs/spec/curia.md): the window, and the
//! Network page's calls to atrium-netconf on threads of their own.
//!
//! Environment: `CURIA_NET_DIR` (atrium-netconf's sockets, /atrium/sockets),
//! `CURIA_DEVICES_SOCK` (dispensatord's device list,
//! /atrium/sockets/device-list/device-list.sock),
//! `CURIA_SIZE` (960x720), `CURIA_SECS` (quit after, for checks).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use atrium_netconf::client::Client;
use atrium_netconf::{Grant, Join};
use curia::network::{Got, NetPage, Want};
use curia::view::CuriaView;
use pergola::embed::Embeds;
use pergola::widgets::{CredentialField, CredentialState};
use pergola::window::StepResult;
use pergola::{App, Mutable, SizeSpec, Window, WindowDesc};

fn dir() -> PathBuf {
    std::env::var_os("CURIA_NET_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/atrium/sockets"))
}

fn connect(g: Grant) -> Result<Client, String> {
    Client::connect(&Client::path(&dir(), g)).map_err(|e| format!("{}: {e}", g.name()))
}

/// Status now and on every change; None while atrium-netconf is away.
fn follow(tx: Sender<Got>) {
    std::thread::spawn(move || loop {
        if let Ok(mut ch) = connect(Grant::Wifi).and_then(|c| c.subscribe().map_err(|f| f.1)) {
            while let Ok(s) = ch.next() {
                if tx.send(Got::Status(Some(s))).is_err() {
                    return;
                }
            }
        }
        if tx.send(Got::Status(None)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(3));
    });
}

/// The device list now and on every change (device-assignment.md §6.10).
fn follow_devices(tx: Sender<Result<Vec<atrium_devices::Device>, String>>) {
    let sock = std::env::var_os("CURIA_DEVICES_SOCK").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/atrium/sockets/device-list/device-list.sock"));
    std::thread::spawn(move || loop {
        let why = match atrium_devices::client::Changes::open(&sock) {
            Ok(mut ch) => loop {
                if let Err(e) = ch.next() {
                    break e;
                }
                let r = atrium_devices::client::list(&sock).map_err(|e| e.to_string());
                if tx.send(r).is_err() {
                    return;
                }
            },
            Err(e) => e,
        };
        let msg = match why.kind() {
            std::io::ErrorKind::PermissionDenied => "only the person using this machine may see it".to_string(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => "dispensatord is not running".to_string(),
            _ => why.to_string(),
        };
        if tx.send(Err(msg)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(5));
    });
}

/// One request; joins can take up to a minute, so each runs on its own
/// thread, and a second scan while one runs is dropped.
fn work(w: Want, tx: Sender<Got>, scanning: Arc<AtomicBool>) {
    use std::sync::atomic::Ordering;
    if w == Want::Scan && scanning.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let e = |f: atrium_netconf::Fail| f.1;
        let g = match &w {
            Want::Scan => {
                let r = connect(Grant::Nearby).and_then(|mut c| c.nearby().map_err(e));
                scanning.store(false, Ordering::SeqCst);
                Got::Nearby(r)
            }
            Want::Profiles => Got::Profiles(connect(Grant::Wifi).and_then(|mut c| c.profiles().map_err(e))),
            Want::Details => Got::Details(connect(Grant::Details).and_then(|mut c| c.interfaces().map_err(e))),
            Want::Join { ssid, open } => {
                let r = connect(Grant::Wifi).and_then(|mut c| c.join(&Join { ssid: ssid.clone(), passphrase: None, handle: None, open: *open }).map_err(e));
                Got::Done { what: w.clone(), result: r }
            }
            Want::JoinWith { ssid, handle } => {
                let j = Join { ssid: ssid.clone(), passphrase: None, handle: Some(*handle), open: false };
                Got::Done { what: w.clone(), result: connect(Grant::Wifi).and_then(|mut c| c.join(&j).map_err(e)) }
            }
            Want::Leave => Got::Done { what: w.clone(), result: connect(Grant::Wifi).and_then(|mut c| c.leave().map_err(e)) },
            Want::Forget(s) => Got::Done { what: w.clone(), result: connect(Grant::Wifi).and_then(|mut c| c.forget(s).map_err(e)) },
            Want::Save(p) => Got::Done { what: w.clone(), result: connect(Grant::Wifi).and_then(|mut c| c.set_profile(p).map_err(e)) },
        };
        let _ = tx.send(g);
    });
}

fn main() {
    let (w, h) = std::env::var("CURIA_SIZE")
        .ok()
        .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
        .unwrap_or((960, 720));
    let mut win = match Window::open(&WindowDesc::new("Settings", SizeSpec::Fixed(w, h))) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("curia: frescod: {e}");
            std::process::exit(1);
        }
    };
    let dirty = Arc::new(AtomicBool::new(true));
    let page = Mutable::with_dirty(NetPage::new(), dirty.clone());
    let cred_state = Mutable::with_dirty(CredentialState::default(), dirty.clone());
    let cred = CredentialField::wifi_passphrase(cred_state.clone());
    let size = Mutable::with_dirty((win.width, win.height), dirty.clone());
    let devs = Mutable::with_dirty(curia::devices::DevPage::default(), dirty.clone());
    let which = Mutable::with_dirty(curia::view::Which::default(), dirty.clone());
    let dev_scroll = Mutable::with_dirty(Default::default(), dirty.clone());
    let view = CuriaView { which, page: page.clone(), devs: devs.clone(), dev_scroll, cred: cred.clone(), size: size.clone() };
    let (dtx, drx) = channel();
    follow_devices(dtx);
    // Arca's passphrase field comes through limend (curia.md §4). Without
    // it the row shows a field that can't be typed into: Curia never takes
    // a passphrase itself.
    let embeds = match Embeds::connect() {
        Ok(e) => {
            win.set_slot_binder(Embeds::binder(&e));
            Some(e)
        }
        Err(e) => {
            eprintln!("curia: limend: {e:?}: no passphrase field");
            None
        }
    };
    let mut embeds = embeds;
    let mut cred_embed = None;
    let mut app = App::new_with_flag(view, dirty);
    let (tx, rx): (Sender<Got>, Receiver<Got>) = channel();
    follow(tx.clone());
    let scanning = Arc::new(AtomicBool::new(false));
    let until = std::env::var("CURIA_SECS").ok().and_then(|s| s.parse::<u64>().ok()).map(|s| Instant::now() + Duration::from_secs(s));
    let mut next_scan = Instant::now() + Duration::from_secs(15);
    loop {
        match win.step(&mut app, Some(Duration::from_millis(100))) {
            Ok(StepResult::Closed) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("curia: frescod: {e}");
                break;
            }
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        while let Ok(r) = drx.try_recv() {
            curia::view::with_devs(&devs, |d| d.apply(r));
        }
        let mut changed = false;
        let mut p = page.get_cloned();
        while let Ok(g) = rx.try_recv() {
            p.apply(g);
            changed = true;
        }
        // Arca's field: asked for while a passphrase row is open.
        if let Some(e) = &embeds {
            if std::mem::take(&mut p.reset_field) {
                if let Some(id) = cred_embed.take() {
                    let _ = e.borrow_mut().detach_embed(id);
                }
                cred_state.set(CredentialState::default());
                changed = true;
            }
            match (p.asking.is_some(), cred_embed) {
                (true, None) => {
                    if let Some(token) = win.embed_token() {
                        let asked = e.borrow_mut().request("credential", token, cred.slot_key());
                        match asked {
                            Ok(id) => {
                                let _ = e.borrow_mut().send(id, cred.configure().encode());
                                cred_embed = Some(id);
                            }
                            Err(err) => eprintln!("curia: Arca's passphrase field: {err:?}"),
                        }
                    }
                }
                (false, Some(id)) => {
                    let _ = e.borrow_mut().detach_embed(id);
                    cred_embed = None;
                    cred_state.set(CredentialState::default());
                }
                _ => {}
            }
            let mut lost = false;
            loop {
                let ev = e.borrow_mut().poll(Duration::from_millis(1));
                match ev {
                    Ok(Some(limen::proto::ParentEvent::Message { embed, bytes })) if Some(embed) == cred_embed => {
                        if let Some(m) = limen::credential::ToParent::decode(&bytes) {
                            cred.apply(m);
                            p.field(cred_state.get_cloned().handle, Instant::now());
                            changed = true;
                        }
                    }
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed, .. })) if Some(embed) == cred_embed => cred_embed = None,
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(err) => {
                        eprintln!("curia: limend: {err:?}");
                        lost = true;
                        break;
                    }
                }
            }
            if lost {
                embeds = None;
            }
        }
        if p.tick(Instant::now()) {
            changed = true;
        }
        if Instant::now() >= next_scan {
            next_scan = Instant::now() + Duration::from_secs(15);
            if p.reachable && p.joining.is_none() {
                p.wants.push(Want::Scan);
            }
        }
        let wants = p.take_wants();
        if changed || !wants.is_empty() {
            page.set(p);
        }
        for w in wants {
            work(w, tx.clone(), scanning.clone());
        }
        if until.is_some_and(|u| Instant::now() >= u) {
            break;
        }
    }
}
