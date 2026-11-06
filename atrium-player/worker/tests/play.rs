//! The supervisor and the worker together, as separate processes, against
//! ffmpeg's reference hashes of what belongs in the video layer.
use std::cell::RefCell;
use std::fs::File;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use atrium_player::supervisor::{LocalSink, Options, Player, PlayerEvent, Source};
use atrium_player::wire::{err, Limits};
use md5::{Digest, Md5};

fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/media").join(name)
}

fn opts() -> Options {
    Options { worker: env!("CARGO_BIN_EXE_atrium-player-decode").into(), ..Options::default() }
}

fn layer_refs(name: &str) -> Vec<String> {
    std::fs::read_to_string(asset(&format!("{name}.layer.framemd5")))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}

#[derive(Default)]
struct Run {
    frames: Vec<(i64, String)>,
    events: Vec<PlayerEvent>,
    pcm: usize,
}

/// Play to the end (or an error), calling `each` on every event.
fn play(source: Source, o: Options, chunk: usize, files: &[&str], mut each: impl FnMut(&mut Player<LocalSink>, &PlayerEvent, usize)) -> Run {
    let frames = Rc::new(RefCell::new(Vec::new()));
    let f2 = frames.clone();
    let sink = LocalSink::new(Box::new(move |b, pts| f2.borrow_mut().push((pts, format!("{:x}", Md5::digest(b))))));
    let mut p = Player::new(o, sink, source).unwrap();
    let mut feeds: Vec<(u8, Vec<u8>, usize)> = files.iter().enumerate().map(|(i, f)| (i as u8, std::fs::read(asset(f)).unwrap(), 0)).collect();
    for (t, b, at) in feeds.iter_mut() {
        let n = (b.len() - *at).min(chunk);
        p.append(*t, b[*at..*at + n].to_vec(), None);
        *at += n;
    }
    let mut run = Run::default();
    let mut pcm = vec![0f32; 1 << 16];
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "no end");
        let ev = p.poll(Duration::from_millis(20));
        run.pcm += p.read_pcm(&mut pcm).0;
        let Some(ev) = ev else { continue };
        if let PlayerEvent::Appended { track } = ev {
            if let Some((t, b, at)) = feeds.iter_mut().find(|(t, _, _)| *t == track) {
                let n = (b.len() - *at).min(chunk);
                if n > 0 {
                    p.append(*t, b[*at..*at + n].to_vec(), None);
                    *at += n;
                }
            }
            if !feeds.is_empty() && feeds.iter().all(|(_, b, at)| *at == b.len()) {
                feeds.clear();
                p.end_of_stream();
            }
        }
        let n = frames.borrow().len();
        each(&mut p, &ev, n);
        let done = matches!(ev, PlayerEvent::Ended | PlayerEvent::Error(_));
        run.events.push(ev);
        if done {
            loop {
                let n = p.read_pcm(&mut pcm).0;
                if n == 0 {
                    break;
                }
                run.pcm += n;
            }
            break;
        }
    }
    run.frames = frames.borrow().clone();
    run
}

fn load(name: &str) -> Source {
    Source::File(File::open(asset(name)).unwrap())
}

const CLIPS: [&str; 6] = ["vp9-opus.webm", "av1-opus.webm", "vp8-vorbis.webm", "vp9-10bit.webm", "av1-opus.mp4", "vp9-opus-frag.mp4"];

#[test]
fn load_matches_ffmpeg() {
    for name in CLIPS {
        let r = play(load(name), opts(), 0, &[], |_, _, _| {});
        assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{name}: {:?}", r.events.last());
        let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
        assert_eq!(hashes, layer_refs(name), "{name}: layer contents");
        if !name.contains("10bit") {
            assert!((95_000..=98_000).contains(&r.pcm), "{name}: {} PCM frames", r.pcm);
        }
    }
}

#[test]
fn feed_matches_ffmpeg_in_small_appends() {
    for name in ["vp9-opus.webm", "av1-opus.webm", "vp9-opus-frag.mp4"] {
        let mime = if name.ends_with("mp4") { "video/mp4" } else { "video/webm" };
        let r = play(Source::Feed(vec![(0, mime.into())]), opts(), 7000, &[name], |_, _, _| {});
        assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{name}: {:?}", r.events.last());
        let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
        assert_eq!(hashes, layer_refs(name), "{name}");
        let last = r.events.iter().rev().find_map(|e| if let PlayerEvent::Buffered { ranges, .. } = e { Some(ranges.clone()) } else { None }).unwrap();
        assert_eq!(last.len(), 1, "{name}: {last:?}");
        assert!(last[0].0 <= 10_000_000 && last[0].1 >= 1_950_000_000, "{name}: buffered {last:?}");
    }
}

#[test]
fn seek_lands_on_the_target_frame() {
    for name in ["vp9-opus.webm", "av1-opus.mp4"] {
        let mut seeked = false;
        let r = play(load(name), opts(), 0, &[], |p, ev, _| {
            if matches!(ev, PlayerEvent::Frame { .. }) && !seeked {
                seeked = true;
                p.seek(1_500_000_000);
            }
        });
        let refs = layer_refs(name);
        // The frame before the seek, then from frame 36 (1.5 s at 24 fps) on.
        assert_eq!(r.frames[0].1, refs[0]);
        let after: Vec<_> = r.frames.iter().skip_while(|f| f.0 < 1_400_000_000).map(|f| f.1.clone()).collect();
        assert_eq!(after, refs[36..].to_vec(), "{name}: frames after the seek");
    }
}

#[test]
fn a_killed_worker_is_replaced_and_playback_continues() {
    let name = "vp9-opus.webm";
    let r = play(load(name), opts(), 0, &[], |p, ev, n| {
        if matches!(ev, PlayerEvent::Frame { .. }) && n == 10 {
            p.kill_worker();
        }
    });
    assert!(r.events.contains(&PlayerEvent::Restarted));
    assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)));
    let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
    assert_eq!(hashes, layer_refs(name), "no frame lost or repeated");
    assert!((95_000..=98_000).contains(&r.pcm), "{} PCM frames", r.pcm);
}

#[test]
fn a_feed_over_its_cap_is_refused() {
    let o = Options { limits: Limits { feed_bytes: 20_000, ..Limits::default() }, ..opts() };
    let r = play(Source::Feed(vec![(0, "video/webm".into())]), o, 16_000, &["vp9-opus.webm"], |_, _, _| {});
    assert!(r.events.contains(&PlayerEvent::Error(err::TOO_BIG)), "{:?}", r.events);
}

#[test]
fn hostile_input_ends_in_an_error_or_less_video() {
    let f = std::fs::read(asset("vp9-opus.webm")).unwrap();
    for (i, seed) in [(200usize, 0xFFu8), (4000, 0x00), (20_000, 0x5A), (40_000, 0x81)] {
        let mut g = f.clone();
        for k in 0..64 {
            let at = (i + k * 131) % g.len();
            g[at] ^= seed.wrapping_add(k as u8);
        }
        let path = std::env::temp_dir().join(format!("atrium-player-hostile-{i}.webm"));
        std::fs::write(&path, &g).unwrap();
        let r = play(Source::File(File::open(&path).unwrap()), opts(), 0, &[], |_, _, _| {});
        let _ = std::fs::remove_file(&path);
        assert!(matches!(r.events.last(), Some(PlayerEvent::Ended | PlayerEvent::Error(_))), "{i}: {:?}", r.events.last());
    }
    // Not media at all.
    let path = std::env::temp_dir().join("atrium-player-hostile-text.webm");
    std::fs::write(&path, b"this is not a video").unwrap();
    let r = play(Source::File(File::open(&path).unwrap()), opts(), 0, &[], |_, _, _| {});
    let _ = std::fs::remove_file(&path);
    assert!(matches!(r.events.last(), Some(PlayerEvent::Error(_))), "{:?}", r.events.last());
}

/// A worker that hangs is killed by the watchdog: restarted once, then the
/// feed fails.
#[test]
fn a_hung_worker_is_killed() {
    let script = std::env::temp_dir().join("atrium-player-hung-worker.sh");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 60\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = Options { worker: script.clone(), ..Options::default() };
    let t = Instant::now();
    let r = play(load("vp9-opus.webm"), o, 0, &[], |_, _, _| {});
    assert!(r.events.contains(&PlayerEvent::Restarted));
    assert_eq!(r.events.last(), Some(&PlayerEvent::Error(err::DECODE)));
    assert!(t.elapsed() < Duration::from_secs(20));
}

/// In capability mode the worker can't open, connect or fork. Only
/// FreeBSD has Capsicum; elsewhere the worker says it isn't sandboxed.
#[test]
fn the_worker_is_sandboxed() {
    let mut probed = None;
    let r = play(load("vp9-opus.webm"), opts(), 0, &[], |p, ev, _| {
        if let PlayerEvent::Sandboxed(_) = ev {
            p.probe_sandbox();
        }
        if let PlayerEvent::Probe { refused } = ev {
            probed = Some(*refused);
        }
    });
    let sandboxed = r.events.iter().find_map(|e| if let PlayerEvent::Sandboxed(s) = e { Some(*s) } else { None }).unwrap();
    assert_eq!(sandboxed, cfg!(target_os = "freebsd"));
    let refused = probed.expect("probe answered");
    if cfg!(target_os = "freebsd") {
        assert_eq!(refused, 7, "open, connect and fork all refused");
    } else {
        assert_eq!(refused & 1, 0, "unsandboxed can open");
    }
}
