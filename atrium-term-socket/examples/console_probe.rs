//! console_probe: a stand-in launcher for scripts/vm-console-check.sh
//! (homes.md §4). Installed with `app-launch`, it opens a window and asks
//! portcullisd for a console: once at start with no press (refused), then
//! on each click in its window with that click's press, for the next app
//! id listed in its bundle's `/targets`. Reports on stderr.

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};
use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};

fn ask(app: &str, activation: [u8; 16]) -> String {
    let Some(path) = portcullis_ipc::resolve_socket_path() else { return "err no portcullisd".into() };
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(path) else { return "err connect".into() };
    if !matches!(round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }), Ok(Response::Hello { .. })) {
        return "err handshake".into();
    }
    match round_trip(&mut s, &Request::Console { app_id: app.into(), activation, tab_of: None, new_window: false }) {
        Ok(Response::ConsoleOpened { title }) => format!("ok {title}"),
        Ok(Response::Error { message }) => format!("err {message}"),
        other => format!("err {other:?}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let targets: Vec<String> = std::fs::read_to_string("/targets").unwrap_or_default().lines().map(str::to_string).collect();
    let mut conn = Connection::connect(fresco_client::default_socket_path())?;
    let win = conn.window_create(300, 200, "console-probe", WindowHints { initial_position: Some((0, 0)), ..Default::default() })?;
    let mut f = conn.frame()?;
    f.rect(RectParams { x: 0.0, y: 0.0, w: 300.0, h: 200.0, r: 0.2, g: 0.3, b: 0.5, a: 1.0, radius: 0.0 })?;
    f.finish()?;
    if let Some(t) = targets.first() {
        eprintln!("console-probe: nopress {t} -> {}", ask(t, [0; 16]));
    }
    let mut next = 0;
    loop {
        let Some(ev) = conn.wait_event(None)? else { continue };
        if let Event::PointerButton { window_id, pressed: true, activation, .. } = ev {
            if window_id != win {
                continue;
            }
            let Some(t) = targets.get(next) else { continue };
            let r = ask(t, activation);
            // A click that only focused the window asks again.
            if !r.contains("lost focus") {
                next += 1;
            }
            eprintln!("console-probe: {t} -> {r}");
        }
    }
}
