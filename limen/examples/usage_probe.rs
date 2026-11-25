//! portcullisd's `InstanceUsage` on an engine's connection
//! (navigator-ui.md §15.3 check 2), against the installed portcullisd.
//!
//! Starts a Navigator engine the way limend does (as `_limen`, through its
//! worker rule, serving a running app instance: portcullisd starts an
//! engine only for one) and asks for its memory on the connection that
//! holds it. The instance is `LIMEN_PROBE_SERVES` (a uid), else the running
//! navigatord's. Stage it outside `/root`, which `_limen` cannot enter:
//!
//!   su -m _limen -c /opt/navigator-live/limen-usage-probe
//!
//! Checks: the first ask is answered with a use above 0 and the part cap,
//! a second ask at once is refused (one a second), one asked after the gap
//! is answered again (the positive control), and the engine's jail goes
//! when the connection does. Exits non-zero on a failure.

use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::{Duration, Instant};

use limen::daemon::Launcher;

fn jail_exists(name: &str) -> bool {
    Command::new("jls").args(["-j", name, "jid"]).output().is_ok_and(|o| o.status.success())
}

/// The uid the engine serves: `LIMEN_PROBE_SERVES`, else navigatord's.
fn serves() -> Option<u32> {
    if let Ok(v) = std::env::var("LIMEN_PROBE_SERVES") { return v.trim().parse().ok(); }
    let pids = Command::new("pgrep").args(["-x", "navigatord"]).output().ok()?;
    let pid = String::from_utf8_lossy(&pids.stdout).split_whitespace().next()?.to_string();
    let uid = Command::new("ps").args(["-o", "uid=", "-p", &pid]).output().ok()?;
    String::from_utf8_lossy(&uid.stdout).trim().parse().ok()
}

fn main() {
    let mut failed = 0;
    let mut ran = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        ran += 1;
        if !ok { failed += 1; }
    };
    // An embed id limend won't use, so the instance name is the probe's.
    let embed = 0xfeed_0000_0000_0000 | u64::from(std::process::id());
    let jail = format!("app-org-atrium-navigator-engine--limen-{embed:016x}");
    let mut p = limen::launch::Portcullis::new(std::env::var("LIMEN_USER").unwrap_or_else(|_| "_limen".into()));
    let (ours, theirs) = UnixStream::pair().expect("socketpair");
    let Some(parent) = serves() else {
        println!("FAIL no app instance to serve: set LIMEN_PROBE_SERVES or run a Navigator");
        std::process::exit(1);
    };
    match p.launch_for(limen::catalogue::NAVIGATOR_ENGINE, embed, theirs, parent) {
        Ok(uid) => println!("engine started as uid {uid} in {jail}, serving uid {parent}"),
        Err(e) => {
            println!("FAIL the engine did not start: {e}");
            std::process::exit(1);
        }
    }
    check(jail_exists(&jail), "its jail is there");
    let mut ask = || p.usage(embed).and_then(|mut u| u.ask());
    let cap = limen::web_island::PART_MEMORY_MB << 20;
    let first = ask();
    check(first.is_some_and(|(m, c)| m > 0 && c == Some(cap)), &format!("answered: {first:?}, cap {cap}"));
    let soon = ask();
    check(soon.is_none(), &format!("asked again at once: refused ({soon:?})"));
    std::thread::sleep(Duration::from_millis(1100));
    let later = ask();
    check(later.is_some_and(|(m, _)| m > 0), &format!("asked after the gap: answered ({later:?})"));
    drop(ours);
    p.end(embed);
    let until = Instant::now() + Duration::from_secs(10);
    while jail_exists(&jail) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(200));
    }
    check(!jail_exists(&jail), "the jail goes when the connection does");

    const DECLARED: u32 = 5;
    if failed > 0 || ran != DECLARED {
        println!("{failed} FAILED, {ran} of {DECLARED} ran");
        std::process::exit(1);
    }
    println!("all passed");
}
