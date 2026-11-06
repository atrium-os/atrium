//! atrium-video-probe — end to end check of Fresco video layers
//! (docs/spec/fresco-video.md) through the real socket: a window with a
//! transparent hole and a video layer under it, frames filled in shared
//! memory and composed under the window, clipped to it, buffers given
//! back, the refusals by the limits, and clicks on the video reaching the
//! window.
//!
//! Needs frescod with the compute renderer and FRESCOD_HEADLESS_PNG; the
//! capture is read from PROBE_CAPTURE.
//!
//! Usage: atrium-video-probe SOCK INJECT_SOCK

use fresco_client::{Connection, Event};
use fresco_protocol::video::{buffer_bytes, format};
use fresco_protocol::{RectParams, WindowHints};
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const WIN_POS: (i32, i32) = (100, 120);
const WIN: (u32, u32) = (320, 240);
const BAR_H: f32 = 40.0;
const SRC: (u32, u32) = (64, 48);

struct Probe {
    failed: u32,
}

impl Probe {
    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            self.failed += 1;
        }
    }
}

/// A buffer, mapped read-write.
struct Buf {
    ptr: *mut u8,
    len: usize,
}

impl Buf {
    fn map(fd: &OwnedFd, len: usize) -> Buf {
        // SAFETY: a shared mapping of a memfd of at least `len` bytes.
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        assert!(ptr != libc::MAP_FAILED, "mmap a video buffer");
        Buf { ptr: ptr as *mut u8, len }
    }

    /// Fill as NV12 of one colour (BT.709 limited Y'CbCr).
    fn nv12(&mut self, y: u8, cb: u8, cr: u8) {
        // SAFETY: our mapping, `len` bytes.
        let b = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) };
        let n = (SRC.0 * SRC.1) as usize;
        b[..n].fill(y);
        for c in b[n..].chunks_exact_mut(2) {
            c[0] = cb;
            c[1] = cr;
        }
    }
}

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

/// The screen as frescod last captured it: (width, RGBA bytes).
fn capture() -> Option<(u32, Vec<u8>)> {
    let path = std::env::var("PROBE_CAPTURE").ok()?;
    // frescod captures at most every 200 ms.
    std::thread::sleep(Duration::from_millis(1500));
    let f = std::fs::File::open(&path).ok()?;
    let mut r = png::Decoder::new(f).read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    (info.color_type == png::ColorType::Rgba).then_some((info.width, buf))
}

fn rgb_at(cap: &(u32, Vec<u8>), x: i32, y: i32) -> (u8, u8, u8) {
    let i = (y as usize * cap.0 as usize + x as usize) * 4;
    (cap.1[i], cap.1[i + 1], cap.1[i + 2])
}

fn red(c: (u8, u8, u8)) -> bool { c.0 > 230 && c.1 < 30 && c.2 < 30 }
fn blue(c: (u8, u8, u8)) -> bool { c.0 < 30 && c.1 < 30 && c.2 > 230 }

/// `--play SOCK FPS FRAMES`: play a clip of `FRAMES` frames at `FPS`
/// through a layer, queued a little ahead on CLOCK_MONOTONIC as a player
/// would, and report when each was presented against when it was due.
/// Then hold a green frame for 30 s for a screen dump.
fn play(sock: &str, fps: u32, frames: u32) -> u32 {
    let mut p = Probe { failed: 0 };
    let mut app = Connection::connect(sock).expect("app socket");
    let hints = WindowHints { initial_position: Some(WIN_POS), ..WindowHints::default() };
    let win = app.window_create(WIN.0, WIN.1, "player", hints).expect("window");
    app.set_default_window(win as u16);
    app.scene_frame_begin().unwrap();
    app.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: WIN.0 as f32, h: BAR_H, r: 1.0, g: 1.0, b: 1.0, a: 1.0, radius: 0.0 }).unwrap();
    app.scene_frame_end().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let n_bufs = 4u32;
    let Some((layer, fds)) = app.video_layer_create(win, format::NV12, SRC.0, SRC.1, n_bufs).unwrap() else {
        p.check(false, "a layer");
        return p.failed;
    };
    let len = buffer_bytes(format::NV12, SRC.0, SRC.1).unwrap() as usize;
    let mut bufs: Vec<Buf> = fds.iter().map(|f| Buf::map(f, len)).collect();
    app.video_layer_place(layer, Some([0.0, BAR_H, WIN.0 as f32, WIN.1 as f32 - BAR_H])).unwrap();
    app.video_layer_rate(layer, fps * 1000).unwrap();
    let period = 1_000_000_000 / u64::from(fps);
    let t0 = now_ns() + 500_000_000;
    let due = |i: u32| t0 + u64::from(i) * period;
    let mut free: Vec<u32> = (0..n_bufs).collect();
    let mut shown: Vec<(u32, u64, u32)> = Vec::new();
    let mut next = 0u32;
    let end = Instant::now() + Duration::from_secs(u64::from(frames / fps.max(1)) + 30);
    while Instant::now() < end && (next < frames || shown.last().map(|s| s.0) != Some(frames - 1)) {
        // Queue ahead: up to 250 ms before a frame is due.
        while next < frames && !free.is_empty() && due(next) <= now_ns() + 250_000_000 {
            let b = free.remove(0);
            let k = (next % 6) as u8;
            bufs[b as usize].nv12(40 + k * 30, 128, 128);
            app.video_frame(layer, b, next, due(next)).unwrap();
            next += 1;
        }
        match app.wait_event(Some(Duration::from_millis(5))) {
            Ok(Some(Event::VideoReleased { buffer, .. })) => free.push(buffer),
            Ok(Some(Event::VideoPresented { seq, vblank_ns, dropped, .. })) => shown.push((seq, vblank_ns, dropped)),
            _ => {}
        }
        if next >= frames && shown.last().is_some_and(|s| s.0 == frames - 1) {
            break;
        }
    }
    let dropped: u32 = shown.iter().map(|s| s.2).sum();
    let in_order = shown.windows(2).all(|w| w[0].0 < w[1].0);
    let mut late: Vec<i64> = shown.iter().map(|s| (s.1 as i64 - due(s.0) as i64) / 1_000).collect();
    late.sort_unstable();
    let pct = |q: f64| late.get(((late.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0);
    println!("played {frames} frames at {fps} fps: {} presented, {dropped} dropped", shown.len());
    println!("presented after due, µs: min {} median {} p95 {} max {}", pct(0.0), pct(0.5), pct(0.95), pct(1.0));
    p.check(shown.len() as u32 + dropped == frames, "every frame presented or counted dropped");
    p.check(in_order, "presented in order");
    p.check(late.first().is_some_and(|&l| l >= 0), "none early");
    // Hold a reference frame: BT.709 limited green.
    let b = free.first().copied().unwrap_or(0);
    bufs[b as usize].nv12(173, 42, 26);
    app.video_frame(layer, b, frames, 0).unwrap();
    let _ = wait(&mut app, |e| matches!(e, Event::VideoPresented { seq, .. } if *seq == frames));
    println!("HOLD green at {},{}", WIN_POS.0 + WIN.0 as i32 / 2, WIN_POS.1 + 140);
    std::thread::sleep(Duration::from_secs(30));
    drop(bufs);
    p.failed
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.first().map(String::as_str) == Some("--play") {
        let (Some(sock), fps, n) = (a.get(1), a.get(2).and_then(|v| v.parse().ok()).unwrap_or(24), a.get(3).and_then(|v| v.parse().ok()).unwrap_or(96)) else {
            eprintln!("usage: atrium-video-probe --play SOCK [FPS] [FRAMES]");
            std::process::exit(2);
        };
        let failed = play(sock, fps, n);
        println!("{}", if failed == 0 { "PASS" } else { "FAILED" });
        std::process::exit(if failed == 0 { 0 } else { 1 });
    }
    let [sock, inject] = &a[..] else {
        eprintln!("usage: atrium-video-probe SOCK INJECT_SOCK");
        std::process::exit(2);
    };
    let mut p = Probe { failed: 0 };
    let mut other = Connection::connect(sock).expect("app socket");
    let hints = WindowHints { initial_position: Some((700, 120)), ..WindowHints::default() };
    let theirs = other.window_create(100, 100, "someone else", hints).expect("window");

    let mut app = Connection::connect(sock).expect("app socket");
    let hints = WindowHints { initial_position: Some(WIN_POS), ..WindowHints::default() };
    let win = app.window_create(WIN.0, WIN.1, "video", hints).expect("window");
    app.set_default_window(win as u16);
    // Controls on top; the rest left transparent for the video under it.
    app.scene_frame_begin().unwrap();
    app.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: WIN.0 as f32, h: BAR_H, r: 1.0, g: 1.0, b: 1.0, a: 1.0, radius: 0.0 }).unwrap();
    app.scene_frame_end().unwrap();
    std::thread::sleep(Duration::from_millis(500));

    // Limits.
    // In split mode window ids are the client's own, so another client's
    // window can't even be named; there `theirs` may be our own id.
    if theirs != win {
        let r = app.video_layer_create(theirs, format::NV12, SRC.0, SRC.1, 3).unwrap();
        p.check(r.is_none(), "a layer under another client's window: refused");
    }
    let r = app.video_layer_create(win + 50, format::NV12, SRC.0, SRC.1, 3).unwrap();
    p.check(r.is_none(), "a layer under no window of ours: refused");
    let r = app.video_layer_create(win, format::NV12, SRC.0, SRC.1, 2).unwrap();
    p.check(r.is_none(), "two buffers: refused");
    let r = app.video_layer_create(win, format::NV12, 63, SRC.1, 3).unwrap();
    p.check(r.is_none(), "odd NV12 width: refused");
    let r = app.video_layer_create(win, format::P010, 7680, 4320, 3).unwrap();
    p.check(r.is_none(), "past 64 MiB of buffers: refused");

    let Some((layer, fds)) = app.video_layer_create(win, format::NV12, SRC.0, SRC.1, 3).unwrap() else {
        p.check(false, "a layer under its own window");
        std::process::exit(1);
    };
    p.check(fds.len() == 3, &format!("layer {layer} with {} buffers", fds.len()));
    let len = buffer_bytes(format::NV12, SRC.0, SRC.1).unwrap() as usize;
    let mut bufs: Vec<Buf> = fds.iter().map(|f| Buf::map(f, len)).collect();

    // Red, under the window below its bar.
    let video = [0.0, BAR_H, WIN.0 as f32, WIN.1 as f32 - BAR_H];
    app.video_layer_place(layer, Some(video)).unwrap();
    bufs[0].nv12(63, 102, 240);
    app.video_frame(layer, 0, 1, 0).unwrap();
    let rel = wait(&mut app, |e| matches!(e, Event::VideoReleased { .. }));
    p.check(matches!(rel, Some(Event::VideoReleased { layer: l, buffer: 0, .. }) if l == layer), &format!("buffer 0 released ({rel:?})"));
    if let Some(cap) = capture() {
        let (cx, cy) = (WIN_POS.0 + WIN.0 as i32 / 2, WIN_POS.1 + 140);
        let c = rgb_at(&cap, cx, cy);
        p.check(red(c), &format!("the frame shows under the window's hole ({c:?})"));
        let c = rgb_at(&cap, cx, WIN_POS.1 + 20);
        p.check(c == (255, 255, 255), &format!("the window's bar is drawn over it ({c:?})"));
    } else {
        p.check(false, "a capture (PROBE_CAPTURE)");
    }

    // Blue, sticking out of the window on both sides: clipped to it.
    app.video_layer_place(layer, Some([-60.0, BAR_H, WIN.0 as f32 + 120.0, 100.0])).unwrap();
    bufs[1].nv12(32, 240, 118);
    app.video_frame(layer, 1, 2, 0).unwrap();
    let rel = wait(&mut app, |e| matches!(e, Event::VideoReleased { .. }));
    p.check(matches!(rel, Some(Event::VideoReleased { buffer: 1, .. })), "buffer 1 released");
    if let Some(cap) = capture() {
        let y = WIN_POS.1 + 80;
        let c = rgb_at(&cap, WIN_POS.0 + 5, y);
        p.check(blue(c), &format!("inside the window: blue ({c:?})"));
        let c = rgb_at(&cap, WIN_POS.0 - 10, y);
        p.check(!blue(c), &format!("left of the window: not drawn ({c:?})"));
        let c = rgb_at(&cap, WIN_POS.0 + WIN.0 as i32 + 10, y);
        p.check(!blue(c), &format!("right of the window: not drawn ({c:?})"));
    }

    // V2: present times. Back to the whole hole, then three frames queued
    // ahead, each to show at its time, in order, announced once shown.
    app.video_layer_place(layer, Some(video)).unwrap();
    app.video_layer_rate(layer, 24_000).unwrap();
    while let Ok(Some(_)) = app.wait_event(Some(Duration::from_millis(50))) {}
    let t0 = now_ns() + 400_000_000;
    let times = [t0, t0 + 400_000_000, t0 + 800_000_000];
    for (i, &t) in times.iter().enumerate() {
        bufs[i].nv12(63 + i as u8 * 20, 128, 128);
        app.video_frame(layer, i as u32, 10 + i as u32, t).unwrap();
    }
    let got = events_for(&mut app, Duration::from_secs(6));
    let shown: Vec<(u32, u64, u32)> = got.iter().filter_map(|e| match e {
        Event::VideoPresented { layer: l, seq, vblank_ns, dropped, .. } if *l == layer => Some((*seq, *vblank_ns, *dropped)),
        _ => None,
    }).collect();
    let released = got.iter().filter(|e| matches!(e, Event::VideoReleased { .. })).count();
    p.check(released == 3, &format!("all three buffers released ({released})"));
    p.check(shown.iter().map(|s| s.0).collect::<Vec<_>>() == [10, 11, 12], &format!("each frame presented once, in order ({shown:?})"));
    let late: Vec<i64> = shown.iter().zip(times).map(|(s, t)| (s.1 as i64 - t as i64) / 1_000_000).collect();
    // Never early; late by at most a capture interval or two under TCG.
    p.check(late.iter().all(|&ms| (0..=600).contains(&ms)), &format!("none early, none far late (ms after its time: {late:?})"));
    p.check(shown.iter().all(|s| s.2 == 0), "none dropped");

    // Two frames for the same time: the later one shows, one dropped.
    let t = now_ns() + 400_000_000;
    bufs[0].nv12(63, 102, 240);
    app.video_frame(layer, 0, 20, t).unwrap();
    bufs[1].nv12(63, 102, 240);
    app.video_frame(layer, 1, 21, t).unwrap();
    let got = events_for(&mut app, Duration::from_secs(3));
    let shown: Vec<(u32, u32)> = got.iter().filter_map(|e| match e {
        Event::VideoPresented { seq, dropped, .. } => Some((*seq, *dropped)),
        _ => None,
    }).collect();
    p.check(shown == [(21, 1)], &format!("two due at once: the later shows, one dropped ({shown:?})"));

    // Hidden: a frame comes back but is never presented.
    app.video_layer_place(layer, None).unwrap();
    bufs[2].nv12(63, 102, 240);
    app.video_frame(layer, 2, 30, 0).unwrap();
    let got = events_for(&mut app, Duration::from_secs(2));
    let rel = got.iter().any(|e| matches!(e, Event::VideoReleased { buffer: 2, .. }));
    let pres = got.iter().any(|e| matches!(e, Event::VideoPresented { .. }));
    p.check(rel && !pres, &format!("hidden: released, not presented ({got:?})"));
    let _ = app.video_layer_rate(layer, 5); // out of range: an error, nothing else
    app.video_layer_place(layer, Some([0.0, BAR_H, 200.0, 100.0])).unwrap();
    bufs[0].nv12(32, 240, 118);
    app.video_frame(layer, 0, 31, 0).unwrap();
    let got = events_for(&mut app, Duration::from_secs(2));
    p.check(got.iter().any(|e| matches!(e, Event::VideoPresented { seq: 31, .. })), "shown again: presented");

    // V3: colour. BT.601 full-range red.
    use fresco_protocol::video::{matrix, primaries, range, transfer, Mastering, VideoLayerColor, VideoLayerCreate};
    let colour = |m, r, p, t, hdr| VideoLayerColor { layer, matrix: m, range: r, primaries: p, transfer: t, hdr };
    let show = |app: &mut Connection, bufs: &mut Vec<Buf>, i: usize, yuv: (u8, u8, u8), seq: u32| {
        bufs[i].nv12(yuv.0, yuv.1, yuv.2);
        app.video_frame(layer, i as u32, seq, 0).unwrap();
        let _ = wait(app, |e| matches!(e, Event::VideoPresented { seq: s, .. } if *s == seq));
    };
    let at = (WIN_POS.0 + 100, WIN_POS.1 + 80);
    app.video_layer_color(colour(matrix::BT601, range::FULL, primaries::BT709, transfer::BT1886, None)).unwrap();
    show(&mut app, &mut bufs, 1, (76, 85, 255), 40);
    if let Some(cap) = capture() {
        let c = rgb_at(&cap, at.0, at.1);
        p.check(red(c), &format!("BT.601 full range red ({c:?})"));
    }
    // HDR10: PQ white at the content's peak is SDR white; 100 cd/m² a
    // little under half of it in linear light.
    let m = Mastering { max_luminance: 1000, max_cll: 1000, ..Default::default() };
    app.video_layer_color(colour(matrix::BT2020, range::FULL, primaries::BT2020, transfer::PQ, Some(m))).unwrap();
    show(&mut app, &mut bufs, 2, (255, 128, 128), 41);
    if let Some(cap) = capture() {
        let c = rgb_at(&cap, at.0, at.1);
        p.check(c.0 >= 250 && c.1 >= 250 && c.2 >= 250, &format!("PQ peak is SDR white ({c:?})"));
    }
    show(&mut app, &mut bufs, 0, (130, 128, 128), 42);
    if let Some(cap) = capture() {
        let c = rgb_at(&cap, at.0, at.1);
        p.check((160..=190).contains(&c.0) && c.0.abs_diff(c.1) <= 2 && c.1.abs_diff(c.2) <= 2, &format!("PQ 100 cd/m² is a grey under SDR white ({c:?})"));
    }
    let _ = app.video_layer_color(colour(9, range::FULL, primaries::BT709, transfer::BT1886, None));
    p.check(app.display_info().is_ok(), "an unknown matrix is refused, the connection stays up");
    app.video_layer_color(colour(matrix::BT709, range::LIMITED, primaries::BT709, transfer::BT1886, None)).unwrap();

    // Protected: black in a capture (headless frescod's output is one).
    let c = VideoLayerCreate { window_id: win, format: format::NV12, width: SRC.0, height: SRC.1, buffers: 3, protected: true };
    if let Some((pl, pfds)) = app.video_layer_create_with(c).unwrap() {
        let mut pb: Vec<Buf> = pfds.iter().map(|f| Buf::map(f, len)).collect();
        app.video_layer_place(pl, Some([220.0, BAR_H, 100.0, 100.0])).unwrap();
        pb[0].nv12(63, 102, 240);
        app.video_frame(pl, 0, 1, 0).unwrap();
        let _ = wait(&mut app, |e| matches!(e, Event::VideoPresented { layer: l, .. } if *l == pl));
        if let Some(cap) = capture() {
            let c = rgb_at(&cap, WIN_POS.0 + 270, WIN_POS.1 + 80);
            p.check(c == (0, 0, 0), &format!("protected: black in the capture ({c:?})"));
        }
        app.video_layer_destroy(pl).unwrap();
        drop(pb);
    } else {
        p.check(false, "a protected layer");
    }

    // A click on the video goes to its window, not through it.
    while let Ok(Some(_)) = app.wait_event(Some(Duration::from_millis(50))) {}
    let inj = |line: &str| {
        let mut s = UnixStream::connect(inject).expect("injector socket");
        writeln!(s, "{line}").unwrap();
    };
    inj(&format!("MOVE {} {}", WIN_POS.0 + 100, WIN_POS.1 + 80));
    let mut hit = false;
    for _ in 0..20 {
        inj("BTN 1 1");
        inj("BTN 1 0");
        if wait_short(&mut app) {
            hit = true;
            break;
        }
    }
    p.check(hit, "a click on the video reaches the window");

    // Destroyed: the hole shows what is under the window again.
    app.video_layer_destroy(layer).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    app.window_present(win).ok();
    if let Some(cap) = capture() {
        let c = rgb_at(&cap, WIN_POS.0 + 100, WIN_POS.1 + 80);
        p.check(!blue(c), &format!("destroyed: gone ({c:?})"));
    }
    // A frame for it now is an error the client hears nothing back for;
    // the connection stays up.
    let _ = app.video_frame(layer, 2, 3, 0);
    let ok = app.display_info().is_ok();
    p.check(ok, "a frame for a destroyed layer leaves the connection up");

    drop(bufs);
    println!("{}", if p.failed == 0 { "PASS" } else { "FAILED" });
    std::process::exit(if p.failed == 0 { 0 } else { 1 });
}

/// CLOCK_MONOTONIC, the clock present times are on.
fn now_ns() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: one timespec of ours for the call.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// Every event in the next `d`.
fn events_for(c: &mut Connection, d: Duration) -> Vec<Event> {
    let end = Instant::now() + d;
    let mut v = Vec::new();
    while Instant::now() < end {
        if let Ok(Some(e)) = c.wait_event(Some(Duration::from_millis(50))) {
            v.push(e);
        }
    }
    v
}

fn wait_short(c: &mut Connection) -> bool {
    let until = Instant::now() + Duration::from_millis(150);
    while Instant::now() < until {
        if let Ok(Some(Event::PointerButton { pressed: true, .. })) = c.wait_event(Some(Duration::from_millis(20))) {
            return true;
        }
    }
    false
}
