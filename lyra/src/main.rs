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
    let tone = std::env::args().any(|a| a == "--tone");

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
