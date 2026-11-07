//! atrium-player. With `--headless` it plays a file or a segment feed
//! without a window and prints what it would show. That is how M3 is
//! tested before Fresco's video layers and Limen's embedding (M4, M5).
//!
//!   atrium-player --headless FILE            # Load: the worker reads FILE
//!   atrium-player --headless --feed FILE...  # one MSE track per FILE, appended in chunks
//!   atrium-player [--size WxH] [--exit-at-end] FILE   # in a window, with controls
//!   atrium-player --limen                    # a Limen media-player child
//!   FILE may be an http(s) URL: atrium-player-fetch (the sandboxed
//!   fetcher, a process of its own) downloads it and the player plays it
//!   as it arrives. ATRIUM_PLAYER_FETCH_UNCONFINED=1 lets the fetcher run
//!   without Capsicum off FreeBSD; ATRIUM_PLAYER_TRUST names its CA file.
//!   options: --seek SECS (after the first frame), --probe-sandbox,
//!            --kill-worker-after N (frames)
use std::fs::File;
use std::time::{Duration, Instant};

use atrium_player::app::{App, LimenHost, Standalone};
use atrium_player::fetch::{self, Fetcher, FromFetcher, UrlHost};
use atrium_player::supervisor::{LocalSink, Options, Player, PlayerEvent, Source};
use md5::{Digest, Md5};

fn usage() -> ! {
    eprintln!("usage: atrium-player [--size WxH] [--exit-at-end] FILE");
    eprintln!("       atrium-player --limen   # a Limen media-player child, channel on stdin");
    eprintln!("       atrium-player --headless [--feed] [--seek SECS] [--probe-sandbox] [--kill-worker-after N] FILE...");
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut headless, mut feed, mut seek, mut probe, mut kill_after) = (false, false, None, false, None);
    let (mut exit_at_end, mut size) = (false, (640u32, 360u32));
    let mut files = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--headless" => headless = true,
            "--feed" => feed = true,
            "--seek" => seek = args.next().and_then(|v| v.parse::<f64>().ok()),
            "--probe-sandbox" => probe = true,
            "--kill-worker-after" => kill_after = args.next().and_then(|v| v.parse::<usize>().ok()),
            "--exit-at-end" => exit_at_end = true,
            "--limen" => return embedded(),
            "--size" => {
                size = args
                    .next()
                    .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
                    .unwrap_or_else(|| usage())
            }
            "-h" | "--help" => usage(),
            _ => files.push(a),
        }
    }
    if files.is_empty() {
        usage();
    }
    if !headless {
        return windowed(&files[0], exit_at_end, size);
    }
    let sink = LocalSink::new(Box::new(|bytes, pts| println!("video {pts} {:x}", Md5::digest(bytes))));
    // A URL: the fetcher's file, played as it grows.
    let mut fetcher = None;
    let source = if !feed && fetch::is_url(&files[0]) {
        let mut f = start_fetcher(&files[0]);
        let file = loop {
            match f.poll(Duration::from_secs(1)) {
                Ok(None) => {}
                Ok(Some((FromFetcher::File { length, .. }, Some(fd)))) => {
                    println!("fetching {} bytes", length.map_or("?".to_string(), |l| l.to_string()));
                    break File::from(fd);
                }
                Ok(Some((FromFetcher::Failed { reason }, _))) => {
                    eprintln!("{}: {reason}", files[0]);
                    std::process::exit(1);
                }
                _ => {
                    eprintln!("{}: the fetcher broke off", files[0]);
                    std::process::exit(1);
                }
            }
        };
        fetcher = Some(f);
        Source::File(file)
    } else if feed {
        Source::Feed(files.iter().enumerate().map(|(i, f)| (i as u8, if f.ends_with(".mp4") { "video/mp4".into() } else { "video/webm".into() })).collect())
    } else {
        match File::open(&files[0]) {
            Ok(f) => Source::File(f),
            Err(e) => {
                eprintln!("{}: {e}", files[0]);
                std::process::exit(1);
            }
        }
    };
    let started = match source {
        Source::File(f) if fetcher.is_some() => Player::new_growing(Options::default(), sink, f),
        source => Player::new(Options::default(), sink, source),
    };
    let mut p = match started {
        Ok(p) => p,
        Err(e) => {
            eprintln!("atrium-player: starting the decode worker: {e}");
            std::process::exit(1);
        }
    };
    if probe {
        p.probe_sandbox();
    }
    // Segment feed: append each file in 64 KiB pieces, one in flight per track.
    let mut pending: Vec<(u8, Vec<u8>, usize)> = Vec::new();
    if feed {
        for (i, f) in files.iter().enumerate() {
            let bytes = std::fs::read(f).unwrap_or_else(|e| {
                eprintln!("{f}: {e}");
                std::process::exit(1)
            });
            pending.push((i as u8, bytes, 0));
        }
        for (t, b, at) in pending.iter_mut() {
            let n = (b.len() - *at).min(64 << 10);
            p.append(*t, b[*at..*at + n].to_vec(), None);
            *at += n;
        }
    }
    let (mut frames, mut pcm_frames, mut seeked) = (0usize, 0usize, false);
    let mut pcm = vec![0f32; 48_000 * 8];
    let mut first_pcm_pts = None;
    loop {
        if let Some(f) = fetcher.as_mut() {
            match f.poll(Duration::ZERO) {
                Ok(None) => {}
                Ok(Some((FromFetcher::Done { bytes }, _))) => {
                    println!("fetched {bytes} bytes");
                    p.input_done();
                    fetcher = None;
                }
                r => {
                    eprintln!("atrium-player: the download stopped ({r:?}); playing what arrived");
                    p.input_done();
                    fetcher = None;
                }
            }
        }
        let ev = p.poll(Duration::from_millis(20));
        let (n, t) = p.read_pcm(&mut pcm);
        if n > 0 {
            first_pcm_pts = first_pcm_pts.or(t);
            pcm_frames += n;
        }
        let Some(ev) = ev else { continue };
        match ev {
            PlayerEvent::Sandboxed(s) => println!("sandboxed {s}"),
            PlayerEvent::VideoTrack { width, height, bit_depth } => println!("video-track {width}x{height} {bit_depth}-bit"),
            PlayerEvent::AudioTrack { rate, channels } => println!("audio-track {rate} Hz {channels} ch"),
            PlayerEvent::Duration(d) => println!("duration {d:.3}"),
            PlayerEvent::Frame { .. } => {
                frames += 1;
                if Some(frames) == kill_after {
                    if let Some(pid) = p.worker_pid() {
                        println!("killing worker {pid}");
                    }
                    p.kill_worker();
                }
                if let (Some(s), false) = (seek, seeked) {
                    seeked = true;
                    println!("seek {s}");
                    p.seek((s * 1e9) as i64);
                }
            }
            PlayerEvent::Appended { track } => {
                if let Some((t, b, at)) = pending.iter_mut().find(|(t, _, _)| *t == track) {
                    let n = (b.len() - *at).min(64 << 10);
                    if n > 0 {
                        p.append(*t, b[*at..*at + n].to_vec(), None);
                        *at += n;
                    }
                }
                if !pending.is_empty() && pending.iter().all(|(_, b, at)| *at == b.len()) {
                    pending.clear();
                    p.end_of_stream();
                }
            }
            PlayerEvent::Buffered { track, ranges } => {
                let r: Vec<String> = ranges.iter().map(|(a, b)| format!("{:.3}-{:.3}", *a as f64 / 1e9, *b as f64 / 1e9)).collect();
                println!("buffered {track} {}", r.join(" "));
            }
            PlayerEvent::Probe { refused } => println!("probe open-refused={} connect-refused={} fork-refused={}", refused & 1 != 0, refused & 2 != 0, refused & 4 != 0),
            PlayerEvent::Restarted => println!("worker restarted"),
            PlayerEvent::AppendRefused { track, code } => println!("append on track {track} refused ({code})"),
            PlayerEvent::Ended => {
                loop {
                    let (n, _) = p.read_pcm(&mut pcm);
                    if n == 0 {
                        break;
                    }
                    pcm_frames += n;
                }
                println!("audio {pcm_frames} frames from {}", first_pcm_pts.unwrap_or(0));
                println!("ended {frames} frames");
                return;
            }
            PlayerEvent::Error(code) => {
                println!("error {code}");
                std::process::exit(3);
            }
        }
    }
}

/// Play `file` in a window of its own: the video in a Fresco layer under a
/// window that is transparent but for the controls.
fn windowed(file: &str, exit_at_end: bool, size: (u32, u32)) {
    let f = if fetch::is_url(file) {
        None
    } else {
        Some(File::open(file).unwrap_or_else(|e| {
            eprintln!("{file}: {e}");
            std::process::exit(1)
        }))
    };
    let mut conn = fresco_client::Connection::connect_default().unwrap_or_else(|e| {
        eprintln!("atrium-player: connecting to frescod: {e}");
        std::process::exit(1)
    });
    let title = std::path::Path::new(file.split(['?', '#']).next().unwrap_or(file)).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let win = conn.window_create(size.0, size.1, title.clone(), fresco_protocol::WindowHints::default()).unwrap_or_else(|e| {
        eprintln!("atrium-player: window: {e}");
        std::process::exit(1)
    });
    conn.set_default_window(win as u16);
    let Some(f) = f else {
        let mut app = App::new(UrlHost::new(start_fetcher(file)), conn, win, size, Options::default());
        app.set_exit_at_end(exit_at_end);
        app.session_name = title.clone();
        run(&mut app);
        if let Some(why) = app.host.failed.take() {
            eprintln!("{file}: {why}");
            std::process::exit(1);
        }
        return;
    };
    let mut app = App::new(Standalone, conn, win, size, Options::default());
    app.session_name = title;
    if let Err(e) = app.play_file(f, exit_at_end) {
        eprintln!("atrium-player: starting the decode worker: {e}");
        std::process::exit(1);
    }
    run(&mut app);
}

/// The fetcher, asked for `url`: the system's (portcullisd, `net-fetch`)
/// when the player may have one, else the helper (fetch.rs).
fn start_fetcher(url: &str) -> Fetcher {
    let want = std::env::var("ATRIUM_PLAYER_FETCHER").unwrap_or_default();
    if want == "system" || (want != "helper" && portcullis_reachable()) {
        match Fetcher::system(url) {
            Ok(f) => return f,
            Err(e) if want == "system" => {
                eprintln!("atrium-player: the system fetcher: {e}");
                std::process::exit(1)
            }
            Err(e) => eprintln!("atrium-player: no system fetcher ({e}); using atrium-player-fetch"),
        }
    }
    start_helper(url)
}

fn portcullis_reachable() -> bool {
    ["/atrium/sockets/portcullis/portcullis.sock", "/atrium/sockets/portcullis.sock"].iter().any(|p| std::path::Path::new(p).exists())
}

/// The helper process (`atrium-player-fetch`), asked for `url`.
fn start_helper(url: &str) -> Fetcher {
    let mut extra = Vec::new();
    if std::env::var("ATRIUM_PLAYER_FETCH_UNCONFINED").as_deref() == Ok("1") {
        extra.push("--unconfined-for-testing".to_string());
    }
    if let Ok(t) = std::env::var("ATRIUM_PLAYER_TRUST") {
        extra.extend(["--trust".to_string(), t]);
    }
    Fetcher::spawn(&fetch::default_fetcher(), url, &extra).unwrap_or_else(|e| {
        eprintln!("atrium-player: starting the fetcher: {e}");
        std::process::exit(1)
    })
}

/// A Limen `media-player` child (limen.md §2.2): limend started us with its
/// channel on stdin and an embed-only frescod connection on it.
fn embedded() {
    use limen::proto::ChildEvent;
    let (mut child, fresco) = limen::client::Child::embedded().unwrap_or_else(|e| {
        eprintln!("atrium-player: no limend channel: {e:?}");
        std::process::exit(1)
    });
    let embed = loop {
        match child.poll(Duration::from_secs(30)) {
            Ok(Some(ChildEvent::Attach { embed, role, .. })) if role == "media-player" => break embed,
            Ok(Some(ChildEvent::Attach { role, .. })) => {
                eprintln!("atrium-player: asked to be {role}");
                std::process::exit(1)
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                eprintln!("atrium-player: limend never attached us");
                std::process::exit(1)
            }
        }
    };
    let mut conn = fresco_client::Connection::wrap(fresco).unwrap_or_else(|e| {
        eprintln!("atrium-player: frescod: {e}");
        std::process::exit(1)
    });
    let hints = fresco_protocol::WindowHints { embed_pending: true, server_decorations: Some(false), ..Default::default() };
    let mut size = (640u32, 360u32);
    let win = conn.window_create(size.0, size.1, "media", hints).unwrap_or_else(|e| {
        eprintln!("atrium-player: window: {e}");
        std::process::exit(1)
    });
    conn.set_default_window(win as u16);
    let until = Instant::now() + Duration::from_secs(10);
    let token = loop {
        match conn.wait_event(Some(Duration::from_millis(100))) {
            Ok(Some(fresco_client::Event::EmbedToken { token, .. })) => break token,
            Ok(Some(fresco_client::Event::Resized { width, height, .. })) => size = (width, height),
            Err(e) => {
                eprintln!("atrium-player: frescod: {e}");
                std::process::exit(1)
            }
            _ if Instant::now() > until => {
                eprintln!("atrium-player: no embed token from frescod");
                std::process::exit(1)
            }
            _ => {}
        }
    };
    if child.ready(embed, token).is_err() {
        std::process::exit(1);
    }
    let mut app = App::new(LimenHost { child, embed, dropped: 0, ticket: None }, conn, win, size, Options::default());
    run(&mut app);
    if app.host.dropped > 0 {
        eprintln!("atrium-player: dropped {} malformed messages", app.host.dropped);
    }
}

fn run<H: atrium_player::app::Host>(app: &mut App<H>) {
    while !app.done {
        if let Err(e) = app.step() {
            eprintln!("atrium-player: frescod: {e}");
            std::process::exit(1);
        }
    }
    let frames = app.frames;
    let s = app.sink().stats.clone();
    let mean = if s.presented > 0 { s.skew_sum / s.presented as i64 } else { 0 };
    println!("frames {frames} presented {} dropped {} skew mean {:.2} ms max {:.2} ms", s.presented, s.dropped, mean as f64 / 1e6, s.skew_max as f64 / 1e6);
}
