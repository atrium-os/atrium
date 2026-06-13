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
use choragus::policy::{Device, DeviceKind, Role, Session, Stream, diff};

fn main() {
    let demo = std::env::args().any(|a| a == "--demo");
    if !demo {
        eprintln!("choragusd: policy/session layer (skeleton). try --demo");
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
