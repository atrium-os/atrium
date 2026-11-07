//! choragusd — Choragus, Lyra's audio policy daemon and the front door apps
//! use for sound (docs/spec/lyra-v1.md §3–5; architecture §7, §9).
//!
//!   choragusd --serve [--limen-uid UID]
//!       The service: one daemon for the seat, identity only from the launch
//!       registry, grants from each user's Portcullis policy, one socket per
//!       audio capability, and audio-attribution/ for limend (served only to
//!       --limen-uid; without it, nobody). Paths from the environment (jaild passes ATRIUM_*):
//!         ATRIUM_CHORAGUS_SOCKETS  socket root   (default /atrium/sockets)
//!         ATRIUM_LYRAD_SOCK        lyrad         (default /atrium/sockets/lyrad/lyrad.sock)
//!         ATRIUM_APP_REGISTRY      registry      (default /var/run/atrium/app-registry)
//!         ATRIUM_POLICY_BASE       user policies (default /var/db/atrium)
//!         ATRIUM_CHORAGUS_STATE    users' volumes (default /var/db/choragus)
//!
//!   choragusd --daemon <app_sock> <lyrad_sock> [--grants F] [--portcullis-grants BASE]
//!                      [--app-registry F] [--seat] [--control SOCK] [--state DIR]
//!                      [--attribution SOCK --limen-uid UID]
//!       Development: one socket allowing every capability (and, with
//!       --control, a volume-UI socket; --state keeps volumes in DIR). Without a registry
//!       the app is named by its uid's user name (and --grants is keyed by it).
//!
//!   choragusd --demo
//!       The policy layer on its own: ducking, hotplug, the privacy check.

use choragus::capability::{check, Access, Capability, Grant};
use choragus::daemon::{self, Config};
use choragus::grant::GrantStore;
use choragus::policy::{diff, Device, DeviceKind, Role, Session, Stream};

fn arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let base = Config {
        listeners: Vec::new(),
        lyrad: String::new(),
        registry: None,
        policy_base: None,
        grants: GrantStore::default(),
        seat: false,
        seat_path: portcullis_peer::seat::ACTIVE_SESSION.to_string(),
        strict: false,
        device_rate: 48_000,
        device_channels: 2,
        period_frames: 128,
        state_dir: None,
        limen_uid: None,
    };

    if args.iter().any(|a| a == "--serve") {
        let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
        let root = env("ATRIUM_CHORAGUS_SOCKETS", "/atrium/sockets");
        let mut listeners: Vec<(String, u8)> = choragus::identity::SOCKETS
            .iter()
            .map(|(name, caps)| (format!("{root}/{name}/{name}.sock"), *caps))
            .collect();
        // limend's door for attribution tickets; no app jail mounts it.
        listeners.push((format!("{root}/audio-attribution/audio-attribution.sock"), choragus::app::CAP_ATTRIBUTION));
        let cfg = Config {
            listeners,
            limen_uid: arg(&args, "--limen-uid").and_then(|v| v.parse().ok()),
            lyrad: env("ATRIUM_LYRAD_SOCK", "/atrium/sockets/lyrad/lyrad.sock"),
            registry: Some(env("ATRIUM_APP_REGISTRY", portcullis_peer::DEFAULT_REGISTRY)),
            policy_base: Some(env("ATRIUM_POLICY_BASE", "/var/db/atrium")),
            state_dir: Some(env("ATRIUM_CHORAGUS_STATE", "/var/db/choragus")),
            seat: true,
            strict: true,
            ..base
        };
        let e = daemon::run(cfg);
        eprintln!("choragusd: {e}; exiting so the supervisor restarts us");
        std::process::exit(1);
    }

    if let Some(i) = args.iter().position(|a| a == "--daemon") {
        use choragus::app::{CAP_AUDIO, CAP_MICROPHONE, CAP_MONITOR};
        let app_sock = args.get(i + 1).cloned().unwrap_or_else(|| "/tmp/choragus.sock".into());
        let lyrad = args.get(i + 2).cloned().unwrap_or_else(|| "/tmp/lyrad.ctl".into());
        let grants = match arg(&args, "--grants") {
            Some(p) => GrantStore::load(p).unwrap_or_else(|e| {
                eprintln!("choragusd: no grants file {p} ({e}); default-deny all");
                GrantStore::default()
            }),
            None => GrantStore::default(),
        };
        let mut listeners = vec![(app_sock, CAP_AUDIO | CAP_MICROPHONE | CAP_MONITOR)];
        if let Some(p) = arg(&args, "--control") {
            listeners.push((p.to_string(), choragus::app::CAP_CONTROL));
        }
        if let Some(p) = arg(&args, "--device") {
            listeners.push((p.to_string(), choragus::app::CAP_DEVICE));
        }
        if let Some(p) = arg(&args, "--media") {
            listeners.push((p.to_string(), choragus::app::CAP_MEDIA));
        }
        if let Some(p) = arg(&args, "--attribution") {
            listeners.push((p.to_string(), choragus::app::CAP_ATTRIBUTION));
        }
        let cfg = Config {
            listeners,
            state_dir: arg(&args, "--state").map(String::from),
            limen_uid: arg(&args, "--limen-uid").and_then(|v| v.parse().ok()),
            lyrad,
            registry: arg(&args, "--app-registry").map(String::from),
            policy_base: arg(&args, "--portcullis-grants").map(String::from),
            grants,
            seat: args.iter().any(|a| a == "--seat"),
            ..base
        };
        let e = daemon::run(cfg);
        eprintln!("choragusd: {e}");
        std::process::exit(1);
    }

    let demo = args.iter().any(|a| a == "--demo");
    if !demo {
        eprintln!("choragusd: try --serve, --daemon <app_sock> <lyrad_sock>, or --demo");
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
