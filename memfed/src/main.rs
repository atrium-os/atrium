//! memfed — Atrium memory federation: water-fill the shared RAM across jails by
//! weight and enforce each jail's share as a per-jail RCTL `memoryuse` cap.
//!
//! This is the PROACTIVE layer (atrium-memory-pressure.md §6): rather than react to
//! thrash, bound each jail's memory budget up front so that Σ budgets ≤ the cap —
//! then no jail can grow enough to pressure the system, by construction. RCTL is the
//! actuator (the hard per-jail boundary that already exists); the federation's job
//! is to set those caps *dynamically* by weight (priority/lifecycle tier), not by
//! static admin rule. memoryd remains the reactive residual for over-commit /
//! uncapped / cross-jail-tier cases.
//!
//! Budgeting (the federation principle: scarcity → budget, not fair-divide): each
//! jail gets its protected `floor` plus a weighted share of the elastic remainder
//! (`water_fill`). The cap is never set below a jail's CURRENT RSS (RCTL `sigkill`
//! would kill it) — an over-budget jail is *frozen* at its current use, to converge
//! down as it shrinks or memoryd reaps, not killed outright.

use std::process::Command;

fn arg(flag: &str, default: &str) -> String {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == flag).and_then(|i| a.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}
fn has(flag: &str) -> bool {
    std::env::args().any(|x| x == flag)
}

/// Physical RAM in MiB.
fn physmem_mib() -> u64 {
    let mut v: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let name = std::ffi::CString::new("hw.physmem").unwrap();
    unsafe {
        libc::sysctlbyname(name.as_ptr(), &mut v as *mut _ as *mut std::os::raw::c_void, &mut len, std::ptr::null_mut(), 0);
    }
    v / (1024 * 1024)
}

/// A jail's current RSS in MiB, via `rctl -hu jail:<name>` (0 if unreadable).
fn jail_rss_mib(name: &str) -> u64 {
    Command::new("rctl").args(["-hu", &format!("jail:{}", name)]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("memoryuse=").map(|v| v.to_string())))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024))
        .unwrap_or(0)
}

#[derive(Clone)]
struct Jail {
    name: String,
    weight: f64,
    floor: u64, // MiB protected minimum
}

/// Water-fill `budget` MiB across jails: each gets its floor, then a weighted share
/// of the elastic remainder. Returns grants (MiB), parallel to `jails`.
fn water_fill(budget: u64, jails: &[Jail]) -> Vec<u64> {
    let floor_sum: u64 = jails.iter().map(|j| j.floor).sum();
    let elastic = budget.saturating_sub(floor_sum) as f64;
    let wsum: f64 = jails.iter().map(|j| j.weight).sum::<f64>().max(1e-9);
    jails.iter().map(|j| j.floor + (elastic * j.weight / wsum) as u64).collect()
}

fn main() {
    let armed = has("--arm"); // actually set rctl rules; default = plan only
    // Budget defaults to a fraction of RAM (the host/kernel need the rest).
    let pct = arg("--budget-pct", "70").parse::<u64>().unwrap_or(70);
    let budget = arg("--budget", "0").parse::<u64>().unwrap_or(0);
    let budget = if budget > 0 { budget } else { physmem_mib() * pct / 100 };

    // Jails as `name:weight:floor_mb` (weight = priority/lifecycle tier).
    let jails: Vec<Jail> = std::env::args().skip(1).filter(|a| a.contains(':')).filter_map(|a| {
        let f: Vec<&str> = a.split(':').collect();
        if f.len() == 3 {
            Some(Jail { name: f[0].to_string(), weight: f[1].parse().ok()?, floor: f[2].parse().ok()? })
        } else {
            None
        }
    }).collect();

    if jails.is_empty() {
        eprintln!("usage: memfed [--arm] [--budget MB | --budget-pct N] name:weight:floor_mb ...");
        std::process::exit(1);
    }

    let grants = water_fill(budget, &jails);
    eprintln!("memfed: {} | budget {}MB over {} jails (Σweight {:.0})",
        if armed { "ARMED (setting rctl caps)" } else { "plan only" },
        budget, jails.len(), jails.iter().map(|j| j.weight).sum::<f64>());

    for (j, &grant) in jails.iter().zip(&grants) {
        let rss = jail_rss_mib(&j.name);
        // Never set the cap below current RSS — RCTL sigkill would kill the jail.
        // Freeze an over-budget jail at its current use; it converges as it shrinks.
        let cap = grant.max(rss);
        let note = if cap > grant { " (over budget → frozen at RSS, not killed)" } else { "" };
        eprintln!("  jail {:<10} w={:<4} floor={}MB rss={}MB -> budget {}MB, cap={}MB{}",
            j.name, j.weight, j.floor, rss, grant, cap, note);
        if armed {
            let r = Command::new("rctl")
                .args(["-a", &format!("jail:{}:memoryuse:sigkill={}M", j.name, cap)])
                .status();
            match r {
                Ok(s) if s.success() => {}
                Ok(s) => eprintln!("    rctl failed: {}", s),
                Err(e) => eprintln!("    rctl error: {}", e),
            }
        }
    }
    let total: u64 = grants.iter().sum();
    eprintln!("Σ budgets = {}MB <= {}MB cap : {}", total, budget,
        if total <= budget { "OK (no jail can thrash the system)" } else { "OVER (floors exceed budget)" });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(name: &str, w: f64, floor: u64) -> Jail {
        Jail { name: name.into(), weight: w, floor }
    }

    #[test]
    fn water_fill_splits_elastic_by_weight_above_floors() {
        // 8192MB over web(w3,floor512) + cache(w1,floor128): floors 640, elastic 7552,
        // split 3:1 -> web 512+5664=6176, cache 128+1888=2016. Σ = 8192 <= budget.
        let jails = [j("web", 3.0, 512), j("cache", 1.0, 128)];
        let g = water_fill(8192, &jails);
        assert_eq!(g[0], 6176);
        assert_eq!(g[1], 2016);
        assert!(g.iter().sum::<u64>() <= 8192, "budgets never exceed the cap");
        assert!(g[0] > g[1] * 2, "higher weight gets a bigger share");
    }

    #[test]
    fn floors_are_protected_when_weights_are_lopsided() {
        // Even a near-zero-weight jail keeps its floor.
        let jails = [j("fg", 1000.0, 256), j("idle", 0.01, 128)];
        let g = water_fill(4096, &jails);
        assert!(g[1] >= 128, "the low-weight jail still gets its floor");
        assert!(g.iter().sum::<u64>() <= 4096);
    }
}
