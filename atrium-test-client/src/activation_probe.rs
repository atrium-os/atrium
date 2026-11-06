//! atrium-activation-probe — end to end check of frescod's activation
//! tokens (docs/spec/tabula.md §2) through the real sockets: an app window
//! gets a token with each injected press, the WM gets the same press with
//! none, and the activation socket spends tokens by the rules. Then a
//! drag from its window to another app's ends in a DROP token for that app.
//!
//! Needs frescod with FRESCOD_INJECT_ACTIVATION=1 (injected presses mint),
//! FRESCOD_ACTIVATION_UIDS and FRESCOD_FORUM_UID set to the probe's uid.
//!
//! Usage: atrium-activation-probe SOCK WM_SOCK INJECT_SOCK ACTIVATION_SOCK

use fresco_client::{Connection, Event};
use fresco_protocol::activation::{kind, op, purpose, refusal, Consume, ConsumeResult, Grant, GrantResult, Token};
use fresco_protocol::{RectParams, WindowHints, WmRole};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const HID_A: u16 = 0x04;
const HID_C: u16 = 0x06;
const HID_LSHIFT: u16 = 0xE1;
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

/// The next event `want` picks, within 20 s.
fn wait(c: &mut Connection, want: impl Fn(&Event) -> bool) -> Option<Event> {
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end {
        if let Ok(Some(e)) = c.wait_event(Some(Duration::from_millis(200))) {
            if want(&e) {
                return Some(e);
            }
        }
    }
    None
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
    // A second app's window, under the first: the drop target.
    let mut other = Connection::connect(sock).expect("app socket");
    let hints = WindowHints { initial_position: Some((400, 100)), ..WindowHints::default() };
    let target = other.window_create(200, 150, "drop here", hints).expect("window");
    other.set_default_window(target as u16);
    other.scene_frame_begin().unwrap();
    other.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 200.0, h: 150.0, r: 0.8, g: 0.5, b: 0.2, a: 1.0, radius: 0.0 }).unwrap();
    other.scene_frame_end().unwrap();
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
    // Under TCG the first input after boot can arrive before the window
    // has focus. Tap Shift until the app hears it, then drop what came.
    let mut ready = false;
    for _ in 0..100 {
        p.inject(&format!("KEY {HID_LSHIFT} 1 0"));
        p.inject(&format!("KEY {HID_LSHIFT} 0 0"));
        if let Ok(Some(Event::Key { .. })) = p.app.wait_event(Some(Duration::from_millis(200))) {
            ready = true;
            break;
        }
    }
    p.check(ready, "the app hears input");
    for c in [&mut p.app, &mut p.wm] {
        while let Ok(Some(_)) = c.wait_event(Some(Duration::from_millis(100))) {}
    }

    let (t, w) = p.press(&format!("KEY {HID_V} 1 {CTRL}"));
    p.inject(&format!("KEY {HID_V} 0 {CTRL}"));
    p.check(t != [0; 16], "the app's Ctrl-V carries a token");
    p.check(w == [0; 16], "the WM's copy of it carries none");
    let r = p.consume(t, uid + 1, purpose::PASTE);
    p.check(refused(r) == refusal::WRONG_UID, "another app cannot spend it");
    let r = p.consume(t, uid, purpose::PASTE);
    // (window_id is frescod's id for it, which in split mode is not the app's.)
    p.check(matches!(r, ConsumeResult::Ok { kind: kind::KEY, .. }), "its own app pastes with it");
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

    // frescod's window ids differ from the apps' in split mode: the target
    // is the one placed at (400, 100).
    let at = p.wm.wm_enumerate().expect("enumerate").into_iter().find(|s| (s.rect.x, s.rect.y) != (400, 100)).expect("our window").rect;
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

    // Drag and drop (tabula.md §4): from the app's window to the other's.
    let rects = p.wm.wm_enumerate().expect("enumerate");
    let rect = |is_target: bool| rects.iter().find(|s| ((s.rect.x, s.rect.y) == (400, 100)) == is_target).expect("window").rect;
    let (a, b) = (rect(false), rect(true));
    println!("source at {},{} {}x{}; target at {},{} {}x{}", a.x, a.y, a.w, a.h, b.x, b.y, b.w, b.h);
    p.inject(&format!("MOVE {} {}", a.x + a.w / 2, a.y + a.h / 2));
    let (t, _) = p.press("BTN 1 1");
    p.app.drag_begin(win, 42, [9; 16], vec!["text/plain".into()]).unwrap();
    let end = wait(&mut p.app, |e| matches!(e, Event::DragEnd { .. }));
    p.check(matches!(end, Some(Event::DragEnd { dropped: false, drag_id: 42, .. })), "a drag needs the press being held");
    p.app.drag_begin(win, 43, t, vec!["text/plain".into(), "text/html".into()]).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    p.inject(&format!("MOVE {} {}", b.x + 20, b.y + 30));
    let enter = wait(&mut other, |e| matches!(e, Event::DragEnter { .. }));
    p.check(matches!(&enter, Some(Event::DragEnter { drag_id: 43, formats, .. }) if formats.len() == 2), "the target sees it come in, with its formats");
    p.inject(&format!("MOVE {} {}", b.x + 25, b.y + 30));
    let over = wait(&mut other, |e| matches!(e, Event::DragOver { .. }));
    p.check(matches!(over, Some(Event::DragOver { drag_id: 43, .. })), "and move over it");
    p.inject("BTN 1 0");
    let dropped = wait(&mut other, |e| matches!(e, Event::Drop { .. }));
    let end = wait(&mut p.app, |e| matches!(e, Event::DragEnd { .. }));
    p.check(matches!(end, Some(Event::DragEnd { dropped: true, drag_id: 43, .. })), "the source hears it was dropped");
    match dropped {
        Some(Event::Drop { window_id, activation, .. }) if window_id == target => {
            let r = p.consume(activation, uid, purpose::PASTE);
            p.check(refused(r) == refusal::WRONG_KIND, "a drop token is not a paste");
            let r = p.consume(activation, uid, purpose::DROP);
            p.check(matches!(r, ConsumeResult::Ok { kind: kind::DROP, drag_id: 43, .. }), "it is the drop of that drag");
        }
        other => p.check(false, &format!("the target gets the drop ({other:?})")),
    }

    // The notification layer (tabula.md §5): a toast asking for the whole
    // screen gets the band, and keys still go to the app.
    let mut shelf = Connection::connect(sock).expect("app socket");
    let hints = WindowHints { role: Some(WmRole::Notification), initial_position: Some((0, 0)), ..WindowHints::default() };
    let toast = shelf.window_create(2000, 2000, "toast", hints).expect("window");
    shelf.set_default_window(toast as u16);
    shelf.scene_frame_begin().unwrap();
    shelf.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 400.0, h: 300.0, r: 0.1, g: 0.1, b: 0.1, a: 1.0, radius: 0.0 }).unwrap();
    shelf.scene_frame_end().unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let rects = p.wm.wm_enumerate().expect("enumerate");
    let band = rects.iter().find(|s| s.role == WmRole::Notification).map(|s| s.rect);
    println!("toast at {band:?}");
    let d = p.app.display_info().expect("display info");
    println!("display {}x{}, reserved top {}", d.width, d.height, d.reserved_top);
    // The band: the top right, NOTIFICATION_BAND_W wide, below the strip,
    // at most half the screen high.
    let (w, h) = (d.width as i32, d.height as i32);
    let bw = fresco_protocol::NOTIFICATION_BAND_W as i32;
    p.check(
        band.is_some_and(|r| r.x >= w - bw && r.x + r.w <= w && r.y >= d.reserved_top as i32 && r.h <= h / 2),
        "a toast is kept inside its band",
    );
    let (t, _) = p.press(&format!("KEY {HID_A} 1 0"));
    p.inject(&format!("KEY {HID_A} 0 0"));
    p.check(t != [0; 16], "keys still go to the app under it");

    // Credential slots (arca-autofill.md §4): a FILL press counts only on
    // a slot frescod saw whole, uncovered, big enough and still for 500 ms.
    let window = |sock: &str, at: (i32, i32), size: (u32, u32), rgba: [f32; 4]| {
        let mut c = Connection::connect(sock).expect("app socket");
        let hints = WindowHints { initial_position: Some(at), ..WindowHints::default() };
        let id = c.window_create(size.0, size.1, "w", hints).expect("window");
        c.set_default_window(id as u16);
        c.scene_frame_begin().unwrap();
        c.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 2000.0, h: 2000.0, r: rgba[0], g: rgba[1], b: rgba[2], a: rgba[3], radius: 0.0 }).unwrap();
        c.scene_frame_end().unwrap();
        (c, id)
    };
    let (_host, _) = window(sock, (100, 300), (400, 200), [0.9, 0.9, 0.9, 1.0]);
    let (mut arca, _) = window(sock, (700, 600), (10, 10), [1.0, 1.0, 1.0, 1.0]);
    std::thread::sleep(Duration::from_millis(500));
    let rects = p.wm.wm_enumerate().expect("enumerate");
    let id_at = |x: i32, y: i32| rects.iter().find(|s| (s.rect.x, s.rect.y) == (x, y)).map(|s| s.surface_id);
    let (Some(host), Some(slot)) = (id_at(100, 300), id_at(700, 600)) else {
        p.check(false, "found the host and slot windows");
        std::process::exit(1);
    };
    let bind = |p: &mut Probe, r: [f32; 4]| {
        p.wm.wm_credential_slot(slot, host, Some(r)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
    };
    // A click on the slot at screen (x, y), spent as FILL.
    let fill = |p: &mut Probe, arca: &mut Connection, x: i32, y: i32| -> u8 {
        // Click until the slot sees a press: under TCG the first click can
        // land before the frame that composes the slot there.
        // Drop presses left over from an earlier round of clicks.
        while let Ok(Some(_)) = arca.wait_event(Some(Duration::from_millis(50))) {}
        p.inject(&format!("MOVE {x} {y}"));
        for _ in 0..200 {
            p.inject("BTN 1 1");
            p.inject("BTN 1 0");
            let until = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < until {
                if let Ok(Some(Event::PointerButton { pressed: true, activation, .. })) =
                    arca.wait_event(Some(Duration::from_millis(20)))
                {
                    return refused(p.consume(activation, p.uid, purpose::FILL));
                }
            }
        }
        255
    };
    let still = || std::thread::sleep(Duration::from_millis(800));
    bind(&mut p, [20.0, 20.0, 200.0, 40.0]); // screen (120, 320) to (320, 360)
    let r = fill(&mut p, &mut arca, 220, 340);
    p.check(r == refusal::OBSCURED, &format!("a slot that just appeared does not fill ({r})"));
    still();
    let r = fill(&mut p, &mut arca, 220, 340);
    p.check(r == 0, &format!("a slot seen whole and still for 500 ms fills ({r})"));

    let (mut cover, cid) = window(sock, (120, 320), (100, 40), [0.2, 0.2, 0.2, 1.0]);
    still();
    let r = fill(&mut p, &mut arca, 300, 340);
    p.check(r == refusal::OBSCURED, &format!("a window over part of it: refused ({r})"));
    cover.window_destroy(cid).unwrap();
    let (mut fade, fid) = window(sock, (120, 320), (150, 40), [1.0, 1.0, 1.0, 0.3]);
    still();
    let r = fill(&mut p, &mut arca, 300, 340);
    p.check(r == refusal::OBSCURED, &format!("a translucent veil over it: refused ({r})"));
    fade.window_destroy(fid).unwrap();
    still();
    let r = fill(&mut p, &mut arca, 220, 340);
    p.check(r == 0, &format!("uncovered and still again: fills ({r})"));

    bind(&mut p, [40.0, 20.0, 200.0, 40.0]); // moved right by 20: (140, 320) to (340, 360)
    let r = fill(&mut p, &mut arca, 330, 340);
    p.check(r == refusal::OBSCURED, &format!("just moved under the pointer: refused ({r})"));
    still();
    bind(&mut p, [40.0, 20.0, 100.0, 20.0]); // shrunk below 120x24
    still();
    let r = fill(&mut p, &mut arca, 180, 330);
    p.check(r == refusal::OBSCURED, &format!("too small: refused ({r})"));
    bind(&mut p, [300.0, 20.0, 200.0, 40.0]); // past the host's right edge
    still();
    let r = fill(&mut p, &mut arca, 450, 330);
    p.check(r == refusal::OBSCURED, &format!("sticking out of its window: refused ({r})"));
    // Leave it fit to fill, so the last capture shows frescod's frame.
    bind(&mut p, [20.0, 20.0, 200.0, 40.0]);
    still();
    if let Ok(png) = std::env::var("PROBE_CAPTURE") {
        // frescod captures at most every 200 ms; take it while we're up.
        std::thread::sleep(Duration::from_millis(1500));
        let _ = std::fs::copy(&png, format!("{png}.slot.png"));
    }

    // Limen slots (limen.md §5): an embed child stays off screen until
    // limend binds it, by the windows' embed tokens, and hides again when
    // released or when its parent goes.
    use fresco_protocol::slot::{kind as sk, op as sop, refusal as sno, SlotBind, SlotBindResult};
    let token_of = |c: &mut Connection| match wait(c, |e| matches!(e, Event::EmbedToken { .. })) {
        Some(Event::EmbedToken { token, .. }) => token,
        _ => [0; 16],
    };
    let slot_bind = |p: &mut Probe, parent: [u8; 16], child: [u8; 16], kind: u8, rect: Option<[f32; 4]>| -> SlotBindResult {
        let b = fresco_protocol::encode(&SlotBind { parent, child, kind, rect }).unwrap();
        fresco_protocol::decode(&p.ask(sop::BIND, b)).unwrap()
    };
    // Whether a click at (x, y) reaches `c`'s window (tried for 2 s).
    let hits = |p: &mut Probe, c: &mut Connection, x: i32, y: i32| -> bool {
        while let Ok(Some(_)) = c.wait_event(Some(Duration::from_millis(50))) {}
        p.inject(&format!("MOVE {x} {y}"));
        for _ in 0..20 {
            p.inject("BTN 1 1");
            p.inject("BTN 1 0");
            let until = Instant::now() + Duration::from_millis(100);
            while Instant::now() < until {
                if let Ok(Some(Event::PointerButton { pressed: true, .. })) = c.wait_event(Some(Duration::from_millis(20))) {
                    return true;
                }
            }
        }
        false
    };
    let (mut lhost, lhost_id) = window(sock, (600, 100), (300, 200), [0.3, 0.6, 0.3, 1.0]);
    let ht = token_of(&mut lhost);
    let mut emb = Connection::connect(sock).expect("app socket");
    let hints = WindowHints { initial_position: Some((650, 150)), embed_pending: true, ..WindowHints::default() };
    let eid = emb.window_create(10, 10, "embed", hints).expect("window");
    emb.set_default_window(eid as u16);
    emb.scene_frame_begin().unwrap();
    emb.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 2000.0, h: 2000.0, r: 0.2, g: 0.2, b: 0.9, a: 1.0, radius: 0.0 }).unwrap();
    emb.scene_frame_end().unwrap();
    let et = token_of(&mut emb);
    p.check(ht != [0; 16] && et != [0; 16] && ht != et, "each window hears its own embed token");
    still();
    let h = !hits(&mut p, &mut emb, 655, 155);
    p.check(h, "an embed_pending window is not on screen");
    let ok = slot_bind(&mut p, ht, et, sk::PLAIN, Some([20.0, 20.0, 150.0, 60.0])); // screen (620, 120) to (770, 180)
    let me = p.uid;
    p.check(ok == SlotBindResult::Ok { parent_uid: me, child_uid: me }, &format!("limend binds it and learns both owners ({ok:?})"));
    still();
    let h = hits(&mut p, &mut emb, 700, 150);
    p.check(h, "bound: it shows in its slot and takes the click");
    let r = slot_bind(&mut p, [1; 16], et, sk::PLAIN, Some([0.0, 0.0, 50.0, 50.0]));
    p.check(r == SlotBindResult::Refused { reason: sno::UNKNOWN_WINDOW }, &format!("a made-up token names no window ({r:?})"));
    let r = slot_bind(&mut p, et, ht, sk::PLAIN, Some([0.0, 0.0, 50.0, 50.0]));
    p.check(r == SlotBindResult::Refused { reason: sno::INVALID }, &format!("a slot can't host one ({r:?})"));
    let r = slot_bind(&mut p, ht, et, sk::PLAIN, None);
    p.check(matches!(r, SlotBindResult::Ok { .. }), "released");
    still();
    let h = !hits(&mut p, &mut emb, 700, 150);
    p.check(h, "released: off screen again");
    let h = hits(&mut p, &mut lhost, 700, 150);
    p.check(h, "and the click goes to its parent");
    let _ = slot_bind(&mut p, ht, et, sk::PLAIN, Some([20.0, 20.0, 150.0, 60.0]));
    still();
    let h = hits(&mut p, &mut emb, 700, 150);
    p.check(h, "bound again");
    lhost.window_destroy(lhost_id).unwrap();
    still();
    let h = !hits(&mut p, &mut emb, 700, 150);
    p.check(h, "its parent went: off screen, not top-level");
    let r = slot_bind(&mut p, ht, et, sk::PLAIN, Some([0.0, 0.0, 50.0, 50.0]));
    p.check(r == SlotBindResult::Refused { reason: sno::UNKNOWN_WINDOW }, "a gone window's token is dead");

    if p.failed > 0 {
        println!("{} FAILED", p.failed);
        std::process::exit(1);
    }
    println!("all passed");
}
