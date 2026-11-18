//! A test app for the link opener (opening-links.md §5): one window; a
//! click or Enter in it asks Monstrator to open a link (the first argument,
//! else `link-probe.url` in its bundle, else the confined gate's fixture page) with that press. Prints
//! `link-probe: up`, then `link-probe: opened` or `link-probe: not opened:
//! <why>` for each ask. Exits after five idle minutes.
use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};

const URL: &str = "https://atlas.example/wiki/Impluvium";

fn draw(c: &mut Connection, w: f32, h: f32) {
    let Ok(mut fr) = c.frame() else { return };
    let _ = fr.rect(RectParams { x: 0.0, y: 0.0, w, h, r: 0.2, g: 0.4, b: 0.8, a: 1.0, radius: 0.0 });
    let _ = fr.finish();
}

fn main() {
    // The first argument, else `link-probe.url` in its bundle (the app's
    // launch passes none), else the fixture page.
    let beside = || {
        let exe = std::env::current_exe().ok().or_else(|| std::env::args().next().map(Into::into))?;
        let t = std::fs::read_to_string(exe.parent()?.parent()?.join("link-probe.url")).ok()?;
        Some(t.trim().to_string()).filter(|t| !t.is_empty())
    };
    let url = std::env::args().nth(1).or_else(beside).unwrap_or_else(|| URL.into());
    let mut c = match Connection::connect(&fresco_client::default_socket_path()) {
        Ok(c) => c,
        Err(e) => return println!("link-probe: frescod: {e}"),
    };
    let (w, h) = (320.0, 200.0);
    let id = match c.window_create(w as u32, h as u32, "Link probe", WindowHints::default()) {
        Ok(id) => id,
        Err(e) => return println!("link-probe: window: {e}"),
    };
    let k = c.window_scale(id);
    draw(&mut c, w * k, h * k);
    println!("link-probe: up");
    let mut idle = Instant::now();
    while idle.elapsed() < Duration::from_secs(300) {
        let ev = match c.poll_event() {
            Ok(Some(ev)) => ev,
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => return println!("link-probe: frescod: {e}"),
        };
        let press = match ev {
            Event::PointerButton { pressed: true, activation, .. } => Some(activation),
            Event::Key { hid_usage: 0x28, pressed: true, repeat: false, activation, .. } => Some(activation),
            Event::Resized { width, height, scale, .. } => {
                let k = if scale > 0.0 { scale } else { k };
                draw(&mut c, width * k, height * k);
                None
            }
            Event::CloseRequested { .. } => return,
            _ => None,
        };
        let Some(token) = press else { continue };
        idle = Instant::now();
        match monstrator::open_link(&monstrator::socket(), &url, token, Duration::from_secs(130)) {
            Ok(Ok(())) => println!("link-probe: opened"),
            Ok(Err(why)) => println!("link-probe: not opened: {why}"),
            Err(e) => println!("link-probe: not opened: {e}"),
        }
    }
}
