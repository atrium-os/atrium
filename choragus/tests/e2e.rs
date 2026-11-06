//! choragusd end to end on the host: a stand-in lyrad, the real daemon, the
//! real client library. Checks identity from the registry, the ring brokered
//! to the app, volumes composed into lyrad's gain, events forwarded, and that
//! an app can only name its own streams.

use choragus::app::{CAP_AUDIO, CAP_CONTROL, CAP_MICROPHONE, CAP_MONITOR};
use choragus::daemon::{self, Config};
use choragus::grant::GrantStore;
use lyra_protocol::client::{Client, Control};
use lyra_protocol::ring::Ring;
use lyra_protocol::wire::{
    self, AppEntry, DenyCode, FromAttribution, FromControl, FromEngine, Output, Role, StreamParams, ToApp, ToAttribution, ToChoragus,
    ToControl, ToEngine,
};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("choragus-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A stand-in lyrad: answers each Open with a named-shm ring (anonymous shm is
/// FreeBSD-only), keeps the consumer ends, reports every message it gets, and
/// answers Flush with Flushed.
fn fake_lyrad(sock: PathBuf, tag: &'static str) -> (mpsc::Receiver<ToEngine>, mpsc::Receiver<Ring>) {
    let l = UnixListener::bind(&sock).unwrap();
    let (tx, rx) = mpsc::channel();
    let (rtx, rrx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut n = 0;
        while let Ok(m) = wire::recv::<_, ToEngine>(&mut c) {
            match &m {
                ToEngine::Open { stream, channels, ring_frames, .. } => {
                    n += 1;
                    let name = format!("/chor-e2e-{tag}-{}-{n}", std::process::id());
                    let p = Ring::create(&name, *ring_frames, *channels as u64).unwrap();
                    let cons = Ring::open(&name, false).unwrap();
                    let fd = std::fs::OpenOptions::new().read(true).write(true)
                        .open(format!("/dev/shm{name}")).unwrap();
                    wire::send(&mut c, &FromEngine::Opened { stream: *stream, ring_frames: *ring_frames, latency_ns: 7 }).unwrap();
                    lyra_protocol::fdpass::send_fd(c.as_raw_fd(), fd.as_raw_fd()).unwrap();
                    let (w, cfd) = lyra_protocol::clock::create().unwrap();
                    w.publish(&lyra_protocol::clock::Clock { state: 1, frame: 100, heard_ns: 5_000, rate_ppm: 0, flush_epoch: 0, rate: 44_100 });
                    lyra_protocol::fdpass::send_fd(c.as_raw_fd(), cfd.as_raw_fd()).unwrap();
                    std::mem::forget(w); // keep the page mapped for the test
                    drop(p);
                    rtx.send(cons).unwrap();
                }
                ToEngine::SetLayout { channels, .. } => {
                    // A stand-in device that takes stereo and 5.1, not 7.1.
                    let got = if *channels == 8 { 2 } else { *channels };
                    wire::send(&mut c, &FromEngine::Layout { channels: got }).unwrap();
                }
                ToEngine::Flush { stream } => {
                    wire::send(&mut c, &FromEngine::Flushed { stream: *stream, frame: 42 }).unwrap();
                }
                _ => {}
            }
            tx.send(m).unwrap();
        }
    });
    (rx, rrx)
}

fn me() -> u32 {
    unsafe { libc::getuid() }
}

fn start(tag: &'static str, registry: &str, grants: &str, socket_caps: u8) -> (PathBuf, mpsc::Receiver<ToEngine>, mpsc::Receiver<Ring>) {
    let (mut socks, eng, rings) = start_with(tag, registry, grants, &[("audio.sock", socket_caps)], None);
    (socks.remove(0), eng, rings)
}

/// The daemon with these sockets (`(file name, caps)`) and volume state dir.
fn start_with(tag: &'static str, registry: &str, grants: &str, listeners: &[(&str, u8)], state_dir: Option<PathBuf>)
    -> (Vec<PathBuf>, mpsc::Receiver<ToEngine>, mpsc::Receiver<Ring>) {
    let d = dir(tag);
    let lyrad = d.join("lyrad.sock");
    let (eng, rings) = fake_lyrad(lyrad.clone(), tag);
    std::fs::write(d.join("registry"), registry).unwrap();
    let socks: Vec<PathBuf> = listeners.iter().map(|(n, _)| d.join(n)).collect();
    let cfg = Config {
        listeners: socks.iter().zip(listeners).map(|(p, (_, c))| (p.to_string_lossy().into(), *c)).collect(),
        lyrad: lyrad.to_string_lossy().into(),
        registry: Some(d.join("registry").to_string_lossy().into()),
        policy_base: None,
        grants: GrantStore::parse(grants),
        seat: false,
        seat_path: d.join("seat").to_string_lossy().into(),
        strict: true,
        device_rate: 48_000,
        device_channels: 2,
        period_frames: 128,
        state_dir: state_dir.map(|p| p.to_string_lossy().into()),
        limen_uid: Some(me()),
    };
    std::thread::spawn(move || daemon::run(cfg));
    for _ in 0..200 {
        if socks.iter().all(|s| s.exists()) { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    (socks, eng, rings)
}

fn params() -> StreamParams {
    StreamParams { role: Role::Media, rate: 44_100, channels: 2, buffer_ms: 50, label: "Video".into() }
}

fn next_engine(rx: &mpsc::Receiver<ToEngine>, want: impl Fn(&ToEngine) -> bool) -> ToEngine {
    loop {
        let m = rx.recv_timeout(Duration::from_secs(3)).expect("lyrad got nothing");
        if want(&m) { return m; }
    }
}

#[test]
fn an_app_plays_through_the_front_door() {
    let reg = format!("{} alice org.atrium.player\n", me());
    let (sock, eng, rings) = start("play", &reg, "org.atrium.player audio\n", CAP_AUDIO);
    let mut c = Client::connect(sock.to_str().unwrap(), None).expect("welcome");
    assert_eq!((c.device_rate, c.device_channels), (48_000, 2));
    let s = c.open(params()).expect("opened");
    assert_eq!(s.latency_ns, 7);
    // The clock page lyrad wrote reaches the app.
    assert_eq!((s.clock().frame, s.clock().heard_ns), (100, 5_000));
    // lyrad was asked for a 44.1 kHz stereo stream with a power-of-two ring.
    assert!(matches!(next_engine(&eng, |m| matches!(m, ToEngine::Open { .. })),
        ToEngine::Open { rate: 44_100, channels: 2, ring_frames: 4096, .. }));
    // The app's samples reach lyrad's end of the ring.
    let cons = rings.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(s.write(&[0.5, -0.5, 0.25, -0.25]), 2);
    let mut out = [0.0f32; 4];
    assert_eq!(cons.read(&mut out), 2);
    assert_eq!(out, [0.5, -0.5, 0.25, -0.25]);
    // The app's own volume becomes lyrad's gain: 0.5 -> about -6 dB.
    c.set_volume(&s, 0.5).unwrap();
    let ToEngine::SetGain { db, .. } = next_engine(&eng, |m| matches!(m, ToEngine::SetGain { db, .. } if *db < -1.0)) else { unreachable!() };
    assert!((db + 6.02).abs() < 0.05, "{db}");
    // Commands reach lyrad; events come back to the app.
    c.start(&s).unwrap();
    next_engine(&eng, |m| matches!(m, ToEngine::Start { .. }));
    c.flush(&s).unwrap();
    assert_eq!(c.next_event().unwrap(), ToApp::Flushed { stream: s.id, frame: 42 });
    // Hanging up closes the stream in lyrad.
    let id = s.id;
    drop(s);
    drop(c);
    assert_eq!(next_engine(&eng, |m| matches!(m, ToEngine::Close { .. })), ToEngine::Close { stream: id });
}

#[test]
fn an_unregistered_uid_or_a_missing_grant_is_refused() {
    // Our uid is not in the registry.
    let (sock, _e, _r) = start("unreg", "4242 alice org.atrium.player\n", "org.atrium.player audio\n", CAP_AUDIO);
    let err = Client::connect(sock.to_str().unwrap(), None).err().expect("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    // Registered, but the user never granted audio.
    let reg = format!("{} alice org.atrium.player\n", me());
    let (sock, _e, _r) = start("nogrant", &reg, "", CAP_AUDIO | CAP_MICROPHONE | CAP_MONITOR);
    assert!(Client::connect(sock.to_str().unwrap(), None).is_err());
    // Granted audio, but reached a socket that doesn't allow it.
    let (sock, _e, _r) = start("wrongsock", &reg, "org.atrium.player audio\n", CAP_MONITOR & !CAP_AUDIO);
    assert!(Client::connect(sock.to_str().unwrap(), None).is_err());
}

#[test]
fn limits_and_other_apps_streams() {
    let reg = format!("{} alice org.atrium.player\n", me());
    let (sock, _eng, _rings) = start("limits", &reg, "org.atrium.player audio\n", CAP_AUDIO);
    let path = sock.to_str().unwrap();
    let mut a = Client::connect(path, None).unwrap();
    // Bad format and too much memory are refused.
    assert!(a.open(StreamParams { channels: 3, ..params() }).is_err());
    let big = StreamParams { rate: 192_000, channels: 8, buffer_ms: 2_000, ..params() };
    assert!(a.open(big).is_err(), "over the per-app ring budget");
    // At most 8 streams.
    let mut held = Vec::new();
    for _ in 0..wire::MAX_STREAMS_PER_APP {
        held.push(a.open(StreamParams { buffer_ms: 10, ..params() }).unwrap());
    }
    let e = a.open(StreamParams { buffer_ms: 10, ..params() }).err().unwrap();
    assert!(e.to_string().contains("TooManyStreams"), "{e}");
    // Another connection can't drive a's stream.
    let mut b = UnixStream::connect(path).unwrap();
    wire::send(&mut b, &ToChoragus::Hello { version: wire::VERSION, ticket: None }).unwrap();
    assert!(matches!(wire::recv::<_, ToApp>(&mut b).unwrap(), ToApp::Welcome { .. }));
    wire::send(&mut b, &ToChoragus::Start { stream: held[0].id }).unwrap();
    assert_eq!(wire::recv::<_, ToApp>(&mut b).unwrap(), ToApp::Denied { tag: 0, code: DenyCode::NoSuchStream });
}

fn gain(eng: &mpsc::Receiver<ToEngine>, want: f32) {
    let ToEngine::SetGain { db, .. } = next_engine(eng, |m| matches!(m, ToEngine::SetGain { db, .. } if (db - want).abs() < 0.05))
        else { unreachable!() };
    assert!((db - want).abs() < 0.05, "{db}");
}

/// The next `Apps` the volume UI gets with `n` apps in it.
fn apps(ui: &mut Control, n: usize) -> (f32, bool, Vec<AppEntry>) {
    loop {
        if let FromControl::Apps { master, master_muted, apps } = ui.recv().unwrap() {
            if apps.len() == n {
                return (master, master_muted, apps);
            }
        }
    }
}

#[test]
fn the_volume_ui_sets_the_users_levels_and_they_persist() {
    let reg = format!("{} alice org.atrium.player\n", me());
    let grants = "org.atrium.player audio,audio-control\n";
    let state = dir("ctl-state");
    let socks = [("audio.sock", CAP_AUDIO), ("audio-control.sock", CAP_CONTROL)];
    let (s1, eng, _rings) = start_with("ctl", &reg, grants, &socks, Some(state.clone()));
    let mut ui = Control::connect(s1[1].to_str().unwrap()).expect("welcome");
    ui.send(&ToControl::List).unwrap();
    assert_eq!(apps(&mut ui, 0), (1.0, false, vec![]));
    // An app starts playing; the volume UI is told unasked.
    let mut c = Client::connect(s1[0].to_str().unwrap(), None).unwrap();
    let s = c.open(params()).unwrap();
    let (_, _, a) = apps(&mut ui, 1);
    assert_eq!(a[0], AppEntry { app_id: "org.atrium.player".into(), label: "Video".into(), role: Role::Media, volume: 1.0, muted: false, streams: 1 });
    // The user's level for the app becomes lyrad's gain: 0.5 -> about -6 dB.
    ui.send(&ToControl::SetAppVolume { app_id: "org.atrium.player".into(), volume: 0.5 }).unwrap();
    gain(&eng, -6.02);
    assert_eq!(apps(&mut ui, 1).2[0].volume, 0.5);
    // The app's own volume multiplies under the user's: it can only go lower.
    c.set_volume(&s, 0.5).unwrap();
    gain(&eng, -12.04);
    // Out-of-range or not-a-number levels never amplify.
    ui.send(&ToControl::SetMaster { volume: f32::NAN }).unwrap();
    gain(&eng, choragus::daemon::HELD_DB);
    ui.send(&ToControl::SetMaster { volume: 7.0 }).unwrap();
    gain(&eng, -12.04);
    // Mutes.
    ui.send(&ToControl::SetMasterMute { muted: true }).unwrap();
    gain(&eng, choragus::daemon::HELD_DB);
    ui.send(&ToControl::SetMasterMute { muted: false }).unwrap();
    gain(&eng, -12.04);
    ui.send(&ToControl::SetAppMute { app_id: "org.atrium.player".into(), muted: true }).unwrap();
    gain(&eng, choragus::daemon::HELD_DB);
    ui.send(&ToControl::SetAppMute { app_id: "org.atrium.player".into(), muted: false }).unwrap();
    gain(&eng, -12.04);
    // Outputs: one sink in v1; an unknown one is refused.
    ui.send(&ToControl::ListOutputs).unwrap();
    loop {
        match ui.recv().unwrap() {
            FromControl::Outputs { current, outputs } => {
                assert_eq!((current, outputs), (0, vec![Output { id: 0, name: "Speakers".into(), balance: 0.0, channels: 2 }]));
                break;
            }
            _ => continue,
        }
    }
    // Balance is the output's: lyrad gets it, the UI sees it, and it's kept.
    ui.send(&ToControl::SetBalance { output: 0, balance: -0.5 }).unwrap();
    assert_eq!(next_engine(&eng, |m| matches!(m, ToEngine::SetBalance { balance, .. } if *balance < 0.0)),
        ToEngine::SetBalance { sink: 0, balance: -0.5 });
    loop {
        if let FromControl::Outputs { outputs, .. } = ui.recv().unwrap() {
            assert_eq!(outputs[0].balance, -0.5);
            break;
        }
    }
    ui.send(&ToControl::SetBalance { output: 0, balance: f32::NAN }).unwrap();
    assert_eq!(next_engine(&eng, |m| matches!(m, ToEngine::SetBalance { .. })), ToEngine::SetBalance { sink: 0, balance: 0.0 });
    ui.send(&ToControl::SetBalance { output: 0, balance: 0.25 }).unwrap();
    next_engine(&eng, |m| matches!(m, ToEngine::SetBalance { balance, .. } if *balance == 0.25));
    // The speaker layout: lyrad is told, and apps learn the new channel count.
    ui.send(&ToControl::SetLayout { output: 0, channels: 6 }).unwrap();
    assert_eq!(next_engine(&eng, |m| matches!(m, ToEngine::SetLayout { channels: 6, .. })), ToEngine::SetLayout { sink: 0, channels: 6 });
    loop {
        match c.next_event().unwrap() {
            ToApp::OutputChanged { device_channels } => { assert_eq!(device_channels, 6); break; }
            _ => continue,
        }
    }
    let c6 = Client::connect(s1[0].to_str().unwrap(), None).unwrap();
    assert_eq!(c6.device_channels, 6, "a new app is welcomed with the layout");
    drop(c6);
    // A layout the device refuses is not kept: the UI sees what it has.
    ui.send(&ToControl::SetLayout { output: 0, channels: 8 }).unwrap();
    let mut seen = Vec::new();
    loop {
        // Earlier requests' Outputs may still be queued: start at the 8.
        if let FromControl::Outputs { outputs, .. } = ui.recv().unwrap() {
            if outputs[0].channels == 8 || !seen.is_empty() {
                seen.push(outputs[0].channels);
            }
            if seen.len() == 2 { break; }
        }
    }
    assert_eq!(seen, vec![8, 2], "asked for, then what the device took");
    ui.send(&ToControl::SetLayout { output: 0, channels: 6 }).unwrap();
    next_engine(&eng, |m| matches!(m, ToEngine::SetLayout { channels: 6, .. }));
    ui.send(&ToControl::SetLayout { output: 0, channels: 5 }).unwrap();
    loop {
        if let FromControl::Denied { code } = ui.recv().unwrap() {
            assert_eq!(code, DenyCode::BadFormat);
            break;
        }
    }
    ui.send(&ToControl::SetOutput { output: 9 }).unwrap();
    loop {
        if let FromControl::Denied { code } = ui.recv().unwrap() {
            assert_eq!(code, DenyCode::NotGranted);
            break;
        }
    }
    // The app hangs up; the volume UI's list empties.
    drop(s);
    drop(c);
    apps(&mut ui, 0);
    // The user's level was saved, and a restarted choragusd plays at it.
    let saved = std::fs::read_to_string(state.join("alice.toml")).unwrap();
    assert!(saved.contains("volume = 0.5"), "{saved}");
    let (s2, eng2, _r2) = start_with("ctl2", &reg, grants, &socks, Some(state));
    assert_eq!(next_engine(&eng2, |m| matches!(m, ToEngine::SetBalance { .. })), ToEngine::SetBalance { sink: 0, balance: 0.25 },
        "a restarted choragusd gives lyrad the kept balance");
    assert_eq!(next_engine(&eng2, |m| matches!(m, ToEngine::SetLayout { .. })), ToEngine::SetLayout { sink: 0, channels: 6 },
        "and the kept layout");
    let mut c = Client::connect(s2[0].to_str().unwrap(), None).unwrap();
    let _s = c.open(params()).unwrap();
    gain(&eng2, -6.02);
}

#[test]
fn only_the_audio_control_grant_reaches_the_volume_ui() {
    let reg = format!("{} alice org.atrium.player\n", me());
    // An app with audio only can't use the control socket.
    let (s, _e, _r) = start_with("ctl-nogrant", &reg, "org.atrium.player audio\n", &[("audio-control.sock", CAP_CONTROL)], None);
    let err = Control::connect(s[0].to_str().unwrap()).err().expect("refused");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    // Holding audio-control, but reaching only the playback socket, gives no control:
    // that socket speaks the app dictionary and the hello doesn't parse.
    let (s, _e, _r) = start_with("ctl-wrongsock", &reg, "org.atrium.player audio,audio-control\n", &[("audio.sock", CAP_AUDIO)], None);
    assert!(Control::connect(s[0].to_str().unwrap()).is_err());
    // And the control grant alone opens no streams.
    let (s, _e, _r) = start_with("ctl-nostream", &reg, "org.atrium.player audio-control\n", &[("audio.sock", CAP_AUDIO | CAP_CONTROL)], None);
    assert!(Client::connect(s[0].to_str().unwrap(), None).is_err());
}

/// limend's side of the attribution socket: one connection, held open (its
/// tickets die with it).
struct Limend(UnixStream);
impl Limend {
    fn ask(&mut self, m: &ToAttribution) -> FromAttribution {
        wire::send(&mut self.0, m).unwrap();
        wire::recv(&mut self.0).unwrap()
    }
}

#[test]
fn an_embedded_player_plays_as_its_parent_only_with_a_ticket() {
    // Our uid is the embedded player's (and limend's): not in the registry.
    // 50001 is the parent, a mail app that holds audio; 50002 holds none.
    let reg = "50001 alice org.example.mail\n50002 alice org.example.notes\n";
    let grants = "org.example.mail audio\n";
    let state = dir("tkt-state");
    // The user turned the mail app down to half: the player's streams follow it.
    std::fs::write(state.join("alice.toml"), "[apps.\"org.example.mail\"]\nvolume = 0.5\nmuted = false\n").unwrap();
    let socks = [("audio.sock", CAP_AUDIO), ("audio-attribution.sock", choragus::app::CAP_ATTRIBUTION)];
    let (s, eng, _rings) = start_with("tkt", reg, grants, &socks, Some(state));
    let app = s[0].to_str().unwrap();
    // No ticket: an uid the registry can't name is refused.
    assert!(Client::connect(app, None).is_err());
    let mut limend = Limend(UnixStream::connect(&s[1]).unwrap());
    // A parent without audio gets no ticket for its player.
    assert_eq!(limend.ask(&ToAttribution::Issue { child_uid: me(), parent_uid: 50002 }), FromAttribution::Refused);
    // Nor does an uid that isn't an app.
    assert_eq!(limend.ask(&ToAttribution::Issue { child_uid: me(), parent_uid: 4242 }), FromAttribution::Refused);
    // A ticket for another uid is no good to us.
    let FromAttribution::Issued { ticket: other } = limend.ask(&ToAttribution::Issue { child_uid: 4242, parent_uid: 50001 }) else { panic!() };
    assert!(Client::connect(app, Some(other)).is_err());
    // Ours: the player plays, counted as the mail app (its user's level for mail).
    let FromAttribution::Issued { ticket } = limend.ask(&ToAttribution::Issue { child_uid: me(), parent_uid: 50001 }) else { panic!() };
    let mut c = Client::connect(app, Some(ticket)).expect("welcome by ticket");
    let st = c.open(params()).unwrap();
    gain(&eng, -6.02);
    // Once only.
    assert!(Client::connect(app, Some(ticket)).is_err(), "a ticket works once");
    // The embed ends: limend cancels, and the player is hung up.
    assert_eq!(limend.ask(&ToAttribution::Cancel { ticket }), FromAttribution::Cancelled);
    assert_eq!(next_engine(&eng, |m| matches!(m, ToEngine::Close { .. })), ToEngine::Close { stream: st.id });
    assert!(c.next_event().is_err());
    // limend going away kills its tickets.
    let FromAttribution::Issued { ticket } = limend.ask(&ToAttribution::Issue { child_uid: me(), parent_uid: 50001 }) else { panic!() };
    drop(limend);
    std::thread::sleep(Duration::from_millis(100));
    assert!(Client::connect(app, Some(ticket)).is_err(), "limend's tickets die with it");
    // A forged ticket is nothing.
    assert!(Client::connect(app, Some([9; 16])).is_err());
}
