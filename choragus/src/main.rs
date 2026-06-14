//! choragusd — the Choragus daemon (audio policy / session layer) skeleton.
//!
//! The non-RT policy brain that sits beside lyrad (the RT engine): it resolves
//! the desired routing/levels from stream roles + devices + rules, enforces the
//! §9 privacy capabilities, and hands lyrad mechanism-agnostic changes (a gain
//! ramp, a re-route). This skeleton wires none of the IPC yet — it runs a
//! scripted scenario end-to-end so the policy logic is exercisable on the host
//! and as a real FreeBSD aarch64 ELF, the seam the daemon grows from (the same
//! shape lyrad started as a scripted `--tone`).
//!
//! usage: choragusd --demo

use choragus::capability::{check, Access, Capability, Grant};
use choragus::control;
use choragus::policy::{diff, Device, DeviceKind, Role, Session, Stream};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // --apply <socket>: resolve the ducking scenario and SEND the resulting
    // changes to a running lyrad over its control socket (the choragusd↔lyrad
    // wire). Pair with `lyrad --control <socket>`.
    if let Some(i) = args.iter().position(|a| a == "--apply") {
        let socket = args.get(i + 1).map(String::as_str).unwrap_or("/tmp/lyrad.ctl");
        // a media stream (id 0) is playing on lyrad; a call arrives -> media ducks.
        let spk = Device { id: 0, kind: DeviceKind::Speakers };
        let mut s = Session::new(vec![spk], spk.id);
        s.open(Stream::new(0, Role::Media)).unwrap();
        let before = s.resolve();
        s.open(Stream::new(1, Role::Communication)).unwrap();
        let changes = diff(&before, &s.resolve());
        eprintln!("choragusd: a call arrived; sending {} change(s) to lyrad at {socket}", changes.len());
        for c in &changes {
            eprintln!("  {c:?}");
        }
        match control::send(socket, &changes) {
            Ok(()) => eprintln!("choragusd: applied."),
            Err(e) => {
                eprintln!("choragusd: send failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    // --session <socket>: drive a full stream lifecycle against a running lyrad,
    // commanding open/close + the policy-resolved levels over time. The session
    // layer as the source of truth for streams; lyrad realises what it's told.
    if let Some(i) = args.iter().position(|a| a == "--session") {
        let socket = args.get(i + 1).map(String::as_str).unwrap_or("/tmp/lyrad.ctl");
        session_demo(socket);
        return;
    }

    // --daemon <app_sock> <lyrad_sock>: the real session daemon. Listens for app
    // registrations, applies policy, and drives lyrad. Apps cause ducking just by
    // registering as Communication — no scripting.
    if let Some(i) = args.iter().position(|a| a == "--daemon") {
        let app_sock = args.get(i + 1).map(String::as_str).unwrap_or("/tmp/choragus.sock");
        let lyrad_sock = args.get(i + 2).map(String::as_str).unwrap_or("/tmp/lyrad.ctl");
        daemon(app_sock, lyrad_sock);
        return;
    }

    // --app <app_sock> <role> <secs>: a stand-in audio app — register with the
    // daemon under a role, hold, then close. Separate process from choragusd.
    if let Some(i) = args.iter().position(|a| a == "--app") {
        use choragus::app::{CAP_AUDIO, CAP_MICROPHONE, CAP_MONITOR};
        let app_sock = args.get(i + 1).map(String::as_str).unwrap_or("/tmp/choragus.sock");
        let role = args.get(i + 2).and_then(|s| choragus::app::role_from_str(s)).unwrap_or(Role::Media);
        let secs: u64 = args.get(i + 3).and_then(|s| s.parse().ok()).unwrap_or(4);
        // every player needs `audio`; "monitor"/"mic" request the privileged ones.
        let mut caps = CAP_AUDIO;
        if args.iter().any(|a| a == "monitor") { caps |= CAP_MONITOR; }
        if args.iter().any(|a| a == "mic") { caps |= CAP_MICROPHONE; }
        app_client(app_sock, role, secs, caps);
        return;
    }

    let demo = args.iter().any(|a| a == "--demo");
    if !demo {
        eprintln!("choragusd: policy/session layer (skeleton). try --demo, --apply <sock>, or --session <sock>");
        eprintln!("  (the RT engine is lyrad; choragusd decides routing/ducking/");
        eprintln!("   volume/exclusivity and enforces audio/mic/monitor privacy.)");
        return;
    }

    // a tiny scripted desktop: speakers + a media app playing.
    let spk = Device { id: 0, kind: DeviceKind::Speakers };
    let mut s = Session::new(vec![spk], spk.id);
    s.open(Stream::new(0, Role::Media)).expect("media opens");
    println!("media playing:");
    for d in s.resolve() {
        println!("  stream {} -> sink {} @ {:+.1} dB", d.stream, d.sink, d.gain_db);
    }

    // a call comes in: media ducks, computed as a single GainRamp.
    let before = s.resolve();
    s.open(Stream::new(1, Role::Communication)).expect("call opens");
    let after = s.resolve();
    println!("a call arrives -> changes lyrad applies:");
    for c in diff(&before, &after) {
        println!("  {c:?}");
    }

    // plug a headset: it becomes default and the call follows (a Reroute).
    let before = s.resolve();
    s.plug(Device { id: 1, kind: DeviceKind::Headset });
    println!("headset plugged -> changes lyrad applies:");
    for c in diff(&before, &s.resolve()) {
        println!("  {c:?}");
    }

    // privacy: a plain audio app cannot tap the system mix (anti-monitor-leak).
    let app = Grant::of(&[Capability::Audio]);
    let tap = check(&app, Access::TapSystemMix);
    println!("privacy: audio-only app tapping the system mix -> {tap:?}");
    assert!(tap.is_err(), "the global mix must never be visible without audio_monitor");

    println!("choragusd demo OK");
}

/// Drive a stream lifecycle against a running `lyrad --control`: open media, then
/// a call arrives (open comms + media ducks), then the call ends (close comms +
/// media restores). Choragus owns the stream set + the policy; lyrad is told.
fn session_demo(socket: &str) {
    use choragus::control::{Conn, Ctl};
    use choragus::policy::diff;
    use std::{thread, time::Duration};

    let mut conn = match Conn::connect(socket) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("choragusd: connect {socket}: {e} (is `lyrad --control {socket}` running?)");
            std::process::exit(1);
        }
    };
    let spk = Device { id: 0, kind: DeviceKind::Speakers };
    let mut sess = Session::new(vec![spk], spk.id);

    // 1. media opens and plays.
    sess.open(Stream::new(0, Role::Media)).unwrap();
    conn.send(Ctl::OpenStream { stream: 0 }).unwrap();
    eprintln!("choragusd: media (stream 0) playing");
    thread::sleep(Duration::from_secs(2));

    // 2. a call arrives: open comms, and media ducks (the policy diff).
    let before = sess.resolve();
    sess.open(Stream::new(1, Role::Communication)).unwrap();
    let after = sess.resolve();
    conn.send(Ctl::OpenStream { stream: 1 }).unwrap();
    conn.apply(&diff(&before, &after)).unwrap();
    eprintln!("choragusd: a call arrived -> comms (stream 1) opened, media ducks");
    thread::sleep(Duration::from_secs(2));

    // 3. the call ends: close comms, and media restores.
    let before = sess.resolve();
    sess.close(1);
    let after = sess.resolve();
    conn.send(Ctl::CloseStream { stream: 1 }).unwrap();
    conn.apply(&diff(&before, &after)).unwrap();
    eprintln!("choragusd: call ended -> comms closed, media restored");
    thread::sleep(Duration::from_secs(1));

    eprintln!("choragusd: session done");
}

// ── The real session daemon ──

struct DState {
    sess: Session,
    conn: choragus::control::Conn,
    next_id: u32,
}

/// Close a stream and apply the resulting policy change (e.g. un-duck media).
fn close_stream(state: &std::sync::Mutex<DState>, stream: u32) {
    use choragus::control::Ctl;
    use choragus::policy::diff;
    let mut st = state.lock().unwrap();
    let before = st.sess.resolve();
    st.sess.close(stream);
    let after = st.sess.resolve();
    let _ = st.conn.send(Ctl::CloseStream { stream });
    let changes = diff(&before, &after);
    let _ = st.conn.apply(&changes);
}

/// Handle one app connection: register/close streams; on disconnect, close any
/// the app left open (crash safety — an app dying un-ducks everyone else).
fn handle_app(mut app: std::os::unix::net::UnixStream, state: std::sync::Arc<std::sync::Mutex<DState>>) {
    use choragus::app::{cap_names, AppMsg, APP_FRAME_LEN, CAP_AUDIO, DENIED};
    use choragus::control::Ctl;
    use choragus::policy::{diff, Stream};
    use std::io::{Read, Write};

    let mut opened: Vec<u32> = Vec::new();
    let mut frame = [0u8; APP_FRAME_LEN];
    while app.read_exact(&mut frame).is_ok() {
        match AppMsg::decode(&frame) {
            Some(AppMsg::Register { role, caps }) => {
                // §9 enforcement: the app's Portcullis grant. STUBBED to {audio}
                // until the Portcullis capability token is wired (L4/L5) — but the
                // default-deny posture is real: anything beyond `audio` (the mic,
                // the system monitor/tap) is refused at the door.
                let granted: u8 = CAP_AUDIO;
                let denied = caps & !granted;
                if denied != 0 {
                    eprintln!("choragusd: {role:?} DENIED — requested {:?}, not granted (default-deny)", cap_names(denied));
                    let _ = app.write_all(&DENIED.to_le_bytes());
                    continue;
                }
                let id;
                let nchanges;
                {
                    let mut st = state.lock().unwrap();
                    id = st.next_id;
                    st.next_id += 1;
                    let before = st.sess.resolve();
                    let _ = st.sess.open(Stream::new(id, role));
                    let after = st.sess.resolve();
                    let _ = st.conn.send(Ctl::OpenStream { stream: id });
                    let changes = diff(&before, &after);
                    nchanges = changes.len();
                    let _ = st.conn.apply(&changes);
                }
                opened.push(id);
                let _ = app.write_all(&id.to_le_bytes());
                eprintln!("choragusd: {role:?} registered -> stream {id} ({nchanges} policy change(s))");
            }
            Some(AppMsg::Close { stream }) => {
                close_stream(&state, stream);
                opened.retain(|&x| x != stream);
                eprintln!("choragusd: app closed stream {stream}");
            }
            None => {}
        }
    }
    for id in opened {
        close_stream(&state, id);
        eprintln!("choragusd: app gone -> stream {id} closed");
    }
}

fn daemon(app_sock: &str, lyrad_sock: &str) {
    use choragus::control::Conn;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    let conn = match Conn::connect(lyrad_sock) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("choragusd: connect lyrad {lyrad_sock}: {e} (is `lyrad --control {lyrad_sock}` up?)");
            std::process::exit(1);
        }
    };
    let spk = Device { id: 0, kind: DeviceKind::Speakers };
    let state = Arc::new(Mutex::new(DState { sess: Session::new(vec![spk], spk.id), conn, next_id: 0 }));

    let _ = std::fs::remove_file(app_sock);
    let listener = match UnixListener::bind(app_sock) {
        Ok(l) => l,
        Err(e) => { eprintln!("choragusd: bind {app_sock}: {e}"); std::process::exit(1); }
    };
    eprintln!("choragusd: session daemon up — apps at {app_sock}, driving lyrad at {lyrad_sock}");
    for app in listener.incoming() {
        let app = match app { Ok(a) => a, Err(_) => continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_app(app, state));
    }
}

/// A stand-in audio app: register under a role (requesting `caps`), hold, close.
fn app_client(app_sock: &str, role: Role, secs: u64, caps: u8) {
    use choragus::app::{cap_names, AppMsg, DENIED};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut s = match UnixStream::connect(app_sock) {
        Ok(s) => s,
        Err(e) => { eprintln!("app: connect {app_sock}: {e} (is choragusd --daemon up?)"); std::process::exit(1); }
    };
    let _ = s.write_all(&AppMsg::Register { role, caps }.encode());
    let mut idb = [0u8; 4];
    if s.read_exact(&mut idb).is_err() {
        eprintln!("app: no id from choragusd");
        std::process::exit(1);
    }
    let id = u32::from_le_bytes(idb);
    if id == DENIED {
        eprintln!("app: registration DENIED by choragusd (requested {:?})", cap_names(caps));
        std::process::exit(2);
    }
    eprintln!("app: registered as {role:?} -> stream {id}; playing {secs}s");
    std::thread::sleep(std::time::Duration::from_secs(secs));
    let _ = s.write_all(&AppMsg::Close { stream: id }.encode());
    eprintln!("app: closed stream {id}");
}
