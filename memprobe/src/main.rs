//! memprobe — how much would compressed RAM actually buy on this workload?
//!
//! The evidence step for hardening `atrium_zram.ko` (docs/spec/atrium-zram-v2.md).
//! The store's first verification ("1.42 GB held in 8 MB, ~175x") used a hog
//! whose pages were mostly zero, which says nothing about Navigator tabs or a
//! build. memprobe reads the resident anonymous pages of real processes and
//! reports, per process and in total:
//!
//!   * zero / same-filled / duplicate fractions;
//!   * the stored size under LZ4 and zstd (levels 1 and 3, the kernel's level),
//!     both ideal and under the allocator the store really uses (FreeBSD
//!     malloc(9) zones today, a packed 32-byte-class allocator proposed);
//!   * per-page compress and decompress time on this CPU;
//!   * a histogram of compressed sizes (sizes the allocator's classes).
//!
//! `watch` samples system memory and pressure (PSI, swap, zram counters) as CSV
//! while a workload runs, optionally taking snapshots of matching processes
//! along the way.
//!
//! Usage:
//!   memprobe snapshot [--pid N]... [--name SUBSTR]... [--all] [--sample N]
//!                     [--min-mb M] [--hist] [--fault-partial]
//!   memprobe watch [--interval S] [--out FILE.csv] [--probe-every S
//!                  --name SUBSTR...] [--sample N] [-- CMD ARGS...]
//!
//! Needs permission to read the targets' memory (root, or same uid with
//! ptrace allowed).

mod analysis;
mod platform;
mod watch;

use analysis::{Analyzer, Stats, HIST_BUCKET, HIST_LEN, PAGE};
use std::io::Write;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Opts {
    pids: Vec<i32>,
    names: Vec<String>,
    all: bool,
    sample: u64,
    min_mb: f64,
    hist: bool,
    fault_partial: bool,
    interval: f64,
    out: Option<String>,
    probe_every: Option<f64>,
    cmd: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { sample: 1, interval: 1.0, min_mb: 1.0, ..Default::default() };
    let mut i = 0;
    let val = |i: &mut usize| -> Result<String, String> {
        *i += 1;
        args.get(*i).cloned().ok_or_else(|| format!("{} needs a value", args[*i - 1]))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--pid" => o.pids.push(val(&mut i)?.parse().map_err(|e| format!("--pid: {e}"))?),
            "--name" => o.names.push(val(&mut i)?),
            "--all" => o.all = true,
            "--sample" => o.sample = val(&mut i)?.parse().map_err(|e| format!("--sample: {e}"))?,
            "--min-mb" => o.min_mb = val(&mut i)?.parse().map_err(|e| format!("--min-mb: {e}"))?,
            "--hist" => o.hist = true,
            "--fault-partial" => o.fault_partial = true,
            "--interval" => o.interval = val(&mut i)?.parse().map_err(|e| format!("--interval: {e}"))?,
            "--out" => o.out = Some(val(&mut i)?),
            "--probe-every" => o.probe_every = Some(val(&mut i)?.parse().map_err(|e| format!("--probe-every: {e}"))?),
            "--" => {
                o.cmd = args[i + 1..].to_vec();
                break;
            }
            a => return Err(format!("unknown argument {a:?}")),
        }
        i += 1;
    }
    Ok(o)
}

fn targets(o: &Opts) -> Vec<(i32, String)> {
    let me = std::process::id() as i32;
    let procs = platform::list_procs();
    procs
        .into_iter()
        .filter(|(pid, name)| {
            *pid != me
                && (o.all || o.pids.contains(pid) || o.names.iter().any(|n| name.contains(n.as_str())))
        })
        .collect()
}

struct ProcResult {
    pid: i32,
    name: String,
    stats: Stats,
    cov: platform::Coverage,
}

fn snapshot(o: &Opts, an: &mut Analyzer) -> (Vec<ProcResult>, Vec<String>) {
    let ropts = platform::ReadOpts { sample: o.sample, fault_partial: o.fault_partial };
    let mut res = Vec::new();
    let mut errs = Vec::new();
    for (pid, name) in targets(o) {
        let mut st = Stats::new();
        match platform::for_each_anon_page(pid, &ropts, &mut |p| an.page(&mut st, p)) {
            Ok(cov) => {
                // Pages the read faulted in were never touched: they read as
                // zero and were not resident before, so they are not part of
                // what the store would see. (Estimate: exact with no swap.)
                let f = (cov.faulted / PAGE as u64).min(st.zero);
                st.zero -= f;
                st.pages -= f;
                res.push(ProcResult { pid, name, stats: st, cov })
            }
            Err(e) => errs.push(format!("{pid} {name}: {e}")),
        }
    }
    (res, errs)
}

fn pct(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        100.0 * a as f64 / b as f64
    }
}

fn ratio(logical: u64, phys: u64) -> String {
    if logical == 0 {
        "-".into()
    } else if phys == 0 {
        "inf".into()
    } else {
        format!("{:.2}x", logical as f64 / phys as f64)
    }
}

fn mb(b: u64) -> f64 {
    b as f64 / 1048576.0
}

fn report(o: &Opts, res: &[ProcResult], w: &mut dyn Write) -> std::io::Result<()> {
    let mut total = Stats::new();
    let mut cov = platform::Coverage::default();
    let codecs: Vec<&str> = total.codecs.iter().map(|(c, _)| c.name()).collect();

    write!(w, "{:>7} {:<18} {:>9} {:>6} {:>6} {:>6}", "pid", "name", "anon_MB", "zero%", "same%", "dup%")?;
    for c in &codecs {
        write!(w, " {:>8}", c)?;
    }
    writeln!(w)?;
    let mut shown = 0;
    for r in res {
        total.merge(&r.stats);
        cov.read += r.cov.read;
        cov.skipped += r.cov.skipped;
        cov.swapped += r.cov.swapped;
        cov.faulted += r.cov.faulted;
        if mb(r.stats.logical_bytes()) < o.min_mb {
            continue;
        }
        shown += 1;
        let s = &r.stats;
        write!(
            w,
            "{:>7} {:<18} {:>9.1} {:>6.1} {:>6.1} {:>6.1}",
            r.pid,
            r.name.chars().take(18).collect::<String>(),
            mb(s.logical_bytes()),
            pct(s.zero, s.pages),
            pct(s.same, s.pages),
            pct(s.dup, s.pages)
        )?;
        for (_, cs) in &s.codecs {
            write!(w, " {:>8}", ratio(s.logical_bytes(), cs.bytes_packed))?;
        }
        writeln!(w)?;
    }
    if shown < res.len() {
        writeln!(w, "  ({} smaller processes under {} MB not listed, included in TOTAL)", res.len() - shown, o.min_mb)?;
    }

    let t = &total;
    writeln!(w)?;
    writeln!(
        w,
        "TOTAL {} processes, {:.1} MB resident anon read | zero {:.1}% same {:.1}% dup {:.1}%",
        res.len(),
        mb(t.logical_bytes()),
        pct(t.zero, t.pages),
        pct(t.same, t.pages),
        pct(t.dup, t.pages)
    )?;
    writeln!(
        w,
        "coverage: read {:.1} MB, skipped {:.1} MB (partially resident, FreeBSD), already swapped {:.1} MB, faulted-in zero {:.1} MB (excluded)",
        mb(cov.read),
        mb(cov.skipped),
        mb(cov.swapped),
        mb(cov.faulted)
    )?;
    if o.sample > 1 {
        writeln!(w, "sampled 1 page in {}", o.sample)?;
    }

    let non_same = t.pages - t.zero - t.same;
    writeln!(w)?;
    writeln!(w, "Effective ratio = logical / physical. Same-filled pages cost 0 bytes in every column;")?;
    writeln!(w, "'excl same' leaves them out, which is the honest number for pages that carry data.")?;
    writeln!(
        w,
        "{:<8} {:>8} {:>8} {:>9} {:>10} {:>6} {:>9} {:>9}",
        "codec", "ideal", "packed", "malloc9", "excl same", "raw%", "comp_us", "decomp_us"
    )?;
    for (c, cs) in &t.codecs {
        let per = |ns: u64| if cs.pages == 0 { 0.0 } else { ns as f64 / cs.pages as f64 / 1000.0 };
        writeln!(
            w,
            "{:<8} {:>8} {:>8} {:>9} {:>10} {:>6.1} {:>9.2} {:>9.2}",
            c.name(),
            ratio(t.logical_bytes(), cs.bytes_ideal),
            ratio(t.logical_bytes(), cs.bytes_packed),
            ratio(t.logical_bytes(), cs.bytes_malloc9),
            ratio(non_same * PAGE as u64, cs.bytes_packed),
            pct(cs.raw, cs.pages),
            per(cs.comp_ns),
            per(cs.decomp_ns)
        )?;
        if cs.errors > 0 {
            writeln!(w, "  !! {} round-trip mismatches in {}", cs.errors, c.name())?;
        }
    }
    let malloc_waste = |cs: &analysis::CodecStats| pct(cs.bytes_malloc9 - cs.bytes_ideal, cs.bytes_malloc9);
    for (c, cs) in &t.codecs {
        writeln!(w, "  {}: malloc(9) rounding wastes {:.1}% of the store; packed wastes {:.1}%", c.name(), malloc_waste(cs), pct(cs.bytes_packed - cs.bytes_ideal, cs.bytes_packed))?;
    }

    if o.hist {
        writeln!(w)?;
        writeln!(w, "Compressed-size histogram (% of codec pages, {HIST_BUCKET}-byte buckets; last = RAW):")?;
        write!(w, "{:<10}", "bytes<")?;
        for (c, _) in &t.codecs {
            write!(w, " {:>8}", c.name())?;
        }
        writeln!(w)?;
        for b in 0..HIST_LEN {
            let label = if b == HIST_LEN - 1 { "RAW".to_string() } else { format!("{}", (b + 1) * HIST_BUCKET) };
            write!(w, "{:<10}", label)?;
            for (_, cs) in &t.codecs {
                write!(w, " {:>8.1}", pct(cs.hist[b], cs.pages))?;
            }
            writeln!(w)?;
        }
    }
    Ok(())
}

fn cmd_snapshot(o: &Opts) -> ExitCode {
    if !o.all && o.pids.is_empty() && o.names.is_empty() {
        eprintln!("snapshot: choose targets with --pid, --name or --all");
        return ExitCode::from(2);
    }
    let mut an = Analyzer::new();
    let t0 = Instant::now();
    let (res, errs) = snapshot(o, &mut an);
    let out = std::io::stdout();
    let mut w = out.lock();
    let _ = report(o, &res, &mut w);
    let _ = writeln!(w, "\n(probe took {:.1} s)", t0.elapsed().as_secs_f64());
    for e in &errs {
        eprintln!("skipped {e}");
    }
    if res.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_watch(o: &Opts) -> ExitCode {
    let mut csv: Box<dyn Write> = match &o.out {
        Some(p) => match std::fs::File::create(p) {
            Ok(f) => Box::new(f),
            Err(e) => {
                eprintln!("watch: {p}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => Box::new(std::io::stdout()),
    };
    let mut child = if o.cmd.is_empty() {
        None
    } else {
        match Command::new(&o.cmd[0]).args(&o.cmd[1..]).spawn() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("watch: {}: {e}", o.cmd[0]);
                return ExitCode::FAILURE;
            }
        }
    };
    let _ = writeln!(csv, "{}", watch::CSV_HEADER);
    let t0 = Instant::now();
    let mut next_probe = o.probe_every.map(|p| t0 + Duration::from_secs_f64(p));
    let mut peak = watch::Sample::default();
    let max = |a: &mut Option<f64>, b: Option<f64>| {
        if let Some(b) = b {
            *a = Some(a.map_or(b, |a| a.max(b)));
        }
    };
    let mut probes = 0;
    loop {
        let s = watch::sample();
        let _ = writeln!(csv, "{}", s.csv(t0.elapsed().as_secs_f64()));
        let _ = csv.flush();
        max(&mut peak.swap_used_mb, s.swap_used_mb);
        max(&mut peak.psi_some_avg10, s.psi_some_avg10);
        max(&mut peak.psi_full_avg10, s.psi_full_avg10);
        max(&mut peak.anon_active_mb, s.anon_active_mb);
        if let Some(f) = s.avail_mb {
            peak.avail_mb = Some(peak.avail_mb.map_or(f, |a: f64| a.min(f)));
        }
        if let Some(np) = next_probe {
            if Instant::now() >= np && !o.names.is_empty() {
                probes += 1;
                let mut an = Analyzer::new();
                let (res, _) = snapshot(o, &mut an);
                eprintln!("\n=== probe {probes} at t={:.0}s ===", t0.elapsed().as_secs_f64());
                let _ = report(o, &res, &mut std::io::stderr());
                next_probe = Some(Instant::now() + Duration::from_secs_f64(o.probe_every.unwrap()));
            }
        }
        if let Some(c) = child.as_mut() {
            if let Ok(Some(st)) = c.try_wait() {
                eprintln!(
                    "\nworkload exited ({st}) after {:.1} s | min avail {:.0} MB, peak active {:.0} MB, peak swap {:.0} MB, peak PSI some/full avg10 {:.1}%/{:.1}%",
                    t0.elapsed().as_secs_f64(),
                    peak.avail_mb.unwrap_or(f64::NAN),
                    peak.anon_active_mb.unwrap_or(f64::NAN),
                    peak.swap_used_mb.unwrap_or(f64::NAN),
                    peak.psi_some_avg10.unwrap_or(f64::NAN),
                    peak.psi_full_avg10.unwrap_or(f64::NAN)
                );
                return if st.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
            }
        }
        std::thread::sleep(Duration::from_secs_f64(o.interval));
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: memprobe snapshot|watch [options]  (see the header of src/main.rs)";
    let Some(sub) = args.get(1) else {
        eprintln!("{usage}");
        return ExitCode::from(2);
    };
    let o = match parse(&args[2..]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n{usage}");
            return ExitCode::from(2);
        }
    };
    match sub.as_str() {
        "snapshot" => cmd_snapshot(&o),
        "watch" => cmd_watch(&o),
        _ => {
            eprintln!("{usage}");
            ExitCode::from(2)
        }
    }
}
