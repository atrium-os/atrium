//! "Show in Files" spends only a live press of the parent's (scrinium.md
//! §4.5, navigator-ui.md §15.3 check 4), against the installed frescod.
//!
//! Runs limend's code in process with the real frescod activation socket.
//! One uid plays the parent and a stand-in Scrinium, so it must be a uid
//! frescod lets spend `show` presses: run it as `_limen`.
//!
//!   su -m _limen -c 'FRESCO_SOCKET=... limen-show-probe'
//!
//! It prints `PRESS x y` when its window is up; the gate injects a click
//! there. Checks: a zeroed press and a made-up press are refused, the real
//! press is spent once (the positive control), and spending it again (a
//! stale press) is refused. Exits non-zero on a failure.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};
use limen::client::{Child, Error, Parent};
use limen::daemon::{serve, Launcher, Shared};
use limen::picker::{Folder, ToChild};
use limen::proto::{refusal, ChildEvent, EmbedId};

struct NoLaunch;
impl Launcher for NoLaunch {
    fn launch(&mut self, _: &str, _: EmbedId, _: UnixStream) -> Result<u32, String> { Err("not in this probe".into()) }
    fn end(&mut self, _: EmbedId) {}
}

/// Where the window asks to be put; the click lands in its middle.
const AT: (i32, i32) = (200, 200);
const SIZE: (u32, u32) = (300, 160);

fn main() {
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let act = std::env::var("LIMEN_FRESCOD_ACTIVATION").unwrap_or_else(|_| limen::fresco::DEFAULT_SOCKET.into());
    // One uid plays every part, so the registry says it is Scrinium.
    let resolve = Box::new(|_: u32| Some(("probe".to_string(), limen::catalogue::SCRINIUM.to_string())));
    let shared = Shared::new(Box::new(limen::fresco::Frescod::new(act)), Box::new(NoLaunch), resolve, Box::new(|_: &str, _: &str| None));
    // SAFETY: getuid has no preconditions.
    let me = unsafe { libc::getuid() };
    let conn = |sh: &Arc<Shared>| {
        let (a, b) = UnixStream::pair().expect("socketpair");
        let sh = sh.clone();
        std::thread::spawn(move || { let _ = serve(aqueduct::Connection::wrap(b).expect("wrap"), me, sh); });
        aqueduct::Connection::wrap(a).expect("wrap")
    };
    let mut failed = 0;
    let mut ran = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        ran += 1;
        if !ok { failed += 1; }
    };

    let mut win = Connection::connect(&sock).expect("frescod app socket");
    let id = win.window_create(SIZE.0, SIZE.1, "show-probe", WindowHints { initial_position: Some(AT), ..Default::default() }).expect("window");
    win.set_default_window(id as u16);
    win.scene_frame_begin().expect("frame");
    win.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 2000.0, h: 2000.0, r: 0.8, g: 0.5, b: 0.2, a: 1.0, radius: 0.0 }).expect("rect");
    win.scene_frame_end().expect("frame");
    let mut token = None;
    let until = Instant::now() + Duration::from_secs(5);
    while token.is_none() && Instant::now() < until {
        if let Ok(Some(Event::EmbedToken { token: t, .. })) = win.wait_event(Some(Duration::from_millis(100))) {
            token = Some(t);
        }
    }
    let Some(host) = token else {
        println!("FAIL no embed token from frescod");
        std::process::exit(1);
    };

    let mut files = Child::wrap(conn(&shared));
    files.provide_role("picker").expect("provide picker");
    let mut app = Parent::wrap(conn(&shared));
    let e = app.request("picker", host).expect("request picker");
    check(matches!(files.poll(Duration::from_secs(2)), Ok(Some(ChildEvent::Attach { embed, .. })) if embed == e), "the stand-in Files hears the picker");

    // Each ask goes the way Navigator's does: a Show message from the
    // parent, which Files spends through limend.
    let mut asked = 0u32;
    let mut show = |press: [u8; 16]| -> Result<(), Error> {
        // Each ask names its own file, so it spends its own message and
        // never one left over from the ask before.
        asked += 1;
        let name = format!("report-{asked}.csv");
        let m = ToChild::Show { folder: Folder::Downloads, name: name.clone(), press };
        app.send(e, m.encode())?;
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            match files.poll(Duration::from_millis(200))? {
                Some(ChildEvent::Message { embed, bytes }) if embed == e => match ToChild::decode(&bytes) {
                    Some(ToChild::Show { name: n, press: p, .. }) if n == name => return files.spend_show_press(e, p),
                    other => println!("     (skipped an earlier message: {other:?})"),
                },
                Some(other) => println!("     (skipped {other:?})"),
                None => {}
            }
        }
        println!("     (no message for {name} in 5 s)");
        Err(Error::Refused(u8::MAX))
    };
    let no_press = |r: &Result<(), Error>| matches!(r, Err(Error::Refused(refusal::NO_PRESS)));
    let r = show([0; 16]);
    check(no_press(&r), &format!("no press: refused ({r:?})"));
    let mut made_up = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut made_up)).expect("urandom");
    let r = show(made_up);
    check(no_press(&r), &format!("a made-up press: refused ({r:?})"));

    println!("PRESS {} {}", AT.0 + SIZE.0 as i32 / 2, AT.1 + SIZE.1 as i32 / 2);
    let mut press = None;
    let until = Instant::now() + Duration::from_secs(60);
    while press.is_none() && Instant::now() < until {
        if let Ok(Some(Event::PointerButton { pressed: true, activation, .. })) = win.wait_event(Some(Duration::from_millis(100))) {
            press = Some(activation).filter(|a| *a != [0; 16]);
        }
    }
    check(press.is_some(), "a click on the window gave a press");
    if let Some(p) = press {
        let first = show(p);
        check(first.is_ok(), &format!("the live press is spent for Show ({first:?})"));
        let again = show(p);
        check(no_press(&again), &format!("the same press again (stale): refused ({again:?})"));
    }

    const DECLARED: u32 = 6;
    if failed > 0 || ran != DECLARED {
        println!("{failed} FAILED, {ran} of {DECLARED} ran");
        std::process::exit(1);
    }
    println!("all passed");
}
