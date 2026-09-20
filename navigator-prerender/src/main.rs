//! Corpus driver: convert documents and report the numbers the profile needs.
//!
//! Usage: prerender <file-or-dir>...
//! The output is a measurement, not a rendering — see spec §11.4.

use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::{Fetcher, HttpFetcher, NoNetwork}};
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

fn collect(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() { collect(&e.path(), out); }
        }
    } else if p.extension().map(|e| e == "html" || e == "htm").unwrap_or(false) {
        out.push(p.to_path_buf());
    }
}

/// Convert exactly one document and print one machine-readable line. The
/// directory driver invokes this in a child process so a pathological
/// document cannot take the corpus run with it.
fn run_one(file: &str, base: Option<&str>, net: bool) -> ! {
    let src = fs::read_to_string(file).unwrap_or_default();
    let mut http = HttpFetcher::new(std::env::temp_dir().join("prerender-jscache"));
    let mut nonet = NoNetwork;
    let fetcher: &mut dyn Fetcher = if net { &mut http } else { &mut nonet };
    // The page's own network is separate from the one that loads its code,
    // and is only wired when the run is networked at all.
    let mut eng = BoaEngine {
        page_fetcher: if net {
            Some(Box::new(HttpFetcher::new(std::env::temp_dir().join("prerender-pagecache"))))
        } else { None },
        ..Default::default()
    };
    let c = convert_with(&src, base, &mut eng, fetcher);
    // fields the parent aggregates; errors last, tab-separated
    println!("R\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        c.elements_before, c.elements_after, c.depth_after,
        c.scripts_total, c.scripts_run, c.scripts_failed, c.script_mutations,
        c.external_total, c.external_fetched, c.module_retries,
        c.observers_registered);
    println!("L\t{}", c.layout_reads);
    println!("T\t{}\t{}", c.timers_fired, c.timers_dropped);
    println!("P\t{}\t{}", c.page_fetches, c.page_fetch_failures);
    println!("V\t{:?}", c.verdict);
    if let Some(f) = &c.first_error {
        println!("F\t{}", f.replace('\t', " ").replace('\n', " "));
    }
    for e in c.errors.iter().take(8) {
        println!("E\t{}", e.replace('\t', " ").replace('\n', " "));
    }
    for (name, n) in c.missing.iter() {
        println!("M\t{n}\t{name}");
    }
    for (name, n) in c.nulls.iter() {
        println!("N\t{n}\t{}", name.replace('\t', " "));
    }
    std::process::exit(0);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(|s| s == "--one").unwrap_or(false) {
        let file = args.get(1).cloned().unwrap_or_default();
        let base = args.get(2).filter(|s| !s.is_empty()).cloned();
        let net = std::env::var("PRERENDER_NET").ok().as_deref() == Some("1");
        run_one(&file, base.as_deref(), net);
    }
    if args.is_empty() { eprintln!("usage: prerender <file-or-dir>..."); std::process::exit(2); }
    let mut files = vec![];
    for a in &args { collect(Path::new(a), &mut files); }
    files.sort();

    // A manifest maps each saved file to the URL it came from, which is what
    // makes relative `src` resolvable. Without it the run is inline-only, and
    // says so rather than pretending to have measured external script.
    let mut base: BTreeMap<String, String> = BTreeMap::new();
    for a in &args {
        let m = Path::new(a).join("manifest.tsv");
        if let Ok(t) = fs::read_to_string(&m) {
            for line in t.lines() {
                if let Some((f, u)) = line.split_once('\t') {
                    base.insert(f.trim().to_string(), u.trim().to_string());
                }
            }
        }
    }
    let net = std::env::var("PRERENDER_NET").ok().as_deref() == Some("1");

    let exe = std::env::current_exe().expect("exe");
    let (mut with_js, mut js_ok, mut js_fail, mut mutated) = (0usize, 0usize, 0usize, 0usize);
    let (mut ext_total, mut ext_ok, mut ext_fail) = (0usize, 0usize, 0usize);
    let mut timed_out = 0usize;
    let mut mod_retries = 0u32;
    let mut observers = 0u32;
    let mut layout_reads = 0u32;
    let (mut t_fired, mut t_dropped, mut t_docs) = (0u32, 0u32, 0usize);
    let (mut pf, mut pff, mut pf_docs) = (0u32, 0u32, 0usize);
    let mut layout_docs = 0usize;
    let mut verdicts: BTreeMap<String, usize> = BTreeMap::new();
    let mut unattributed: Vec<String> = vec![];
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut missing: BTreeMap<String, u32> = BTreeMap::new();
    let mut nulls: BTreeMap<String, u32> = BTreeMap::new();
    let mut missing_docs: BTreeMap<String, usize> = BTreeMap::new();
    let mut elems = vec![];

    for f in &files {
        let Ok(src) = fs::read_to_string(f) else { continue };
        let _ = &src;
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let b = base.get(&name).cloned().unwrap_or_default();
        let Some(c) = convert_in_child(&exe, f, &b, net, DEADLINE) else {
            timed_out += 1;
            continue;
        };
        if c.scripts_total > 0 && !c.verdict.is_empty() {
            *verdicts.entry(c.verdict.clone()).or_default() += 1;
            if c.verdict == "Unknown" {
                if let Some(f) = &c.first_error {
                    unattributed.push(format!("{}\n        in {}",
                        f.chars().take(120).collect::<String>(), name));
                }
            }
        }
        mod_retries += c.module_retries;
        observers += c.observers;
        layout_reads += c.layout_reads;
        pf += c.page_fetches; pff += c.page_fetch_failures;
        if c.page_fetches > 0 { pf_docs += 1; }
        t_fired += c.timers_fired; t_dropped += c.timers_dropped;
        if c.timers_fired > 0 { t_docs += 1; }
        if c.layout_reads > 0 { layout_docs += 1; }
        ext_total += c.external_total;
        ext_ok += c.external_fetched;
        ext_fail += c.external_total - c.external_fetched;
        elems.push(c.elements_after);
        if c.scripts_total > 0 {
            with_js += 1;
            if c.scripts_failed == 0 { js_ok += 1 } else { js_fail += 1 }
            if c.script_mutations > 0 { mutated += 1 }
            for (name, n) in &c.nulls { *nulls.entry(name.clone()).or_default() += n; }
            for (name, n) in &c.missing {
                *missing.entry(name.clone()).or_default() += n;
                *missing_docs.entry(name.clone()).or_default() += 1;
            }
            for e in &c.errors {
                // bucket by the leading phrase so the report names the gap
                let key: String = e.split(':').take(2).collect::<Vec<_>>().join(":");
                *errors.entry(key.chars().take(90).collect()).or_default() += 1;
            }
        }
    }

    elems.sort();
    let pct = |q: f64| elems.get(((elems.len() as f64) * q) as usize).copied().unwrap_or(0);
    println!("documents            {}", files.len());
    if timed_out > 0 { println!("  TIMED OUT / crashed {timed_out}  (child killed at {DEADLINE:?})"); }
    println!("  with inline script {with_js}");
    println!("  scripts all ran    {js_ok}");
    println!("  some script failed {js_fail}");
    println!("  DOM actually changed by script {mutated}");
    if mod_retries > 0 { println!("  parsed as MODULE after a classic parse failed: {mod_retries}"); }
    if pf > 0 || pff > 0 {
        println!("  requests the PAGE made: {pf} in {pf_docs} docs ({pff} failed/refused)");
    }
    if t_fired > 0 || t_dropped > 0 {
        println!("  timer callbacks fired: {t_fired} in {t_docs} docs; {t_dropped} still pending at the horizon");
    }
    if layout_reads > 0 {
        println!("  layout metrics read (no layout exists — nominal values): {layout_reads} in {layout_docs} docs");
    }
    if observers > 0 {
        println!("  geometry observations registered, never delivered: {observers}  (no layout — see GLOBALS)");
    }
    println!("external scripts  referenced={ext_total} fetched={ext_ok} failed={ext_fail}{}",
        if net { "" } else { "   (network OFF — set PRERENDER_NET=1)" });

    if !elems.is_empty() {
        println!("elements  median={} p95={} max={}", pct(0.5), pct(0.95), elems.last().unwrap());
    }
    if !missing.is_empty() {
        println!("MOST-WANTED APIs (what scripts asked a host object for and did not get)");
        println!("  a miss is not automatically a gap: feature-detection probes deliberately");
        println!("  hit absent properties. Frequency still ranks the work.");
        let mut v: Vec<_> = missing.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (k, n) in v.into_iter().take(20) {
            println!("  {n:5} hits  {:2} docs  {k}", missing_docs.get(&k).copied().unwrap_or(0));
        }
    }
    if !verdicts.is_empty() {
        let g = |k: &str| verdicts.get(k).copied().unwrap_or(0);
        let (ours, browser, unknown, clean) =
            (g("OurGap"), g("BrowserToo"), g("Unknown"), g("Clean"));
        let scripted = ours + browser + unknown + clean;
        println!("VERDICT, by each document's FIRST failure (later ones may cascade)");
        println!("  clean                {clean}");
        println!("  OUR gap              {ours}");
        println!("  browser would fail too {browser}   <- NOT a conversion failure");
        println!("  unattributed         {unknown}");
        // ★ A RANGE, not a point estimate. The unattributed bucket is the
        // uncertainty, and quoting a single percentage would bury it — every
        // unattributed document is one that might be reachable or might be a
        // gap, and at this sample size there are enough of them to move the
        // answer by tens of points.
        let lo = if scripted > 0 { (clean + browser) * 100 / scripted } else { 0 };
        let hi = if scripted > 0 { (clean + browser + unknown) * 100 / scripted } else { 0 };
        println!("  -> tier-2 reachable: {lo}% to {hi}%  ({}/{scripted} known, +{unknown} unattributed)",
            clean + browser);
        if unknown == 0 {
            println!("  ★ every failure attributed; the spread is closed. Still a");
            println!("    heuristic, not a browser diff — a real control runs the same");
            println!("    page in a browser and compares DOMs.");
        } else {
            println!("  ★ heuristic, not a browser diff. The spread IS the uncertainty:");
            println!("    shrink it by attributing the unattributed, not by adding APIs.");
        }
    }
    if !unattributed.is_empty() {
        println!("UNATTRIBUTED first failures (the bucket that sets the spread)");
        for u in unattributed.iter().take(10) { println!("  {u}"); }
    }
    if !nulls.is_empty() {
        println!("LOOKUPS THAT FOUND NOTHING (an API we DO have, returning null)");
        println!("  the missing-API report cannot see this class: the property was");
        println!("  never missing. `-UNPARSEABLE` is our selector gap; `-no-match`");
        println!("  means the document genuinely lacks it.");
        let mut v: Vec<_> = nulls.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (k, n) in v.into_iter().take(15) { println!("  {n:5}  {k}"); }
    }
    if !errors.is_empty() {
        println!("script failures (symptoms; the lists above say what to build):");
        let mut v: Vec<_> = errors.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in v.into_iter().take(12) { println!("  {n:5}  {k}"); }
    }
}

const DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Run one document in a child process, killing it at the deadline.
fn convert_in_child(
    exe: &Path, file: &Path, base: &str, net: bool, deadline: std::time::Duration,
) -> Option<Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(exe);
    cmd.arg("--one").arg(file).arg(base).stdout(Stdio::piped()).stderr(Stdio::null());
    if net { cmd.env("PRERENDER_NET", "1"); }
    let mut ch = cmd.spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match ch.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > deadline => { let _ = ch.kill(); let _ = ch.wait(); return None; }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    let out = ch.wait_with_output().ok()?;
    parse_child(&String::from_utf8_lossy(&out.stdout))
}

/// What the parent needs back from a child run.
pub struct Child {
    pub elements_before: usize,
    pub elements_after: usize,
    pub scripts_total: usize,
    pub scripts_failed: usize,
    pub script_mutations: u64,
    pub external_total: usize,
    pub external_fetched: usize,
    pub errors: Vec<String>,
    pub missing: Vec<(String, u32)>,
    pub nulls: Vec<(String, u32)>,
    pub verdict: String,
    pub first_error: Option<String>,
    pub module_retries: u32,
    pub observers: u32,
    pub layout_reads: u32,
    pub timers_fired: u32,
    pub timers_dropped: u32,
    pub page_fetches: u32,
    pub page_fetch_failures: u32,
}

fn parse_child(s: &str) -> Option<Child> {
    let mut c = Child { elements_before: 0, elements_after: 0, scripts_total: 0,
        scripts_failed: 0, script_mutations: 0, external_total: 0, external_fetched: 0,
        errors: vec![], missing: vec![], nulls: vec![], verdict: String::new(),
        first_error: None,
        module_retries: 0, observers: 0, layout_reads: 0,
        timers_fired: 0, timers_dropped: 0, page_fetches: 0, page_fetch_failures: 0 };
    let mut saw = false;
    for line in s.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.first() {
            Some(&"R") if f.len() >= 10 => {
                saw = true;
                c.elements_before = f[1].parse().unwrap_or(0);
                c.elements_after = f[2].parse().unwrap_or(0);
                c.scripts_total = f[4].parse().unwrap_or(0);
                c.scripts_failed = f[6].parse().unwrap_or(0);
                c.script_mutations = f[7].parse().unwrap_or(0);
                c.external_total = f[8].parse().unwrap_or(0);
                c.external_fetched = f[9].parse().unwrap_or(0);
                if f.len() >= 11 { c.module_retries = f[10].parse().unwrap_or(0); }
                if f.len() >= 12 { c.observers = f[11].parse().unwrap_or(0); }
            }
            Some(&"E") if f.len() >= 2 => c.errors.push(f[1].to_string()),
            Some(&"M") if f.len() >= 3 => {
                c.missing.push((f[2].to_string(), f[1].parse().unwrap_or(1)));
            }
            Some(&"L") if f.len() >= 2 => c.layout_reads = f[1].parse().unwrap_or(0),
            Some(&"P") if f.len() >= 3 => {
                c.page_fetches = f[1].parse().unwrap_or(0);
                c.page_fetch_failures = f[2].parse().unwrap_or(0);
            }
            Some(&"T") if f.len() >= 3 => {
                c.timers_fired = f[1].parse().unwrap_or(0);
                c.timers_dropped = f[2].parse().unwrap_or(0);
            }
            Some(&"V") if f.len() >= 2 => c.verdict = f[1].to_string(),
            Some(&"F") if f.len() >= 2 => c.first_error = Some(f[1].to_string()),
            Some(&"N") if f.len() >= 3 => {
                c.nulls.push((f[2].to_string(), f[1].parse().unwrap_or(1)));
            }
            _ => {}
        }
    }
    saw.then_some(c)
}
