//! frescod/src/injector_reader.rs — synthetic input injection (headless/dev harness).
//!
//! Reads a tiny line protocol from a UNIX socket and feeds the **same**
//! `DisplayEvent` pipeline the real `/dev/hidraw*` readers use
//! (`input_reader` / `pointer_reader`), through the same compositor cursor +
//! hit-test + focus logic — so an injected event routes to a window identically
//! to a real one. This is the interactive harness for environments with no
//! HID/GPU device (the headless VM): drive and debug the input → event → WM →
//! render loop deterministically and scriptably.
//!
//! Enabled by `FRESCOD_INPUT_SOCK=<path>`. Line protocol (one command per line):
//!   MOVE <x> <y>                  absolute cursor → hit-test → InputPointerMotion
//!   BTN  <button> <0|1>           button at cursor → InputPointerButton (1=primary)
//!   KEY  <hid_usage> <0|1> [mods] to the focused window → InputKey (usage hex ok: 0x04)
//!   SCROLL <dy>                   at cursor → InputPointerScroll
//! Whitespace-separated; blank / `#` lines ignored. Multiple clients welcome.

use fresco_scene_server::window::{Compositor, DisplayEvent};

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

pub fn spawn(
    sink: Sender<DisplayEvent>,
    comp: Arc<Mutex<Compositor>>,
    screen_w: u32,
    screen_h: u32,
    sock_path: String,
) {
    std::thread::Builder::new()
        .name("frescod-injector".into())
        .spawn(move || serve(sink, comp, screen_w as f32, screen_h as f32, &sock_path))
        .ok();
}

fn serve(sink: Sender<DisplayEvent>, comp: Arc<Mutex<Compositor>>, sw: f32, sh: f32, path: &str) {
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => { eprintln!("frescod: injector bind {path}: {e}"); return; }
    };
    /* Seed the cursor at screen center, same as pointer_reader. */
    { comp.lock().unwrap().cursor = (sw / 2.0, sh / 2.0); }
    eprintln!("frescod: input injector listening on {path}");
    for stream in listener.incoming() {
        let stream = match stream { Ok(s) => s, Err(_) => continue };
        for line in BufReader::new(stream).lines() {
            let line = match line { Ok(l) => l, Err(_) => break };
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            handle(&sink, &comp, sw, sh, line);
        }
    }
}

fn handle(sink: &Sender<DisplayEvent>, comp: &Arc<Mutex<Compositor>>, sw: f32, sh: f32, line: &str) {
    let mut it = line.split_whitespace();
    match it.next().unwrap_or("") {
        "MOVE" => {
            if let (Some(x), Some(y)) = (next_f32(&mut it), next_f32(&mut it)) {
                let (lx, ly, target) = cursor_to(comp, x.clamp(0.0, sw - 1.0), y.clamp(0.0, sh - 1.0));
                eprintln!("frescod: inject MOVE {x} {y} -> window {target}");
                let _ = sink.send(DisplayEvent::InputPointerMotion { window_id: target, x: lx, y: ly });
            }
        }
        "BTN" => {
            let button = it.next().and_then(|s| s.parse::<u8>().ok());
            let pressed = next_bool(&mut it);
            if let (Some(button), Some(pressed)) = (button, pressed) {
                let (lx, ly, target) = cursor_here(comp);
                eprintln!("frescod: inject BTN {button} {} -> window {target}", pressed as u8);
                let _ = sink.send(DisplayEvent::InputPointerButton {
                    window_id: target, x: lx, y: ly, button, pressed, modifiers: 0,
                });
            }
        }
        "KEY" => {
            let usage = it.next().and_then(parse_u16);
            let pressed = next_bool(&mut it);
            let mods = it.next().and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
            if let (Some(hid_usage), Some(pressed)) = (usage, pressed) {
                let target = focused_window(comp);
                eprintln!("frescod: inject KEY {hid_usage} {} -> window {target}", pressed as u8);
                let _ = sink.send(DisplayEvent::InputKey { window_id: target, hid_usage, pressed, modifiers: mods });
            }
        }
        "SCROLL" => {
            if let Some(dy) = next_f32(&mut it) {
                let (_lx, _ly, target) = cursor_here(comp);
                let _ = sink.send(DisplayEvent::InputPointerScroll { window_id: target, dx: 0.0, dy });
            }
        }
        other => eprintln!("frescod: injector: unknown command {other:?}"),
    }
}

/// Set the cursor to `(x, y)`, hit-test, return `(local_x, local_y, target)` —
/// the same critical-section shape `pointer_reader` uses.
fn cursor_to(comp: &Arc<Mutex<Compositor>>, x: f32, y: f32) -> (f32, f32, u32) {
    let mut g = comp.lock().unwrap();
    g.cursor = (x, y);
    hit(&g, x, y)
}

fn cursor_here(comp: &Arc<Mutex<Compositor>>) -> (f32, f32, u32) {
    let g = comp.lock().unwrap();
    let (x, y) = g.cursor;
    hit(&g, x, y)
}

fn hit(g: &Compositor, x: f32, y: f32) -> (f32, f32, u32) {
    let target = g.hit_test(x, y).unwrap_or(0);
    let win_pos = if target == 0 {
        (0.0, 0.0)
    } else {
        g.windows.get(&target).map(|w| w.pos).unwrap_or((0.0, 0.0))
    };
    (x - win_pos.0, y - win_pos.1, target as u32)
}

/// The focused window (or the topmost non-screen window), mirroring `input_reader`.
fn focused_window(comp: &Arc<Mutex<Compositor>>) -> u32 {
    let g = comp.lock().unwrap();
    let focus_id = g.focus.unwrap_or(0);
    let effective = if focus_id == 0 {
        g.z_order.iter().rev().find(|&&id| id != 0).copied().unwrap_or(0)
    } else {
        focus_id
    };
    effective as u32
}

fn next_f32<'a>(it: &mut impl Iterator<Item = &'a str>) -> Option<f32> {
    it.next().and_then(|s| s.parse::<f32>().ok())
}
fn next_bool<'a>(it: &mut impl Iterator<Item = &'a str>) -> Option<bool> {
    it.next().map(|s| s == "1" || s == "down" || s == "press")
}
fn parse_u16(s: &str) -> Option<u16> {
    s.strip_prefix("0x")
        .map(|h| u16::from_str_radix(h, 16).ok())
        .unwrap_or_else(|| s.parse().ok())
}
