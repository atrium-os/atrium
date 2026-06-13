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
    if args.iter().any(|a| a == "--effect") {
        effect_mode(&args);
        return;
    }
    if args.iter().any(|a| a == "--control") {
        control_mode(&args);
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
fn feed_mode(args: &[String]) {
    let secs: u64 = args.iter().position(|a| a == "--feed").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(10);
    let spinners: usize = args.iter().position(|a| a == "--feed").and_then(|i| args.get(i + 2)).and_then(|s| s.parse().ok()).unwrap_or(16);
    let use_lane = args.iter().any(|a| a == "lane");

    // Order matters: open the sink, take the band, and PRIME the buffer BEFORE
    // any load exists — otherwise the feed thread fights the spinners with an
    // empty buffer and an un-banded priority, which is the start-time crackle.
    let nfrags: u32 = std::env::var("LYRA_NFRAGS").ok().and_then(|s| s.parse().ok()).unwrap_or(6);
    let sink = match OssSink::open(RATE, 2, FRAMES_PER_PERIOD, nfrags) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lyrad: no OSS sink ({e})");
            std::process::exit(1);
        }
    };
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
    println!(
        "lyrad feed: {secs}s, {spinners} spinners, lane={} => play_underruns={underruns}",
        use_lane
    );
    std::process::exit(if underruns == 0 { 0 } else { 1 });
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

/// `lyrad --control <socket> [secs]` — play a 440 Hz tone and apply live control
/// changes (Aqueduct class 5) from choragusd over a Unix socket: a `SetGainDb`
/// ramps the tone's gain through the zipper-free smoother. The choragusd↔lyrad
/// wire — the policy layer decides, the engine applies.
fn control_mode(args: &[String]) {
    use lyra::gain::Gain;
    use lyra_protocol::Ctl;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    let idx = args.iter().position(|a| a == "--control").unwrap();
    let socket = args.get(idx + 1).cloned().unwrap_or_else(|| "/tmp/lyrad.ctl".into());
    let secs: u64 = args.get(idx + 2).and_then(|s| s.parse().ok()).unwrap_or(8);

    // shared linear-gain target (f32 bits), updated by the control thread.
    let target = Arc::new(AtomicU32::new(1.0f32.to_bits()));

    let _ = std::fs::remove_file(&socket);
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => { eprintln!("lyrad: bind {socket}: {e}"); std::process::exit(1); }
    };
    eprintln!("lyrad: control socket at {socket}");

    // control thread: accept connections, decode Ctl frames, update the target.
    {
        let target = Arc::clone(&target);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let mut conn = match conn { Ok(c) => c, Err(_) => continue };
                let mut frame = [0u8; lyra_protocol::FRAME_LEN];
                while conn.read_exact(&mut frame).is_ok() {
                    match Ctl::decode(&frame) {
                        Some(Ctl::SetGainDb { stream, db }) => {
                            let lin = 10f32.powf(db / 20.0);
                            target.store(lin.to_bits(), Ordering::Relaxed);
                            eprintln!("lyrad: ctl SetGainDb stream={stream} {db:+.1} dB (gain {lin:.3})");
                        }
                        Some(Ctl::Reroute { stream, sink }) => {
                            eprintln!("lyrad: ctl Reroute stream={stream} -> sink {sink} (noted; single-sink build)");
                        }
                        None => eprintln!("lyrad: bad control frame"),
                    }
                }
            }
        });
    }

    let sink = match OssSink::open(RATE, 2, FRAMES_PER_PERIOD, 8) {
        Ok(s) => s,
        Err(e) => { eprintln!("lyrad: no OSS sink ({e})"); let _ = std::fs::remove_file(&socket); std::process::exit(1); }
    };
    let mut gain = Gain::new(1.0, RATE as f32, 20.0); // 20 ms ramp = zipper-free
    let mut phase = 0.0f32;
    let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
    let mut buf = vec![0.0f32; (FRAMES_PER_PERIOD * 2) as usize];
    let mut out = vec![0i16; (FRAMES_PER_PERIOD * 2) as usize];
    let periods = (secs * RATE as u64) / FRAMES_PER_PERIOD as u64;
    for _ in 0..periods {
        gain.set_target(f32::from_bits(target.load(Ordering::Relaxed)));
        for f in 0..FRAMES_PER_PERIOD as usize {
            let s = phase.sin() * 0.25;
            buf[f * 2] = s;
            buf[f * 2 + 1] = s;
            phase += step;
            if phase > 2.0 * std::f32::consts::PI { phase -= 2.0 * std::f32::consts::PI; }
        }
        gain.process(&mut buf);
        for i in 0..buf.len() { out[i] = (buf[i] * 32767.0) as i16; }
        if sink.write_i16(&out).is_err() { break; }
    }
    let _ = sink.drain();
    let _ = std::fs::remove_file(&socket);
    eprintln!("lyrad: control session done ({secs}s)");
}
