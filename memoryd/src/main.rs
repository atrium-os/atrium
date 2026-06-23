//! memoryd — Atrium's lifecycle-aware memory-pressure daemon.
//!
//! The live port of the controller proven in `gpusim engine/src/controller.rs`
//! (atrium-memory-pressure.md Phase 5). It reads the kernel PSI-equivalent signal
//! (`kern.pressure.memory`, Phase 1) and, under sustained thrash, sheds the
//! lowest-lifecycle-tier member — before the kernel's blunt largest-RSS
//! `vm_pageout_oom` — so the foreground survives.
//!
//! Three-tier app-cooperative cascade on the chosen victim, escalating only while
//! the system stays under pressure (each tier gets a posture-scaled grace window):
//!
//!   1. TRIM (SIGINFO) — "shed your caches but keep running": the app's chance to
//!      free reclaimable memory without dying (the `onTrimMemory` analog). A
//!      non-cooperative app ignores SIGINFO (its default), and we escalate.
//!   2. EXIT (SIGTERM) — "exit gracefully, persist state" (the `onDestroy` analog).
//!   3. KILL (SIGKILL) — force, when graceful exit is ignored.
//!
//! If pressure clears at any tier (a SPARE), we stop — the cheapest sufficient
//! action wins, and ideally the app only had to *trim*, never die. A reap-in-flight
//! guard keeps one victim in focus until it is resolved.
//!
//! Members register in a file (`pid tier name` per line) — the stand-in for
//! Portcullis registering each jailed app with its manifest tier. Tier = weight =
//! the Android lmkd priority; lower tier is shed first.
//!
//! Thrash detection: the live kernel signal is PSI `some` only (the `full`
//! all-stalled signal is a later kernel refinement), so — like systemd-oomd / lmkd
//! — we gate sustained-high `some` (avg10) with a low-free-memory floor.

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
        libc::sysctlbyname(c.as_ptr(), &mut v as *mut _ as *mut c_void, &mut len, std::ptr::null_mut(), 0)
    };
    if ok == 0 {
        Some(v)
    } else {
        None
    }
}

fn free_mib() -> u64 {
    let pages = sysctl_i32("vm.stats.vm.v_free_count").unwrap_or(0) as u32 as u64;
    let pgsz = sysctl_i32("hw.pagesize").unwrap_or(4096) as u64;
    pages * pgsz / (1024 * 1024)
}

/// PSI `some` avg10 as a percentage (the kernel exposes fraction ×10000).
fn avg10_pct() -> f64 {
    sysctl_i32("kern.pressure.memory.avg10").unwrap_or(0) as f64 / 100.0
}

/// PSI `full` avg10 as a percentage, or None if the kernel lacks the signal (an
/// older kernel without the per-task `full` accounting). `full` is the true thrash
/// signal — the % of recent time NOTHING progressed because the workload was all
/// blocked on memory — so when present it replaces the some+free-floor heuristic.
fn full_avg10_pct() -> Option<f64> {
    sysctl_i32("kern.pressure.memory.full_avg10").map(|v| v as f64 / 100.0)
}

/// The live system power posture (0..10) — the SAME knob that drives CPU parking,
/// GPU gating and display PSR (atrium-power-posture.md). Following it here makes the
/// one posture span memory reclaim too: powersave reaps eagerly, performance
/// patiently. Falls back to balanced if the sysctl is absent (no Laminar kernel).
fn live_posture() -> u32 {
    sysctl_i32("kern.sched.power_policy").unwrap_or(5).clamp(0, 10) as u32
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

/// Resident set size in KiB (via ps; the daemon polls at 1 Hz so this is cheap).
/// Used only to tie-break within a tier and to quantify the lmkd-vs-RSS contrast.
fn rss_kb(pid: i32) -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn mib(kb: u64) -> u64 {
    kb / 1024
}

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

/// The cascade tier currently applied to the victim.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Trim,
    Exit,
    Kill,
}
impl Stage {
    fn label(self) -> &'static str {
        match self {
            Stage::Trim => "TRIM(SIGINFO,shed)",
            Stage::Exit => "EXIT(SIGTERM)",
            Stage::Kill => "KILL(SIGKILL)",
        }
    }
}

struct Pending {
    pid: i32,
    tier: i32,
    name: String,
    stage: Stage,
    since: Instant,
}

/// kqueue edge-trigger on `/dev/pressure` (the kernel's PSI poll/trigger,
/// kern_pressure.c): register an EVFILT_READ knote whose `data` carries the `full`
/// threshold in basis points (fraction ×10000 — 40% = 4000). The kernel KNOTEs it
/// each aggregation tick, so we sleep in `kevent` with ZERO wakeups until pressure
/// crosses the threshold — no 1 Hz poll while idle ([[feedback_kqueue_native]]).
struct PressureKq {
    kq: i32,
    fd: i32,
}

impl PressureKq {
    fn new(trip_bp: i64) -> Option<Self> {
        let path = CString::new("/dev/pressure").ok()?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            return None; // older kernel without the trigger → caller falls back to polling
        }
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            unsafe { libc::close(fd) };
            return None;
        }
        let mut kev: libc::kevent = unsafe { std::mem::zeroed() };
        kev.ident = fd as usize;
        kev.filter = libc::EVFILT_READ;
        kev.flags = (libc::EV_ADD | libc::EV_CLEAR) as u16;
        kev.data = trip_bp; // threshold delivered to the knote as kn_sdata
        let r = unsafe {
            libc::kevent(kq, &kev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
        if r < 0 {
            unsafe { libc::close(kq); libc::close(fd) };
            return None;
        }
        Some(PressureKq { kq, fd })
    }

    /// Block until a pressure edge (or `timeout`, if given). `None` = sleep
    /// indefinitely — the zero-wakeup idle path.
    fn wait(&self, timeout: Option<Duration>) {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        let ts = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut ev, 1, tsp) };
    }
}

impl Drop for PressureKq {
    fn drop(&mut self) {
        unsafe { libc::close(self.kq); libc::close(self.fd) };
    }
}

fn main() {
    let armed = has("--arm");
    let trip = arg("--trip", "80").parse::<f64>().unwrap_or(80.0);
    let free_floor = arg("--free-floor", "256").parse::<u64>().unwrap_or(256);
    // `full` avg10 % above which the system is thrashing (the primary gate when the
    // kernel exposes PSI `full`). full = nothing progressing because the workload is
    // blocked on memory — a true thrash signal, no free-floor heuristic needed.
    let full_trip = arg("--full-trip", "40").parse::<f64>().unwrap_or(40.0);
    let registry = arg("--registry", "/var/run/memoryd/members");
    // Posture: an explicit --posture pins it; otherwise FOLLOW the live system
    // power posture (kern.sched.power_policy) so the one knob spans memory too.
    let fixed_posture: Option<u32> = if has("--posture") {
        Some(arg("--posture", "5").parse::<u32>().unwrap_or(5).min(10))
    } else {
        None
    };

    let have_full = full_avg10_pct().is_some();
    // Edge-trigger when the kernel exposes `full` (the trip is on full_avg10, which
    // is exactly what /dev/pressure fires on). Without `full` the gate is some+free,
    // which the trigger does not track, so fall back to the 1 s poll.
    let pkq = if have_full { PressureKq::new((full_trip * 100.0) as i64) } else { None };
    eprintln!(
        "memoryd: {} | thrash = {} | wakeup = {} | posture {} | cascade TRIM->EXIT->KILL | registry {}",
        if armed { "ARMED" } else { "dry-run" },
        if have_full { format!("full_avg10>={:.0}% (PSI full)", full_trip) }
        else { format!("avg10>={:.0}% & free<={}MB (some+free fallback)", trip, free_floor) },
        if pkq.is_some() { "kqueue edge (/dev/pressure, 0-wakeup idle)" } else { "1s poll" },
        match fixed_posture { Some(p) => format!("pinned {}", p), None => "FOLLOW kern.sched.power_policy".into() },
        registry
    );

    let signal = |pid: i32, sig: i32| -> i32 {
        if armed { unsafe { libc::kill(pid, sig) } } else { 0 }
    };
    let dry = |s: &str| if armed { format!(" {}", s) } else { format!(" [dry-run:{}]", s) };

    let mut thrash_run = 0u32;
    let mut pending: Option<Pending> = None;
    loop {
        let avg10 = avg10_pct();
        let full = full_avg10_pct();
        let free = free_mib();
        let nstalled = sysctl_i32("kern.pressure.memory.nstalled").unwrap_or(0);
        let members = read_members(&registry);
        // Thrash gate = sustained-thrash confirmation AND still-stalled-right-now.
        // PSI `full` (when present) is the sustained signal — but it's a 10s decaying
        // average that LAGS, so on its own it keeps firing after pressure clears and
        // would reap innocents during the decay tail. Gate it with `nstalled > 0`
        // (instantaneous: zero the moment nobody is blocked) so reaping stops the
        // instant a kill relieves the pressure. Fallback (no `full`) = some+free-floor.
        let thrash = match full {
            Some(f) => f >= full_trip && nstalled > 0,
            None => avg10 >= trip && free <= free_floor,
        };
        // Re-read the posture each tick so a runtime change to kern.sched.power_policy
        // adapts memory reclaim live, in lock-step with CPU/GPU/display.
        let posture = fixed_posture.unwrap_or_else(live_posture);
        let tolerance = 1 + posture; // sustained thrash seconds before acting
        let grace = Duration::from_secs((1 + posture / 2) as u64); // per-tier escalation window

        match pending.take() {
            Some(mut p) => {
                if !alive(p.pid) {
                    eprintln!("RESOLVED pid={} tier={} name={} exited at {}", p.pid, p.tier, p.name, p.stage.label());
                    thrash_run = 0;
                } else if p.stage == Stage::Kill {
                    // SIGKILL sent; await teardown without re-signalling (the guard).
                    eprintln!("avg10={:.1}% free={}MB n={} awaiting-death pid={}", avg10, free, nstalled, p.pid);
                    pending = Some(p);
                } else if !thrash {
                    // pressure relieved at this tier — the cheapest sufficient action
                    // won (a TRIM that freed enough never has to become an EXIT/KILL).
                    eprintln!("SPARE pid={} tier={} name={} pressure cleared after {} — no escalation",
                        p.pid, p.tier, p.name, p.stage.label());
                    thrash_run = 0;
                } else if p.since.elapsed() >= grace {
                    match p.stage {
                        Stage::Trim => {
                            let r = signal(p.pid, libc::SIGTERM);
                            eprintln!("ESCALATE pid={} tier={} name={} trim insufficient -> EXIT(SIGTERM){}",
                                p.pid, p.tier, p.name, dry(&format!("rc={}", r)));
                            p.stage = Stage::Exit;
                        }
                        Stage::Exit => {
                            let r = signal(p.pid, libc::SIGKILL);
                            eprintln!("ESCALATE pid={} tier={} name={} ignored SIGTERM -> KILL(SIGKILL){}",
                                p.pid, p.tier, p.name, dry(&format!("rc={}", r)));
                            p.stage = Stage::Kill;
                        }
                        Stage::Kill => {}
                    }
                    p.since = Instant::now();
                    pending = Some(p);
                } else {
                    eprintln!("avg10={:.1}% free={}MB n={} await pid={} at {} ({:.0}s/{}s)",
                        avg10, free, nstalled, p.pid, p.stage.label(), p.since.elapsed().as_secs_f64(), grace.as_secs());
                    pending = Some(p);
                }
            }
            None => {
                if thrash { thrash_run += 1 } else { thrash_run = 0 }
                let fulls = full.map(|f| format!("{:.1}%", f)).unwrap_or_else(|| "n/a".into());
                eprintln!("full={} some={:.1}% free={}MB nstalled={} members={} posture={}(tol {}s) thrash_run={}{}",
                    fulls, avg10, free, nstalled, members.len(), posture, tolerance, thrash_run, if thrash { " [THRASH]" } else { "" });
                if thrash_run >= tolerance {
                    if !members.is_empty() {
                        // Victim = lowest lifecycle tier; within that tier, the
                        // largest RSS (frees the most, fastest — relief speed matters
                        // under thrash). NOT the largest RSS overall (that's the
                        // kernel's blunt vm_pageout_oom, which would hit the foreground).
                        let min_tier = members.iter().map(|m| m.tier).min().unwrap();
                        let v = members.iter().filter(|m| m.tier == min_tier)
                            .max_by_key(|m| rss_kb(m.pid)).unwrap().clone();
                        let oom = members.iter().max_by_key(|m| rss_kb(m.pid)).unwrap();
                        eprintln!("DECISION lmkd-tier vs largest-RSS-OOM: kernel OOM would kill {}(t{}, {}MB); memoryd sheds {}(t{}, {}MB) — sparing the foreground",
                            oom.name, oom.tier, mib(rss_kb(oom.pid)), v.name, v.tier, mib(rss_kb(v.pid)));
                        let r = signal(v.pid, libc::SIGINFO);
                        eprintln!("REAP pid={} tier={} name={} -> TRIM(SIGINFO, shed-not-die){}",
                            v.pid, v.tier, v.name, dry(&format!("rc={}", r)));
                        pending = Some(Pending { pid: v.pid, tier: v.tier, name: v.name.clone(), stage: Stage::Trim, since: Instant::now() });
                        thrash_run = 0;
                    } else {
                        eprintln!("thrash but no registered members to shed");
                    }
                }
            }
        }
        // Next tick. When idle (nothing in flight, no thrash accumulating), block on
        // the kernel pressure edge — zero wakeups until pressure crosses the trip.
        // While managing a victim or counting toward tolerance, keep the 1 s cadence
        // (the kernel re-KNOTEs each second of sustained pressure anyway, but the
        // timeout also covers a victim that dies while `full` has dipped below trip).
        match &pkq {
            Some(kq) if pending.is_none() && thrash_run == 0 => kq.wait(None),
            Some(kq) => kq.wait(Some(Duration::from_secs(1))),
            None => sleep(Duration::from_secs(1)),
        }
    }
}
