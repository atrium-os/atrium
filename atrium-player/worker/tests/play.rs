//! The supervisor and the worker together, as separate processes, against
//! ffmpeg's reference hashes of what belongs in the video layer.
use std::cell::RefCell;
use std::fs::File;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use atrium_player::supervisor::{LocalSink, Options, Player, PlayerEvent, Source, Subtitles};
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
        // A refused append is answered by that alone: no Appended comes to
        // send the next piece on.
        let done = matches!(ev, PlayerEvent::Ended | PlayerEvent::Error(_) | PlayerEvent::AppendRefused { .. });
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
    assert_eq!(r.events.last(), Some(&PlayerEvent::AppendRefused { track: 0, code: err::TOO_BIG }), "{:?}", r.events);
    // Each accepted append: its Buffered, then its Appended.
    let order: Vec<_> = r.events.iter().filter(|e| matches!(e, PlayerEvent::Appended { .. } | PlayerEvent::Buffered { .. })).collect();
    assert!(order.len() >= 2 && order.chunks(2).all(|p| p.len() < 2 || matches!((p[0], p[1]), (PlayerEvent::Buffered { .. }, PlayerEvent::Appended { .. }))), "{order:?}");
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
        assert_eq!(refused & 3, 3, "open and connect refused");
        // RLIMIT_NPROC stops fork, but root is exempt from it; the jail
        // never runs the worker as root.
        #[allow(unsafe_code)]
        // SAFETY: plain syscall.
        let root = unsafe { libc::geteuid() } == 0;
        assert_eq!(refused & 4 != 0, !root, "fork refused unless root");
    } else {
        assert_eq!(refused & 1, 0, "unsandboxed can open");
    }
}

/// Streaming: a seek past what the page has appended waits, however long
/// the network takes (past the hung-worker watchdog), then plays from the
/// target once the segment arrives; a seek back into what is buffered plays
/// at once.
#[test]
fn a_feed_seek_waits_for_its_segment() {
    let name = "vp9-opus.webm";
    let f = std::fs::read(asset(name)).unwrap();
    let clusters: Vec<usize> = f.windows(4).enumerate().filter(|(_, w)| *w == [0x1F, 0x43, 0xB6, 0x75]).map(|(i, _)| i).collect();
    assert!(clusters.len() >= 2, "{} clusters", clusters.len());
    let refs = layer_refs(name);
    let frames = Rc::new(RefCell::new(Vec::new()));
    let f2 = frames.clone();
    let sink = LocalSink::new(Box::new(move |b, pts| f2.borrow_mut().push((pts, format!("{:x}", Md5::digest(b))))));
    let mut p = Player::new(opts(), sink, Source::Feed(vec![(0, "video/webm".into())])).unwrap();
    let pid = p.worker_pid();
    let mut pcm = vec![0f32; 1 << 16];
    let mut errors = Vec::new();
    let mut pump = |p: &mut Player<LocalSink>, d: Duration| {
        let until = Instant::now() + d;
        while Instant::now() < until {
            if let Some(PlayerEvent::Error(e)) = p.poll(Duration::from_millis(10)) {
                errors.push(e);
            }
            p.read_pcm(&mut pcm);
        }
    };
    // The page has the init segment and the first cluster.
    p.append(0, f[..clusters[1]].to_vec(), None);
    pump(&mut p, Duration::from_millis(800));
    assert!(!frames.borrow().is_empty(), "plays what it has");
    // Seek ahead of it; the next segment takes longer than the watchdog.
    p.seek(1_500_000_000);
    let n0 = frames.borrow().len();
    pump(&mut p, crate_watchdog() + Duration::from_millis(1500));
    assert_eq!(frames.borrow().len(), n0, "nothing shown while the target isn't there");
    assert_eq!(p.worker_pid(), pid, "the worker is not taken for hung");
    // It arrives: playback starts at the target.
    p.append(0, f[clusters[1]..].to_vec(), None);
    pump(&mut p, Duration::from_millis(1500));
    let after: Vec<_> = frames.borrow()[n0..].to_vec();
    assert!(!after.is_empty(), "plays once the segment is there");
    assert_eq!(after[0].1, refs[36], "from the target's frame ({} ns)", after[0].0);
    // Back into what is buffered: at once.
    p.seek(200_000_000);
    let n1 = frames.borrow().len();
    pump(&mut p, Duration::from_millis(800));
    let back = frames.borrow()[n1..].to_vec();
    assert!(!back.is_empty(), "plays after seeking back");
    assert_eq!(back[0].1, refs[5], "from the frame at 0.2 s ({} ns)", back[0].0);
    // The page evicts the first second, then the user seeks back into it:
    // nothing from later on is shown while it is fetched again.
    pump(&mut p, Duration::from_millis(300));
    p.remove(0, 0, 1_000_000_000);
    p.seek(500_000_000);
    let n2 = frames.borrow().len();
    pump(&mut p, Duration::from_millis(1000));
    assert_eq!(frames.borrow().len(), n2, "nothing shown before the evicted part is back");
    p.append(0, f[clusters[0]..clusters[1]].to_vec(), None);
    pump(&mut p, Duration::from_millis(1000));
    let again = frames.borrow()[n2..].to_vec();
    assert!(!again.is_empty(), "plays once it is back ({errors:?})");
    assert_eq!(again[0].1, refs[12], "from the frame at 0.5 s ({} ns)", again[0].0);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(p.worker_pid(), pid);
}

fn crate_watchdog() -> Duration {
    atrium_player::supervisor::WATCHDOG
}

/// The worker reports the colour the bitstream and the container give, and
/// leaves unsaid what neither does.
#[test]
fn the_worker_reports_colour() {
    for (name, want) in [("color-hdr10.webm", (9, 16, 9, 1)), ("color-full.mp4", (1, 1, 1, 2)), ("vp8-vorbis.webm", (2, 2, 2, 0))] {
        let mut p = Player::new(opts(), LocalSink::new(Box::new(|_, _| {})), load(name)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pcm = vec![0f32; 1 << 16];
        while p_sink_color(&p).is_none() && Instant::now() < deadline {
            p.poll(Duration::from_millis(10));
            p.read_pcm(&mut pcm);
        }
        let c = p_sink_color(&p).expect("a colour record");
        assert_eq!((c.primaries, c.transfer, c.matrix, c.range), want, "{name}");
    }
}

fn p_sink_color(p: &Player<LocalSink>) -> Option<atrium_media_demux::Color> {
    p.sink().color
}

/// Subtitles in the file reach the supervisor as plain cues over their times.
#[test]
fn subtitle_cues_show_over_their_times() {
    for name in ["subs-vtt.webm", "subs-tx3g.mp4"] {
        let mut p = Player::new(opts(), LocalSink::new(Box::new(|_, _| {})), load(name)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pcm = vec![0f32; 1 << 16];
        while p.cues_at(1_200_000_000).is_empty() && Instant::now() < deadline {
            p.poll(Duration::from_millis(10));
            p.read_pcm(&mut pcm);
        }
        assert_eq!(p.cues_at(100_000_000), Vec::<&str>::new(), "{name}: before the first");
        assert_eq!(p.cues_at(500_000_000), vec!["Hello, world"], "{name}: markup stripped");
        assert_eq!(p.cues_at(950_000_000), Vec::<&str>::new(), "{name}: between");
        assert_eq!(p.cues_at(1_200_000_000), vec!["Second line\ntwo rows"], "{name}: two lines");
        assert_eq!(p.cues_at(1_900_000_000), Vec::<&str>::new(), "{name}: after the last");
    }
}

#[test]
fn a_growing_file_waits_at_its_end_until_load_done() {
    use std::io::Write;
    let name = "vp9-opus.webm";
    let f = std::fs::read(asset(name)).unwrap();
    let clusters: Vec<usize> = f.windows(4).enumerate().filter(|(_, w)| *w == [0x1F, 0x43, 0xB6, 0x75]).map(|(i, _)| i).collect();
    // A download cut mid-cluster, mid-block.
    let cut = clusters[1] + 777;
    let path = std::env::temp_dir().join(format!("atrium-growing-{}.webm", std::process::id()));
    let mut out = File::create(&path).unwrap();
    out.write_all(&f[..cut]).unwrap();
    let frames = Rc::new(RefCell::new(Vec::new()));
    let f2 = frames.clone();
    let sink = LocalSink::new(Box::new(move |b, pts| f2.borrow_mut().push((pts, format!("{:x}", Md5::digest(b))))));
    let mut p = Player::new_growing(opts(), sink, File::open(&path).unwrap()).unwrap();
    let pid = p.worker_pid();
    let mut pcm = vec![0f32; 1 << 16];
    let mut events = Vec::new();
    let mut pump = |p: &mut Player<LocalSink>, d: Duration, events: &mut Vec<PlayerEvent>| {
        let until = Instant::now() + d;
        while Instant::now() < until {
            if let Some(e) = p.poll(Duration::from_millis(10)) {
                let end = matches!(e, PlayerEvent::Ended);
                events.push(e);
                if end {
                    break;
                }
            }
            p.read_pcm(&mut pcm);
        }
    };
    // Plays what has arrived, then waits past the watchdog for more.
    pump(&mut p, crate_watchdog() + Duration::from_millis(2000), &mut events);
    let n0 = frames.borrow().len();
    assert!(n0 > 0, "plays what it has");
    assert!(!events.iter().any(|e| matches!(e, PlayerEvent::Ended | PlayerEvent::Error(_))), "{events:?}");
    assert_eq!(p.worker_pid(), pid, "a worker waiting for bytes is not taken for hung");
    // A seek back into what has arrived plays from there.
    p.seek(200_000_000);
    pump(&mut p, Duration::from_millis(800), &mut events);
    let back = frames.borrow()[n0..].to_vec();
    assert!(!back.is_empty(), "plays after seeking back");
    assert_eq!(back[0].1, layer_refs(name)[5], "from the frame at 0.2 s");
    // Then to the start, so the whole clip plays once more below.
    p.seek(0);
    pump(&mut p, Duration::from_millis(200), &mut events);
    frames.borrow_mut().clear();
    p.seek(0);
    // The rest arrives, in two pieces, and the download finishes.
    let mid = cut + (f.len() - cut) / 2;
    out.write_all(&f[cut..mid]).unwrap();
    pump(&mut p, Duration::from_millis(300), &mut events);
    out.write_all(&f[mid..]).unwrap();
    pump(&mut p, Duration::from_millis(300), &mut events);
    assert!(!events.iter().any(|e| matches!(e, PlayerEvent::Ended)), "no end before LoadDone");
    p.input_done();
    pump(&mut p, Duration::from_secs(15), &mut events);
    assert!(matches!(events.last(), Some(PlayerEvent::Ended)), "ends after LoadDone: {:?}", events.last());
    let hashes: Vec<_> = frames.borrow().iter().map(|f| f.1.clone()).collect();
    assert_eq!(hashes, layer_refs(name), "every frame, once, in order");
    assert_eq!(p.worker_pid(), pid);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn page_subtitles_show_when_chosen() {
    // The media has its own cues; the page adds a file of its own.
    let mut p = Player::new(opts(), LocalSink::new(Box::new(|_, _| {})), load("subs-vtt.webm")).unwrap();
    let pid = p.worker_pid();
    p.add_text_file(2, File::open(asset("subs.vtt")).unwrap()).unwrap();
    let fr = std::env::temp_dir().join(format!("atrium-subs-fr-{}.vtt", std::process::id()));
    std::fs::write(&fr, "WEBVTT\n\nNOTE French\n\n1\n00:00.300 --> 00:00.800\nBonjour &amp; <b>salut</b>\n").unwrap();
    p.add_text_file(5, File::open(&fr).unwrap()).unwrap();
    assert!(p.add_text_file(6, File::open("/dev/null").unwrap()).is_err(), "only a regular file");
    let mut pcm = vec![0f32; 1 << 16];
    let deadline = Instant::now() + Duration::from_secs(10);
    let have = |p: &mut Player<LocalSink>, s: Subtitles, t: i64| {
        p.set_subtitles(s);
        !p.cues_at(t).is_empty()
    };
    while !(have(&mut p, Subtitles::InBand, 1_200_000_000) && have(&mut p, Subtitles::Page(2), 1_200_000_000) && have(&mut p, Subtitles::Page(5), 500_000_000)) {
        assert!(Instant::now() < deadline, "cues arrive");
        p.poll(Duration::from_millis(10));
        p.read_pcm(&mut pcm);
    }
    p.set_subtitles(Subtitles::Page(5));
    assert_eq!(p.cues_at(500_000_000), vec!["Bonjour & salut"]);
    assert_eq!(p.cues_at(1_200_000_000), Vec::<&str>::new(), "only the chosen file's cues");
    p.set_subtitles(Subtitles::Page(2));
    assert_eq!(p.cues_at(500_000_000), vec!["Hello, world"]);
    assert_eq!(p.cues_at(1_200_000_000), vec!["Second line\ntwo rows"]);
    p.set_subtitles(Subtitles::None);
    assert_eq!(p.cues_at(500_000_000), Vec::<&str>::new());
    // A new file under an id replaces the old one's cues.
    std::fs::write(&fr, "WEBVTT\n\n00:01.000 --> 00:01.500\nAu revoir\n").unwrap();
    p.add_text_file(5, File::open(&fr).unwrap()).unwrap();
    p.set_subtitles(Subtitles::Page(5));
    let deadline = Instant::now() + Duration::from_secs(5);
    while p.cues_at(1_200_000_000).is_empty() {
        assert!(Instant::now() < deadline, "the new file's cues arrive");
        p.poll(Duration::from_millis(10));
        p.read_pcm(&mut pcm);
    }
    assert_eq!(p.cues_at(500_000_000), Vec::<&str>::new(), "the old file's cue is gone");
    assert_eq!(p.cues_at(1_200_000_000), vec!["Au revoir"]);
    assert_eq!(p.worker_pid(), pid);
    let _ = std::fs::remove_file(&fr);
}

/// H.264 Baseline through hospes-codec (Cisco's OpenH264 under Hospes), against ffmpeg's
/// own decode. Needs `ATRIUM_PLAYER_OPENH264` (the user-installed library) and
/// `ATRIUM_PLAYER_HOSPES_CODEC` (a built hospes-codec: `scripts/hospes-build.sh codec`);
/// skipped without them.
#[cfg(feature = "h264")]
fn h264_opts() -> Option<Options> {
    let library: PathBuf = std::env::var_os("ATRIUM_PLAYER_OPENH264")?.into();
    let codec: PathBuf = std::env::var_os("ATRIUM_PLAYER_HOSPES_CODEC")?.into();
    Some(Options { h264: Some(atrium_player::supervisor::H264 { codec, library, mem_mib: 256 }), ..opts() })
}

#[cfg(feature = "h264")]
#[test]
fn h264_through_hospes_matches_ffmpeg() {
    let Some(o) = h264_opts() else {
        eprintln!("skipped: set ATRIUM_PLAYER_OPENH264 and ATRIUM_PLAYER_HOSPES_CODEC");
        return;
    };
    let name = "h264-baseline.mp4";
    let r = play(load(name), o.clone(), 0, &[], |_, _, _| {});
    assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{name}: {:?}", r.events);
    let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
    assert_eq!(hashes, layer_refs(name), "{name}: layer contents");
    assert!((95_000..=98_000).contains(&r.pcm), "{name}: {} PCM frames", r.pcm);

    // A killed worker (and with it its codec) is replaced, and the H.264 track goes on.
    let mut killed = false;
    let r = play(load(name), o, 0, &[], |p, ev, n| {
        if !killed && n >= 10 && matches!(ev, PlayerEvent::Frame { .. }) {
            killed = true;
            p.kill_worker();
        }
    });
    assert!(killed);
    assert!(r.events.contains(&PlayerEvent::Restarted), "{:?}", r.events);
    assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{:?}", r.events.last());
}

/// Clear Key media through hospes-cdm and the test CDM (hospes.md §11.3): decrypt-only, our own
/// decoders after. Needs `ATRIUM_PLAYER_HOSPES_CDM` (`scripts/hospes-build.sh cdm TRIPLE`);
/// the test CDM is the one in the repository for this machine. Skipped without the host.
const CLEARKEY: &str = "00112233445566778899aabbccddeeff:3a2b1c0d4e5f60718293a4b5c6d7e8f9";

fn cdm_opts(keys: &str) -> Option<Options> {
    let host: PathBuf = std::env::var_os("ATRIUM_PLAYER_HOSPES_CDM")?.into();
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    let library = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../hospes/test-cdm/bin/{arch}-clearkey.so"));
    let licences = atrium_player::cdm::Licences::ClearKey(atrium_player::cdm::parse_keys(keys).unwrap());
    Some(Options { cdm: Some(atrium_player::cdm::CdmOptions { host, library, store: None, licences, mem_mib: 256, key_system: "org.w3.clearkey".into(), system_origin: None }), ..opts() })
}

/// (encrypted clip, its clear source).
const ENCRYPTED: [(&str, &str); 3] =
    [("clearkey/vp9-opus-cenc.mp4", "vp9-opus-frag.mp4"), ("clearkey/av1-opus-cbcs.mp4", "av1-opus.mp4"), ("clearkey/vp9-opus-enc.webm", "vp9-opus.webm")];

#[test]
fn clear_key_through_hospes_matches_the_clear_source() {
    let Some(o) = cdm_opts(CLEARKEY) else {
        eprintln!("skipped: set ATRIUM_PLAYER_HOSPES_CDM");
        return;
    };
    for (enc, clear) in ENCRYPTED {
        let r = play(load(enc), o.clone(), 0, &[], |_, _, _| {});
        assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{enc}: {:?}", r.events.last());
        let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
        assert_eq!(hashes, layer_refs(clear), "{enc}: layer contents");
        assert!((95_000..=98_000).contains(&r.pcm), "{enc}: {} PCM frames", r.pcm);
        eprintln!("{enc}: {} frames equal {clear}'s, {} PCM frames", hashes.len(), r.pcm);
    }
}

#[test]
fn clear_key_feed_in_small_appends() {
    let Some(o) = cdm_opts(CLEARKEY) else { return };
    for (enc, clear) in [ENCRYPTED[0], ENCRYPTED[2]] {
        let mime = if enc.ends_with("mp4") { "video/mp4" } else { "video/webm" };
        let r = play(Source::Feed(vec![(0, mime.into())]), o.clone(), 7000, &[enc], |_, _, _| {});
        assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{enc}: {:?}", r.events.last());
        let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
        assert_eq!(hashes, layer_refs(clear), "{enc}");
    }
}

/// A killed worker takes its CDM with it; the new pair licenses again and playback goes on.
#[test]
fn clear_key_survives_a_worker_restart() {
    let Some(o) = cdm_opts(CLEARKEY) else { return };
    let (enc, clear) = ENCRYPTED[2];
    let r = play(load(enc), o, 0, &[], |p, ev, n| {
        if matches!(ev, PlayerEvent::Frame { .. }) && n == 10 {
            p.kill_worker();
        }
    });
    assert!(r.events.contains(&PlayerEvent::Restarted));
    assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{:?}", r.events.last());
    let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
    assert_eq!(hashes, layer_refs(clear), "no frame lost or repeated");
}

/// A CDM host that dies alone takes its worker with it, and the pair is replaced.
#[test]
fn clear_key_survives_a_cdm_crash() {
    let Some(o) = cdm_opts(CLEARKEY) else { return };
    let (enc, clear) = ENCRYPTED[0];
    let r = play(load(enc), o, 0, &[], |p, ev, n| {
        if matches!(ev, PlayerEvent::Frame { .. }) && n == 10 {
            let pid = p.cdm_pid().unwrap();
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    });
    assert!(r.events.contains(&PlayerEvent::Restarted), "{:?}", r.events);
    assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{:?}", r.events.last());
    let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
    assert_eq!(hashes, layer_refs(clear), "no frame lost or repeated");
}

/// No key for the media's key id: an error at once, not a hang until the watchdog.
#[test]
fn clear_key_without_the_key_is_an_error() {
    let Some(o) = cdm_opts("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf:0f1e2d3c4b5a69788796a5b4c3d2e1f0") else { return };
    for (enc, _) in ENCRYPTED {
        let t = Instant::now();
        let r = play(load(enc), o.clone(), 0, &[], |_, _, _| {});
        assert_eq!(r.events.last(), Some(&PlayerEvent::Error(err::UNSUPPORTED)), "{enc}");
        assert!(r.frames.is_empty(), "{enc}");
        assert!(t.elapsed() < Duration::from_secs(3), "{enc}: took {:?}", t.elapsed());
    }
}

/// Without a CDM, encrypted media is refused (the decoders never see ciphertext).
#[test]
fn encrypted_media_without_a_cdm_is_refused() {
    for (enc, _) in ENCRYPTED {
        let r = play(load(enc), Options { cdm: None, ..opts() }, 0, &[], |_, _, _| {});
        assert_eq!(r.events.last(), Some(&PlayerEvent::Error(err::UNSUPPORTED)), "{enc}: {:?}", r.events);
        assert!(r.frames.is_empty(), "{enc}");
    }
}

/// portcullisd, then a Nuntius that answers one licence POST as a Clear Key licence server
/// with `keys` would (H4: the licence route through the system fetcher, hospes.md §8.5).
fn licence_fetcher(keys: &'static str, posts: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> std::io::Result<nuntius::launch::Fetcher> {
    use nuntius::{server, Event, Method, Request};
    use portcullis_ipc::{read_request, recv_fds, write_response, Response};
    use std::os::unix::net::UnixStream;
    let (app, d) = UnixStream::pair()?;
    std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(d.try_clone().unwrap());
        let mut w = d;
        read_request(&mut r).unwrap();
        write_response(&mut w, &Response::Hello { version: portcullis_ipc::PROTO_VERSION }).unwrap();
        read_request(&mut r).unwrap();
        write_response(&mut w, &Response::ReadyForFds).unwrap();
        let fds = recv_fds(&w, 3).unwrap();
        let (mut reqs, mut out) = server::channel(UnixStream::from(fds.into_iter().next().unwrap())).unwrap();
        let Some(Request::Open(o)) = reqs.recv().unwrap() else { panic!("an Open first") };
        assert_eq!(o.method, Method::Post);
        assert_eq!(o.url, "https://licence.test/clearkey");
        let body = o.body.expect("a body").bytes;
        posts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let keys = atrium_player::cdm::parse_keys(keys).unwrap();
        let lic = atrium_player::cdm::request_kids(&body).and_then(|k| atrium_player::cdm::clear_key_licence(&keys, &k)).unwrap();
        let id = o.id;
        out.send(Event::Head { id, status: 200, final_url: o.url.clone(), redirects: 0, content_type: Some("application/json".into()),
                               length: Some(lic.len() as u64), accept_ranges: false, validator: None }).unwrap();
        out.send(Event::Data { id, offset: 0, bytes: lic.clone() }).unwrap();
        out.send(Event::Done { id, bytes: lic.len() as u64 }).unwrap();
        while let Ok(Some(_)) = reqs.recv() {}
        let _ = w;
    });
    nuntius::launch::open_at(app)
}

#[test]
fn clear_key_from_a_licence_server_through_the_system_fetcher() {
    let Some(mut o) = cdm_opts(CLEARKEY) else { return };
    let posts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let p2 = posts.clone();
    o.cdm.as_mut().unwrap().licences = atrium_player::cdm::Licences::Server {
        url: "https://licence.test/clearkey".into(),
        fetcher: atrium_player::cdm::FetcherSource::new(move || licence_fetcher(CLEARKEY, p2.clone())),
    };
    let (enc, clear) = ENCRYPTED[0];
    let r = play(load(enc), o, 0, &[], |_, _, _| {});
    assert!(matches!(r.events.last(), Some(PlayerEvent::Ended)), "{:?}", r.events.last());
    let hashes: Vec<_> = r.frames.iter().map(|f| f.1.clone()).collect();
    assert_eq!(hashes, layer_refs(clear));
    assert!(posts.load(std::sync::atomic::Ordering::SeqCst) >= 1, "the licence came by POST");
}
