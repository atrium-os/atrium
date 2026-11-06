//! A credential field end to end with the real Arca (arca-autofill.md §9).
//!
//! The parent side of a login form: an app window with a `credential` slot,
//! filled by arca-ui through limend, frescod and arcad, all of them real
//! processes. Run it as the app (a uid in the launch registry) with:
//!
//!   limen-arca-probe FRESCO_SOCKET LIMEN_SOCKET INJECT_SOCKET [expect-none]
//!
//! frescod needs FRESCOD_INJECT_ACTIVATION=1 (injected presses mint
//! tokens), limend's uid in FRESCOD_SLOT_UIDS and arca-ui's in
//! FRESCOD_ACTIVATION_UIDS. With `expect-none` the app has nothing saved
//! for it, so no press may fill.
//!
//! Checks: the slot attaches, nothing fills on load, a press on Arca's chip
//! fills (the username, then a handle), typing over the fill clears it,
//! and Tab hands over a handle for what was typed.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use fresco_client::Connection;
use fresco_protocol::{RectParams, WindowHints};
use limen::client::Parent;
use limen::credential::{Purpose, ToChild, ToParent};
use limen::proto::{EmbedId, ParentEvent};

const WIN: (i32, i32) = (100, 100);
const SLOT: [f32; 4] = [20.0, 20.0, 240.0, 36.0];
const HID_A: u16 = 0x04;
const HID_TAB: u16 = 0x2B;

fn inject(sock: &str, lines: &[String]) {
    let mut s = UnixStream::connect(sock).expect("injector socket");
    for l in lines {
        writeln!(s, "{l}").unwrap();
    }
}

/// The next message from the child, skipping lifecycle events.
fn next(app: &mut Parent, e: EmbedId, wait: Duration) -> Option<ToParent> {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        match app.poll(Duration::from_millis(50)).expect("limend") {
            Some(ParentEvent::Message { embed, bytes }) if embed == e => {
                return Some(ToParent::decode(&bytes).expect("a well-formed message"));
            }
            Some(ev) => println!("     ({ev:?})"),
            None => {}
        }
    }
    None
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (sock, limen_sock, isock) = match &a[..] {
        [s, l, i] | [s, l, i, _] => (s.as_str(), l.as_str(), i.as_str()),
        _ => {
            eprintln!("usage: limen-arca-probe FRESCO_SOCKET LIMEN_SOCKET INJECT_SOCKET [expect-none]");
            std::process::exit(2);
        }
    };
    let expect_none = a.get(3).is_some_and(|m| m == "expect-none");
    let mut failed = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            failed += 1;
        }
    };

    let mut win = Connection::connect(sock).expect("frescod app socket");
    let id = win
        .window_create(400, 200, "login", WindowHints { initial_position: Some(WIN), ..Default::default() })
        .expect("window");
    win.set_default_window(id as u16);
    win.scene_frame_begin().unwrap();
    win.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 400.0, h: 200.0, r: 0.9, g: 0.9, b: 0.9, a: 1.0, radius: 0.0 }).unwrap();
    win.scene_frame_end().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    let token = loop {
        if let Ok(Some(fresco_client::Event::EmbedToken { token, .. })) = win.wait_event(Some(Duration::from_millis(100))) {
            break token;
        }
        assert!(Instant::now() < until, "no embed token from frescod");
    };

    let mut app = Parent::at(limen_sock).expect("limend");
    let e = app.request("credential", token).expect("request");
    let cfg = ToChild::Configure { purpose: Purpose::CurrentPassword, username_field: true };
    app.send(e, postcard::to_stdvec(&cfg).unwrap()).unwrap();
    app.place(e, Some(SLOT)).unwrap();
    let mut attached = false;
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until && !attached {
        attached = app.poll(Duration::from_millis(100)).unwrap() == Some(ParentEvent::Attached { embed: e });
    }
    check(attached, "arca-ui's field is attached in the slot");

    let early = next(&mut app, e, Duration::from_secs(2));
    check(early.is_none(), &format!("nothing fills on load ({early:?})"));

    // A press on the chip: the right part of the slot, once the field has
    // focus (the first press gives it focus, and the chip shows). Under TCG
    // a press can land before the frame that shows the chip, and the slot
    // must have been still for 500 ms, so press until something comes.
    let x = WIN.0 as f32 + SLOT[0] + SLOT[2] * 0.8;
    let y = WIN.1 as f32 + SLOT[1] + SLOT[3] / 2.0;
    let click = vec![format!("MOVE {x} {y}"), "BTN 1 1".into(), "BTN 1 0".into()];
    let mut got = None;
    let until = Instant::now() + Duration::from_secs(if expect_none { 15 } else { 90 });
    while Instant::now() < until && got.is_none() {
        inject(isock, &click);
        got = next(&mut app, e, Duration::from_millis(900));
    }
    if expect_none {
        check(got.is_none(), &format!("an app with nothing saved: no press fills ({got:?})"));
    } else {
        let filled = matches!(&got, Some(ToParent::Filled { username: Some(u) }) if !u.is_empty());
        check(filled, &format!("a press on the chip fills, with the username ({got:?})"));
        let h = next(&mut app, e, Duration::from_secs(5));
        check(matches!(h, Some(ToParent::Handle { handle }) if handle != [0; 16]), &format!("then a handle ({h:?})"));

        let key = |u: u16| vec![format!("KEY {u} 1 0"), format!("KEY {u} 0 0")];
        inject(isock, &key(HID_A));
        let c = next(&mut app, e, Duration::from_secs(10));
        check(c == Some(ToParent::Cleared), &format!("typing over the fill clears it ({c:?})"));
        inject(isock, &key(HID_TAB));
        let h = next(&mut app, e, Duration::from_secs(10));
        check(matches!(h, Some(ToParent::Handle { .. })), &format!("Tab holds what was typed ({h:?})"));
    }
    app.detach(e).unwrap();

    if failed > 0 {
        println!("{failed} FAILED");
        std::process::exit(1);
    }
    println!("all passed");
}
