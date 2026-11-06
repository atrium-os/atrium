//! atrium-player as a Limen `media-player` child, end to end (M5): limend's
//! code in process with the real frescod slot op, a parent window, and the
//! player started the way portcullisd would start it, with its channel on
//! stdin. The parent feeds `red-blue.webm` (one second red, one blue) MSE
//! style and drives it like a page's video element would.
//!
//!   FRESCOD_SLOT_UIDS=$(id -u) FRESCOD_HEADLESS_PNG=/tmp/cap frescod &
//!   FRESCO_SOCKET=... PROBE_CAPTURE=/tmp/cap.png PROBE_CHILD_UID=$(id -u navtest) \
//!       atrium-media-probe CLIP
//!
//! The player runs as PROBE_CHILD_UID, which must not be limend's own.
//!
//! Checks: the slot attaches, CanPlay answers, the feed is appended in
//! pieces and buffered, a poster frame shows before Play, the video plays
//! red then blue in the slot, Time advances, Ended arrives, a seek while
//! paused shows its frame, a malformed message is dropped, a hidden slot
//! pauses, and detach ends the player. Then a second embed is given the file
//! by descriptor (`Load`), after limend refuses a socket in its place.
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child as Proc, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fresco_client::{Connection, Event};
use fresco_protocol::{RectParams, WindowHints};
use limen::client::Parent;
use limen::daemon::{serve, Launcher, Shared};
use limen::media_player::{ToChild, ToParent, APPEND_MAX};
use limen::proto::{EmbedId, ParentEvent};

const WIN_POS: (i32, i32) = (100, 100);
const SLOT: [f32; 4] = [40.0, 40.0, 320.0, 192.0];

/// Starts the player as portcullisd would: the channel is its stdin.
struct Spawn(Arc<Mutex<Vec<Proc>>>);
impl Launcher for Spawn {
    fn launch(&mut self, _: &str, _: EmbedId, channel: UnixStream) -> Result<u32, String> {
        let bin = std::env::var_os("PROBE_PLAYER").map(PathBuf::from).unwrap_or_else(|| {
            let me = std::env::current_exe().unwrap();
            me.parent().unwrap().parent().unwrap().join("atrium-player")
        });
        // The child runs as another uid than limend's, as in its jail
        // (frescod gives no embed connection to a binder's own uid).
        // SAFETY: plain syscall.
        let uid: u32 = std::env::var("PROBE_CHILD_UID").ok().and_then(|u| u.parse().ok()).unwrap_or_else(|| unsafe { libc::getuid() });
        let mut cmd = Command::new(&bin);
        cmd.arg("--limen").stdin(Stdio::from(OwnedFd::from(channel)));
        // SAFETY: plain syscall.
        if uid != unsafe { libc::getuid() } {
            use std::os::unix::process::CommandExt;
            cmd.uid(uid).gid(uid);
        }
        let c = cmd.spawn().map_err(|e| format!("{}: {e}", bin.display()))?;
        self.0.lock().unwrap().push(c);
        Ok(uid)
    }
    fn end(&mut self, _: EmbedId) {}
}

struct Probe {
    app: Parent,
    e: EmbedId,
    failed: u32,
    /// Every message from the player, oldest first.
    heard: Vec<ToParent>,
    /// Those no `wait` has taken yet.
    pending: std::collections::VecDeque<ToParent>,
}

impl Probe {
    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            self.failed += 1;
        }
    }
    fn send(&mut self, m: ToChild) {
        self.app.send(self.e, m.encode()).expect("send");
    }
    /// Read the player's messages until `want` matches one or `t` passes.
    fn wait(&mut self, t: Duration, want: impl Fn(&ToParent) -> bool) -> Option<ToParent> {
        if let Some(i) = self.pending.iter().position(&want) {
            return self.pending.remove(i);
        }
        let until = Instant::now() + t;
        while Instant::now() < until {
            match self.app.poll(Duration::from_millis(50)) {
                Ok(Some(ParentEvent::Message { embed, bytes })) if embed == self.e => {
                    let Some(m) = ToParent::decode(&bytes) else {
                        println!("FAIL a malformed message from the player");
                        self.failed += 1;
                        continue;
                    };
                    self.heard.push(m.clone());
                    if want(&m) {
                        return Some(m);
                    }
                    self.pending.push_back(m);
                }
                Ok(Some(ParentEvent::ChildLost { .. })) => {
                    println!("FAIL the player died");
                    self.failed += 1;
                    return None;
                }
                _ => {}
            }
        }
        None
    }
}

fn capture() -> Option<(u32, Vec<u8>)> {
    let path = std::env::var("PROBE_CAPTURE").ok()?;
    let f = std::fs::File::open(&path).ok()?;
    let mut r = png::Decoder::new(f).read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    (info.color_type == png::ColorType::Rgba).then_some((info.width, buf))
}

/// The colour at the slot's centre, waiting up to `t` for it to satisfy `ok`.
fn slot_colour(t: Duration, ok: impl Fn((u8, u8, u8)) -> bool) -> Option<(u8, u8, u8)> {
    let (x, y) = (WIN_POS.0 + (SLOT[0] + SLOT[2] / 2.0) as i32, WIN_POS.1 + (SLOT[1] + SLOT[3] / 3.0) as i32);
    let until = Instant::now() + t;
    let mut last = None;
    while Instant::now() < until {
        if let Some((w, px)) = capture() {
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            if let Some(p) = px.get(i..i + 3) {
                let c = (p[0], p[1], p[2]);
                last = Some(c);
                if ok(c) {
                    return last;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    last
}

fn red(c: (u8, u8, u8)) -> bool {
    c.0 > 180 && c.1 < 90 && c.2 < 90
}
fn blue(c: (u8, u8, u8)) -> bool {
    c.2 > 180 && c.0 < 90 && c.1 < 90
}

fn main() {
    let clip = std::env::args().nth(1).unwrap_or_else(|| "test-assets/media/red-blue.webm".into());
    let bytes = std::fs::read(&clip).unwrap_or_else(|e| panic!("{clip}: {e}"));
    let sock = std::env::var("FRESCO_SOCKET").unwrap_or_else(|_| "/tmp/frescod.sock".into());
    let act = std::env::var("LIMEN_FRESCOD_ACTIVATION").unwrap_or_else(|_| limen::fresco::DEFAULT_SOCKET.into());
    let procs = Arc::new(Mutex::new(Vec::new()));
    let resolve = Box::new(|_: u32| Some(("probe".to_string(), "org.example.page".to_string())));
    let defaults = Box::new(|_: &str, role: &str| (role == "media-player").then(|| "org.atrium.player".to_string()));
    let shared = Shared::new(Box::new(limen::fresco::Frescod::new(act)), Box::new(Spawn(procs.clone())), resolve, defaults);
    // SAFETY: plain syscall.
    let me = unsafe { libc::getuid() };
    let (a, b) = UnixStream::pair().unwrap();
    let sh = shared.clone();
    std::thread::spawn(move || {
        let _ = serve(aqueduct::Connection::wrap(b).unwrap(), me, sh);
    });

    // The page's window: grey, with the slot in it.
    let mut host = Connection::connect(&sock).expect("frescod app socket");
    let id = host.window_create(640, 400, "media-probe", WindowHints { initial_position: Some(WIN_POS), ..Default::default() }).expect("window");
    host.set_default_window(id as u16);
    host.scene_frame_begin().unwrap();
    host.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 2000.0, h: 2000.0, r: 0.2, g: 0.2, b: 0.2, a: 1.0, radius: 0.0 }).unwrap();
    host.scene_frame_end().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let token = loop {
        if let Ok(Some(Event::EmbedToken { token, .. })) = host.wait_event(Some(Duration::from_millis(100))) {
            break token;
        }
        assert!(Instant::now() < until, "no embed token from frescod");
    };

    let mut app = Parent::wrap(aqueduct::Connection::wrap(a).unwrap());
    let e = app.request("media-player", token).expect("request");
    app.place(e, Some(SLOT)).unwrap();
    let mut p = Probe { app, e, failed: 0, heard: Vec::new(), pending: Default::default() };
    let until = Instant::now() + Duration::from_secs(20);
    let mut attached = false;
    while Instant::now() < until && !attached {
        attached = matches!(p.app.poll(Duration::from_millis(100)), Ok(Some(ParentEvent::Attached { embed })) if embed == e);
    }
    p.check(attached, "the player's window is bound into the slot");

    p.send(ToChild::CanPlay { mime: "video/webm; codecs=\"vp9, opus\"".into() });
    let r = p.wait(Duration::from_secs(5), |m| matches!(m, ToParent::CanPlay { .. }));
    p.check(matches!(r, Some(ToParent::CanPlay { supported: true, .. })), &format!("CanPlay webm/vp9: yes ({r:?})"));
    p.send(ToChild::CanPlay { mime: "video/mp4; codecs=\"avc1.42E01E\"".into() });
    let r = p.wait(Duration::from_secs(5), |m| matches!(m, ToParent::CanPlay { .. }));
    p.check(matches!(r, Some(ToParent::CanPlay { supported: false, .. })), &format!("CanPlay mp4/avc1: no ({r:?})"));

    // A malformed message: limend carries it, the player drops it.
    p.app.send(e, vec![0xff, 0xff, 0xff, 0xff, 0x01]).unwrap();

    // The feed, in appends limend can carry, one in flight.
    p.send(ToChild::OpenSource);
    p.send(ToChild::AddTrack { track: 0, mime: "video/webm; codecs=vp9".into() });
    let mut appended = 0;
    for chunk in bytes.chunks(APPEND_MAX.min(4 << 10)) {
        p.send(ToChild::Append { track: 0, bytes: chunk.to_vec(), timestamp_offset: None });
        if p.wait(Duration::from_secs(10), |m| matches!(m, ToParent::Appended { track: 0 })).is_some() {
            appended += 1;
        }
    }
    let n = bytes.len().div_ceil(4 << 10);
    p.check(appended == n, &format!("{appended} of {n} appends acknowledged one by one"));
    p.send(ToChild::EndOfStream { error: None });
    let whole = |m: &ToParent| matches!(m, ToParent::Buffered { track: 0, ranges } if ranges.last().is_some_and(|r| r.1 > 1.9));
    p.wait(Duration::from_secs(5), whole);
    let b = p.heard.iter().rev().find_map(|m| if let ToParent::Buffered { track: 0, ranges } = m { Some(ranges.clone()) } else { None });
    p.check(b.as_ref().is_some_and(|r| r.len() == 1 && r[0].0 < 0.05 && r[0].1 > 1.9), &format!("buffered 0 to 2 s ({b:?})"));
    let r = p.wait(Duration::from_secs(10), |m| matches!(m, ToParent::Ready { .. }));
    p.check(matches!(r, Some(ToParent::Ready { width: 160, height: 96, .. })), &format!("Ready, 160x96 ({r:?})"));
    let c = slot_colour(Duration::from_secs(5), red);
    p.check(c.is_some_and(red), &format!("a red poster frame before Play ({c:?})"));

    p.send(ToChild::Play);
    let r = p.wait(Duration::from_secs(5), |m| matches!(m, ToParent::Playing));
    p.check(r.is_some(), "Playing");
    let c = slot_colour(Duration::from_secs(10), blue);
    p.check(c.is_some_and(blue), &format!("then blue in the slot ({c:?})"));
    let r = p.wait(Duration::from_secs(15), |m| matches!(m, ToParent::Ended));
    p.check(r.is_some(), "Ended");
    let times: Vec<f64> = p.heard.iter().filter_map(|m| if let ToParent::Time { secs } = m { Some(*secs) } else { None }).collect();
    p.check(times.len() >= 2 && times.windows(2).all(|w| w[0] <= w[1]) && times.last().is_some_and(|t| *t > 1.0), &format!("Time advances ({times:?})"));

    // Ended leaves it paused; a seek shows the frame there.
    p.send(ToChild::Seek { secs: 0.5 });
    let c = slot_colour(Duration::from_secs(10), red);
    p.check(c.is_some_and(red), &format!("a seek to 0.5 s while paused shows red ({c:?})"));
    if let (Ok(from), Ok(to)) = (std::env::var("PROBE_CAPTURE"), std::env::var("PROBE_SAVE")) {
        // Paused: the controls are up over the picture.
        std::thread::sleep(Duration::from_millis(600));
        let _ = std::fs::copy(from, to);
    }

    // A hidden slot suspends the player; it doesn't play on while hidden.
    p.send(ToChild::Play);
    p.app.place(e, None).unwrap();
    p.wait(Duration::from_millis(1500), |_| false);
    p.heard.clear();
    p.pending.clear();
    p.wait(Duration::from_millis(1000), |_| false);
    let ticking = p.heard.iter().filter(|m| matches!(m, ToParent::Time { .. })).count();
    p.check(ticking == 0, &format!("no Time while hidden ({ticking})"));
    p.app.place(e, Some(SLOT)).unwrap();
    let r = p.wait(Duration::from_secs(5), |m| matches!(m, ToParent::Time { .. } | ToParent::Ended));
    p.check(r.is_some(), &format!("and it plays on when shown again ({r:?})"));

    // Still answering after the malformed message.
    p.send(ToChild::CanPlay { mime: "audio/webm; codecs=opus".into() });
    let r = p.wait(Duration::from_secs(5), |m| matches!(m, ToParent::CanPlay { .. }));
    p.check(matches!(r, Some(ToParent::CanPlay { supported: true, .. })), "still answering after a malformed message");

    p.app.detach(e).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while Instant::now() < until && !exited {
        exited = procs.lock().unwrap().iter_mut().all(|c| c.try_wait().ok().flatten().is_some());
        std::thread::sleep(Duration::from_millis(100));
    }
    p.check(exited, "detach ends the player");

    // A second embed, given the whole file by descriptor (Load).
    let e2 = p.app.request("media-player", token).expect("request");
    p.app.place(e2, Some(SLOT)).unwrap();
    p.e = e2;
    p.pending.clear();
    let until = Instant::now() + Duration::from_secs(20);
    let mut attached = false;
    while Instant::now() < until && !attached {
        attached = matches!(p.app.poll(Duration::from_millis(100)), Ok(Some(ParentEvent::Attached { embed })) if embed == e2);
    }
    p.check(attached, "a second player is bound into the slot");
    {
        use std::os::fd::AsFd;
        let (sock, _peer) = UnixStream::pair().unwrap();
        let r = p.app.send_fd(e2, ToChild::Load { source: limen::media_player::Source::Fd, mime: "video/webm".into(), growing: false }.encode(), sock.as_fd());
        p.check(matches!(r, Err(limen::client::Error::Refused(limen::proto::refusal::BAD_FD))), &format!("limend refuses a socket as the Load descriptor ({r:?})"));
        let f = std::fs::File::open(&clip).unwrap();
        let r = p.app.send_fd(e2, ToChild::Load { source: limen::media_player::Source::Fd, mime: "video/webm".into(), growing: false }.encode(), f.as_fd());
        p.check(r.is_ok(), &format!("Load with the clip's descriptor ({r:?})"));
    }
    let r = p.wait(Duration::from_secs(10), |m| matches!(m, ToParent::Ready { .. }));
    p.check(matches!(r, Some(ToParent::Ready { width: 160, height: 96, .. })), &format!("Ready from the loaded file ({r:?})"));
    let c = slot_colour(Duration::from_secs(5), red);
    p.check(c.is_some_and(red), &format!("its red poster frame ({c:?})"));
    p.send(ToChild::Play);
    let c = slot_colour(Duration::from_secs(10), blue);
    p.check(c.is_some_and(blue), &format!("then blue ({c:?})"));
    let r = p.wait(Duration::from_secs(15), |m| matches!(m, ToParent::Ended));
    p.check(r.is_some(), "and Ended");
    p.app.detach(e2).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    for c in procs.lock().unwrap().iter_mut() {
        let _ = c.kill();
    }
    if p.failed > 0 {
        println!("{} FAILED", p.failed);
        std::process::exit(1);
    }
    println!("all passed");
}
