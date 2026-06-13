//! lyrad — the Lyra audio graph engine daemon (skeleton, phase L2).
//!
//! Brings up the deadline-broker side end to end against the modeled path: open
//! `/dev/laminar`, build the canonical consumer graph (`source → mix → sink`),
//! admit it (the proven planner), and report the per-node lane reservations.
//! The device-feed loop (sponsoring real node threads against a real or HVF DAC,
//! the in-VM gate) is the next slice — this binary proves the planner + broker
//! wiring compile and run, and degrades cleanly where the lane is absent.

use lyra::graph::consumer_graph;
use lyra::lane::LaneBroker;
use lyra::oss::OssSink;

const RATE: u32 = 48_000;
const FRAMES_PER_PERIOD: u32 = 128; // ≈ 2.667 ms

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--feed") {
        feed_mode(&args);
        return;
    }
    let tone = args.iter().any(|a| a == "--tone");

    // 48 kHz, 128-frame buffer ≈ 2667 µs period; a 1 ms client budget.
    let period_us = (FRAMES_PER_PERIOD as u64 * 1_000_000) / RATE as u64;
    let graph = consumer_graph(period_us, 1000);
    let reservations = match graph.admit() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lyrad: graph not admissible: {e:?}");
            std::process::exit(1);
        }
    };
    println!("lyrad: consumer graph admitted ({} nodes, T = {period_us} us)", reservations.len());
    for r in &reservations {
        println!("  node {} : Q = {} us, deadline +{} us", r.node, r.q_us, r.deadline_offset_us);
    }

    let broker = match LaneBroker::open() {
        Ok(b) => {
            b.set_anchor_now();
            eprintln!("lyrad: deadline broker up on /dev/laminar");
            Some(b)
        }
        Err(e) => {
            eprintln!("lyrad: no deadline lane ({e}); running without");
            None
        }
    };

    // L2-b first light: open the OSS sink and play a 1 s 440 Hz tone, reporting
    // the hardware clock (played_frames) advancing. This is the device feed the
    // sink node will run on; the lane-sponsored per-period loop is the next step.
    if tone {
        match OssSink::open(RATE, 2, FRAMES_PER_PERIOD) {
            Ok(sink) => {
                eprintln!("lyrad: OSS sink open ({} Hz, {} ch)", sink.rate_hz(), sink.channels());
                let mut buf = vec![0i16; (FRAMES_PER_PERIOD * 2) as usize];
                let periods = RATE / FRAMES_PER_PERIOD; // ~1 s
                let mut phase = 0.0f32;
                let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
                for _ in 0..periods {
                    for f in 0..FRAMES_PER_PERIOD as usize {
                        let s = (phase.sin() * 8000.0) as i16; // ~ -12 dBFS
                        buf[f * 2] = s;
                        buf[f * 2 + 1] = s;
                        phase += step;
                    }
                    if let Err(e) = sink.write_i16(&buf) {
                        eprintln!("lyrad: write: {e}");
                        break;
                    }
                }
                match sink.played_frames() {
                    Ok(n) => eprintln!("lyrad: tone done; hw clock = {n} frames consumed"),
                    Err(e) => eprintln!("lyrad: played_frames: {e}"),
                }
            }
            Err(e) => eprintln!("lyrad: no OSS sink ({e}); skipping tone"),
        }
    }

    if let Some(b) = broker {
        b.withdraw_all();
    }
}

/// `lyrad --feed <secs> <spinners> [lane]` — the L2-c gate. Continuously feed a
/// tone to OSS under spinner load; with `lane`, self-sponsor the feed thread so
/// it is scheduled promptly on every device wakeup. Reports the device's own
/// underrun count. The headline: under load, the lane holds underruns at ~0
/// where plain timeshare does not — the metronome result, on real hardware.
fn feed_mode(args: &[String]) {
    let secs: u64 = args.iter().position(|a| a == "--feed").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(10);
    let spinners: usize = args.iter().position(|a| a == "--feed").and_then(|i| args.get(i + 2)).and_then(|s| s.parse().ok()).unwrap_or(16);
    let use_lane = args.iter().any(|a| a == "lane");

    // fork spinner load.
    let mut kids = Vec::new();
    for _ in 0..spinners {
        match unsafe { libc::fork() } {
            0 => {
                let mut acc = 0u64;
                loop {
                    acc = acc.wrapping_add(1);
                    std::hint::black_box(acc);
                }
            }
            pid if pid > 0 => kids.push(pid),
            _ => {}
        }
    }

    let sink = match OssSink::open(RATE, 2, FRAMES_PER_PERIOD) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lyrad: no OSS sink ({e})");
            for pid in kids { unsafe { libc::kill(pid, libc::SIGKILL); } }
            std::process::exit(1);
        }
    };

    // self-sponsor the feed thread (this thread) for band-priority prompt wakes.
    let _lane = if use_lane {
        match lyra::lane::self_sponsor(1000, period_us_for_feed()) {
            Ok(fd) => { eprintln!("lyrad: feed thread sponsored on the lane"); Some(fd) }
            Err(e) => { eprintln!("lyrad: self_sponsor failed ({e}); feeding without lane"); None }
        }
    } else {
        None
    };

    let mut buf = vec![0i16; (FRAMES_PER_PERIOD * 2) as usize];
    let mut phase = 0.0f32;
    let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
    let _ = sink.play_underruns(); // clear the counter
    let periods = (secs * RATE as u64) / FRAMES_PER_PERIOD as u64;
    for _ in 0..periods {
        for f in 0..FRAMES_PER_PERIOD as usize {
            let s = (phase.sin() * 8000.0) as i16;
            buf[f * 2] = s;
            buf[f * 2 + 1] = s;
            phase += step;
        }
        if sink.write_i16(&buf).is_err() {
            break;
        }
    }

    let underruns = sink.play_underruns().unwrap_or(u32::MAX);
    for pid in kids {
        unsafe { libc::kill(pid, libc::SIGKILL); libc::waitpid(pid, std::ptr::null_mut(), 0); }
    }
    println!(
        "lyrad feed: {secs}s, {spinners} spinners, lane={} => play_underruns={underruns}",
        use_lane
    );
    std::process::exit(if underruns == 0 { 0 } else { 1 });
}

fn period_us_for_feed() -> u64 {
    (FRAMES_PER_PERIOD as u64 * 1_000_000) / RATE as u64
}
