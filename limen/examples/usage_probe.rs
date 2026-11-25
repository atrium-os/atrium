//! portcullisd's `InstanceUsage` on an engine's connection
//! (navigator-ui.md §15.3 check 2), against the installed portcullisd.
//!
//! Starts a Navigator engine the way limend does (as `_limen`, through its
//! worker rule) and asks for its memory on the connection that holds it:
//!
//!   su -m _limen -c limen-usage-probe
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
    match p.launch(limen::catalogue::NAVIGATOR_ENGINE, embed, theirs) {
        Ok(uid) => println!("engine started as uid {uid} in {jail}"),
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
