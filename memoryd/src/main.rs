//! memoryd — Atrium's lifecycle-aware memory-pressure daemon.
//!
//! The live port of the controller proven deterministically in
//! `gpusim engine/src/controller.rs` (atrium-memory-pressure.md Phase 5). It reads
//! the kernel PSI-equivalent signal (`kern.pressure.memory`, built in Phase 1) and,
//! under sustained thrash, reaps the lowest-lifecycle-tier member **early** — before
//! the kernel's blunt largest-RSS `vm_pageout_oom` — so the foreground survives.
//!
//! Members register in a simple registry file (one `pid tier name` per line) — the
//! stand-in for Portcullis registering each jailed app with its manifest tier. Tier
//! = weight = the Android lmkd priority; lower tier is reaped first.
//!
//! Thrash detection: the live kernel signal is PSI `some` only (the `full`
//! all-stalled signal is a later kernel refinement). So, like systemd-oomd / lmkd,
//! we gate sustained-high `some` (avg10) with a low-free-memory floor — cache churn
//! that is coping does not hold both high. Posture sets the patience.

use std::ffi::CString;
use std::fs;
use std::os::raw::c_void;
use std::thread::sleep;
use std::time::Duration;

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

fn sysctl_u32(name: &str) -> Option<u32> {
    sysctl_i32(name).map(|v| v as u32)
}

/// Free RAM in MiB (v_free_count pages × pagesize).
fn free_mib() -> u64 {
    let pages = sysctl_u32("vm.stats.vm.v_free_count").unwrap_or(0) as u64;
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

fn main() {
    let armed = has("--arm");
    let trip = arg("--trip", "80").parse::<f64>().unwrap_or(80.0); // avg10 % to act on
    let free_floor = arg("--free-floor", "256").parse::<u64>().unwrap_or(256); // MiB
    let posture = arg("--posture", "5").parse::<u32>().unwrap_or(5).min(10);
    let registry = arg("--registry", "/var/run/memoryd/members");
    let tolerance = 1 + posture; // mem_thrash_tolerance: powersave eager, perf patient

    eprintln!(
        "memoryd: {} | trip avg10>={:.0}% & free<={}MB | posture {} (tolerate {}s) | registry {}",
        if armed { "ARMED (will SIGKILL)" } else { "dry-run (decisions only)" },
        trip, free_floor, posture, tolerance, registry
    );

    let mut thrash_run = 0u32;
    loop {
        let avg10 = avg10_pct();
        let free = free_mib();
        let nstalled = sysctl_i32("kern.pressure.memory.nstalled").unwrap_or(0);
        let members = read_members(&registry);

        let thrash = avg10 >= trip && free <= free_floor;
        if thrash {
            thrash_run += 1;
        } else {
            thrash_run = 0;
        }

        eprintln!(
            "avg10={:.1}% free={}MB nstalled={} members={} thrash_run={}{}",
            avg10, free, nstalled, members.len(), thrash_run,
            if thrash { " [THRASH]" } else { "" }
        );

        if thrash_run >= tolerance {
            // Reap the lowest lifecycle tier (lmkd weight), NOT the largest RSS.
            if let Some(victim) = members.iter().min_by_key(|m| m.tier) {
                let largest_note = members
                    .iter()
                    .max_by_key(|m| m.tier)
                    .map(|m| format!(" (highest tier alive: {} t{})", m.name, m.tier))
                    .unwrap_or_default();
                if armed {
                    let r = unsafe { libc::kill(victim.pid, libc::SIGKILL) };
                    eprintln!(
                        "REAP pid={} tier={} name={} -> SIGKILL rc={}{}",
                        victim.pid, victim.tier, victim.name, r, largest_note
                    );
                } else {
                    eprintln!(
                        "WOULD REAP pid={} tier={} name={} (lowest tier){}",
                        victim.pid, victim.tier, victim.name, largest_note
                    );
                }
                thrash_run = 0;
            } else {
                eprintln!("thrash but no registered members to reap");
            }
        }

        sleep(Duration::from_secs(1));
    }
}
