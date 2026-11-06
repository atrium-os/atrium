//! atrium-chrome-demo — exercises frescod's trusted chrome (F4d,
//! docs/spec/fresco-f4-split.md). Needs frescod in split mode with
//! `FRESCOD_CHROME_SOCK`.
//!
//! Plays the Navigator: opens two document connections over the chrome
//! channel, one window each, and sets their origins there. Document 2 is
//! hostile: it paints a fake origin badge, "https://bank.example", at its
//! own top-left. frescod's badge is drawn over it with document 2's real
//! origin, and only for the focused window, which alternates every 4 s.
//!
//! Usage: atrium-chrome-demo <chrome-sock> <wm-sock>

use fresco_client::Connection;
use fresco_protocol::{RectParams, WindowHints, WmDeclareLayoutPayload, WmRect, WmSlot};
use fresco_split::chan::{self, Raw};
use fresco_split::chrome::ChromeMsg;
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn rect(x: f32, y: f32, w: f32, h: f32, c: [f32; 4]) -> RectParams {
    RectParams { x, y, w, h, r: c[0], g: c[1], b: c[2], a: c[3], radius: 0.0 }
}

/// Open a document connection over the chrome channel.
fn open(chrome: &chan::Chan) -> std::io::Result<(u32, Connection)> {
    chrome.send_raw(&ChromeMsg::OpenClient.encode(), &[])?;
    match chrome.recv_raw()? {
        Raw::Rec(rec, fds) if fds.len() == 1 => match ChromeMsg::decode(&rec) {
            Ok(ChromeMsg::Opened { handle }) => {
                let s = UnixStream::from(fds.into_iter().next().unwrap());
                Ok((handle, Connection::wrap(s)?))
            }
            other => Err(std::io::Error::other(format!("unexpected answer {other:?}"))),
        },
        _ => Err(std::io::Error::other("no OPENED with one fd")),
    }
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let chrome_sock = args.next().unwrap_or_else(|| "/tmp/chrome.sock".into());
    let wm_sock = args.next().unwrap_or_else(|| "/tmp/wm.sock".into());
    let chrome = chan::connect(std::path::Path::new(&chrome_sock))?;
    let (w, h) = (600u32, 400u32);

    let (h1, mut d1) = open(&chrome)?;
    let w1 = d1.window_create(w, h, "bank", WindowHints::default())?;
    d1.set_default_window(w1 as u16);
    d1.scene_frame_begin()?;
    d1.scene_node_rect(1, rect(0.0, 0.0, w as f32, h as f32, [0.9, 0.9, 0.85, 1.0]))?;
    d1.scene_node_rect(2, rect(40.0, 80.0, 300.0, 60.0, [0.1, 0.4, 0.2, 1.0]))?;
    d1.scene_frame_end()?;

    let (h2, mut d2) = open(&chrome)?;
    let w2 = d2.window_create(w, h, "evil", WindowHints::default())?;
    d2.set_default_window(w2 as u16);
    d2.scene_frame_begin()?;
    d2.scene_node_rect(1, rect(0.0, 0.0, w as f32, h as f32, [0.85, 0.8, 0.9, 1.0]))?;
    // The fake badge: a dark strip where frescod puts its own.
    d2.scene_node_rect(2, rect(2.0, 2.0, 260.0, 22.0, [0.08, 0.08, 0.1, 1.0]))?;
    d2.scene_node_rect(3, rect(8.0, 8.0, 230.0, 10.0, [0.95, 0.95, 0.95, 1.0]))?;
    d2.scene_frame_end()?;

    let set = |handle, window, origin: &str| {
        chrome.send_raw(&ChromeMsg::SetOrigin { handle, window, origin: origin.into() }.encode(), &[])
    };
    set(h1, w1, "https://bank.example")?;
    set(h2, w2, "https://evil.example:8443")?;
    eprintln!("documents: handle {h1} window {w1}, handle {h2} window {w2}");

    let mut wm = Connection::connect(&wm_sock)?;
    // The broker's ids for the two windows, in creation order.
    let ids = loop {
        let mut s: Vec<u32> = wm.wm_enumerate()?.iter().map(|s| s.surface_id).filter(|&i| i != 0).collect();
        s.sort();
        if s.len() >= 2 {
            break [s[0], s[1]];
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let slot = |id, x, y, layer| WmSlot { surface_id: id, rect: WmRect { x, y, w: w as i32, h: h as i32 }, layer };
    for step in 0.. {
        let focus = ids[step % 2];
        wm.wm_declare_layout(&WmDeclareLayoutPayload {
            slots: vec![slot(ids[0], 100, 100, 1), slot(ids[1], 500, 250, 0)],
            focus,
            focus_cause: 0,
        })?;
        eprintln!("step {step}: focus {focus}");
        for c in [&mut d1, &mut d2, &mut wm] {
            while c.poll_event()?.is_some() {}
        }
        std::thread::sleep(Duration::from_secs(4));
    }
    Ok(())
}
