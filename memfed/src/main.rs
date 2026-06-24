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

/// A jail's current RSS in MiB, via `rctl -u jail:<name>` (raw bytes — NOT `-hu`,
/// whose human-readable "5120M" can't be byte-parsed). 0 if unreadable.
fn jail_rss_mib(name: &str) -> u64 {
    Command::new("rctl").args(["-u", &format!("jail:{}", name)]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("memoryuse=").map(|v| v.to_string())))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024))
        .unwrap_or(0)
}

/// A jail's numeric id, via `jls -j <name> jid`. None if the jail isn't running.
fn jail_jid(name: &str) -> Option<i32> {
    Command::new("jls").args(["-j", name, "jid"]).output().ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
}

/// Per-jail PSI `full` stall (ns) keyed by jid, from `kern.pressure.memory.jails`
/// (kern_pressure.c). A rising `full` means the jail is locally thrashing — its
/// pages keep getting reclaimed, so its true demand exceeds its measured RSS. memfed
/// uses the delta as a HIDDEN-DEMAND correction: boost a thrashing jail's demand so
/// the weighted water-fill gives it more budget *if its weight earns it* (a low-
/// weight thrasher still loses the competition and stays contained).
fn jail_full_ns() -> std::collections::HashMap<i32, u64> {
    let mut m = std::collections::HashMap::new();
    let name = std::ffi::CString::new("kern.pressure.memory.jails").unwrap();
    let mut len: libc::size_t = 0;
    unsafe {
        if libc::sysctlbyname(name.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len == 0 {
            return m;
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr() as *mut std::os::raw::c_void, &mut len, std::ptr::null_mut(), 0) != 0 {
            return m;
        }
        buf.truncate(len.saturating_sub(1));
        if let Ok(s) = String::from_utf8(buf) {
            for line in s.lines() {
                // "jail <jid> some_ns=<n> full_ns=<n>"
                let f: Vec<&str> = line.split_whitespace().collect();
                if f.len() >= 4 && f[0] == "jail" {
                    if let (Ok(jid), Some(full)) = (
                        f[1].parse::<i32>(),
                        f[3].strip_prefix("full_ns=").and_then(|v| v.parse().ok()),
                    ) {
                        m.insert(jid, full);
                    }
                }
            }
        }
    }
    m
}

#[derive(Clone)]
struct Jail {
    name: String,
    weight: f64,
    floor: u64, // MiB protected minimum
}

/// Demand-aware max-min fair water-fill (the federation): each jail gets its floor,
/// then up to its *demand* from the elastic budget, weighted — a jail that wants
/// less than its weighted share releases the slack to the others (so idle jails do
/// not hoard budget). `demands`/`floors`/`weights` parallel to `jails`; returns
/// grants in MiB. Mirrors gpusim federation.rs water_fill_floored / the kernel
/// energy_water_fill.
fn water_fill(budget: u64, demands: &[u64], floors: &[u64], weights: &[f64]) -> Vec<u64> {
    let n = demands.len();
    let floor_sum: u64 = floors.iter().sum();
    let mut remaining = budget.saturating_sub(floor_sum) as f64;
    // elastic demand above each floor.
    let elastic: Vec<f64> = (0..n).map(|i| demands[i].saturating_sub(floors[i]) as f64).collect();
    let mut grant = vec![0.0f64; n];
    let mut active = vec![true; n];
    loop {
        let wsum: f64 = (0..n).filter(|&i| active[i]).map(|i| weights[i]).sum();
        if wsum <= 0.0 || remaining <= 0.0 {
            break;
        }
        let mut progress = false;
        for i in 0..n {
            if !active[i] {
                continue;
            }
            let share = remaining * weights[i] / wsum;
            if elastic[i] <= share {
                grant[i] = elastic[i]; // fits: take demand, release slack
                remaining -= elastic[i];
                active[i] = false;
                progress = true;
                break; // recompute shares with the freed budget
            }
        }
        if !progress {
            // all remaining members saturated: split the rest by weight.
            for i in 0..n {
                if active[i] {
                    grant[i] = remaining * weights[i] / wsum;
                    active[i] = false;
                }
            }
            break;
        }
    }
    (0..n).map(|i| floors[i] + grant[i] as u64).collect()
}

/// One federation tick: read each jail's RSS and per-jail `full`, demand-aware
/// water-fill the budget (a thrashing jail's demand is boosted), and (if armed) set
/// the per-jail RCTL caps. Returns the per-jail `full` snapshot to use as the next
/// tick's baseline for the thrash delta.
fn rebudget(jails: &[Jail], budget: u64, headroom: u64, thrash_boost: u64,
            prev_full: &std::collections::HashMap<i32, u64>, armed: bool)
    -> std::collections::HashMap<i32, u64>
{
    let rss: Vec<u64> = jails.iter().map(|j| jail_rss_mib(&j.name)).collect();
    let cur_full = jail_full_ns();
    // A jail is thrashing iff its per-jail `full` climbed since the last tick.
    let thrashing: Vec<bool> = jails.iter().map(|j| jail_jid(&j.name).is_some_and(|jid| {
        cur_full.get(&jid).copied().unwrap_or(0) > prev_full.get(&jid).copied().unwrap_or(0)
    })).collect();
    // Demand = RSS (or floor) + headroom, PLUS a thrash boost = the hidden demand a
    // stalling jail can't show in RSS (its pages keep getting reclaimed). The
    // water-fill grants the boost only if the jail's weight wins it — high-weight
    // thrashers grow and stop stalling; low-weight thrashers stay contained.
    let demands: Vec<u64> = jails.iter().zip(&rss).zip(&thrashing)
        .map(|((j, &r), &t)| r.max(j.floor) + headroom + if t { thrash_boost } else { 0 })
        .collect();
    let floors: Vec<u64> = jails.iter().map(|j| j.floor).collect();
    let weights: Vec<f64> = jails.iter().map(|j| j.weight).collect();
    let grants = water_fill(budget, &demands, &floors, &weights);

    for (i, j) in jails.iter().enumerate() {
        // Never below current RSS — RCTL sigkill would kill it; freeze, don't kill.
        let cap = grants[i].max(rss[i]);
        let note = if cap > grants[i] { " (over budget → frozen at RSS)" } else { "" };
        let tnote = if thrashing[i] { " [THRASH +boost]" } else { "" };
        eprintln!("  jail {:<10} w={:<4} rss={}MB demand={}MB{} -> grant {}MB cap={}MB{}",
            j.name, j.weight, rss[i], demands[i], tnote, grants[i], cap, note);
        if armed {
            let _ = Command::new("rctl")
                .args(["-a", &format!("jail:{}:memoryuse:sigkill={}M", j.name, cap)])
                .status();
        }
    }
    let total: u64 = grants.iter().sum();
    eprintln!("  Σ grants = {}MB / {}MB budget{}", total, budget,
        if total <= budget { "" } else { " (floors exceed budget!)" });
    cur_full
}

fn main() {
    let armed = has("--arm");
    let pct = arg("--budget-pct", "70").parse::<u64>().unwrap_or(70);
    let budget_arg = arg("--budget", "0").parse::<u64>().unwrap_or(0);
    let budget = if budget_arg > 0 { budget_arg } else { physmem_mib() * pct / 100 };
    let headroom = arg("--headroom", "512").parse::<u64>().unwrap_or(512); // MiB room to grow/tick
    // Extra demand granted to a jail that is locally thrashing (per-jail PSI `full`
    // climbing) — its hidden demand above RSS. Granted by weight via the water-fill.
    let thrash_boost = arg("--thrash-boost", "1024").parse::<u64>().unwrap_or(1024);
    // --interval N → run as a daemon re-budgeting every N s (tracks demand + churn);
    // default = one shot.
    let interval = arg("--interval", "0").parse::<u64>().unwrap_or(0);

    let jails: Vec<Jail> = std::env::args().skip(1).filter(|a| a.contains(':')).filter_map(|a| {
        let f: Vec<&str> = a.split(':').collect();
        if f.len() == 3 {
            Some(Jail { name: f[0].to_string(), weight: f[1].parse().ok()?, floor: f[2].parse().ok()? })
        } else {
            None
        }
    }).collect();

    if jails.is_empty() {
        eprintln!("usage: memfed [--arm] [--budget MB|--budget-pct N] [--headroom MB] [--interval S] name:weight:floor ...");
        std::process::exit(1);
    }

    eprintln!("memfed: {} | budget {}MB, headroom {}MB/tick, thrash-boost {}MB over {} jails | {}",
        if armed { "ARMED" } else { "plan-only" }, budget, headroom, thrash_boost, jails.len(),
        if interval > 0 { format!("daemon, re-budget every {}s", interval) } else { "one-shot".into() });

    let mut prev_full = std::collections::HashMap::new();
    loop {
        prev_full = rebudget(&jails, budget, headroom, thrash_boost, &prev_full, armed);
        if interval == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbounded_demand_splits_elastic_by_weight() {
        // Both jails want everything (demand >> budget): pure weighted split above
        // floors. 8192 over web(w3,floor512) + cache(w1,floor128): floors 640,
        // elastic 7552 split 3:1 -> web 6176, cache 2016.
        let g = water_fill(8192, &[99999, 99999], &[512, 128], &[3.0, 1.0]);
        assert_eq!(g, vec![6176, 2016]);
        assert!(g.iter().sum::<u64>() <= 8192);
    }

    #[test]
    fn idle_jail_releases_slack_to_the_busy_one() {
        // web (w3) wants 7000MB; cache (w1) is idle (demand only 200MB). cache takes
        // just its 200, releasing the rest to web — demand-awareness, not a static
        // 3:1 split that would reserve 2016 for the idle cache.
        let g = water_fill(8192, &[7000, 200], &[256, 128], &[3.0, 1.0]);
        assert_eq!(g[1], 200, "idle jail capped at its demand, not its weighted share");
        assert!(g[0] >= 7000, "the busy jail gets the released slack");
        assert!(g.iter().sum::<u64>() <= 8192);
    }

    #[test]
    fn floors_protected_under_contention() {
        // Both demand a lot; the low-weight jail still keeps its floor.
        let g = water_fill(4096, &[99999, 99999], &[256, 128], &[1000.0, 0.01]);
        assert!(g[1] >= 128, "low-weight jail keeps its floor");
        assert!(g.iter().sum::<u64>() <= 4096);
    }

    // The thrash boost is applied to a thrashing jail's DEMAND, then the same
    // water-fill allocates by weight. These pin the two ends of that behaviour.

    #[test]
    fn thrash_boost_grows_high_weight_jail_when_budget_allows() {
        // web (w3) thrashing → demand boosted 1000→2024; ample budget. The boost
        // flows straight through: the high-weight jail grows to relieve its stall.
        let floors = [256u64, 128];
        let weights = [3.0, 1.0];
        let idle = water_fill(4096, &[1000, 200], &floors, &weights);
        let boosted = water_fill(4096, &[2024, 200], &floors, &weights);
        assert_eq!(idle[0], 1000);
        assert_eq!(boosted[0], 2024, "high-weight thrasher gets the full boost");
        assert!(boosted[0] > idle[0]);
        assert!(boosted.iter().sum::<u64>() <= 4096);
    }

    #[test]
    fn thrash_boost_is_contained_for_a_low_weight_jail() {
        // cache (w1) thrashing → demand boosted 200→1224, under a TIGHT budget with a
        // high-weight idle web (w3). cache gets *some* relief but nowhere near its
        // boosted demand — its weight loses the competition, so it stays contained
        // and cannot starve web.
        let floors = [256u64, 128];
        let weights = [3.0, 1.0]; // web heavy, cache light
        let unboosted = water_fill(1024, &[600, 200], &floors, &weights);
        let boosted = water_fill(1024, &[600, 1224], &floors, &weights);
        assert_eq!(boosted[0], 600, "the high-weight idle jail is unaffected");
        assert!(boosted[1] > unboosted[1], "the low-weight thrasher gets some relief");
        assert!(boosted[1] < 1224, "but is contained well below its boosted demand");
        assert!(boosted.iter().sum::<u64>() <= 1024);
    }
}
