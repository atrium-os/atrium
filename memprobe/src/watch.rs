//! System-wide memory sampling over a workload: free / anon / swap, the PSI
//! pressure averages and the compressed store's own counters, one CSV row per
//! interval.
//!
//! FreeBSD reads Atrium's `kern.pressure.memory.*` (kern_pressure.c) and
//! `kern.zram.*` (atrium_zram.ko); Linux reads `/proc/pressure/memory` and
//! `/sys/block/zram0/mm_stat`, so the same harness runs on a Linux host as a
//! proxy. A missing source is an empty cell, never a fake zero.

#[derive(Debug, Default, Clone)]
pub struct Sample {
    pub total_mb: Option<f64>,
    pub free_mb: Option<f64>,
    /// Linux MemAvailable; FreeBSD free + inactive + laundry (what could be
    /// had without killing anything).
    pub avail_mb: Option<f64>,
    pub anon_active_mb: Option<f64>,
    pub anon_inactive_mb: Option<f64>,
    pub laundry_mb: Option<f64>,
    pub wired_mb: Option<f64>,
    pub swap_used_mb: Option<f64>,
    pub psi_some_avg10: Option<f64>,
    pub psi_full_avg10: Option<f64>,
    /// Logical (uncompressed) bytes held by the compressed store.
    pub zram_orig_mb: Option<f64>,
    /// Physical bytes the store's data occupies.
    pub zram_phys_mb: Option<f64>,
}

pub const CSV_HEADER: &str = "t_s,total_mb,free_mb,avail_mb,anon_active_mb,anon_inactive_mb,laundry_mb,wired_mb,swap_used_mb,psi_some_avg10_pct,psi_full_avg10_pct,zram_orig_mb,zram_phys_mb";

impl Sample {
    pub fn csv(&self, t: f64) -> String {
        let f = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_default();
        format!(
            "{t:.1},{},{},{},{},{},{},{},{},{},{},{},{}",
            f(self.total_mb),
            f(self.free_mb),
            f(self.avail_mb),
            f(self.anon_active_mb),
            f(self.anon_inactive_mb),
            f(self.laundry_mb),
            f(self.wired_mb),
            f(self.swap_used_mb),
            f(self.psi_some_avg10),
            f(self.psi_full_avg10),
            f(self.zram_orig_mb),
            f(self.zram_phys_mb),
        )
    }
}

pub fn sample() -> Sample {
    imp::sample()
}

#[cfg(target_os = "linux")]
mod imp {
    use super::Sample;
    use std::fs;

    fn meminfo() -> std::collections::HashMap<String, f64> {
        let mut m = std::collections::HashMap::new();
        for line in fs::read_to_string("/proc/meminfo").unwrap_or_default().lines() {
            if let Some((k, v)) = line.split_once(':') {
                if let Some(kb) = v.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()) {
                    m.insert(k.to_string(), kb / 1024.0);
                }
            }
        }
        m
    }

    /// `some avg10=1.23 avg60=... total=...` → 1.23 (already a percentage).
    fn psi(kind: &str) -> Option<f64> {
        let s = fs::read_to_string("/proc/pressure/memory").ok()?;
        let line = s.lines().find(|l| l.starts_with(kind))?;
        line.split_whitespace().find_map(|w| w.strip_prefix("avg10=")).and_then(|x| x.parse().ok())
    }

    pub fn sample() -> Sample {
        let m = meminfo();
        let g = |k: &str| m.get(k).copied();
        let swap = match (g("SwapTotal"), g("SwapFree")) {
            (Some(t), Some(f)) => Some(t - f),
            _ => None,
        };
        // mm_stat: orig_data_size compr_data_size mem_used_total ...
        let mm: Vec<f64> = fs::read_to_string("/sys/block/zram0/mm_stat")
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        Sample {
            total_mb: g("MemTotal"),
            free_mb: g("MemFree"),
            avail_mb: g("MemAvailable"),
            anon_active_mb: g("Active(anon)"),
            anon_inactive_mb: g("Inactive(anon)"),
            laundry_mb: None,
            wired_mb: g("Unevictable"),
            swap_used_mb: swap,
            psi_some_avg10: psi("some"),
            psi_full_avg10: psi("full"),
            zram_orig_mb: mm.first().map(|b| b / 1048576.0),
            zram_phys_mb: mm.get(2).map(|b| b / 1048576.0),
        }
    }
}

#[cfg(target_os = "freebsd")]
mod imp {
    use super::Sample;
    use std::collections::HashMap;
    use std::process::Command;

    const OIDS: &[&str] = &[
        "hw.physmem",
        "vm.stats.vm.v_page_size",
        "vm.stats.vm.v_free_count",
        "vm.stats.vm.v_active_count",
        "vm.stats.vm.v_inactive_count",
        "vm.stats.vm.v_laundry_count",
        "vm.stats.vm.v_wire_count",
        "kern.pressure.memory.avg10",
        "kern.pressure.memory.full_avg10",
        "kern.zram.orig_bytes",
        "kern.zram.phys_bytes",
    ];

    /// `sysctl -i` skips OIDs this kernel lacks (no PSI, zram not loaded), so
    /// parse `name: value` rather than relying on line order.
    fn sysctls() -> HashMap<String, f64> {
        let mut m = HashMap::new();
        if let Ok(o) = Command::new("sysctl").arg("-i").args(OIDS).output() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if let Some((k, v)) = line.split_once(": ") {
                    if let Ok(x) = v.trim().parse::<f64>() {
                        m.insert(k.to_string(), x);
                    }
                }
            }
        }
        m
    }

    /// `swapinfo -k` last line: `Total <kblocks> <used> <avail> <cap>`, or a
    /// single device line when there is only one.
    fn swap_used_mb() -> Option<f64> {
        let o = Command::new("swapinfo").arg("-k").output().ok()?;
        let s = String::from_utf8_lossy(&o.stdout);
        let line = s.lines().skip(1).last()?;
        line.split_whitespace().nth(2)?.parse::<f64>().ok().map(|kb| kb / 1024.0)
    }

    pub fn sample() -> Sample {
        let m = sysctls();
        let g = |k: &str| m.get(k).copied();
        let ps = g("vm.stats.vm.v_page_size").unwrap_or(4096.0) / 1048576.0;
        let pages = |k: &str| g(k).map(|n| n * ps);
        let (free, inact, laun) = (
            pages("vm.stats.vm.v_free_count"),
            pages("vm.stats.vm.v_inactive_count"),
            pages("vm.stats.vm.v_laundry_count"),
        );
        // kern.pressure.memory.* are basis points (fraction x 10000).
        let bp = |k: &str| g(k).map(|x| x / 100.0);
        Sample {
            total_mb: g("hw.physmem").map(|b| b / 1048576.0),
            free_mb: free,
            avail_mb: match (free, inact, laun) {
                (Some(a), Some(b), Some(c)) => Some(a + b + c),
                _ => None,
            },
            // FreeBSD's queues mix anon and file pages; these are the queue
            // totals, labelled as such in the spec.
            anon_active_mb: pages("vm.stats.vm.v_active_count"),
            anon_inactive_mb: inact,
            laundry_mb: laun,
            wired_mb: pages("vm.stats.vm.v_wire_count"),
            swap_used_mb: swap_used_mb(),
            psi_some_avg10: bp("kern.pressure.memory.avg10"),
            psi_full_avg10: bp("kern.pressure.memory.full_avg10"),
            zram_orig_mb: g("kern.zram.orig_bytes").map(|b| b / 1048576.0),
            zram_phys_mb: g("kern.zram.phys_bytes").map(|b| b / 1048576.0),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
mod imp {
    pub fn sample() -> super::Sample {
        super::Sample::default()
    }
}
