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

fn main() {
    // 48 kHz, 128-frame buffer ≈ 2667 µs period; a 1 ms client budget.
    let period_us = 2667;
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
        println!(
            "  node {} : Q = {} us, deadline +{} us within the period",
            r.node, r.q_us, r.deadline_offset_us
        );
    }

    match LaneBroker::open() {
        Ok(broker) => {
            broker.set_anchor_now();
            eprintln!("lyrad: deadline broker up on /dev/laminar");
            // device-feed loop (sponsor node threads, drain misses) lands next.
            let _ = broker;
        }
        Err(e) => {
            eprintln!("lyrad: no deadline lane ({e}); planner verified, running without");
        }
    }
}
