//! lyra-effect — a Lyra graph effect node as a separate process
//! (`docs/spec/atrium-lyra-architecture.md` §6, phase L3).
//!
//! The audio analog of moving the kernel's `feeder_eq.c` out of the kernel and
//! into a sandboxed userspace node: a standalone process that reads its input
//! edge (a shared-memory [`Ring`]), processes one buffer at a time, and writes
//! its output edge. Because it is a separate process, a crash or a runaway here
//! is isolated — lyrad detects the stall and bypasses it, and the device never
//! underruns (the property a kernel mixer or an in-process plugin cannot give).
//!
//! This binary is the *node*; lyrad creates the rings, spawns it, and connects
//! it into the graph. A real plugin would (a) run in a Portcullis jail with the
//! `deadline_broker`-granted reservation and (b) implement the C node ABI; this
//! is the process-isolation skeleton those layer onto.
//!
//! usage: lyra-effect <in_ring> <out_ring> [tremolo_hz]
//! `--crash-after <n>` exits after processing n buffers (the isolation gate).

use lyra::ring::Ring;

const CH: usize = 2;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <in_ring> <out_ring> [tremolo_hz] [--crash-after N]", args[0]);
        std::process::exit(2);
    }
    let in_name = &args[1];
    let out_name = &args[2];
    let trem_hz: f32 = args.get(3).filter(|s| !s.starts_with("--")).and_then(|s| s.parse().ok()).unwrap_or(6.0);
    let crash_after: u64 = args.iter().position(|a| a == "--crash-after")
        .and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(u64::MAX);

    let inr = Ring::open(in_name, false).expect("open in ring");
    let outr = Ring::open(out_name, true).expect("open out ring");

    // a tremolo: amplitude modulation by a slow LFO — audibly obvious that the
    // node is in the path, and that bypass (dry) is different from processed.
    let rate = 48_000.0f32;
    let mut lfo = 0.0f32;
    let lfo_step = 2.0 * std::f32::consts::PI * trem_hz / rate;

    let mut buf = vec![0.0f32; 256 * CH];
    let mut processed: u64 = 0;
    loop {
        let n = inr.read(&mut buf) as usize;
        if n == 0 {
            std::hint::spin_loop();
            unsafe { libc::usleep(200) };
            continue;
        }
        for f in 0..n {
            // tremolo when trem_hz > 0; otherwise passthrough (unity gain) so
            // the output is bit-exactly the input — a clean pipeline check.
            let g = if trem_hz > 0.0 { 0.5 + 0.5 * lfo.sin() } else { 1.0 };
            buf[f * CH] *= g;
            buf[f * CH + 1] *= g;
            lfo += lfo_step;
        }
        // write the processed buffer out, waiting for space.
        let mut off = 0usize;
        while off < n {
            let w = outr.write(&buf[off * CH..n * CH]) as usize;
            if w == 0 {
                unsafe { libc::usleep(200) };
            }
            off += w;
        }
        processed += n as u64;
        if processed >= crash_after {
            // the isolation gate: abort mid-stream. lyrad must bypass us.
            eprintln!("lyra-effect: crashing after {processed} frames (deliberate)");
            std::process::abort();
        }
    }
}
