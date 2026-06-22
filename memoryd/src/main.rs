//! memoryd — Atrium's lifecycle-aware memory-pressure daemon.
//!
//! The live port of the controller proven in `gpusim engine/src/controller.rs`
//! (atrium-memory-pressure.md Phase 5). It reads the kernel PSI-equivalent signal
//! (`kern.pressure.memory`, Phase 1) and, under sustained thrash, sheds the
//! lowest-lifecycle-tier member **early** — before the kernel's blunt largest-RSS
//! `vm_pageout_oom` — so the foreground survives.
//!
//! Two-stage reap (the cascade's graceful tier before the destructive one): the
//! victim is first asked to exit with **SIGTERM** — its chance to run its cleanup /
//! persist-state handler (the lifecycle `onDestroy` / `onTrimMemory` analog) — and
//! only escalated to **SIGKILL** if it is still alive and the system is still
//! thrashing after a posture-scaled grace period. A reap-in-flight guard means we
//! never pick a second victim while one is mid-reap.
//!
//! Members register in a file (`pid tier name` per line) — the stand-in for
//! Portcullis registering each jailed app with its manifest tier. Tier = weight =
//! the Android lmkd priority; lower tier is shed first.
//!
//! Thrash detection: the live kernel signal is PSI `some` only (the `full`
//! all-stalled signal is a later kernel refinement), so — like systemd-oomd / lmkd
//! — we gate sustained-high `some` (avg10) with a low-free-memory floor; cache churn
//! that is coping does not hold both. Posture sets patience and grace.

use std::ffi::CString;
use std::fs;
use std::os::raw::c_void;
use std::thread::sleep;
use std::time::{Duration, Instant};

fn sysctl_i32(name: &str) -> Option<i32> {
    let c = CString::new(name).ok()?;
    let mut v: i32 = 0;
    let mut len = std::mem::size_of::<i32>();
    let ok = unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            &mut v as *mut _ as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        Some(v)
    } else {
        None
    }
}

/// Free RAM in MiB (v_free_count pages × pagesize).
fn free_mib() -> u64 {
    let pages = sysctl_i32("vm.stats.vm.v_free_count").unwrap_or(0) as u32 as u64;
    let pgsz = sysctl_i32("hw.pagesize").unwrap_or(4096) as u64;
    pages * pgsz / (1024 * 1024)
}

/// PSI avg10 as a percentage (the kernel exposes fraction ×10000).
fn avg10_pct() -> f64 {
    sysctl_i32("kern.pressure.memory.avg10").unwrap_or(0) as f64 / 100.0
}

#[derive(Clone)]
struct Member {
    pid: i32,
    tier: i32,
    name: String,
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Parse the registry: `pid tier name` per line. Drop dead pids.
fn read_members(path: &str) -> Vec<Member> {
    let mut v = Vec::new();
    if let Ok(s) = fs::read_to_string(path) {
        for line in s.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 2 {
                if let (Ok(pid), Ok(tier)) = (f[0].parse(), f[1].parse()) {
                    if alive(pid) {
                        v.push(Member { pid, tier, name: f.get(2).unwrap_or(&"?").to_string() });
                    }
                }
            }
        }
    }
    v
}

fn arg(flag: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}
fn has(flag: &str) -> bool {
    std::env::args().any(|x| x == flag)
}

/// A reap in progress: the victim has been asked to exit (SIGTERM); we wait for it
/// to comply (the graceful tier) before escalating to SIGKILL.
struct Pending {
    pid: i32,
    tier: i32,
    name: String,
    asked_at: Instant,
    killed: bool, // SIGKILL already escalated; now just await death (no re-signal)
}

fn main() {
    let armed = has("--arm");
    let trip = arg("--trip", "80").parse::<f64>().unwrap_or(80.0); // avg10 % to act on
    let free_floor = arg("--free-floor", "256").parse::<u64>().unwrap_or(256); // MiB
    let posture = arg("--posture", "5").parse::<u32>().unwrap_or(5).min(10);
    let registry = arg("--registry", "/var/run/memoryd/members");
    let tolerance = 1 + posture; // sustained thrash seconds before acting
    let grace = Duration::from_secs((1 + posture / 2) as u64); // SIGTERM -> SIGKILL window

    eprintln!(
        "memoryd: {} | trip avg10>={:.0}% & free<={}MB | posture {} (tolerate {}s, grace {}s) | registry {}",
        if armed { "ARMED" } else { "dry-run" },
        trip, free_floor, posture, tolerance, grace.as_secs(), registry
    );

    let signal = |pid: i32, sig: i32| -> i32 {
        if armed {
            unsafe { libc::kill(pid, sig) }
        } else {
            0 // dry-run: decide + log, do not signal
        }
    };

    let mut thrash_run = 0u32;
    let mut pending: Option<Pending> = None;
    loop {
        let avg10 = avg10_pct();
        let free = free_mib();
        let nstalled = sysctl_i32("kern.pressure.memory.nstalled").unwrap_or(0);
        let members = read_members(&registry);
        let thrash = avg10 >= trip && free <= free_floor;

        match pending.take() {
            // --- a reap is in flight: wait for it to die; escalate ONCE if the
            //     graceful SIGTERM is ignored past the grace window ---
            Some(mut p) => {
                if !alive(p.pid) {
                    eprintln!("RESOLVED pid={} tier={} name={} exited ({})", p.pid, p.tier, p.name,
                        if p.killed { "forced after SIGKILL" } else { "graceful, SIGTERM sufficed" });
                    thrash_run = 0; // pending dropped (taken)
                } else if p.killed {
                    // SIGKILL sent; the starved victim is still tearing down. Wait,
                    // do NOT re-signal or pick anyone new (the reap-in-flight guard).
                    eprintln!("avg10={:.1}% free={}MB n={} awaiting-death pid={} (SIGKILL sent)", avg10, free, nstalled, p.pid);
                    pending = Some(p);
                } else if p.asked_at.elapsed() >= grace {
                    if thrash {
                        let r = signal(p.pid, libc::SIGKILL);
                        eprintln!("ESCALATE pid={} tier={} name={} ignored SIGTERM past {}s grace -> SIGKILL{}",
                            p.pid, p.tier, p.name, grace.as_secs(), if armed { format!(" rc={}", r) } else { " [dry-run]".into() });
                        p.killed = true;
                        pending = Some(p); // keep guarding until it actually dies
                    } else {
                        eprintln!("SPARE pid={} tier={} name={} pressure cleared during grace — no SIGKILL", p.pid, p.tier, p.name);
                        thrash_run = 0; // pending dropped
                    }
                } else {
                    // within the grace window: give the app time to exit cleanly.
                    eprintln!("avg10={:.1}% free={}MB n={} await-exit pid={} ({:.0}s/{}s grace)",
                        avg10, free, nstalled, p.pid, p.asked_at.elapsed().as_secs_f64(), grace.as_secs());
                    pending = Some(p);
                }
            }
            // --- idle: accumulate thrash, act when it persists past tolerance ---
            None => {
                if thrash { thrash_run += 1 } else { thrash_run = 0 }
                eprintln!("avg10={:.1}% free={}MB nstalled={} members={} thrash_run={}{}",
                    avg10, free, nstalled, members.len(), thrash_run, if thrash { " [THRASH]" } else { "" });
                if thrash_run >= tolerance {
                    if let Some(v) = members.iter().min_by_key(|m| m.tier) {
                        let hi = members.iter().max_by_key(|m| m.tier)
                            .map(|m| format!(" (sparing highest tier: {} t{})", m.name, m.tier)).unwrap_or_default();
                        let r = signal(v.pid, libc::SIGTERM);
                        eprintln!("REAP pid={} tier={} name={} -> SIGTERM (graceful exit request){}{}",
                            v.pid, v.tier, v.name, hi, if armed { format!(" rc={}", r) } else { " [dry-run]".into() });
                        pending = Some(Pending { pid: v.pid, tier: v.tier, name: v.name.clone(), asked_at: Instant::now(), killed: false });
                        thrash_run = 0;
                    } else {
                        eprintln!("thrash but no registered members to shed");
                    }
                }
            }
        }

        sleep(Duration::from_secs(1));
    }
}
