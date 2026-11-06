//! Play a clip's sound through Lyra, no window: the M6 check on real lyrad.
//!
//!   atrium-audio-play SOCKET FILE [RATE]
//!
//! Prints how long the sound took, how far the audio clock says it got,
//! and the underruns lyrad reported.
use std::fs::File;
use std::time::{Duration, Instant};

use atrium_player::audio_out::AudioOut;
use atrium_player::supervisor::{LocalSink, Options, Player, PlayerEvent, Source};

fn now_ns() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: one timespec we own.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 3 {
        eprintln!("usage: atrium-audio-play SOCKET FILE [RATE]");
        std::process::exit(2);
    }
    let rate: f64 = a.get(3).and_then(|r| r.parse().ok()).unwrap_or(1.0);
    let mut opts = Options::default();
    if let Ok(w) = std::env::var("ATRIUM_PLAYER_WORKER") {
        opts.worker = w.into();
    }
    let mut p = Player::new(opts, LocalSink::new(Box::new(|_, _| {})), Source::File(File::open(&a[2]).expect("open"))).expect("player");
    let mut out: Option<AudioOut> = None;
    let (mut ended, mut t0, mut first) = (false, Instant::now(), None);
    let mut last = Instant::now();
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        assert!(Instant::now() < deadline, "timed out");
        while let Some(ev) = p.poll(Duration::from_millis(2)) {
            match ev {
                PlayerEvent::AudioTrack { rate: hz, channels } => {
                    let mut o = AudioOut::open(&a[1], None, hz, channels).expect("Lyra");
                    o.set_rate(rate);
                    o.set_playing(true);
                    out = Some(o);
                    t0 = Instant::now();
                }
                PlayerEvent::Ended => ended = true,
                _ => {}
            }
        }
        let Some(o) = out.as_mut() else { continue };
        assert!(o.pump(&mut p, ended), "Lyra went away");
        if std::env::var_os("AP_DEBUG").is_some() && last.elapsed() > Duration::from_secs(1) {
            last = Instant::now();
            let c = o.stream_clock();
            eprintln!("ended {ended} written {} clock frame {} state {} pos {:?}", o.written_frames(), c.frame, c.state, o.position_ns(now_ns()));
        }
        if let Some(pos) = o.position_ns(now_ns()) {
            first.get_or_insert((now_ns(), pos));
            if o.drained() {
                let (t1, p1) = (now_ns(), pos);
                let (tf, pf) = first.unwrap();
                println!("played {:.3} s of media in {:.3} s (clock {:.3} s of media per s), {} underrun frames, total {:.3} s",
                    (p1 - pf) as f64 / 1e9, (t1 - tf) as f64 / 1e9, (p1 - pf) as f64 / (t1 - tf) as f64, o.underruns, t0.elapsed().as_secs_f64());
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(3));
    }
}
