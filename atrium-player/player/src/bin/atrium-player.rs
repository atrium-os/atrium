//! atrium-player. With `--headless` it plays a file or a segment feed
//! without a window and prints what it would show. That is how M3 is
//! tested before Fresco's video layers and Limen's embedding (M4, M5).
//!
//!   atrium-player --headless FILE            # Load: the worker reads FILE
//!   atrium-player --headless --feed FILE...  # one MSE track per FILE, appended in chunks
//!   options: --seek SECS (after the first frame), --probe-sandbox,
//!            --kill-worker-after N (frames)
use std::fs::File;
use std::time::Duration;

use atrium_player::supervisor::{LocalSink, Options, Player, PlayerEvent, Source};
use md5::{Digest, Md5};

fn usage() -> ! {
    eprintln!("usage: atrium-player --headless [--feed] [--seek SECS] [--probe-sandbox] [--kill-worker-after N] FILE...");
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut headless, mut feed, mut seek, mut probe, mut kill_after) = (false, false, None, false, None);
    let mut files = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--headless" => headless = true,
            "--feed" => feed = true,
            "--seek" => seek = args.next().and_then(|v| v.parse::<f64>().ok()),
            "--probe-sandbox" => probe = true,
            "--kill-worker-after" => kill_after = args.next().and_then(|v| v.parse::<usize>().ok()),
            "-h" | "--help" => usage(),
            _ => files.push(a),
        }
    }
    if !headless || files.is_empty() {
        // The windowed player (Limen child, Fresco layer) is M4 and M5.
        usage();
    }
    let sink = LocalSink::new(Box::new(|bytes, pts| println!("video {pts} {:x}", Md5::digest(bytes))));
    let source = if feed {
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
    let mut p = match Player::new(Options::default(), sink, source) {
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
