//! atrium-popup-probe — the pop-up window check (docs/spec/fresco-popups.md
//! §5 step 2). Opens a window the size of the screen and drives frescod's
//! input injector (FRESCOD_INPUT_SOCK) itself, so it needs no input device:
//!
//! 1. a press in the window lets it open a pop-up (it need not be focused);
//! 2. the pop-up opens below its anchor, and gets the keys (it grabs);
//! 3. a press inside it is delivered to it and does not dismiss it;
//! 4. re-placed at the screen's bottom edge, it flips above the anchor;
//! 5. a submenu opens to the right of it;
//! 6. a press outside the chain dismisses both, submenu first, and neither
//!    that press nor its release reaches the window; the next press does;
//! 7. an anchor outside the parent is refused;
//! 8. a tooltip (no grab) takes no keys and outlives an outside press;
//! 9. the bar (a second client, role Chrome, never focused) opens a menu
//!    when pressed; it is above the app's window, takes the keys, and the
//!    app cannot open a pop-up while it is open.
//!
//! Prints `ok …` / `FAIL …` lines and ends `popup: N failed`.
//!
//! Usage: atrium-popup-probe FRESCO_SOCK INPUT_SOCK

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{popup_align, popup_dismiss, popup_side, RectParams, WindowHints};

struct Probe {
    conn: Connection,
    inject: UnixStream,
    fails: u32,
}

impl Probe {
    fn check(&mut self, ok: bool, what: &str) {
        if ok {
            println!("ok {what}");
        } else {
            println!("FAIL {what}");
            self.fails += 1;
        }
        let _ = std::io::stdout().flush();
    }

    fn send(&mut self, line: &str) -> std::io::Result<()> {
        self.inject.write_all(format!("{line}\n").as_bytes())
    }

    /// Every event that arrives within `ms`.
    fn events(&mut self, ms: u64) -> std::io::Result<Vec<Event>> {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut out = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match self.conn.wait_event(Some(left.max(Duration::from_millis(1))))? {
                Some(e) => {
                    println!("  event {e:?}");
                    out.push(e);
                }
                None => break,
            }
        }
        Ok(out)
    }

    /// Press and release button 1 at screen (`x`, `y`); the events that follow.
    fn click(&mut self, x: f32, y: f32) -> std::io::Result<Vec<Event>> {
        self.send(&format!("MOVE {x} {y}"))?;
        self.send("BTN 1 1")?;
        let mut ev = self.events(700)?;
        self.send("BTN 1 0")?;
        ev.extend(self.events(700)?);
        Ok(ev)
    }

    /// Wait until window `id` is on screen at (`x`, `y`) (frescod hit-tests
    /// what it last showed, which takes a while under emulation): move the
    /// pointer there until a motion reaches it.
    fn wait_shown(&mut self, id: u32, x: f32, y: f32) -> std::io::Result<bool> {
        let end = Instant::now() + Duration::from_secs(90);
        let mut nudge = 0.0;
        while Instant::now() < end {
            self.send(&format!("MOVE {} {y}", x + nudge))?;
            nudge = 1.0 - nudge;
            let ev = self.events(500)?;
            if ev.iter().any(|e| matches!(e, Event::PointerMotion { window_id, .. } if *window_id == id)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn key(&mut self, usage: u16) -> std::io::Result<Vec<Event>> {
        self.send(&format!("KEY {usage:#x} 1"))?;
        self.send(&format!("KEY {usage:#x} 0"))?;
        self.events(700)
    }

    fn fill(&mut self, win: u32, w: u32, h: u32, rgb: (f32, f32, f32)) -> std::io::Result<()> {
        self.conn.set_default_window(win as u16);
        self.conn.scene_frame_begin()?;
        self.conn.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: w as f32, h: h as f32, r: rgb.0, g: rgb.1, b: rgb.2, a: 1.0, radius: 0.0 })?;
        self.conn.scene_frame_end()
    }
}

fn placed(ev: &[Event], id: u32) -> Option<(f32, f32, u32, u32, u8)> {
    ev.iter().rev().find_map(|e| match *e {
        Event::PopupPlaced { window_id, x, y, width, height, side } if window_id == id => Some((x, y, width, height, side)),
        _ => None,
    })
}

fn dismissed(ev: &[Event]) -> Vec<(u32, u8)> {
    ev.iter().filter_map(|e| match *e {
        Event::PopupDismissed { window_id, reason } => Some((window_id, reason)),
        _ => None,
    }).collect()
}

fn buttons(ev: &[Event], id: u32) -> Vec<bool> {
    ev.iter().filter_map(|e| match *e {
        Event::PointerButton { window_id, pressed, .. } if window_id == id => Some(pressed),
        _ => None,
    }).collect()
}

fn keys(ev: &[Event], id: u32) -> usize {
    ev.iter().filter(|e| matches!(e, Event::Key { window_id, .. } if *window_id == id)).count()
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(sock), Some(inj)) = (args.next(), args.next()) else {
        eprintln!("usage: atrium-popup-probe FRESCO_SOCK INPUT_SOCK");
        std::process::exit(2);
    };
    let mut conn = Connection::connect(&sock)?;
    let d = conn.display_info()?;
    let (sw, sh) = (d.width, d.height);
    let win = conn.window_create(sw, sh, "popup-probe", WindowHints::default())?;
    let inject = UnixStream::connect(&inj)?;
    let mut p = Probe { conn, inject, fails: 0 };
    p.fill(win, sw, sh, (0.1, 0.2, 0.3))?;
    println!("popup-probe: window {win}, screen {sw}x{sh}");
    let shown = p.wait_shown(win, 40.0, 40.0)?;
    p.check(shown, "the window is on screen");

    // 1. A press in the window, which then opens a pop-up.
    let ev = p.click(40.0, 40.0)?;
    p.check(buttons(&ev, win) == [true, false], "a press in the window reaches it");
    let anchor = [100.0, 100.0, 80.0, 24.0];
    let menu = p.conn.popup_create(win, anchor, popup_side::BELOW, popup_align::START, 200, 150, true);
    p.check(menu.is_ok(), "the pressed window opens a pop-up");
    let menu = menu?;
    p.fill(menu, 200, 150, (0.9, 0.9, 0.9))?;
    let ev = p.events(1000)?;
    p.check(placed(&ev, menu) == Some((100.0, 124.0, 200, 150, popup_side::BELOW)), "it is placed below its anchor");

    let shown = p.wait_shown(menu, 150.0, 180.0)?;
    p.check(shown, "the pop-up is on screen");

    // 2. Keys go to the pop-up.
    let ev = p.key(0x51)?; // Down
    p.check(keys(&ev, menu) == 2 && keys(&ev, win) == 0, "the pop-up gets the keys");

    // 3. A press inside it is its own.
    let ev = p.click(150.0, 180.0)?;
    p.check(buttons(&ev, menu) == [true, false] && dismissed(&ev).is_empty(), "a press inside the pop-up reaches it and keeps it open");

    // 4. At the bottom edge it flips.
    let low = [100.0, sh as f32 - 30.0, 80.0, 24.0];
    let r = p.conn.popup_place(menu, low, popup_side::BELOW, popup_align::START, 200, 150);
    p.check(r.is_ok(), "the pop-up is placed again");
    let ev = p.events(1000)?;
    let want = Some((100.0, sh as f32 - 30.0 - 150.0, 200, 150, popup_side::ABOVE));
    p.check(placed(&ev, menu) == want, "at the screen's bottom edge it flips above its anchor");

    // 5. A submenu to the right.
    let sub = p.conn.popup_create(menu, [0.0, 0.0, 200.0, 24.0], popup_side::RIGHT, popup_align::START, 160, 90, true);
    p.check(sub.is_ok(), "a submenu opens from the pop-up");
    let sub = sub?;
    p.fill(sub, 160, 90, (0.8, 0.8, 0.95))?;
    let ev = p.events(1000)?;
    p.check(placed(&ev, sub).is_some_and(|(x, _, _, _, s)| x == 200.0 && s == popup_side::RIGHT), "the submenu opens to the right");

    // 6. A press outside the chain dismisses it, submenu first, and is not delivered.
    let (ox, oy) = (sw as f32 * 0.75, sh as f32 * 0.25);
    let ev = p.click(ox, oy)?;
    let d = dismissed(&ev);
    p.check(d == [(sub, popup_dismiss::PRESS_OUTSIDE), (menu, popup_dismiss::PRESS_OUTSIDE)], "a press outside dismisses the chain, topmost first");
    p.check(buttons(&ev, win).is_empty(), "the dismissing press and its release are not delivered");
    let ev = p.click(ox, oy)?;
    p.check(buttons(&ev, win) == [true, false], "the next press is delivered");

    // 7. An anchor outside the parent is refused.
    let r = p.conn.popup_create(win, [sw as f32 + 10.0, 10.0, 10.0, 10.0], popup_side::BELOW, popup_align::START, 50, 50, true);
    p.check(r.is_err(), "an anchor outside the parent is refused");

    // 8. A tooltip takes no keys and outlives an outside press.
    let tip = p.conn.popup_create(win, [300.0, 300.0, 40.0, 20.0], popup_side::ABOVE, popup_align::CENTER, 120, 30, false);
    p.check(tip.is_ok(), "a tooltip opens");
    let tip = tip?;
    p.fill(tip, 120, 30, (1.0, 1.0, 0.8))?;
    let ev = p.events(1000)?;
    p.check(placed(&ev, tip) == Some((260.0, 270.0, 120, 30, popup_side::ABOVE)), "the tooltip is centred above its anchor");
    let ev = p.key(0x04)?;
    p.check(keys(&ev, tip) == 0, "the tooltip takes no keys");
    let ev = p.click(600.0, 600.0)?;
    p.check(dismissed(&ev).is_empty() && buttons(&ev, win) == [true, false], "a press elsewhere leaves the tooltip and reaches the window");

    // 9. The bar's menu.
    let mut bar = Connection::connect(&sock)?;
    let hints = WindowHints { role: Some(fresco_protocol::WmRole::Chrome), initial_position: Some((0, 100)), ..Default::default() };
    let bw = bar.window_create(sw, 40, "bar", hints)?;
    bar.set_default_window(bw as u16);
    bar.scene_frame_begin()?;
    bar.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: sw as f32, h: 40.0, r: 0.2, g: 0.2, b: 0.2, a: 1.0, radius: 0.0 })?;
    bar.scene_frame_end()?;
    let mut drain = |c: &mut Connection, ms: u64| -> std::io::Result<Vec<Event>> {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut out = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match c.wait_event(Some(left.max(Duration::from_millis(1))))? {
                Some(e) => { println!("  bar event {e:?}"); out.push(e) }
                None => break,
            }
        }
        Ok(out)
    };
    // On screen: a motion over it reaches the bar.
    let end = Instant::now() + Duration::from_secs(90);
    let mut on = false;
    let mut nudge = 0.0;
    while !on && Instant::now() < end {
        p.send(&format!("MOVE {} 120", 700.0 + nudge))?;
        nudge = 1.0 - nudge;
        on = drain(&mut bar, 500)?.iter().any(|e| matches!(e, Event::PointerMotion { window_id, .. } if *window_id == bw));
    }
    p.check(on, "the bar is on screen");
    // Its role, as the WM sees it: Chrome only where frescod grants it
    // (FRESCOD_CHROME_UIDS); PROBE_CHROME=0 expects it refused.
    if let Some(wm_sock) = std::env::var_os("PROBE_WM_SOCK") {
        let want_chrome = std::env::var("PROBE_CHROME").map_or(true, |v| v != "0");
        let mut wm = Connection::connect(&wm_sock)?;
        // By its place (ids are the renderer's own in split mode).
        let role = wm.wm_enumerate()?.into_iter().find(|s| s.rect.y == 100 && s.rect.h == 40).map(|s| s.role);
        let want = if want_chrome { fresco_protocol::WmRole::Chrome } else { fresco_protocol::WmRole::Document };
        p.check(role == Some(want), &format!("the bar's role is {want:?}"));
    }
    p.send("BTN 1 1")?;
    p.send("BTN 1 0")?;
    drain(&mut bar, 1000)?;
    let bm = bar.popup_create(bw, [600.0, 0.0, 80.0, 40.0], popup_side::BELOW, popup_align::START, 240, 200, true);
    p.check(bm.is_ok(), "the pressed bar opens a menu");
    let bm = bm?;
    bar.set_default_window(bm as u16);
    bar.scene_frame_begin()?;
    bar.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 240.0, h: 200.0, r: 0.95, g: 0.95, b: 0.95, a: 1.0, radius: 0.0 })?;
    bar.scene_frame_end()?;
    let ev = drain(&mut bar, 1000)?;
    p.check(placed(&ev, bm) == Some((600.0, 40.0, 240, 200, popup_side::BELOW)), "the bar's menu hangs below its button");
    // Refused: an error, or in split mode (where frescod decides after
    // the renderer answered) an immediate dismissal.
    let r = p.conn.popup_create(win, [10.0, 400.0, 50.0, 20.0], popup_side::BELOW, popup_align::START, 100, 100, true);
    let ev = p.events(1000)?;
    let refused = match r {
        Err(_) => true,
        Ok(id) => placed(&ev, id).is_none() && dismissed(&ev).iter().any(|&(w, _)| w == id),
    };
    p.check(refused, "the app cannot open a pop-up while the bar's menu is open");
    p.send("KEY 0x51 1")?;
    p.send("KEY 0x51 0")?;
    let ev = drain(&mut bar, 1000)?;
    let app_ev = p.events(300)?;
    p.check(keys(&ev, bm) == 2 && keys(&app_ev, win) == 0, "the bar's menu takes the keys, the app sees none");

    println!("popup: {} failed", p.fails);
    std::process::exit(if p.fails == 0 { 0 } else { 1 });
}
