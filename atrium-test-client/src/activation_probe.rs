//! atrium-activation-probe — end to end check of frescod's activation
//! tokens (docs/spec/tabula.md §2) through the real sockets: an app window
//! gets a token with each injected press, the WM gets the same press with
//! none, and the activation socket spends tokens by the rules.
//!
//! Needs frescod with FRESCOD_INJECT_ACTIVATION=1 (injected presses mint),
//! FRESCOD_ACTIVATION_UIDS and FRESCOD_FORUM_UID set to the probe's uid.
//!
//! Usage: atrium-activation-probe SOCK WM_SOCK INJECT_SOCK ACTIVATION_SOCK

use fresco_client::{Connection, Event};
use fresco_protocol::activation::{kind, op, purpose, refusal, Consume, ConsumeResult, Grant, GrantResult, Token};
use fresco_protocol::{RectParams, WindowHints};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const HID_A: u16 = 0x04;
const HID_C: u16 = 0x06;
const HID_V: u16 = 0x19;
const CTRL: u8 = 1 << 1;

struct Probe {
    app: Connection,
    wm: Connection,
    inject: String,
    act: aqueduct::Connection,
    uid: u32,
    failed: u32,
}

impl Probe {
    fn inject(&self, line: &str) {
        let mut s = UnixStream::connect(&self.inject).expect("injector socket");
        writeln!(s, "{line}").unwrap();
    }

    /// The token the next press brings each side: (app's, WM's).
    fn press(&mut self, line: &str) -> (Token, Token) {
        self.inject(line);
        let pick = |c: &mut Connection| -> Token {
            let end = Instant::now() + Duration::from_secs(20);
            while Instant::now() < end {
                match c.wait_event(Some(Duration::from_millis(200))) {
                    Ok(Some(Event::Key { pressed: true, activation, .. })) => return activation,
                    Ok(Some(Event::PointerButton { pressed: true, activation, .. })) => return activation,
                    Ok(_) => {}
                    Err(e) => panic!("event: {e}"),
                }
            }
            panic!("no press arrived for {line:?}");
        };
        (pick(&mut self.app), pick(&mut self.wm))
    }

    fn ask(&mut self, op: u16, payload: Vec<u8>) -> Vec<u8> {
        self.act.send_message(aqueduct::classes::CLASS_DISPLAY, op, aqueduct::flag::RESPONSE_EXPECTED, &payload).unwrap();
        let m = self.act.recv_message().unwrap();
        assert!(m.op == op && m.flags & aqueduct::flag::IS_RESPONSE != 0, "reply op {:#x}", m.op);
        m.payload
    }

    fn consume(&mut self, token: Token, presenter_uid: u32, p: u8) -> ConsumeResult {
        let b = fresco_protocol::encode(&Consume { token, presenter_uid, purpose: p }).unwrap();
        fresco_protocol::decode(&self.ask(op::CONSUME, b)).unwrap()
    }

    fn grant(&mut self, token: Token, for_uid: u32, p: u8) -> GrantResult {
        let b = fresco_protocol::encode(&Grant { token, for_uid, purpose: p }).unwrap();
        fresco_protocol::decode(&self.ask(op::GRANT, b)).unwrap()
    }

    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            self.failed += 1;
        }
    }
}

fn refused(r: ConsumeResult) -> u8 {
    match r {
        ConsumeResult::Ok { .. } => 0,
        ConsumeResult::Refused { reason } => reason,
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [sock, wm_sock, inject, act_sock] = &a[..] else {
        eprintln!("usage: atrium-activation-probe SOCK WM_SOCK INJECT_SOCK ACTIVATION_SOCK");
        std::process::exit(2);
    };
    let mut app = Connection::connect(sock).expect("app socket");
    let win = app.window_create(200, 150, "activation", WindowHints::default()).expect("window");
    app.set_default_window(win as u16);
    app.scene_frame_begin().unwrap();
    app.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 200.0, h: 150.0, r: 0.2, g: 0.5, b: 0.8, a: 1.0, radius: 0.0 }).unwrap();
    app.scene_frame_end().unwrap();
    let wm = Connection::connect(wm_sock).expect("wm socket");
    std::thread::sleep(Duration::from_millis(500));
    let act = aqueduct::Connection::connect(act_sock).expect("activation socket");
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut p = Probe { app, wm, inject: inject.clone(), act, uid, failed: 0 };
    println!("window {win}, uid {uid}");

    let (t, w) = p.press(&format!("KEY {HID_V} 1 {CTRL}"));
    p.inject(&format!("KEY {HID_V} 0 {CTRL}"));
    p.check(t != [0; 16], "the app's Ctrl-V carries a token");
    p.check(w == [0; 16], "the WM's copy of it carries none");
    let r = p.consume(t, uid + 1, purpose::PASTE);
    p.check(refused(r) == refusal::WRONG_UID, "another app cannot spend it");
    let r = p.consume(t, uid, purpose::PASTE);
    p.check(matches!(r, ConsumeResult::Ok { kind: kind::KEY, window_id, .. } if window_id == win), "its own app pastes with it");
    let r = p.consume(t, uid, purpose::PASTE);
    p.check(refused(r) == refusal::UNKNOWN, "only once");

    let (t, _) = p.press(&format!("KEY {HID_A} 1 0"));
    p.inject(&format!("KEY {HID_A} 0 0"));
    let r = p.consume(t, uid, purpose::PASTE);
    p.check(refused(r) == refusal::WRONG_KIND, "typing 'a' authorises no paste");

    let (t, _) = p.press(&format!("KEY {HID_C} 1 {CTRL}"));
    p.inject(&format!("KEY {HID_C} 0 {CTRL}"));
    let r = p.consume(t, uid, purpose::COPY);
    p.check(matches!(r, ConsumeResult::Ok { .. }), "Ctrl-C copies");

    let at = p.wm.wm_enumerate().expect("enumerate").into_iter().find(|s| s.surface_id == win).expect("our window").rect;
    p.inject(&format!("MOVE {} {}", at.x + at.w / 2, at.y + at.h / 2));
    let (t, _) = p.press("BTN 1 1");
    p.inject("BTN 1 0");
    std::thread::sleep(Duration::from_millis(1100));
    let r = p.consume(t, uid, purpose::PASTE);
    p.check(refused(r) == refusal::EXPIRED, "a click is good for a second");

    let (t, _) = p.press("BTN 1 1");
    p.inject("BTN 1 0");
    let g = p.grant(t, uid, purpose::ACTION);
    let granted = match g {
        GrantResult::Ok { token } => token,
        GrantResult::Refused { reason } => {
            p.check(false, &format!("Forum's click grants a token (refused {reason})"));
            [0; 16]
        }
    };
    let r = p.consume(granted, uid, purpose::PASTE);
    p.check(refused(r) == refusal::WRONG_KIND, "a granted token is for its one purpose");
    let r = p.consume(granted, uid, purpose::ACTION);
    p.check(matches!(r, ConsumeResult::Ok { kind: kind::GRANTED, .. }), "and spends on it");
    let r = p.consume([7; 16], uid, purpose::PASTE);
    p.check(refused(r) == refusal::UNKNOWN, "a made-up token is unknown");

    if p.failed > 0 {
        println!("{} FAILED", p.failed);
        std::process::exit(1);
    }
    println!("all passed");
}
