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
