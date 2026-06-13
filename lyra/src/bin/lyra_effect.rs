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
//! it into the graph. With `--plugin <path.so>` it hosts a real C node
//! (`node_abi`, the `lyra_node.h` ABI) instead of the built-in tremolo — so the
//! crash-isolation demo runs *actual third-party DSP*: a fault inside the C
//! plugin's process() takes down only this confined process, and lyrad bypasses
//! it.
//!
//! Confinement here is **Capsicum** (`--capsicum`): the process sandboxes
//! *itself* by calling `cap_enter()`. That is distinct from a **FreeBSD jail**
//! (Portcullis), which lyrad would impose from the *outside* before exec — a
//! forced container the node cannot opt out of. The two are complementary
//! defence-in-depth: the trusted shim Capsicum-confines itself before dlopening
//! the untrusted plugin, and (still to come) runs inside a Portcullis jail so
//! the confinement does not depend on the shim cooperating.
//!
//! usage: lyra-effect <in_ring> <out_ring> [tremolo_hz] [--plugin <path.so>]
//! `--crash-after <n>` exits after processing n buffers (the isolation gate).

use lyra::node_abi::HostedNode;
use lyra::ring::Ring;
use std::path::PathBuf;

const CH: usize = 2;

/// Enter **Capsicum** capability mode — the node sandboxes *itself* (opt-in;
/// not to be confused with a FreeBSD jail, which is forced from outside). After
/// this, the process keeps only the fds it already holds (the rings via their
/// mmap, the lane fd, stderr) and can make NO global-namespace syscall: no
/// open(), no connect(), no new files or sockets. A buggy or hostile plugin's
/// process() can still scribble its own buffers or crash (contained by the
/// separate process), but it cannot exfiltrate, phone home, or touch the
/// filesystem. Must be called AFTER every open (rings + lane + dlopen).
/// FreeBSD-only; a no-op elsewhere (the host build), where the proof is the
/// cross-build + in-VM run.
#[cfg(target_os = "freebsd")]
fn enter_capsicum() -> bool {
    unsafe { libc::cap_enter() == 0 }
}
#[cfg(not(target_os = "freebsd"))]
fn enter_capsicum() -> bool {
    false
}

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
    let plugin: Option<PathBuf> = args.iter().position(|a| a == "--plugin")
        .and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let want_capsicum = args.iter().any(|a| a == "--capsicum");
    // --adopt <pid> <tid>: the client entity to run on (K-b, charge-back).
    let adopt_target: Option<(i32, i32)> = args.iter().position(|a| a == "--adopt")
        .and_then(|i| Some((args.get(i + 1)?.parse().ok()?, args.get(i + 2)?.parse().ok()?)));

    let inr = Ring::open(in_name, false).expect("open in ring");
    let outr = Ring::open(out_name, true).expect("open out ring");
    // K-b: open /dev/laminar BEFORE cap_enter (Capsicum blocks new opens). The
    // adopt ioctl runs on this held fd, after self-confinement.
    let lane_fd = adopt_target.and_then(|_| lyra::lane::open_lane().ok());

    let rate = 48_000.0f32;

    // Hosted C node, if a plugin was given (the real third-party path); else the
    // built-in tremolo. The C node is dlopen'd HERE, inside the isolated
    // process, so its fault is contained exactly like the built-in crash gate.
    // SAFETY: this process is the isolation boundary — a fault is contained.
    let mut node = plugin.as_ref().map(|p| {
        let n = unsafe { HostedNode::load(p, rate as u32, CH as u32) }
            .unwrap_or_else(|e| {
                eprintln!("lyra-effect: plugin load failed: {e:?}");
                std::process::exit(3);
            });
        eprintln!("lyra-effect: hosting C node '{}' (latency {} frames)", n.name(), n.latency_frames());
        n
    });

    // CAPSICUM SELF-CONFINEMENT: every open is done (rings + lane + dlopen), so
    // drop into capability mode. From here the node is confined to the fds it
    // holds — a hostile plugin cannot reach the filesystem or network. (Opt-in,
    // by us, the trusted shim — before we dlopen the untrusted plugin.)
    if want_capsicum {
        if enter_capsicum() {
            eprintln!("lyra-effect: confined (Capsicum capability mode)");
        } else {
            // refuse to run unconfined when confinement was explicitly asked for.
            eprintln!("lyra-effect: cap_enter unavailable on this platform; refusing --capsicum");
            std::process::exit(4);
        }
    }

    // the built-in tremolo: amplitude modulation by a slow LFO — audibly obvious
    // that the node is in the path, and that bypass (dry) differs from processed.
    let mut lfo = 0.0f32;
    let lfo_step = 2.0 * std::f32::consts::PI * trem_hz / rate;

    let mut buf = vec![0.0f32; 256 * CH];
    let mut processed: u64 = 0;
    let mut adopted = false;
    loop {
        let n = inr.read(&mut buf) as usize;
        if n == 0 {
            std::hint::spin_loop();
            unsafe { libc::usleep(200) };
            continue;
        }
        // K-b: adopt the client's entity on the FIRST buffer (by now lyrad is
        // sponsored, so the entity exists). From here this node's CPU charges the
        // client's CBS budget — a heavy plugin throttles the client, not lyrad.
        if !adopted {
            adopted = true; // attempt once; processing proceeds regardless.
            if let (Some(fd), Some((pid, tid))) = (lane_fd.as_ref(), adopt_target) {
                match lyra::lane::adopt(fd, pid, tid) {
                    Ok(()) => eprintln!("lyra-effect: adopted client entity (pid {pid} tid {tid}); charged to its budget"),
                    Err(e) => eprintln!("lyra-effect: adopt failed: {e}"),
                }
            }
        }
        if let Some(node) = node.as_mut() {
            // the hosted C node processes the buffer in place.
            node.process(processed, &mut buf[..n * CH]);
        } else {
            for f in 0..n {
                // tremolo when trem_hz > 0; otherwise passthrough (unity gain) so
                // the output is bit-exactly the input — a clean pipeline check.
                let g = if trem_hz > 0.0 { 0.5 + 0.5 * lfo.sin() } else { 1.0 };
                buf[f * CH] *= g;
                buf[f * CH + 1] *= g;
                lfo += lfo_step;
            }
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
