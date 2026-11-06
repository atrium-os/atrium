//! lyrad — the Lyra audio graph engine daemon.
//!
//! `lyrad --serve` is the service (lyra-v1.md §3, `lyra::server`): it mixes the
//! streams choragusd opens and plays them to the OSS device until stopped. The
//! other modes are the measurement and bring-up tools below.
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
    if args.iter().any(|a| a == "--calibrate") {
        calibrate_mode(&args);
        return;
    }
    if args.iter().any(|a| a == "--effect") {
        effect_mode(&args);
        return;
    }
    if args.iter().any(|a| a == "--serve" || a == "--control") {
        serve_mode(&args);
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
        match OssSink::open(RATE, 2, FRAMES_PER_PERIOD, 8) {
            Ok(sink) => {
                eprintln!("lyrad: OSS sink open ({} Hz, {} ch)", sink.rate_hz(), sink.channels());
                let mut buf = vec![0i16; (FRAMES_PER_PERIOD * 2) as usize];
                let periods = RATE / FRAMES_PER_PERIOD; // ~1 s
                let mut phase = 0.0f32;
                let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
                // a short raised-cosine fade in/out kills the start/stop clicks.
                let fade = (FRAMES_PER_PERIOD * 2) as usize; // ~5 ms ramp
                for p in 0..periods {
                    let base = (p * FRAMES_PER_PERIOD) as usize;
                    for f in 0..FRAMES_PER_PERIOD as usize {
                        let i = base + f;
                        let total = (periods * FRAMES_PER_PERIOD) as usize;
                        let g = if i < fade {
                            i as f32 / fade as f32
                        } else if i >= total - fade {
                            (total - i) as f32 / fade as f32
                        } else {
                            1.0
                        };
                        let s = (phase.sin() * 8000.0 * g) as i16; // ~ -12 dBFS
                        buf[f * 2] = s;
                        buf[f * 2 + 1] = s;
                        phase += step;
                    }
                    if let Err(e) = sink.write_i16(&buf) {
                        eprintln!("lyrad: write: {e}");
                        break;
                    }
                }
                let _ = sink.drain(); // play the buffer out before closing
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
/// Run ONE glitch measurement: prime a `nfrags`-deep OSS buffer, (optionally)
/// sponsor the feed thread on the deadline lane, fork `spinners` CPU hogs, feed
/// a 440 Hz tone for `secs`, and return the steady-state `play_underruns`.
///
/// The spinner lifecycle is owned entirely here: this function forks the hogs
/// AND SIGKILL+reaps every one of them before returning. Because it is
/// synchronous and self-contained, a caller can loop it (e.g. calibration)
/// without any risk of orphaned spinners accumulating across trials — the
/// failure mode that a shell-loop driver has when a child hangs.
fn run_feed_trial(secs: u64, spinners: usize, use_lane: bool, nfrags: u32, verbose: bool) -> u32 {
    let sink = match OssSink::open(RATE, 2, FRAMES_PER_PERIOD, nfrags) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lyrad: no OSS sink ({e})");
            std::process::exit(1);
        }
    };
    // Order matters: prime the buffer BEFORE any load exists — otherwise the
    // feed thread fights the spinners with an empty buffer and an un-banded
    // priority, which is the start-time crackle.
    let _lane = if use_lane {
        match lyra::lane::self_sponsor(1000, period_us_for_feed()) {
            Ok(fd) => { if verbose { eprintln!("lyrad: feed thread sponsored on the lane"); } Some(fd) }
            Err(e) => { if verbose { eprintln!("lyrad: self_sponsor failed ({e}); feeding without lane"); } None }
        }
    } else {
        None
    };

    let mut buf = vec![0i16; (FRAMES_PER_PERIOD * 2) as usize];
    let mut phase = 0.0f32;
    let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
    let fill = |buf: &mut [i16], phase: &mut f32| {
        for f in 0..FRAMES_PER_PERIOD as usize {
            let s = (phase.sin() * 8000.0) as i16;
            buf[f * 2] = s;
            buf[f * 2 + 1] = s;
            *phase += step;
        }
    };
    // prime the (small) buffer while there is no contention.
    for _ in 0..nfrags {
        fill(&mut buf, &mut phase);
        let _ = sink.write_i16(&buf);
    }

    // NOW start the load — buffer primed, thread banded.
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

    let _ = sink.play_underruns(); // clear the counter; measure steady state
    let periods = (secs * RATE as u64) / FRAMES_PER_PERIOD as u64;
    for _ in 0..periods {
        fill(&mut buf, &mut phase);
        if sink.write_i16(&buf).is_err() {
            break;
        }
    }

    // fade the last fragment to zero so the tone stops at silence (no end click),
    // then drain so closing the fd does not truncate the buffer.
    for f in 0..FRAMES_PER_PERIOD as usize {
        let g = 1.0 - (f as f32 / FRAMES_PER_PERIOD as f32);
        let s = (phase.sin() * 8000.0 * g) as i16;
        buf[f * 2] = s;
        buf[f * 2 + 1] = s;
        phase += step;
    }
    let _ = sink.write_i16(&buf);

    let underruns = sink.play_underruns().unwrap_or(u32::MAX);
    let _ = sink.drain();
    for pid in kids {
        unsafe { libc::kill(pid, libc::SIGKILL); libc::waitpid(pid, std::ptr::null_mut(), 0); }
    }
    underruns
}

fn feed_mode(args: &[String]) {
    let secs: u64 = args.iter().position(|a| a == "--feed").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(10);
    let spinners: usize = args.iter().position(|a| a == "--feed").and_then(|i| args.get(i + 2)).and_then(|s| s.parse().ok()).unwrap_or(16);
    let use_lane = args.iter().any(|a| a == "lane");
    let nfrags: u32 = std::env::var("LYRA_NFRAGS").ok().and_then(|s| s.parse().ok()).unwrap_or(6);

    let underruns = run_feed_trial(secs, spinners, use_lane, nfrags, true);
    println!("lyrad feed: {secs}s, {spinners} spinners, lane={use_lane} => play_underruns={underruns}");
    std::process::exit(if underruns == 0 { 0 } else { 1 });
}

/// Read a `u64` sysctl by name, or 0 if unavailable.
fn sysctl_u64(name: &str) -> u64 {
    std::process::Command::new("sysctl").arg("-n").arg(name).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// `lyrad --calibrate [spinners] [secs] [trials]` — find the per-platform
/// sweet-spot audio buffer for the deadline lane. Sweeps buffer depth under
/// load, reports the smallest depth that is glitch-free across all trials, and
/// cross-checks against the scheduler's measured worst-case wake latency. All
/// spinners are forked and reaped in-process (via run_feed_trial), so the sweep
/// can never leak load — the cascade a shell driver risks.
fn calibrate_mode(args: &[String]) {
    let i = args.iter().position(|a| a == "--calibrate").unwrap();
    let spinners: usize = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(16);
    let secs: u64 = args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let trials: u32 = args.get(i + 3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let period_us = period_us_for_feed();

    let _ = std::process::Command::new("sysctl").arg("kern.sched.deadline_enable=1").output();
    let _ = std::process::Command::new("sysctl").arg("kern.sched.wake_pick_max_us=0").output();

    println!("=== Laminar deadline-lane audio calibration ===");
    println!("period {period_us} us ({FRAMES_PER_PERIOD} frames @ {RATE} Hz); load {spinners} spinners; {trials} trials/step");
    println!("buffer sweep (smallest glitch-free depth wins):");

    let depths: [u32; 7] = [3, 4, 6, 8, 10, 12, 16];
    let mut best: Option<u32> = None;
    for &nf in &depths {
        let buf_ms = nf * FRAMES_PER_PERIOD * 1000 / RATE;
        let mut worst = 0u32;
        let mut runs = String::new();
        for _ in 0..trials {
            let u = run_feed_trial(secs, spinners, true, nf, false);
            worst = worst.max(u);
            runs.push_str(&format!(" {u}"));
        }
        let verdict = if worst == 0 {
            if best.is_none() { best = Some(nf); }
            "CLEAN".to_string()
        } else {
            format!("glitch (worst={worst})")
        };
        println!("  NFRAGS={nf:<2} {buf_ms:>3} ms  runs:{runs:<18}  {verdict}");
        if let Some(b) = best { if nf >= b + 4 { break; } }
    }

    // Diagnostics only. NOTE: wake_pick_max is the worst wake across ALL
    // threads, so it captures non-lane background threads that the lane does
    // NOT protect (and thus over-estimates the audio path's real latency — the
    // very reason the recommendation is empirical, not derived from this). A
    // trustworthy analytical floor needs a per-lane-entity wake-to-run metric,
    // which the scheduler does not yet export.
    let wake = sysctl_u64("kern.sched.wake_pick_max_us");
    let late = sysctl_u64("kern.sched.lane_max_late_us");
    println!("diagnostics: worst wake-to-run (all threads) {wake} us; worst replenish late {late} us");

    match best {
        Some(b) => {
            let rec = b + 2; // +2-fragment tail margin for rare events
            let rec_ms = rec * FRAMES_PER_PERIOD * 1000 / RATE;
            println!("RESULT: smallest glitch-free NFRAGS={b}; RECOMMENDED LYRA_NFRAGS={rec}  ({rec_ms} ms buffer, +2-frag margin)");
        }
        None => {
            println!("RESULT: no glitch-free depth <=16 (buffer < platform jitter);");
            println!("        reduce scheduling jitter (renice/pin the audio path) or raise the ceiling.");
        }
    }
}

fn period_us_for_feed() -> u64 {
    (FRAMES_PER_PERIOD as u64 * 1_000_000) / RATE as u64
}

/// `lyrad --effect <secs> [crash_at_frame]` — the L3 gate. Route a tone THROUGH
/// a separate effect process (lyra-effect, applying tremolo) via shared-memory
/// rings, out to OSS. With `crash_at_frame`, the effect aborts mid-stream; lyrad
/// detects the stall, **bypasses** the dead node, and the device keeps playing —
/// fault isolation a kernel mixer or in-process plugin cannot give (JACK's
/// one-xrun-kills-everyone, inverted).
fn effect_mode(args: &[String]) {
    use lyra::ring::Ring;
    let secs: u64 = args.iter().position(|a| a == "--effect").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(8);
    let crash_at: u64 = args.iter().position(|a| a == "--effect").and_then(|i| args.get(i + 2)).and_then(|s| s.parse().ok()).unwrap_or(0);

    let pid = unsafe { libc::getpid() };
    let dry_name = format!("/lyra_dry_{pid}");
    let wet_name = format!("/lyra_wet_{pid}");
    // 4096-frame rings (~85 ms) — generous headroom for the cross-process hop.
    let dry = Ring::create(&dry_name, 4096, 2).expect("dry ring");
    let wet = Ring::create(&wet_name, 4096, 2).expect("wet ring");

    // K-b: with LYRA_ADOPT, lyrad sponsors its OWN (this) thread as the graph
    // entity, then hands its (pid, tid) to the effect, which adopts it — so the
    // whole chain (generate -> effect -> output) runs on ONE reservation, charged
    // back. A heavy plugin eats lyrad's budget, not extra band. Sponsor BEFORE
    // spawning so the entity exists when the effect's first buffer triggers adopt.
    // Hold _lane_fd for lyrad's lifetime (closing it withdraws the sponsorship).
    let mut adopt_args: Option<(i32, i32)> = None;
    let _lane_fd = if std::env::var("LYRA_ADOPT").is_ok() {
        let t_us = (FRAMES_PER_PERIOD as u64 * 1_000_000) / RATE as u64;
        // budget = half the period (50% util, under deadline_util_max). One
        // reservation covers BOTH lyrad's feed and the adopted effect's DSP.
        match lyra::lane::self_sponsor(t_us / 2, t_us) {
            Ok(fd) => {
                adopt_args = Some((pid, lyra::lane::current_tid()));
                eprintln!("lyrad: graph entity sponsored (pid {pid} tid {})", lyra::lane::current_tid());
                Some(fd)
            }
            Err(e) => { eprintln!("lyrad: self_sponsor failed ({e}); effect runs un-adopted"); None }
        }
    } else {
        None
    };

    // spawn the effect node (sibling binary next to lyrad).
    let me = std::env::current_exe().expect("current_exe");
    let effect_bin = me.with_file_name("lyra-effect");
    let mut cmd = std::process::Command::new(&effect_bin);
    let trem = std::env::var("LYRA_TREMOLO").unwrap_or_else(|_| "0".into());
    cmd.arg(&dry_name).arg(&wet_name).arg(&trem); // 0 = passthrough
    if crash_at > 0 {
        cmd.arg("--crash-after").arg(crash_at.to_string());
    }
    // LYRA_PLUGIN=<path.so> hosts a real C node (node ABI) in the effect process;
    // LYRA_CAPSICUM=1 makes it Capsicum-self-confine (opt-in, distinct from a
    // forced FreeBSD/Portcullis jail).
    if let Ok(p) = std::env::var("LYRA_PLUGIN") {
        cmd.arg("--plugin").arg(p);
    }
    if std::env::var("LYRA_CAPSICUM").is_ok() {
        cmd.arg("--capsicum");
    }
    if let Some((p, t)) = adopt_args {
        cmd.arg("--adopt").arg(p.to_string()).arg(t.to_string());
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lyrad: spawn {effect_bin:?}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("lyrad: effect node spawned (pid {})", child.id());

    let sink = match OssSink::open(RATE, 2, FRAMES_PER_PERIOD, 8) {
        Ok(s) => s,
        Err(e) => { eprintln!("lyrad: no OSS sink ({e})"); let _ = child.kill(); std::process::exit(1); }
    };

    let per = FRAMES_PER_PERIOD as u64;
    let mut phase = 0.0f32;
    let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
    let mut out_buf = vec![0i16; (FRAMES_PER_PERIOD * 2) as usize];
    let mut dry_f = vec![0.0f32; (FRAMES_PER_PERIOD * 2) as usize];
    let mut wet_f = vec![0.0f32; (FRAMES_PER_PERIOD * 2) as usize];
    let gen = |phase: &mut f32, df: &mut [f32]| {
        for f in 0..FRAMES_PER_PERIOD as usize {
            let s = phase.sin() * 0.25; // ~ -12 dBFS as float
            df[f * 2] = s; df[f * 2 + 1] = s;
            *phase += step;
        }
    };
    // push exactly one period of dry into the ring, waiting for space. Returns
    // false if the effect died (ring stays full because nothing consumes it).
    let push_dry = |dry: &lyra::ring::Ring, df: &[f32], child: &mut std::process::Child| -> bool {
        let mut off = 0u64;
        let mut spins = 0;
        while off < per {
            let w = dry.write(&df[(off * 2) as usize..]);
            off += w;
            if w == 0 {
                spins += 1;
                if spins > 100 && matches!(child.try_wait(), Ok(Some(_))) { return false; }
                unsafe { libc::usleep(100) };
            } else { spins = 0; }
        }
        true
    };

    // PRIME the pipeline coherently: fill the dry ring, then wait for the effect
    // to produce a few periods of wet, so the FIRST sound out is processed audio
    // (no dry-then-wet phase jump). Emit ONLY wet in steady state.
    for _ in 0..8 {
        gen(&mut phase, &mut dry_f);
        if !push_dry(&dry, &dry_f, &mut child) { break; }
    }
    // wait (bounded) for wet to have headroom.
    for _ in 0..2000 {
        if wet.readable() >= 4 * per { break; }
        unsafe { libc::usleep(200) };
    }

    // optional ground-truth dump: the EXACT bytes lyrad emits, independent of
    // the (lossy) QEMU wav capture. Analyze this to verify the pipeline.
    use std::io::Write;
    let mut dump = std::env::var("LYRA_DUMP").ok().and_then(|p| std::fs::File::create(p).ok());

    let _ = sink.play_underruns();
    let periods = (secs * RATE as u64) / per;
    let mut bypassed = false;
    for _ in 0..periods {
        // keep the dry ring fed (one period per output period — balanced).
        gen(&mut phase, &mut dry_f);
        if !bypassed && !push_dry(&dry, &dry_f, &mut child) {
            bypassed = true;
            eprintln!("lyrad: effect node gone; BYPASSING -> dry to device");
        }

        if !bypassed {
            // pull exactly one period of WET (processed); spin until ready, but
            // bail to bypass if the effect has died.
            let mut spins = 0;
            loop {
                if wet.read(&mut wet_f) == per {
                    for i in 0..wet_f.len() { out_buf[i] = (wet_f[i] * 32767.0) as i16; }
                    break;
                }
                spins += 1;
                if spins > 100 && matches!(child.try_wait(), Ok(Some(_))) {
                    bypassed = true;
                    eprintln!("lyrad: effect node exited; BYPASSING -> dry to device");
                    break;
                }
                unsafe { libc::usleep(100) };
            }
        }
        if bypassed {
            // the node is gone: emit the (coherent) generated dry — audio lives.
            for i in 0..dry_f.len() { out_buf[i] = (dry_f[i] * 32767.0) as i16; }
        }
        if let Some(f) = dump.as_mut() {
            let bytes = unsafe { std::slice::from_raw_parts(out_buf.as_ptr() as *const u8, out_buf.len() * 2) };
            let _ = f.write_all(bytes);
        }
        if sink.write_i16(&out_buf).is_err() { break; }
    }

    let underruns = sink.play_underruns().unwrap_or(u32::MAX);
    let _ = sink.drain();
    let _ = child.kill();
    let _ = child.wait();
    drop(dry); drop(wet);
    println!(
        "lyrad effect: {secs}s through a separate node{} => play_underruns={underruns}",
        if crash_at > 0 { format!(", node crashed at {crash_at} frames -> bypassed") } else { String::new() }
    );
}

/// The demo synth frequency for a stream id (real streams are ring-fed; the
/// control-plane demo synthesises a distinct tone per id so the mix is audible
/// and spectrally measurable). id 0 → 440 Hz, id 1 → 660 Hz, id 2 → 880 Hz, …
/// `lyrad --serve`: the service (lyra-v1.md §3). Socket and trusted peers from
/// `ATRIUM_LYRAD_SOCK` / `ATRIUM_LYRAD_PEERS` (defaults
/// `/atrium/sockets/lyrad/lyrad.sock`, `_choragus`).
///
/// `lyrad --control <socket>`: the same service for development, on `socket`,
/// trusting only lyrad's own uid unless `ATRIUM_LYRAD_PEERS` says otherwise.
fn serve_mode(args: &[String]) {
    let mut cfg = match lyra::server::Config::from_env() {
        Ok(c) => c,
        Err(e) => { eprintln!("lyrad: {e}"); std::process::exit(1); }
    };
    if let Some(i) = args.iter().position(|a| a == "--control") {
        if let Some(sock) = args.get(i + 1) {
            cfg.socket = sock.into();
        }
        if std::env::var("ATRIUM_LYRAD_PEERS").is_err() {
            cfg.peers.clear(); // own uid only
        }
    }
    lyra::server::run(cfg)
}
