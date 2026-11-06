//! Sound through Lyra (M6) against a stand-in choragusd that plays the
//! stream's ring in real time and publishes the clock page as lyrad would:
//! every sample decoded reaches it, in order; pause stops it; a seek
//! flushes and the next run starts at the target; the audio clock runs at
//! real time.
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atrium_player::audio_out::AudioOut;
use atrium_player::supervisor::{LocalSink, Options, Player, PlayerEvent, Source};
use lyra_protocol::clock::{self, Clock, STATE_PAUSED, STATE_PLAYING};
use lyra_protocol::ring::Ring;
use lyra_protocol::wire::{self, ToApp, ToChoragus};

fn asset(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-assets/media").join(name)
}

fn opts() -> Options {
    Options { worker: env!("CARGO_BIN_EXE_atrium-player-decode").into(), ..Options::default() }
}

fn now_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: one timespec we own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

const LATENCY_NS: i64 = 5_000_000;

#[derive(Default)]
struct Heard {
    samples: Vec<f32>,
    /// (when, frames consumed so far)
    log: Vec<(Instant, u64)>,
    flushes: u32,
    volume: Option<f32>,
    starts: u32,
    /// (stream frame, index into `samples`) at each read.
    marks: Vec<(u64, usize)>,
}

/// One app, one stream; the ring is drained at the stream's rate while it
/// plays, and the clock page says when each frame is heard.
fn fake_choragus(tag: &str) -> (String, Arc<Mutex<Heard>>) {
    let path = std::env::temp_dir().join(format!("ap-chor-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path).unwrap();
    let heard = Arc::new(Mutex::new(Heard::default()));
    let h = heard.clone();
    let tag = tag.to_string();
    std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let Ok(ToChoragus::Hello { .. }) = wire::recv::<_, ToChoragus>(&mut c) else { return };
        wire::send(&mut c, &ToApp::Welcome { device_rate: 48_000, device_channels: 2, period_frames: 480 }).unwrap();
        let mut stream: Option<(Ring, Ring, clock::ClockWriter, u32, usize)> = None;
        let mut playing = false;
        let mut last = Instant::now();
        loop {
            // A message, if one has started to arrive.
            let mut b = [0u8; 1];
            // SAFETY: a one-byte peek into a local buffer.
            let n = unsafe { libc::recv(c.as_raw_fd(), b.as_mut_ptr() as *mut _, 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
            if n == 0 {
                return;
            }
            if n > 0 {
                let Ok(m) = wire::recv::<_, ToChoragus>(&mut c) else { return };
                match m {
                    ToChoragus::Open { tag: t, params } => {
                        let name = format!("/ap-chor-{tag}-{}", std::process::id());
                        let frames = params.ring_frames();
                        let prod = Ring::create(&name, frames, params.channels as u64).unwrap();
                        let cons = Ring::open(&name, false).unwrap();
                        let fd = std::fs::OpenOptions::new().read(true).write(true).open(format!("/dev/shm{name}")).unwrap();
                        wire::send(&mut c, &ToApp::Opened { tag: t, stream: 1, ring_frames: frames, latency_ns: LATENCY_NS as u64 }).unwrap();
                        lyra_protocol::fdpass::send_fd(c.as_raw_fd(), fd.as_raw_fd()).unwrap();
                        let (w, cfd) = clock::create().unwrap();
                        w.publish(&Clock { state: STATE_PAUSED, frame: 0, heard_ns: now_ns(), rate_ppm: 0, flush_epoch: 0, rate: params.rate });
                        lyra_protocol::fdpass::send_fd(c.as_raw_fd(), cfd.as_raw_fd()).unwrap();
                        stream = Some((prod, cons, w, params.rate, params.channels as usize));
                    }
                    ToChoragus::Start { .. } => {
                        playing = true;
                        last = Instant::now();
                        h.lock().unwrap().starts += 1;
                    }
                    ToChoragus::Pause { .. } => playing = false,
                    ToChoragus::Flush { stream: s } => {
                        if let Some((_, cons, ..)) = stream.as_ref() {
                            // A slow answer, as from a busy choragusd.
                            std::thread::sleep(Duration::from_millis(150));
                            let pos = cons.discard();
                            h.lock().unwrap().flushes += 1;
                            wire::send(&mut c, &ToApp::Flushed { stream: s, frame: pos }).unwrap();
                        }
                    }
                    ToChoragus::SetVolume { gain, .. } => h.lock().unwrap().volume = Some(gain),
                    _ => {}
                }
                continue;
            }
            if let Some((_, cons, w, rate, ch)) = stream.as_ref() {
                if playing {
                    // Play what real time says has gone by.
                    let due = (last.elapsed().as_secs_f64() * *rate as f64) as usize;
                    if due > 0 {
                        last = Instant::now();
                        let mut out = vec![0f32; due * ch];
                        let at = cons.consumed();
                        let got = cons.read(&mut out) as usize * ch;
                        let mut g = h.lock().unwrap();
                        let len = g.samples.len();
                        g.marks.push((at, len));
                        g.samples.extend_from_slice(&out[..got]);
                        g.log.push((Instant::now(), cons.consumed()));
                    }
                    w.publish(&Clock { state: STATE_PLAYING, frame: cons.consumed(), heard_ns: now_ns() + LATENCY_NS, rate_ppm: 0, flush_epoch: 0, rate: *rate });
                } else {
                    w.publish(&Clock { state: STATE_PAUSED, frame: cons.consumed(), heard_ns: now_ns() + LATENCY_NS, rate_ppm: 0, flush_epoch: 0, rate: *rate });
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    (path.to_string_lossy().into_owned(), heard)
}

/// Every PCM sample of `clip`, read straight from the worker.
fn reference_pcm(clip: &str) -> Vec<f32> {
    let mut p = Player::new(opts(), LocalSink::new(Box::new(|_, _| {})), Source::File(File::open(asset(clip)).unwrap())).unwrap();
    let mut all = Vec::new();
    let mut buf = vec![0f32; 1 << 16];
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline);
        let ev = p.poll(Duration::from_millis(5));
        let (n, _) = p.read_pcm(&mut buf);
        all.extend_from_slice(&buf[..n]);
        if ev == Some(PlayerEvent::Ended) {
            loop {
                let (n, _) = p.read_pcm(&mut buf);
                if n == 0 {
                    return all;
                }
                all.extend_from_slice(&buf[..n]);
            }
        }
    }
}

struct Session {
    p: Player<LocalSink>,
    a: Option<AudioOut>,
    sock: String,
    ended: bool,
}

impl Session {
    fn new(clip: &str, sock: String) -> Self {
        let p = Player::new(opts(), LocalSink::new(Box::new(|_, _| {})), Source::File(File::open(asset(clip)).unwrap())).unwrap();
        Session { p, a: None, sock, ended: false }
    }
    /// Run for `d`, or until the audio has all been heard.
    fn run(&mut self, d: Duration, heard: &Arc<Mutex<Heard>>, total: usize) {
        let until = Instant::now() + d;
        while Instant::now() < until {
            while let Some(ev) = self.p.poll(Duration::from_millis(2)) {
                match ev {
                    PlayerEvent::AudioTrack { rate, channels } => {
                        let mut a = AudioOut::open(&self.sock, None, rate, channels).unwrap();
                        a.set_playing(true);
                        self.a = Some(a);
                    }
                    PlayerEvent::Ended => self.ended = true,
                    _ => {}
                }
            }
            if let Some(a) = self.a.as_mut() {
                assert!(a.pump(&mut self.p, self.ended), "Lyra stays");
            }
            if total > 0 && heard.lock().unwrap().samples.len() >= total {
                return;
            }
            std::thread::sleep(Duration::from_millis(3));
        }
    }
}

#[test]
fn every_sample_is_heard_in_order_and_the_clock_runs_at_real_time() {
    let want = reference_pcm("vp9-opus.webm");
    let (sock, heard) = fake_choragus("all");
    let mut s = Session::new("vp9-opus.webm", sock);
    // A second in: the audio clock maps media time to now.
    s.run(Duration::from_millis(1000), &heard, 0);
    let a = s.a.as_ref().expect("a stream");
    let (t0, p0) = (now_ns(), a.position_ns(now_ns()).expect("playing"));

    s.run(Duration::from_millis(500), &heard, 0);
    let t1 = now_ns();


    let p1 = s.a.as_ref().unwrap().position_ns(now_ns()).unwrap();
    let (ran, wall) = ((p1 - p0) as f64 / 1e6, (t1 - t0) as f64 / 1e6);
    assert!((ran - wall).abs() < 30.0, "the media clock ran {ran:.1} ms in {wall:.1} ms");
    s.run(Duration::from_secs(10), &heard, want.len());
    let got = heard.lock().unwrap().samples.clone();
    assert_eq!(got.len(), want.len(), "every frame heard");
    assert!(got == want, "and the same samples, in order");
    assert_eq!(heard.lock().unwrap().starts, 1);
}

#[test]
fn pause_stops_the_stream_and_a_seek_flushes_and_starts_at_the_target() {
    let (sock, heard) = fake_choragus("seek");
    let mut s = Session::new("vp9-opus.webm", sock);
    s.run(Duration::from_millis(500), &heard, 0);
    s.a.as_mut().unwrap().set_playing(false);
    s.run(Duration::from_millis(100), &heard, 0);
    let n0 = heard.lock().unwrap().samples.len();
    s.run(Duration::from_millis(400), &heard, 0);
    assert_eq!(heard.lock().unwrap().samples.len(), n0, "nothing heard while paused");
    s.a.as_mut().unwrap().set_volume(0.5);

    // Seek to 1.5 s (paused), then play: the run starts at the target.
    s.p.seek(1_500_000_000);
    s.a.as_mut().unwrap().seek(1_500_000_000);
    s.a.as_mut().unwrap().set_playing(true);
    let before = heard.lock().unwrap().samples.len();
    s.run(Duration::from_secs(5), &heard, 0);
    let a = s.a.as_ref().unwrap();
    let (_, media0) = a.anchor().expect("a run after the seek");
    assert!((1_500_000_000..1_500_000_000 + 1_000_000).contains(&media0), "starts at the target, pre-roll trimmed ({media0})");
    let g = heard.lock().unwrap();
    assert_eq!(g.flushes, 1);
    assert_eq!(g.volume, Some(0.5));
    let after = g.samples.len() - before;
    // The clip's audio is 96648 frames; from 1.5 s on that is 24648, give
    // or take WebM's millisecond timestamps.
    assert!((24_600..=24_700).contains(&after), "{after} frames after the seek");
}

#[test]
fn a_faster_rate_plays_shorter_and_the_clock_runs_faster() {
    let want = reference_pcm("vp9-opus.webm");
    let (sock, heard) = fake_choragus("rate");
    let mut s = Session::new("vp9-opus.webm", sock);
    let mut first = true;
    let until = Instant::now() + Duration::from_millis(800);
    while Instant::now() < until {
        s.run(Duration::from_millis(10), &heard, 0);
        if first && s.a.is_some() {
            first = false;
            s.a.as_mut().unwrap().set_rate(1.5);
        }
    }
    let a = s.a.as_ref().expect("a stream");
    let (t0, p0) = (now_ns(), a.position_ns(now_ns()).expect("playing"));
    s.run(Duration::from_millis(500), &heard, 0);
    let (t1, p1) = (now_ns(), s.a.as_ref().unwrap().position_ns(now_ns()).unwrap());
    let (ran, wall) = ((p1 - p0) as f64 / 1e6, (t1 - t0) as f64 / 1e6);
    assert!((ran - 1.5 * wall).abs() < 40.0, "the media clock ran {ran:.1} ms in {wall:.1} ms at 1.5x");
    // The whole clip, a third shorter (WSOLA keeps its last segment).
    let short = (want.len() as f64 / 1.5) as usize;
    s.run(Duration::from_secs(8), &heard, short - 48_000 / 10);
    let got = heard.lock().unwrap().samples.len();
    assert!((got as f64 - short as f64).abs() < 0.02 * short as f64 + 4_000.0, "{got} samples heard, want about {short}");
}

#[test]
fn quick_seeks_one_after_another_end_with_sound_and_clock_at_the_last_target() {
    let want = reference_pcm("vp9-opus.webm");
    let (sock, heard) = fake_choragus("quick");
    let mut s = Session::new("vp9-opus.webm", sock);
    s.run(Duration::from_millis(500), &heard, 0);
    // Far right, let a little play, then straight back, then once more.
    for (to, ms) in [(1_800_000_000i64, 200u64), (300_000_000, 0), (900_000_000, 200), (500_000_000, 0)] {
        s.p.seek(to);
        s.a.as_mut().unwrap().seek(to);
        s.run(Duration::from_millis(ms), &heard, 0);
    }
    s.run(Duration::from_secs(5), &heard, 0);
    let a = s.a.as_ref().unwrap();
    let (frame0, media0) = a.anchor().expect("a run after the seeks");
    assert!((500_000_000..501_000_000).contains(&media0), "the run starts at the last target ({media0})");
    // What is heard from the anchor's frame on is the clip from the target.
    let g = heard.lock().unwrap();
    let &(f, i) = g.marks.iter().rev().find(|(f, _)| *f <= frame0).expect("heard from the anchor");
    // (The clip is mono.)
    let got = &g.samples[i + (frame0 - f) as usize..];
    let start = (media0 as i128 * 48_000 / 1_000_000_000) as usize;
    // Within WebM's millisecond timestamps of the target.
    assert!(got.len().abs_diff(want.len() - start) <= 48, "the rest of the clip, once: {} frames, want {}", got.len(), want.len() - start);
    assert!(got == &want[want.len() - got.len()..], "the stream frame the clock anchors starts the rest of the clip");
}
