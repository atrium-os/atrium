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
        let grants = args.iter().position(|a| a == "--grants").and_then(|j| args.get(j + 1)).map(String::as_str);
        // --portcullis-grants <base>: read real per-user Portcullis grants from
        // <base>/<user>/policy.toml (the authoritative source).
        let pcull = args.iter().position(|a| a == "--portcullis-grants").and_then(|j| args.get(j + 1)).map(String::as_str);
        // --app-registry <file>: the Portcullis uid→app launch registry (verified
        // identity); without it, the app's hello id is trusted (legacy/test).
        let reg = args.iter().position(|a| a == "--app-registry").and_then(|j| args.get(j + 1)).map(String::as_str);
        // --seat: session-aware — only the active session's audio reaches the
        // engine; a seat switch (active-session change) makes the audio follow.
        let seat = args.iter().any(|a| a == "--seat");
        daemon(app_sock, lyrad_sock, grants, pcull, reg, seat);
        return;
    }

    // --app <app_sock> <role> <secs>: a stand-in audio app — register with the
    // daemon under a role, hold, then close. Separate process from choragusd.
    if let Some(i) = args.iter().position(|a| a == "--app") {
        use choragus::app::{CAP_AUDIO, CAP_MICROPHONE, CAP_MONITOR};
        let app_sock = args.get(i + 1).map(String::as_str).unwrap_or("/tmp/choragus.sock");
        let role = args.get(i + 2).and_then(|s| choragus::app::role_from_str(s)).unwrap_or(Role::Media);
        let secs: u64 = args.get(i + 3).and_then(|s| s.parse().ok()).unwrap_or(4);
        let app_id = args.iter().position(|a| a == "--id").and_then(|j| args.get(j + 1))
            .map(String::as_str).unwrap_or("org.atrium.player");
        // every player needs `audio`; "monitor"/"mic" request the privileged ones.
        let mut caps = CAP_AUDIO;
        if args.iter().any(|a| a == "monitor") { caps |= CAP_MONITOR; }
        if args.iter().any(|a| a == "mic") { caps |= CAP_MICROPHONE; }
        app_client(app_sock, app_id, role, secs, caps);
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
    grants: choragus::grant::GrantStore,
    /// If set, grants come from the REAL Portcullis per-user store under this
    /// base dir (`<base>/<user>/policy.toml`), resolved by the getpeereid'd user
    /// per connection — instead of the single hand-written `grants` file.
    portcullis_base: Option<String>,
    /// If set, the Portcullis launch registry (`uid → (user, app-id)`): the app's
    /// identity is its dedicated uid via this binding, not its self-declared id.
    app_registry: Option<String>,
    /// lyrad's data socket — where choragusd fetches a stream's ring fd to broker
    /// to the app (so the app talks only to choragusd, one front door).
    lyrad_data_sock: String,
    /// Session-aware: only the ACTIVE session's audio reaches the engine. When on,
    /// a stream owned by a non-active session is muted; a seat switch follows.
    seat_aware: bool,
    /// stream id → the owning human session (for seat gating on a switch).
    stream_owner: std::collections::HashMap<u32, String>,
}

/// The gain (dB) for a stream muted because its session isn't the active one.
const SEAT_MUTE_DB: f32 = -120.0;

/// Fetch the data-plane ring fd for `stream` from lyrad (connect, send the id,
/// receive the fd). The broker half of the single-front-door data path.
fn fetch_ring_fd(data_sock: &str, stream: u32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(data_sock)?;
    s.write_all(&stream.to_le_bytes())?;
    lyra_protocol::fdpass::recv_fd(s.as_raw_fd())
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
    st.stream_owner.remove(&stream);
}

/// Re-apply seat gating after the active session may have changed: each stream
/// owned by the active session plays (0 dB); every other session's stream is
/// muted. This is what makes a seat switch *follow* — the previous session's
/// audio detaches and the new session's attaches. Called by the seat poll loop.
fn apply_seat(state: &std::sync::Mutex<DState>) {
    use choragus::control::Ctl;
    let mut st = state.lock().unwrap();
    let owners: Vec<(u32, String)> =
        st.stream_owner.iter().map(|(s, u)| (*s, u.clone())).collect();
    for (stream, owner) in owners {
        let db = if portcullis_peer::seat::is_active(&owner) { 0.0 } else { SEAT_MUTE_DB };
        let _ = st.conn.send(Ctl::SetGainDb { stream, db });
    }
}

/// Handle one app connection: register/close streams; on disconnect, close any
/// the app left open (crash safety — an app dying un-ducks everyone else).
fn handle_app(mut app: std::os::unix::net::UnixStream, state: std::sync::Arc<std::sync::Mutex<DState>>) {
    use choragus::app::{cap_names, AppMsg, APP_FRAME_LEN, DENIED};
    use choragus::control::Ctl;
    use choragus::policy::{diff, Stream};
    use std::io::{Read, Write};

    use choragus::grant::GrantStore;
    use portcullis_peer::AppRegistry;
    use std::os::fd::AsRawFd;

    // the unforgeable handle: the kernel's getpeereid uid (platform primitive).
    let (uid, _gid) = portcullis_peer::uid_gid(app.as_raw_fd()).unwrap_or((u32::MAX, u32::MAX));
    // the app's CLAIM (advisory once a registry is in play).
    let claimed = match choragus::app::read_hello(&mut app) {
        Ok(id) => id,
        Err(_) => return,
    };

    // pull what we need from the daemon state, then do the IO unlocked.
    let (reg_path, pcull_base) = {
        let st = state.lock().unwrap();
        (st.app_registry.clone(), st.portcullis_base.clone())
    };

    // Resolve the VERIFIED identity. With a Portcullis launch registry, the uid
    // IS the identity: an app cannot get another app's grant by claiming its id.
    let (app_id, user, granted): (String, String, u8) = match &reg_path {
        Some(rp) => {
            let reg = AppRegistry::load(rp).unwrap_or_default();
            match reg.resolve(uid) {
                Some((owner, verified)) => {
                    if verified != claimed {
                        eprintln!("choragusd: SPOOF? uid {uid} claimed '{claimed}' but Portcullis launched '{verified}' — using the verified id");
                    }
                    let g = match &pcull_base {
                        Some(base) => GrantStore::load_portcullis(base, owner).granted(verified),
                        None => state.lock().unwrap().grants.granted(verified),
                    };
                    (verified.to_string(), owner.to_string(), g)
                }
                None => {
                    // not launched by Portcullis at a known uid → no verified
                    // identity → default-deny everything.
                    eprintln!("choragusd: uid {uid} not in the launch registry (claimed '{claimed}') — denying");
                    (claimed.clone(), format!("uid{uid}"), 0)
                }
            }
        }
        None => {
            // legacy: no registry, trust the hello; grant scoped to the
            // getpeereid'd user.
            let user = portcullis_peer::username(uid).unwrap_or_else(|| format!("uid{uid}"));
            let g = match &pcull_base {
                Some(base) => GrantStore::load_portcullis(base, &user).granted(&claimed),
                None => state.lock().unwrap().grants.granted(&claimed),
            };
            (claimed.clone(), user, g)
        }
    };
    eprintln!("choragusd: app '{app_id}' connected (peer uid={uid} user={user}, granted {:?})",
        choragus::app::cap_names(granted));

    let mut opened: Vec<u32> = Vec::new();
    let mut frame = [0u8; APP_FRAME_LEN];
    while app.read_exact(&mut frame).is_ok() {
        match AppMsg::decode(&frame) {
            Some(AppMsg::Register { role, caps }) => {
                let role = choragus::app::role_from_u8(role).unwrap_or(Role::Media);
                // §9 enforcement: requested caps must be within the app's grant
                // (read from the store; in production Portcullis + user approval
                // populate it). Default-deny — anything beyond the grant (the mic,
                // the system monitor/tap) is refused at the door.
                let denied = caps & !granted;
                if denied != 0 {
                    eprintln!("choragusd: {role:?} DENIED — requested {:?}, not granted (default-deny)", cap_names(denied));
                    let _ = app.write_all(&DENIED.to_le_bytes());
                    continue;
                }
                let id;
                let nchanges;
                let data_sock;
                {
                    let mut st = state.lock().unwrap();
                    id = st.next_id;
                    st.next_id += 1;
                    data_sock = st.lyrad_data_sock.clone();
                    let before = st.sess.resolve();
                    let _ = st.sess.open(Stream::new(id, role));
                    let after = st.sess.resolve();
                    let _ = st.conn.send(Ctl::OpenStream { stream: id });
                    let changes = diff(&before, &after);
                    nchanges = changes.len();
                    let _ = st.conn.apply(&changes);
                    // Seat gating: remember which human session owns this stream,
                    // and if that session isn't the active one, mute it — its audio
                    // does not reach the engine until its session is bound.
                    st.stream_owner.insert(id, user.clone());
                    if st.seat_aware && !portcullis_peer::seat::is_active(&user) {
                        let _ = st.conn.send(Ctl::SetGainDb { stream: id, db: SEAT_MUTE_DB });
                        eprintln!("choragusd: stream {id} (session '{user}') not active — muted");
                    }
                }
                opened.push(id);
                let _ = app.write_all(&id.to_le_bytes());
                // broker the data-plane ring: fetch its fd from lyrad and pass it
                // on to the app (SCM_RIGHTS). The app talks only to choragusd.
                match fetch_ring_fd(&data_sock, id) {
                    Ok(fd) => {
                        use std::os::fd::AsRawFd;
                        let _ = lyra_protocol::fdpass::send_fd(app.as_raw_fd(), fd.as_raw_fd());
                    }
                    Err(e) => eprintln!("choragusd: broker ring for stream {id}: {e}"),
                }
                eprintln!("choragusd: {role:?} registered -> stream {id} ({nchanges} policy change(s), ring brokered)");
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

fn daemon(app_sock: &str, lyrad_sock: &str, grants_path: Option<&str>, portcullis_base: Option<&str>, app_registry: Option<&str>, seat_aware: bool) {
    use choragus::control::Conn;
    use choragus::grant::GrantStore;
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};

    let conn = match Conn::connect(lyrad_sock) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("choragusd: connect lyrad {lyrad_sock}: {e} (is `lyrad --control {lyrad_sock}` up?)");
            std::process::exit(1);
        }
    };
    // grants: the REAL per-user Portcullis store (preferred), else the simple file.
    if let Some(b) = portcullis_base {
        eprintln!("choragusd: grants from Portcullis per-user store under {b} (getpeereid-resolved)");
    }
    if let Some(r) = app_registry {
        eprintln!("choragusd: verified app identity from the Portcullis launch registry {r} (uid→app)");
    }
    let grants = match grants_path {
        Some(p) => match GrantStore::load(p) {
            Ok(g) => { eprintln!("choragusd: loaded grants from {p}"); g }
            Err(e) => { eprintln!("choragusd: no grants file {p} ({e}); default-deny all"); GrantStore::default() }
        },
        None => GrantStore::default(),
    };
    let spk = Device { id: 0, kind: DeviceKind::Speakers };
    let state = Arc::new(Mutex::new(DState {
        sess: Session::new(vec![spk], spk.id),
        conn,
        next_id: 0,
        grants,
        portcullis_base: portcullis_base.map(String::from),
        app_registry: app_registry.map(String::from),
        lyrad_data_sock: format!("{lyrad_sock}.data"),
        seat_aware,
        stream_owner: std::collections::HashMap::new(),
    }));

    // Seat awareness: watch the active session and, on a switch, re-gate streams
    // so only the bound session's audio reaches the engine (a switch follows).
    if seat_aware {
        match portcullis_peer::seat::active() {
            Some(s) => eprintln!("choragusd: seat-aware — active session is '{s}'"),
            None => eprintln!("choragusd: seat-aware — no active session yet ({})",
                portcullis_peer::seat::ACTIVE_SESSION),
        }
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let mut last = portcullis_peer::seat::active();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(200));
                let now = portcullis_peer::seat::active();
                if now != last {
                    eprintln!("choragusd: active session {:?} -> {:?} — re-gating audio", last, now);
                    apply_seat(&state);
                    last = now;
                }
            }
        });
    }

    let _ = std::fs::remove_file(app_sock);
    let listener = match UnixListener::bind(app_sock) {
        Ok(l) => l,
        Err(e) => { eprintln!("choragusd: bind {app_sock}: {e}"); std::process::exit(1); }
    };
    // The front door must be reachable by every app's dedicated uid (each app
    // runs as its own 50000+ uid). Connecting is not what authorizes — the peer's
    // getpeereid identity + its grant are — so the socket itself is open to all.
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(app_sock, std::fs::Permissions::from_mode(0o666));
    }
    eprintln!("choragusd: session daemon up — apps at {app_sock}, driving lyrad at {lyrad_sock}");
    for app in listener.incoming() {
        let app = match app { Ok(a) => a, Err(_) => continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_app(app, state));
    }
}

/// A stand-in audio app: announce identity, register under a role (requesting
/// `caps`), hold, close.
fn app_client(app_sock: &str, app_id: &str, role: Role, secs: u64, caps: u8) {
    use choragus::app::{cap_names, write_hello, AppMsg, DENIED};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let mut s = match UnixStream::connect(app_sock) {
        Ok(s) => s,
        Err(e) => { eprintln!("app: connect {app_sock}: {e} (is choragusd --daemon up?)"); std::process::exit(1); }
    };
    let _ = write_hello(&mut s, app_id);
    let _ = s.write_all(&AppMsg::Register { role: choragus::app::role_to_u8(role), caps }.encode());
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
