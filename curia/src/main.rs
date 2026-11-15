//! Curia, the Settings app (docs/spec/curia.md): the window, and the
//! Network page's calls to atrium-netconf on threads of their own.
//!
//! Environment: `CURIA_NET_DIR` (atrium-netconf's sockets, /atrium/sockets),
//! `CURIA_DEVICES_SOCK` (dispensatord's device list,
//! /atrium/sockets/device-list/device-list.sock),
//! `DISPLAY_SETTINGS_SOCKET` (forum-wm's settings socket,
//! /atrium/sockets/display-settings/display-settings.sock),
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
/// A quiet stream is checked every 30 s with a status call of its own, so a
/// service that hangs (alive, socket open, not answering) is seen as away.
fn follow(tx: Sender<Got>) {
    std::thread::spawn(move || loop {
        if let Ok(mut ch) = connect(Grant::Wifi).and_then(|c| c.subscribe().map_err(|f| f.1)) {
            loop {
                match ch.next_within(Duration::from_secs(30)) {
                    Ok(Some(s)) => {
                        if tx.send(Got::Status(Some(s))).is_err() {
                            return;
                        }
                    }
                    Ok(None) if connect(Grant::Wifi).and_then(|mut c| c.status().map_err(|f| f.1)).is_ok() => {}
                    _ => break,
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
                match ch.next_within(Duration::from_secs(30)) {
                    Ok(Some(_)) => {}
                    // Quiet: is it still there? (The list below asks.)
                    Ok(None) => {}
                    Err(e) => break e,
                }
                match atrium_devices::client::list(&sock) {
                    Ok(v) => {
                        if tx.send(Ok(v)).is_err() {
                            return;
                        }
                    }
                    Err(e) => break e,
                }
            },
            Err(e) => e,
        };
        let msg = match why.kind() {
            std::io::ErrorKind::PermissionDenied => "only the person using this machine may see it".to_string(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => "dispensatord is not running".to_string(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "dispensatord is not answering".to_string(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset => "dispensatord went away; asking again".to_string(),
            _ => why.to_string(),
        };
        if tx.send(Err(msg)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(5));
    });
}

/// What the Sound page's connection thread reports.
enum SoundMsg {
    Up,
    Got(lyra_protocol::wire::FromControl),
    Lost(String),
}

type SoundSend = Arc<std::sync::Mutex<Option<lyra_protocol::client::Control>>>;

/// choragusd's volume-control socket (lyra-v1.md §7), followed on a thread:
/// `Apps` and `Outputs` arrive unasked on every change. A quiet connection
/// is asked for its outputs every 30 s; no answer in 10 s and it is dropped
/// as hung. Gone: say why, try again every 5 s.
fn follow_sound(tx: Sender<SoundMsg>, send: SoundSend) {
    use lyra_protocol::wire::ToControl;
    let path = std::env::var("CURIA_AUDIO_SOCK").unwrap_or_else(|_| "/atrium/sockets/audio-control/audio-control.sock".into());
    std::thread::spawn(move || loop {
        let why = match lyra_protocol::client::Control::connect_within(&path, Some(Duration::from_secs(10))) {
            Ok(mut c) => {
                *send.lock().unwrap_or_else(|p| p.into_inner()) = c.try_clone().ok();
                let _ = c.socket().set_read_timeout(Some(Duration::from_secs(30)));
                if tx.send(SoundMsg::Up).is_err() {
                    return;
                }
                let mut probing = false;
                let why = if c.send(&ToControl::List).and_then(|_| c.send(&ToControl::ListOutputs)).is_err() {
                    "choragusd went away".to_string()
                } else {
                    loop {
                        match c.recv() {
                            Ok(m) => {
                                if probing {
                                    probing = false;
                                    let _ = c.socket().set_read_timeout(Some(Duration::from_secs(30)));
                                }
                                if tx.send(SoundMsg::Got(m)).is_err() {
                                    return;
                                }
                            }
                            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) && !probing => {
                                probing = true;
                                let _ = c.socket().set_read_timeout(Some(Duration::from_secs(10)));
                                if c.send(&ToControl::ListOutputs).is_err() {
                                    break "choragusd went away".to_string();
                                }
                            }
                            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break "choragusd is not answering".to_string(),
                            Err(_) => break "choragusd went away".to_string(),
                        }
                    }
                };
                *send.lock().unwrap_or_else(|p| p.into_inner()) = None;
                why
            }
            Err(e) => match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => "choragusd is not running".to_string(),
                std::io::ErrorKind::PermissionDenied => "Settings may not change the volume".to_string(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "choragusd is not answering".to_string(),
                _ => format!("choragusd: {e}"),
            },
        };
        if tx.send(SoundMsg::Lost(why)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(5));
    });
}

/// One request to forum-wm's settings socket, 5 s at most.
fn ask_wm(i: &forum_ctl::Intent) -> Result<forum_ctl::Reply, String> {
    use std::os::unix::net::UnixStream;
    let path = forum_ctl::display_settings_socket_path();
    let io = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => "Forum is not running".to_string(),
        std::io::ErrorKind::PermissionDenied => "Settings may not change the displays".to_string(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "Forum is not answering".to_string(),
        _ => format!("Forum went away ({e})"),
    };
    let mut s = UnixStream::connect(&path).map_err(io)?;
    let limit = Some(Duration::from_secs(5));
    s.set_read_timeout(limit).and_then(|_| s.set_write_timeout(limit)).map_err(io)?;
    let b = forum_ctl::encode(i).map_err(|e| e.to_string())?;
    forum_ctl::write_frame(&mut s, &b).map_err(io)?;
    let r = forum_ctl::read_frame(&mut s).map_err(io)?;
    match forum_ctl::decode::<forum_ctl::Reply>(&r).map_err(|e| e.to_string())? {
        forum_ctl::Reply::Err { message } => Err(message),
        r => Ok(r),
    }
}

/// The Display page's whole state.
fn display_snapshot() -> Result<curia::display::Snapshot, String> {
    use forum_ctl::{Intent, Reply};
    let (dark, text_size) = match ask_wm(&Intent::GetAppearance)? {
        Reply::Appearance { dark, text_size_milli } => (dark, text_size_milli),
        r => return Err(format!("unexpected answer {r:?}")),
    };
    let outputs = match ask_wm(&Intent::ListOutputs)? {
        Reply::Outputs { outputs } => outputs,
        r => return Err(format!("unexpected answer {r:?}")),
    };
    // A machine with no monitor lit lists no modes; that is not an error.
    let (modes, current) = match ask_wm(&Intent::ListDisplayModes) {
        Ok(Reply::DisplayModes { modes, current }) => (modes, current),
        _ => (Vec::new(), u32::MAX),
    };
    Ok(curia::display::Snapshot { dark, text_size, outputs, modes, current })
}

/// One Display page request on a thread of its own; one refresh at a time.
fn display_work(w: curia::display::Want, tx: Sender<curia::display::Got>, refreshing: Arc<AtomicBool>) {
    use curia::display::{Got as DGot, Place, Want as DWant};
    use forum_ctl::{Intent, Side};
    use std::sync::atomic::Ordering;
    if w == DWant::Refresh && refreshing.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let intent = match &w {
            DWant::Refresh => {
                let r = display_snapshot();
                refreshing.store(false, Ordering::SeqCst);
                let _ = tx.send(DGot::State(r));
                return;
            }
            DWant::Dark(dark) => Intent::SetAppearance { dark: *dark },
            DWant::TextSize(m) => Intent::SetTextSize { text_size_milli: *m },
            DWant::UiSize { output, milli } => Intent::SetOutputUiSize { output: *output, ui_size_milli: *milli },
            DWant::Mode { width, height, refresh_mhz } => Intent::SetDisplayMode { width: *width, height: *height, refresh_mhz: *refresh_mhz },
            DWant::Place { output, place, of } => Intent::PlaceOutput {
                output: *output,
                side: match place {
                    Place::LeftOf => Side::LeftOf,
                    Place::RightOf => Side::RightOf,
                    Place::Above => Side::Above,
                    Place::Below => Side::Below,
                },
                of: *of,
            },
            DWant::Mirror { output, of } => Intent::MirrorOutput { output: *output, of: *of },
        };
        let result = ask_wm(&intent).map(|_| ());
        let _ = tx.send(DGot::Done { what: w, result });
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
    let sound = Mutable::with_dirty(curia::sound::SoundPage::default(), dirty.clone());
    let (stx, srx) = channel();
    let sound_send: SoundSend = Arc::new(std::sync::Mutex::new(None));
    follow_sound(stx, sound_send.clone());
    let disp = Mutable::with_dirty(curia::display::DispPage::new(), dirty.clone());
    let view = CuriaView { which: which.clone(), page: page.clone(), devs: devs.clone(), dev_scroll, disp: disp.clone(), disp_scroll: Mutable::with_dirty(Default::default(), dirty.clone()), sound: sound.clone(), sound_scroll: Mutable::with_dirty(Default::default(), dirty.clone()), cred: cred.clone(), size: size.clone() };
    let (ptx, prx) = channel();
    let refreshing = Arc::new(AtomicBool::new(false));
    let mut next_disp = Instant::now();
    let (dtx, drx) = channel();
    follow_devices(dtx);
    // Arca's passphrase field comes through limend (curia.md §4). Without
    // it the row shows a field that can't be typed into: Curia never takes
    // a passphrase itself.
    // limend going away (or not there yet) is retried every 5 s; the field
    // is asked for again at most every 2 s.
    // Said once per outage, not every 5 s.
    let mut limend_said = false;
    let mut link_limend = |win: &mut Window| match Embeds::connect() {
        Ok(e) => {
            win.set_slot_binder(Embeds::binder(&e));
            eprintln!("curia: limend: connected");
            limend_said = false;
            Some(e)
        }
        Err(e) => {
            if !limend_said {
                eprintln!("curia: limend: {e:?}: no passphrase field for now");
                limend_said = true;
            }
            None
        }
    };
    let mut embeds = link_limend(&mut win);
    let mut next_limend = Instant::now() + Duration::from_secs(5);
    let mut cred_embed = None;
    let mut next_ask = Instant::now();
    let mut failed = false;
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
                // Exit as a failure, so supervision (restart = on-crash)
                // brings Settings back when frescod does.
                eprintln!("curia: frescod: {e}");
                failed = true;
                break;
            }
        }
        if size.get() != (win.width, win.height) {
            size.set((win.width, win.height));
        }
        while let Ok(r) = drx.try_recv() {
            curia::view::with_devs(&devs, |d| d.apply(r));
        }
        // The Sound page: what choragusd says, and what the page asks.
        {
            let mut sp = sound.get_cloned();
            let mut sch = false;
            while let Ok(m) = srx.try_recv() {
                match m {
                    SoundMsg::Up => sp.connected(),
                    SoundMsg::Got(m) => sp.apply(m),
                    SoundMsg::Lost(why) => sp.lost(why),
                }
                sch = true;
            }
            let wants = sp.take_wants();
            if !wants.is_empty() {
                if let Some(c) = sound_send.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                    for w in &wants {
                        let _ = c.send(w);
                    }
                }
                sch = true;
            }
            if sch {
                sound.set(sp);
            }
        }
        // The Display page: forum-wm is asked again every 3 s while it shows
        // (a monitor plugged in, the bar's light/dark switch), and an unkept
        // resolution goes back.
        {
            let mut d = disp.get_cloned();
            let mut dch = false;
            while let Ok(g) = prx.try_recv() {
                d.apply(g);
                dch = true;
            }
            if d.tick(Instant::now()) {
                dch = true;
            }
            if which.get() == curia::view::Which::Display && Instant::now() >= next_disp {
                next_disp = Instant::now() + Duration::from_secs(3);
                d.wants.push(curia::display::Want::Refresh);
            }
            let wants = d.take_wants();
            if dch || !wants.is_empty() {
                disp.set(d);
            }
            for w in wants {
                display_work(w, ptx.clone(), refreshing.clone());
            }
        }
        let mut changed = false;
        let mut p = page.get_cloned();
        while let Ok(g) = rx.try_recv() {
            p.apply(g);
            changed = true;
        }
        // Arca's field: asked for while a passphrase row is open.
        if embeds.is_none() && Instant::now() >= next_limend {
            next_limend = Instant::now() + Duration::from_secs(5);
            embeds = link_limend(&mut win);
        }
        let field_up = embeds.is_some() && cred_embed.is_some();
        if p.field_up != field_up {
            p.field_up = field_up;
            changed = true;
        }
        if let Some(e) = &embeds {
            if std::mem::take(&mut p.reset_field) {
                if let Some(id) = cred_embed.take() {
                    let _ = e.borrow_mut().detach_embed(id);
                }
                cred_state.set(CredentialState::default());
                changed = true;
            }
            match (p.asking.is_some(), cred_embed) {
                (true, None) if Instant::now() >= next_ask => {
                    next_ask = Instant::now() + Duration::from_secs(2);
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
                    // Arca's side went (arca-ui restarted): what was typed is
                    // gone with it; a new field is asked for.
                    Ok(Some(limen::proto::ParentEvent::ChildLost { embed, .. })) if Some(embed) == cred_embed => {
                        cred_embed = None;
                        cred_state.set(CredentialState::default());
                        p.field(None, Instant::now());
                        changed = true;
                    }
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
                cred_embed = None;
                cred_state.set(CredentialState::default());
                p.field(None, Instant::now());
                changed = true;
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
    if failed {
        std::process::exit(1);
    }
}
