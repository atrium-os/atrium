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

/// Read a string sysctl (two-call: size, then fill).
fn sysctl_string(name: &str) -> Result<String, ()> {
    let c = CString::new(name).map_err(|_| ())?;
    let mut len: libc::size_t = 0;
    unsafe {
        if libc::sysctlbyname(c.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 || len == 0 {
            return Err(());
        }
        let mut buf = vec![0u8; len];
        if libc::sysctlbyname(c.as_ptr(), buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut(), 0) != 0 {
            return Err(());
        }
        buf.truncate(len.saturating_sub(1)); // drop the trailing NUL
        String::from_utf8(buf).map_err(|_| ())
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
    jid: Option<i32>,       // the jail the member runs in (4th registry field)
    jail: Option<String>,   // the jail's NAME (5th field) — the GovernReap target in --broker mode
}

/// Per-jail `full` stall (ns) from `kern.pressure.memory.jails`, keyed by jid. The
/// thrash CULPRIT is the jail whose `full_ns` is climbing fastest — the one that is
/// locally stalled, not merely the one using the most RAM. Used to bias the
/// within-tier tie-break toward the jail actually causing the pressure.
fn jail_full_ns() -> std::collections::HashMap<i32, u64> {
    let mut m = std::collections::HashMap::new();
    if let Ok(s) = sysctl_string("kern.pressure.memory.jails") {
        for line in s.lines() {
            // "jail <jid> some_ns=<n> full_ns=<n>"
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 && f[0] == "jail" {
                if let Ok(jid) = f[1].parse::<i32>() {
                    if let Some(full) = f[3].strip_prefix("full_ns=").and_then(|v| v.parse().ok()) {
                        m.insert(jid, full);
                    }
                }
            }
        }
    }
    m
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
                        // optional 4th field = the member's jail id, 5th = the
                        // jail NAME (Portcullis supplies both); absent for bare
                        // host-process members (then --broker can't reap them).
                        let jid = f.get(3).and_then(|s| s.parse().ok());
                        let jail = f.get(4).map(|s| s.to_string());
                        v.push(Member { pid, tier, name: f.get(2).unwrap_or(&"?").to_string(), jid, jail });
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
    jail: Option<String>,   // the victim's jail name, for --broker escalation
    stage: Stage,
    since: Instant,
}

/// Reap a jail through portcullisd (which capability-checks us, then forwards to
/// jaild) — the jailed-governor act path. Connect, Hello, GovernReap. Returns 0 on
/// Ok, nonzero on any failure (logged). atrium-memory-pressure.md §9.5.
fn broker_reap(sock: &str, jail: &str, sig: portcullis_ipc::GovSignal) -> i32 {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    use std::os::unix::net::UnixStream;
    let mut s = match UnixStream::connect(sock) {
        Ok(s) => s,
        Err(e) => { eprintln!("broker connect {sock}: {e}"); return 1; }
    };
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => {}
        other => { eprintln!("broker handshake: {other:?}"); return 2; }
    }
    let req = Request::GovernReap { jail_name: jail.to_string(), signal: sig };
    match round_trip(&mut s, &req) {
        Ok(Response::Ok) => 0,
        Ok(Response::Error { message }) => { eprintln!("broker reap rejected: {message}"); 3 }
        Ok(other) => { eprintln!("broker reap unexpected: {other:?}"); 4 }
        Err(e) => { eprintln!("broker reap io: {e}"); 5 }
    }
}

/// Map a cascade signal number to the bounded GovSignal the broker accepts.
fn gov_signal(sig: i32) -> portcullis_ipc::GovSignal {
    match sig {
        libc::SIGINFO => portcullis_ipc::GovSignal::Trim,
        libc::SIGTERM => portcullis_ipc::GovSignal::Exit,
        _ => portcullis_ipc::GovSignal::Kill,
    }
}

/// One per-jail entry in a PRESSURE_GET snapshot. Layout MUST match
/// `struct pressure_jail_stat` in <sys/pressure.h> (averages in basis points).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PressureJailStat {
    jid: i32,
    full_avg10: u32,
    full_avg60: u32,
    full_avg300: u32,
    some_ns: u64,
    full_ns: u64,
}

const PRESSURE_MAX_JAILS: usize = 16;

/// The complete pressure state, read in one PRESSURE_GET ioctl on /dev/pressure —
/// the jailed-governor read path (sysctls aren't reachable from inside a jail).
/// Layout MUST match `struct pressure_snapshot` in <sys/pressure.h>.
#[repr(C)]
struct PressureSnapshot {
    some_ns: u64,
    full_ns: u64,
    some_avg10: u32,
    some_avg60: u32,
    some_avg300: u32,
    full_avg10: u32,
    full_avg60: u32,
    full_avg300: u32,
    nstalled: i32,
    njails: u32,
    jails: [PressureJailStat; PRESSURE_MAX_JAILS],
}

/// `_IOR('P', 1, struct pressure_snapshot)`, computed from the FreeBSD ioctl
/// encoding so it tracks the struct size automatically.
const fn pressure_get_ioctl() -> libc::c_ulong {
    const IOC_OUT: libc::c_ulong = 0x4000_0000;
    const IOCPARM_MASK: libc::c_ulong = 0x1fff;
    let size = std::mem::size_of::<PressureSnapshot>() as libc::c_ulong;
    IOC_OUT | ((size & IOCPARM_MASK) << 16) | ((b'P' as libc::c_ulong) << 8) | 1
}

/// kqueue edge-trigger on `/dev/pressure` (the kernel's PSI poll/trigger,
/// kern_pressure.c): register an EVFILT_READ knote whose `data` carries the `full`
/// threshold in basis points (fraction ×10000 — 40% = 4000). The kernel KNOTEs it
/// each aggregation tick, so we sleep in `kevent` with ZERO wakeups until pressure
/// crosses the threshold — no 1 Hz poll while idle ([[feedback_kqueue_native]]).
/// The same fd serves PRESSURE_GET (the full snapshot) — one granted device.
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

    /// Read the full pressure snapshot via PRESSURE_GET. This is the jailed read
    /// path: everything (global + per-jail) from this one granted device, no host
    /// sysctl. `None` if the ioctl fails (caller falls back to sysctls).
    fn snapshot(&self) -> Option<PressureSnapshot> {
        let mut s: PressureSnapshot = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::ioctl(self.fd, pressure_get_ioctl(), &mut s as *mut PressureSnapshot)
        };
        if r < 0 { None } else { Some(s) }
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

    // --broker <sock>: reap through portcullisd -> jaild (the jailed governor can't
    // signal across jail PID namespaces, so it brokers). Default = direct kill (the
    // v1 host-side bring-up). A victim without a jail name falls back to direct kill.
    let broker: Option<String> = has("--broker")
        .then(|| arg("--broker", "/atrium/sockets/portcullis.sock"));
    eprintln!("memoryd: act = {}",
        match &broker {
            Some(s) => format!("broker via portcullisd {s} (jailed-governor path)"),
            None => "direct kill(2) (v1 host-side)".into(),
        });
    let signal = |pid: i32, jail: Option<&str>, sig: i32| -> i32 {
        if !armed {
            return 0;
        }
        match (&broker, jail) {
            (Some(sock), Some(j)) => broker_reap(sock, j, gov_signal(sig)),
            _ => unsafe { libc::kill(pid, sig) },
        }
    };
    let dry = |s: &str| if armed { format!(" {}", s) } else { format!(" [dry-run:{}]", s) };

    let mut thrash_run = 0u32;
    let mut pending: Option<Pending> = None;
    let mut prev_jail_full: std::collections::HashMap<i32, u64> = std::collections::HashMap::new();
    loop {
        // Primary read path: one PRESSURE_GET ioctl on /dev/pressure delivers the
        // whole state (global + per-jail) — the jailed governor never touches a host
        // sysctl. Fall back to the sysctls when the device snapshot is unavailable
        // (older kernel, or /dev/pressure not in the ruleset).
        let snap = pkq.as_ref().and_then(|kq| kq.snapshot());
        let avg10 = snap.as_ref().map_or_else(avg10_pct, |s| s.some_avg10 as f64 / 100.0);
        let full = match &snap {
            Some(s) => Some(s.full_avg10 as f64 / 100.0),
            None => full_avg10_pct(),
        };
        let free = free_mib(); // not pressure data — always a vm stat
        let nstalled = snap.as_ref().map_or_else(
            || sysctl_i32("kern.pressure.memory.nstalled").unwrap_or(0),
            |s| s.nstalled);
        let members = read_members(&registry);
        // The culprit jail = the one whose per-jail `full` climbed most since the
        // last tick (locally stalled, not merely RAM-hungry). Used only to bias the
        // within-tier tie-break — tier still dominates. None if no jail is thrashing.
        let cur_jail_full = match &snap {
            Some(s) => s.jails[..s.njails as usize].iter()
                .map(|j| (j.jid, j.full_ns)).collect(),
            None => jail_full_ns(),
        };
        let culprit: Option<i32> = cur_jail_full.iter()
            .map(|(&jid, &f)| (jid, f.saturating_sub(*prev_jail_full.get(&jid).unwrap_or(&0))))
            .filter(|&(_, delta)| delta > 0)
            .max_by_key(|&(_, delta)| delta)
            .map(|(jid, _)| jid);
        prev_jail_full = cur_jail_full;
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
                            let r = signal(p.pid, p.jail.as_deref(), libc::SIGTERM);
                            eprintln!("ESCALATE pid={} tier={} name={} trim insufficient -> EXIT(SIGTERM){}",
                                p.pid, p.tier, p.name, dry(&format!("rc={}", r)));
                            p.stage = Stage::Exit;
                        }
                        Stage::Exit => {
                            let r = signal(p.pid, p.jail.as_deref(), libc::SIGKILL);
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
                eprintln!("full={} some={:.1}% free={}MB nstalled={} members={} culprit={:?} posture={}(tol {}s) thrash_run={}{}",
                    fulls, avg10, free, nstalled, members.len(), culprit, posture, tolerance, thrash_run, if thrash { " [THRASH]" } else { "" });
                if thrash_run >= tolerance {
                    if !members.is_empty() {
                        // Victim = lowest lifecycle tier (tier dominates — never shed
                        // a higher-tier app to spare a lower one). Within that tier,
                        // bias toward a member of the CULPRIT jail (the one actually
                        // thrashing, per per-jail `full`) — shedding it addresses the
                        // cause, not an innocent member of a healthy jail. Failing
                        // that, the largest RSS (frees the most, fastest). NEVER the
                        // largest RSS overall (the kernel's blunt vm_pageout_oom).
                        let min_tier = members.iter().map(|m| m.tier).min().unwrap();
                        let culprit_in_tier = culprit.is_some()
                            && members.iter().any(|m| m.tier == min_tier && m.jid == culprit);
                        let v = members.iter()
                            .filter(|m| m.tier == min_tier && (!culprit_in_tier || m.jid == culprit))
                            .max_by_key(|m| rss_kb(m.pid)).unwrap().clone();
                        if culprit_in_tier {
                            eprintln!("ATTRIBUTION culprit jail {} (highest per-jail full delta); biasing the tier-{} tie-break toward it",
                                culprit.unwrap(), min_tier);
                        }
                        let oom = members.iter().max_by_key(|m| rss_kb(m.pid)).unwrap();
                        eprintln!("DECISION lmkd-tier vs largest-RSS-OOM: kernel OOM would kill {}(t{}, {}MB); memoryd sheds {}(t{}, {}MB) — sparing the foreground",
                            oom.name, oom.tier, mib(rss_kb(oom.pid)), v.name, v.tier, mib(rss_kb(v.pid)));
                        let r = signal(v.pid, v.jail.as_deref(), libc::SIGINFO);
                        eprintln!("REAP pid={} tier={} name={} -> TRIM(SIGINFO, shed-not-die){}",
                            v.pid, v.tier, v.name, dry(&format!("rc={}", r)));
                        pending = Some(Pending { pid: v.pid, tier: v.tier, name: v.name.clone(), jail: v.jail.clone(), stage: Stage::Trim, since: Instant::now() });
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
