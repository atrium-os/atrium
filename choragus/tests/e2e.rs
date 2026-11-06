//! choragusd end to end on the host: a stand-in lyrad, the real daemon, the
//! real client library. Checks identity from the registry, the ring brokered
//! to the app, volumes composed into lyrad's gain, events forwarded, and that
//! an app can only name its own streams.

use choragus::app::{CAP_AUDIO, CAP_MICROPHONE, CAP_MONITOR};
use choragus::daemon::{self, Config};
use choragus::grant::GrantStore;
use lyra_protocol::client::Client;
use lyra_protocol::ring::Ring;
use lyra_protocol::wire::{self, DenyCode, FromEngine, Role, StreamParams, ToApp, ToChoragus, ToEngine};
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
    let d = dir(tag);
    let lyrad = d.join("lyrad.sock");
    let (eng, rings) = fake_lyrad(lyrad.clone(), tag);
    std::fs::write(d.join("registry"), registry).unwrap();
    let app = d.join("audio.sock");
    let cfg = Config {
        listeners: vec![(app.to_string_lossy().into(), socket_caps)],
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
    };
    std::thread::spawn(move || daemon::run(cfg));
    for _ in 0..200 {
        if app.exists() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    (app, eng, rings)
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
