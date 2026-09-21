//! Corpus driver: convert documents and report the numbers the profile needs.
//!
//! Usage: prerender <file-or-dir>...
//! The output is a measurement, not a rendering — see spec §11.4.

use navigator_prerender::{boa_impl::BoaEngine, convert_with, fetch::{Fetcher, HttpFetcher, NoNetwork}};
use std::{collections::BTreeMap, fs, path::{Path, PathBuf}};

/// Name the cause from the message, for failures that recorded no event.
fn classify_message(msg: &str) -> String {
    if let Some(rest) = msg.strip_prefix("ReferenceError: ") {
        if let Some(name) = rest.split(" is not defined").next() {
            if !name.is_empty() && name.len() < 60 {
                return format!("missing global {name}");
            }
        }
    }
    if msg.starts_with("SyntaxError: unexpected token '<'") {
        // ★ DO NOT CALL THIS "the body is HTML". I did, confidently, and it
        // was wrong for every document it matched: they were classic scripts
        // wrapped in the legacy `<!-- ... //-->` idiom, which is
        // standardised JavaScript (Annex B.1.1) that the engine was simply
        // not configured to accept. Enabling boa's `annex-b` feature cleared
        // all of them. What remains under this message is genuinely
        // unidentified, so it says so.
        return "SyntaxError at '<' (unidentified)".to_string();
    }
    if msg.contains("not supported in this browser") {
        return "environment probe rejected us".to_string();
    }
    if msg.starts_with("SyntaxError") { return "SyntaxError (parse)".to_string() }
    if msg.contains("could not open file") || msg.contains("bare module specifier") {
        return "module resolution".to_string()
    }
    "UNATTRIBUTED".to_string()
}

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
    // PRERENDER_DUMP=1 prints the converted artifact itself, for inspecting
    // a single document by hand. The parent never sets it.
    if std::env::var("PRERENDER_DUMP").ok().as_deref() == Some("1") {
        eprintln!("{}", c.html);
    }
    // fields the parent aggregates; errors last, tab-separated
    println!("R\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        c.elements_before, c.elements_after, c.depth_after,
        c.scripts_total, c.scripts_run, c.scripts_failed, c.script_mutations,
        c.external_total, c.external_fetched, c.module_retries,
        c.observers_registered);
    println!("L\t{}", c.layout_reads);
    println!("O\t{}\t{}\t{}", c.observers_registered, c.mutation_records, c.ce_upgrades);
    println!("H\t{}\t{}", c.history_writes, c.history_refused);
    println!("D\t{}\t{}", c.events_dispatched, c.event_listeners_run);
    println!("W\t{}\t{}", c.doc_writes, c.doc_writes_refused);
    println!("J\t{}\t{}", c.injected_scripts_run, c.injected_scripts_refused);
    println!("X\t{}\t{}", c.text_before, c.text_after);
    println!("T\t{}\t{}", c.timers_fired, c.timers_dropped);
    println!("P\t{}\t{}\t{}\t{}", c.page_fetches, c.page_fetch_failures,
        c.page_blocked, c.beacons_suppressed);
    for (k, n) in c.blocked_hosts.iter() { println!("B\t{n}\t{k}"); }
    println!("V\t{:?}", c.verdict);
    // The PROXIMATE CAUSE of the first failure: the last recorded event
    // before the throw. A symptom bucket like "null or undefined" is useless
    // without it — the message names what broke, this names why.
    if let Some((kind, what)) = &c.cause {
        println!("C\t{kind}\t{}", what.replace('\t', " "));
    }
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
    let mut mo_records = 0u32;
    let mut ce_up = 0u32;
    let mut hist_w = 0u32; let mut hist_r = 0u32; let mut hist_docs = 0usize;
    let mut ev_d = 0u32; let mut ev_l = 0u32; let mut ev_docs = 0usize;
    let mut dw = 0u32; let mut dwr = 0u32; let mut dw_docs = 0usize;
    let mut inj = 0u32; let mut injr = 0u32; let mut inj_docs = 0usize;
    let mut ce_docs = 0usize;
    let mut mo_docs = 0usize;
    let (mut t_fired, mut t_dropped, mut t_docs) = (0u32, 0u32, 0usize);
    let (mut pf, mut pff, mut pf_docs) = (0u32, 0u32, 0usize);
    let (mut pblk, mut pbeac) = (0u32, 0u32);
    let mut blocked_where: BTreeMap<String, u32> = BTreeMap::new();
    let mut layout_docs = 0usize;
    let mut verdicts: BTreeMap<String, usize> = BTreeMap::new();
    let mut unattributed: Vec<String> = vec![];
    // First failures bucketed by PROXIMATE CAUSE rather than by message.
    let mut causes: BTreeMap<String, usize> = BTreeMap::new();
    let mut cause_docs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    let mut missing: BTreeMap<String, u32> = BTreeMap::new();
    let mut nulls: BTreeMap<String, u32> = BTreeMap::new();
    let mut missing_docs: BTreeMap<String, usize> = BTreeMap::new();
    let mut elems = vec![];
    let mut gains: Vec<(i64, String)> = vec![];

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
        // ★ Attribute the first failure by CAUSE. The message buckets are
        // symptoms: 51 occurrences of "cannot convert null or undefined to
        // object" is one sentence covering ten unrelated gaps, and reading it
        // as a single item is what sends you building the wrong thing.
        if c.first_error.is_some() {
            let msg = c.first_error.as_deref().unwrap_or("");
            // ★ A ReferenceError NAMES ITS OWN CAUSE, and the message beats
            // the event log. The proximate cause is the last thing recorded
            // before the throw, which for a bare global lookup is whatever
            // harmless probe ran just before it — that is how `window.$`
            // and `window.CSS` kept surfacing as causes for documents whose
            // real failures were `pageYOffset` and `File`. When the message
            // is this specific, nothing else is better evidence.
            let key = if msg.starts_with("ReferenceError: ") {
                classify_message(msg)
            } else { match &c.cause {
                Some((k, what)) if k == "missing" => format!("missing {what}"),
                Some((k, what)) if k == "no-match" => format!("no-match {what}"),
                Some((k, what)) => format!("{k} {what}"),
                // ★ No recorded event does NOT mean unattributed. A bare
                // global lookup never touches a host object, so nothing is
                // logged — but the MESSAGE names the cause outright. Reading
                // these as "unattributed" overstated the unknown by 8 docs.
                None => classify_message(msg),
            } };
            *causes.entry(key.clone()).or_default() += 1;
            cause_docs.entry(key).or_default().push(name.clone());
        }
        mod_retries += c.module_retries;
        observers += c.observers;
        layout_reads += c.layout_reads;
        mo_records += c.mutation_records;
        ce_up += c.ce_upgrades;
        inj += c.injected_scripts_run; injr += c.injected_scripts_refused;
        if c.injected_scripts_run > 0 { inj_docs += 1; }
        dw += c.doc_writes; dwr += c.doc_writes_refused;
        if c.doc_writes > 0 { dw_docs += 1; }
        ev_d += c.events_dispatched; ev_l += c.event_listeners_run;
        if c.events_dispatched > 0 { ev_docs += 1; }
        hist_w += c.history_writes; hist_r += c.history_refused;
        if c.history_writes > 0 { hist_docs += 1; }
        if c.ce_upgrades > 0 { ce_docs += 1; }
        if c.mutation_records > 0 { mo_docs += 1; }
        pf += c.page_fetches; pff += c.page_fetch_failures;
        pblk += c.page_blocked; pbeac += c.beacons;
        for (k, n) in &c.blocked_hosts { *blocked_where.entry(k.clone()).or_default() += n; }
        if c.page_fetches > 0 { pf_docs += 1; }
        t_fired += c.timers_fired; t_dropped += c.timers_dropped;
        if c.timers_fired > 0 { t_docs += 1; }
        if c.layout_reads > 0 { layout_docs += 1; }
        ext_total += c.external_total;
        ext_ok += c.external_fetched;
        ext_fail += c.external_total - c.external_fetched;
        elems.push(c.elements_after);
        if c.scripts_total > 0 {
            gains.push((c.text_after as i64 - c.text_before as i64, name.clone()));
        }
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
    if pf > 0 || pff > 0 || pblk > 0 || pbeac > 0 {
        println!("  requests the PAGE made: {pf} allowed in {pf_docs} docs, {pff} failed");
        println!("  REFUSED by policy (same-origin GET only): {pblk}; beacons suppressed: {pbeac}");
        let mut v: Vec<_> = blocked_where.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (k, n) in v.into_iter().take(10) { println!("      {n:4}  {k}"); }
    }
    if t_fired > 0 || t_dropped > 0 {
        println!("  timer callbacks fired: {t_fired} in {t_docs} docs; {t_dropped} still pending at the horizon");
    }
    if inj > 0 || injr > 0 {
        println!("  scripts the page INJECTED at runtime: {inj} run in {inj_docs} docs; \
                  {injr} REFUSED (third-party src)");
    }
    if dw > 0 || dwr > 0 {
        println!("  document.write: {dw} applied at the script's position in {dw_docs} docs; \
                  {dwr} REFUSED (would have erased the document)");
    }
    if ev_d > 0 {
        println!("  events the page dispatched ITSELF: {ev_d} in {ev_docs} docs, \
                  running {ev_l} listeners (no user event is ever synthesised)");
    }
    if hist_w > 0 || hist_r > 0 {
        println!("  same-document history writes: {hist_w} in {hist_docs} docs; \
                  {hist_r} navigations REFUSED (would leave the document)");
    }
    if ce_up > 0 {
        println!("  custom elements UPGRADED (constructor + connectedCallback): {ce_up} in {ce_docs} docs");
    }
    if mo_records > 0 {
        println!("  mutation records DELIVERED (real, not invented): {mo_records} in {mo_docs} docs");
    }
    if layout_reads > 0 {
        println!("  layout metrics read (no layout exists — nominal values): {layout_reads} in {layout_docs} docs");
    }
    if observers > 0 {
        println!("  geometry observations registered, never delivered: {observers}  (no layout — see GLOBALS)");
    }
    if !causes.is_empty() {
        println!("FIRST FAILURES BY PROXIMATE CAUSE (what to build, ranked by documents)");
        println!("  the message buckets are symptoms; one message can cover many gaps.");
        let mut v: Vec<_> = causes.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (k, n) in v.into_iter().take(30) {
            println!("   {n:3} docs  {k}");
        }
    }
    if !gains.is_empty() {
        gains.sort();
        let grew = gains.iter().filter(|(d, _)| *d > 0).count();
        let shrank: Vec<_> = gains.iter().filter(|(d, _)| *d < 0).collect();
        let flat = gains.len() - grew - shrank.len();
        let total: i64 = gains.iter().map(|(d, _)| *d).sum();
        let median = gains[gains.len() / 2].0;
        println!("CONTENT GAIN (visible text added by running the scripts)");
        println!("  the question tier 2 has to answer: is the artifact better");
        println!("  than the raw HTML? Running cleanly and adding nothing is not.");
        println!("   grew {grew}   unchanged {flat}   SHRANK {}   of {} scripted docs",
            shrank.len(), gains.len());
        println!("   total {total:+} chars, median {median:+}");
        for (d, n) in gains.iter().rev().take(3) { println!("     best  {d:+8}  {n}"); }
        // A document that LOSES text is the dangerous case: the converter ran
        // and made the artifact worse than not converting at all.
        for (d, n) in shrank.iter().take(5) { println!("     LOST  {d:+8}  {n}"); }
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
        for (k, n) in v.into_iter().take(30) { println!("  {n:5}  {k}"); }
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
    pub mutation_records: u32,
    pub ce_upgrades: u32,
    pub history_writes: u32,
    pub history_refused: u32,
    pub events_dispatched: u32,
    pub event_listeners_run: u32,
    pub doc_writes: u32,
    pub doc_writes_refused: u32,
    pub injected_scripts_run: u32,
    pub injected_scripts_refused: u32,
    pub text_before: usize,
    pub text_after: usize,
    pub cause: Option<(String, String)>,
    pub timers_fired: u32,
    pub timers_dropped: u32,
    pub page_fetches: u32,
    pub page_fetch_failures: u32,
    pub page_blocked: u32,
    pub beacons: u32,
    pub blocked_hosts: Vec<(String, u32)>,
}

fn parse_child(s: &str) -> Option<Child> {
    let mut c = Child { elements_before: 0, elements_after: 0, scripts_total: 0,
        scripts_failed: 0, script_mutations: 0, external_total: 0, external_fetched: 0,
        errors: vec![], missing: vec![], nulls: vec![], verdict: String::new(),
        first_error: None,
        module_retries: 0, observers: 0, layout_reads: 0, mutation_records: 0, ce_upgrades: 0, history_writes: 0, history_refused: 0,
        events_dispatched: 0, event_listeners_run: 0,
        doc_writes: 0, doc_writes_refused: 0,
        injected_scripts_run: 0, injected_scripts_refused: 0,
        text_before: 0, text_after: 0,
        cause: None,
        timers_fired: 0, timers_dropped: 0, page_fetches: 0, page_fetch_failures: 0,
        page_blocked: 0, beacons: 0, blocked_hosts: vec![] };
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
            Some(&"X") if f.len() >= 3 => {
                c.text_before = f[1].parse().unwrap_or(0);
                c.text_after = f[2].parse().unwrap_or(0);
            }
            Some(&"J") if f.len() >= 3 => {
                c.injected_scripts_run = f[1].parse().unwrap_or(0);
                c.injected_scripts_refused = f[2].parse().unwrap_or(0);
            }
            Some(&"W") if f.len() >= 3 => {
                c.doc_writes = f[1].parse().unwrap_or(0);
                c.doc_writes_refused = f[2].parse().unwrap_or(0);
            }
            Some(&"D") if f.len() >= 3 => {
                c.events_dispatched = f[1].parse().unwrap_or(0);
                c.event_listeners_run = f[2].parse().unwrap_or(0);
            }
            Some(&"H") if f.len() >= 3 => {
                c.history_writes = f[1].parse().unwrap_or(0);
                c.history_refused = f[2].parse().unwrap_or(0);
            }
            Some(&"C") if f.len() >= 3 => c.cause = Some((f[1].to_string(), f[2].to_string())),
            Some(&"L") if f.len() >= 2 => c.layout_reads = f[1].parse().unwrap_or(0),
            Some(&"O") if f.len() >= 3 => {
                c.observers = f[1].parse().unwrap_or(0);
                c.mutation_records = f[2].parse().unwrap_or(0);
                if f.len() >= 4 { c.ce_upgrades = f[3].parse().unwrap_or(0); }
            }
            Some(&"P") if f.len() >= 5 => {
                c.page_fetches = f[1].parse().unwrap_or(0);
                c.page_fetch_failures = f[2].parse().unwrap_or(0);
                c.page_blocked = f[3].parse().unwrap_or(0);
                c.beacons = f[4].parse().unwrap_or(0);
            }
            Some(&"B") if f.len() >= 3 => {
                c.blocked_hosts.push((f[2].to_string(), f[1].parse().unwrap_or(1)));
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
