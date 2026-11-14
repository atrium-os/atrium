//! limend end to end against a live frescod (limen.md §12 L5).
//!
//! Runs limend's code in process with the real frescod slot op, a stand-in
//! Arca (a window opened `embed_pending` that provides `credential`), and a
//! parent window. Run as a uid on frescod's slot allowlist:
//!
//!   FRESCOD_SLOT_UIDS=$(id -u) frescod &   # or the session's frescod
//!   FRESCO_SOCKET=... limen-embed-probe
//!
//! Checks: the child is attached only once frescod has bound it, frescod
//! sizes it to the slot, a hidden slot suspends it, detach reaches the
//! child, and a parent naming another app's window loses the embed.

use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};
use limen::client::{Child, Parent};
use limen::daemon::{serve, Launcher, Shared};
use limen::proto::{ChildEvent, EmbedId, ParentEvent};

struct NoLaunch;
impl Launcher for NoLaunch {
    fn launch(&mut self, _: &str, _: EmbedId, _: UnixStream) -> Result<u32, String> { Err("not in this probe".into()) }
    fn end(&mut self, _: EmbedId) {}
}

fn window(sock: &str, size: (u32, u32), hints: WindowHints, rgba: [f32; 4]) -> (Connection, [u8; 16]) {
    let mut c = Connection::connect(sock).expect("frescod app socket");
    let id = c.window_create(size.0, size.1, "limen-probe", hints).expect("window");
    c.set_default_window(id as u16);
    c.scene_frame_begin().unwrap();
    c.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 2000.0, h: 2000.0, r: rgba[0], g: rgba[1], b: rgba[2], a: rgba[3], radius: 0.0 }).unwrap();
    c.scene_frame_end().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if let Ok(Some(Event::EmbedToken { token, .. })) = c.wait_event(Some(Duration::from_millis(100))) {
            return (c, token);
        }
    }
    panic!("no embed token from frescod");
}

fn main() {
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let act = std::env::var("LIMEN_FRESCOD_ACTIVATION").unwrap_or_else(|_| limen::fresco::DEFAULT_SOCKET.into());
    // One uid plays every part, so the registry says it is Arca.
    let resolve = Box::new(|_: u32| Some(("probe".to_string(), limen::catalogue::ARCA.to_string())));
    let shared = Shared::new(Box::new(limen::fresco::Frescod::new(act)), Box::new(NoLaunch), resolve, Box::new(|_: &str, _: &str| None));
    let me = unsafe { libc::getuid() };
    let conn = |sh: &Arc<Shared>| {
        let (a, b) = UnixStream::pair().unwrap();
        let sh = sh.clone();
        std::thread::spawn(move || { let _ = serve(aqueduct::Connection::wrap(b).unwrap(), me, sh); });
        aqueduct::Connection::wrap(a).unwrap()
    };
    let mut failed = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok { failed += 1; }
    };

    let (_host, host_tok) = window(&sock, (400, 200), WindowHints { initial_position: Some((100, 100)), ..Default::default() }, [0.3, 0.6, 0.3, 1.0]);
    let (mut arca_win, arca_tok) = window(&sock, (10, 10), WindowHints { embed_pending: true, ..Default::default() }, [0.2, 0.2, 0.9, 1.0]);
    let mut arca = Child::wrap(conn(&shared));
    arca.provide_role("credential").expect("provide");
    let mut app = Parent::wrap(conn(&shared));

    let e = app.request("credential", host_tok).expect("request");
    let attach = arca.poll(Duration::from_secs(2)).unwrap();
    check(matches!(attach, Some(ChildEvent::Attach { embed, .. }) if embed == e), "Arca hears the new instance");
    app.place(e, Some([20.0, 20.0, 240.0, 36.0])).unwrap();
    let early = app.poll(Duration::from_millis(200)).unwrap();
    check(early.is_none(), "not attached before the child is ready");
    arca.ready(e, arca_tok).unwrap();
    let ev = app.poll(Duration::from_secs(2)).unwrap();
    check(ev == Some(ParentEvent::Attached { embed: e }), &format!("attached once frescod bound it ({ev:?})"));
    let mut sized = false;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !sized {
        if let Ok(Some(Event::Resized { buffer_width, buffer_height, .. })) = arca_win.wait_event(Some(Duration::from_millis(100))) {
            sized = (buffer_width, buffer_height) == (240, 36);
        }
    }
    check(sized, "frescod sized Arca's window to the slot");

    app.place(e, None).unwrap();
    let ev = app.poll(Duration::from_secs(2)).unwrap();
    check(ev == Some(ParentEvent::Suspended { embed: e }), &format!("hiding the slot suspends it ({ev:?})"));
    app.place(e, Some([20.0, 20.0, 240.0, 36.0])).unwrap();
    let ev = app.poll(Duration::from_secs(2)).unwrap();
    check(ev == Some(ParentEvent::Resumed { embed: e }), &format!("and placing it again resumes it ({ev:?})"));
    app.detach(e).unwrap();
    let mut detaching = false;
    while let Ok(Some(ev)) = arca.poll(Duration::from_millis(500)) {
        detaching |= ev == ChildEvent::Detaching { embed: e };
    }
    check(detaching, "detach reaches Arca");

    // A parent naming a window that is a slot child (Arca's own) as its
    // host: frescod refuses or the owners don't match; either way no embed.
    let e2 = app.request("credential", arca_tok).expect("request");
    let _ = arca.poll(Duration::from_secs(2));
    app.place(e2, Some([0.0, 0.0, 200.0, 30.0])).unwrap();
    arca.ready(e2, host_tok).unwrap();
    let ev = app.poll(Duration::from_millis(800)).unwrap();
    check(!matches!(ev, Some(ParentEvent::Attached { .. })), &format!("swapped windows are not attached ({ev:?})"));
    let _ = shared.embeds();

    if failed > 0 {
        println!("{failed} FAILED");
        std::process::exit(1);
    }
    println!("all passed");
}
