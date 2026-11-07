//! Input from tactusd (docs/spec/atrium-input.md §2): Tactus reads the
//! devices and sends canonical events; frescod integrates the cursor,
//! hit-tests, routes by focus and mints activation tokens, as its own
//! hidraw readers did.
//!
//! This is frescod's only device input: it parses no device reports and
//! its jail has no input nodes. The socket is `FRESCOD_TACTUS_SOCKET`, or
//! /atrium/sockets/tactus/tactus.sock; while tactusd is down frescod
//! retries every second and has no input. (Tests also have the injector
//! socket, which mints no activation tokens.)
//!
//! Mapping, as frescod's old hidraw readers did it:
//! - keyboard page keys go to `keyboard_window()` as `InputKey` with the
//!   usage's low 16 bits; modifier keys (0xE0..0xE7) update the modifier
//!   bitmap and are not sent themselves;
//! - consumer-page keys go to the WM only (window 0, `0x0C00 | usage`, no
//!   token: atrium-player-pip-keys.md §4.3);
//! - key repeats go to the window the press went to, flagged `repeat`
//!   and without a token, and stop if that window loses the keyboard;
//! - pointer buttons, motion (relative or absolute) and wheels move the
//!   cursor and go to the window under it; a touchpad's two-finger scroll
//!   goes, with its phases, to the window it began over, content following
//!   the fingers; swipes and pinches are not routed yet.
//! Every press from tactusd mints an activation token: tactusd is the
//! only peer on that socket (its directory is _tactus:_frescod 0750).

use std::os::unix::net::UnixStream;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fresco_scene_server::window::{Compositor, DisplayEvent, MINT_HERE};
use fresco_protocol::scroll_phase;
use tactus::wire::{self, Axis, Message, Phase};

pub const DEFAULT_SOCKET: &str = "/atrium/sockets/tactus/tactus.sock";

/// tactusd's socket.
pub fn socket() -> String {
    std::env::var("FRESCOD_TACTUS_SOCKET").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| DEFAULT_SOCKET.to_string())
}

/// `fallback`: the screen size while no mode is set (headless).
pub fn spawn(sink: Sender<DisplayEvent>, comp: Arc<Mutex<Compositor>>, path: String, fallback: (u32, u32)) {
    let _ = FALLBACK.set(fallback);
    std::thread::Builder::new()
        .name("frescod-tactus".into())
        .spawn(move || {
            let mut said = false;
            loop {
                match UnixStream::connect(&path) {
                    Ok(s) => {
                        eprintln!("frescod: input from tactusd ({path})");
                        said = false;
                        let mut seat = Seat::default();
                        let e = seat.run(s, &sink, &comp);
                        eprintln!("frescod: tactusd connection ended: {e}");
                        // Whatever was held is released: tactusd is gone.
                        seat.release(&sink, &comp);
                    }
                    Err(e) if !said => {
                        eprintln!("frescod: tactusd not reachable at {path} ({e}); retrying");
                        said = true;
                    }
                    Err(_) => {}
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .expect("spawn tactus client");
}

#[derive(Default)]
struct Seat {
    /// HID modifier byte (LCtrl=1 .. RGui=128), across keyboards.
    mods: u8,
    /// Keys sent as pressed, with the window they went to.
    keys: Vec<(u32, u16)>,
    buttons: Vec<u8>,
    /// The window a touchpad scroll is going to, between Begin and End.
    scrolling: Option<u32>,
}

/// The HID modifier byte to the wire's bits (shift 1, ctrl 2, alt 4).
fn wire_mods(hid: u8) -> u8 {
    let mut out = 0u8;
    if hid & 0b0010_0010 != 0 {
        out |= 0x01;
    }
    if hid & 0b0001_0001 != 0 {
        out |= 0x02;
    }
    if hid & 0b0100_0100 != 0 {
        out |= 0x04;
    }
    out
}

impl Seat {
    fn run(&mut self, mut s: UnixStream, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) -> std::io::Error {
        match wire::read(&mut s) {
            Ok(Message::Hello { version }) if version == wire::VERSION => {}
            Ok(m) => return std::io::Error::other(format!("unexpected hello {m:?}")),
            Err(e) => return e,
        }
        {
            let (w, h) = screen();
            comp.lock().unwrap().cursor = (w / 2.0, h / 2.0);
        }
        loop {
            let m = match wire::read(&mut s) {
                Ok(m) => m,
                Err(e) => return e,
            };
            self.handle(m, sink, comp);
        }
    }

    fn handle(&mut self, m: Message, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) {
        match m {
            Message::Device { id, bus, classes, name, .. } => {
                eprintln!("frescod: input device {id} {name:?} ({bus:?}, {classes:?})");
            }
            Message::Key { usage, down, repeat, .. } => {
                let (page, id) = ((usage >> 16) as u16, (usage & 0xffff) as u16);
                if repeat {
                    if page == 0x07 {
                        self.repeat(sink, comp, id);
                    }
                    return;
                }
                match page {
                    0x07 if (0xe0..=0xe7).contains(&id) => {
                        let bit = 1u8 << (id - 0xe0);
                        if down {
                            self.mods |= bit;
                        } else {
                            self.mods &= !bit;
                        }
                    }
                    0x07 => self.key(sink, comp, id, down),
                    0x0c if id < 0x400 => {
                        let _ = sink.send(DisplayEvent::InputKey {
                            window_id: 0,
                            hid_usage: 0x0c00 | id,
                            pressed: down,
                            modifiers: 0,
                            activation: [0; 16],
                            repeat: false,
                        });
                    }
                    _ => {}
                }
            }
            Message::Button { usage, down, .. } => {
                if usage >> 16 == 0x09 && (1..=32).contains(&(usage & 0xffff)) {
                    let b = (usage & 0xffff) as u8;
                    self.buttons.retain(|&x| x != b);
                    if down {
                        self.buttons.push(b);
                    }
                    self.button(sink, comp, b, down);
                }
            }
            Message::Motion { dx, dy, .. } => {
                let (w, h) = screen();
                let mut g = comp.lock().unwrap();
                let (x, y) = g.cursor;
                self.moved(sink, &mut g, (x + dx).clamp(0.0, w - 1.0), (y + dy).clamp(0.0, h - 1.0));
            }
            Message::Absolute { x, y, .. } => {
                let (w, h) = screen();
                let mut g = comp.lock().unwrap();
                self.moved(sink, &mut g, x * (w - 1.0), y * (h - 1.0));
            }
            Message::Scroll { axis, value_120, .. } => {
                let (target, _) = {
                    let g = comp.lock().unwrap();
                    let (x, y) = g.cursor;
                    g.pointer_target(x, y)
                };
                // HID: positive is away from the user; the wire's positive
                // dy scrolls content down. 8 px per detent, as before.
                let v = value_120 as f32 / 120.0 * 8.0;
                let (dx, dy) = match axis {
                    Axis::Vertical => (0.0, -v),
                    Axis::Horizontal => (v, 0.0),
                };
                let _ = sink.send(DisplayEvent::InputPointerScroll { window_id: target as u32, dx, dy, phase: scroll_phase::NONE });
            }
            Message::ScrollPixels { dx, dy, phase, .. } => {
                // Content follows the fingers ("natural" scrolling): fingers
                // up scroll the content up, i.e. the view down (+dy). The
                // whole scroll goes to the window it began over.
                let window_id = match (phase, self.scrolling) {
                    (Phase::Begin, _) | (_, None) => {
                        let g = comp.lock().unwrap();
                        let (x, y) = g.cursor;
                        g.pointer_target(x, y).0 as u32
                    }
                    (_, Some(w)) => w,
                };
                self.scrolling = (phase != Phase::End).then_some(window_id);
                let phase = match phase {
                    Phase::Begin => scroll_phase::BEGIN,
                    Phase::Update => scroll_phase::UPDATE,
                    Phase::End => scroll_phase::END,
                };
                let _ = sink.send(DisplayEvent::InputPointerScroll { window_id, dx: -dx, dy: -dy, phase });
            }
            // Swipes and pinches have no route to Forum or apps yet
            // (atrium-input.md §8 T3 follow-up).
            Message::Gesture { .. } => {}
            Message::Gone { id } => eprintln!("frescod: input device {id} gone"),
            Message::Hello { .. } => {}
        }
    }

    fn key(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>, id: u16, down: bool) {
        // A release goes where the press went, even if focus moved.
        let window_id = if down {
            let w = comp.lock().unwrap().keyboard_window();
            self.keys.push((w, id));
            w
        } else {
            match self.keys.iter().position(|&(_, k)| k == id) {
                Some(i) => self.keys.remove(i).0,
                None => return,
            }
        };
        let _ = sink.send(DisplayEvent::InputKey {
            window_id,
            hid_usage: id,
            pressed: down,
            modifiers: wire_mods(self.mods),
            activation: if down { MINT_HERE } else { [0; 16] },
            repeat: false,
        });
    }

    /// A repeat of held key `id`, to the window its press went to while
    /// that window still has the keyboard.
    fn repeat(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>, id: u16) {
        let Some(&(window_id, _)) = self.keys.iter().find(|&&(_, k)| k == id) else { return };
        if comp.lock().unwrap().keyboard_window() != window_id {
            return;
        }
        let _ = sink.send(DisplayEvent::InputKey {
            window_id,
            hid_usage: id,
            pressed: true,
            modifiers: wire_mods(self.mods),
            activation: [0; 16],
            repeat: true,
        });
    }

    fn button(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>, button: u8, down: bool) {
        let (target, (x, y)) = {
            let g = comp.lock().unwrap();
            let (cx, cy) = g.cursor;
            g.pointer_target(cx, cy)
        };
        let _ = sink.send(DisplayEvent::InputPointerButton {
            window_id: target as u32,
            x,
            y,
            button,
            pressed: down,
            modifiers: wire_mods(self.mods),
            activation: if down { MINT_HERE } else { [0; 16] },
        });
    }

    fn moved(&mut self, sink: &Sender<DisplayEvent>, g: &mut Compositor, x: f32, y: f32) {
        if g.cursor == (x, y) {
            return;
        }
        g.cursor = (x, y);
        let (target, (lx, ly)) = g.pointer_target(x, y);
        let _ = sink.send(DisplayEvent::InputPointerMotion { window_id: target as u32, x: lx, y: ly });
    }

    /// tactusd went away: release what it held.
    fn release(&mut self, sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>) {
        for (_, k) in self.keys.clone() {
            self.key(sink, comp, k, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            self.button(sink, comp, b, false);
        }
        self.mods = 0;
    }
}

static FALLBACK: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();

fn screen() -> (f32, f32) {
    let (mut w, mut h, _) = crate::screen::get();
    if w == 0 || h == 0 {
        (w, h) = FALLBACK.get().copied().unwrap_or((1, 1));
    }
    (w.max(1) as f32, h.max(1) as f32)
}
