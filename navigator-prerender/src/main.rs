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

fn name_of(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
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

/// The report lines the parent aggregates; errors last, tab-separated.
/// `fetchd` is (requests, bytes) when every fetch went through a fetchd.
fn report(c: &navigator_prerender::Conversion, fetchd: Option<(u64, u64)>) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    // fields the parent aggregates; errors last, tab-separated
    let _ = writeln!(o, "R\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        c.elements_before, c.elements_after, c.depth_after,
        c.scripts_total, c.scripts_run, c.scripts_failed, c.script_mutations,
        c.external_total, c.external_fetched, c.module_retries,
        c.observers_registered);
    let _ = writeln!(o, "L\t{}", c.layout_reads);
    let _ = writeln!(o, "O\t{}\t{}\t{}", c.observers_registered, c.mutation_records, c.ce_upgrades);
    let _ = writeln!(o, "H\t{}\t{}", c.history_writes, c.history_refused);
    let _ = writeln!(o, "D\t{}\t{}", c.events_dispatched, c.event_listeners_run);
    let _ = writeln!(o, "W\t{}\t{}", c.doc_writes, c.doc_writes_refused);
    let _ = writeln!(o, "J\t{}\t{}", c.injected_scripts_run, c.injected_scripts_refused);
    let _ = writeln!(o, "X\t{}\t{}", c.text_before, c.text_after);
    let _ = writeln!(o, "Z\t{}", c.removals_refused);
    let _ = writeln!(o, "S\t{}\t{}\t{}\t{}\t{}", c.interactive_found, c.transitions.len(),
        c.transitions.iter().filter(|t| t.anchored).count(),
        c.transitions.iter().filter(|t| t.is_attribute_only()).count(),
        c.transitions.iter().filter(|t| t.effects.iter()
            .any(|e| matches!(e, navigator_prerender::Effect::Truncated { .. }))).count());
    // The POLICY's verdict travels with the measurements, so the parent can
    // report what would be published without re-deriving it.
    {
        let (_, d) = navigator_prerender::TierPolicy::default().artifact(&c);
        let _ = writeln!(o, "Y\t{:?}\t{}", d.tier, d.reason);
        let _ = writeln!(o, "Q\t{}", c.origin_refusal.unwrap_or(""));
        for v in &c.profile_violations {
            let _ = writeln!(o, "V2\t{}\t{}\t{}", v.ceiling, v.measured, v.allowed);
        }
    }
    let _ = writeln!(o, "T\t{}\t{}", c.timers_fired, c.timers_dropped);
    if let Some((requests, bytes)) = fetchd {
        let _ = writeln!(o, "G\t{requests}\t{bytes}");
    }
    let _ = writeln!(o, "P\t{}\t{}\t{}\t{}", c.page_fetches, c.page_fetch_failures,
        c.page_blocked, c.beacons_suppressed);
    for (k, n) in c.blocked_hosts.iter() { let _ = writeln!(o, "B\t{n}\t{k}"); }
    let _ = writeln!(o, "V\t{:?}", c.verdict);
    // The PROXIMATE CAUSE of the first failure: the last recorded event
    // before the throw. A symptom bucket like "null or undefined" is useless
    // without it — the message names what broke, this names why.
    if let Some((kind, what)) = &c.cause {
        let _ = writeln!(o, "C\t{kind}\t{}", what.replace('\t', " "));
    }
    if let Some(f) = &c.first_error {
        let _ = writeln!(o, "F\t{}", f.replace('\t', " ").replace('\n', " "));
    }
    for e in c.errors.iter().take(8) {
        let _ = writeln!(o, "E\t{}", e.replace('\t', " ").replace('\n', " "));
    }
    for (name, n) in c.missing.iter() {
        let _ = writeln!(o, "M\t{n}\t{name}");
    }
    for (name, n) in c.nulls.iter() {
        let _ = writeln!(o, "N\t{n}\t{}", name.replace('\t', " "));
    }
    // Per-element exploration coverage (legacy-web §5.4.5.6): flags are
    // n native, e explorable, p probed, c cut by the probe bound, r recorded,
    // t truncated, d probed as a delegated target, D a delegating root whose
    // delegated probe recorded, x outside the interactive universe, h cut but
    // its handlers probed elsewhere, a a probe on it hit the loop limit. The
    // 14th column lists the gestures probed.
    {
        let s = &c.explore_stats;
        let per: Vec<String> = s.per_handler.iter().map(|n| n.to_string()).collect();
        let _ = writeln!(o, "U\t{}\t{}\t{}\t{}\t{}\t{}\t{}", s.handlers, s.probes, s.cut, s.rounds, s.micros, per.join(","), s.aborted);
    }
    for k in &c.coverage {
        let flags: String = [(k.native, 'n'), (k.explorable, 'e'), (k.probed, 'p'), (k.over_cap, 'c'),
                             (k.recorded, 'r'), (k.truncated, 't'), (k.delegated, 'd'), (k.via, 'D'),
                             (k.outside, 'x'), (k.represented, 'h'), (k.aborted, 'a')]
            .iter().filter(|(on, _)| *on).map(|(_, ch)| *ch).collect();
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        let _ = writeln!(o, "K\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            clean(&k.path), clean(&k.tag), clean(&k.input_type), clean(&k.events.join(",")), flags,
            k.net_get, k.net_blocked, k.beacons, k.timers_scheduled, k.timers_left, k.intervals_left,
            clean(&k.effect_targets.join("|")), clean(&k.gestures.join(",")));
    }
    o
}

/// PRERENDER_PROBES=<per handler>,<per document>: override the exploration
/// bounds, for MEASURING them (legacy-web §5.4.5.6b). Not part of the job
/// protocol: a deployment always runs the compiled defaults.
fn probe_bounds_env() -> Option<(usize, usize)> {
    let v = std::env::var("PRERENDER_PROBES").ok()?;
    let (a, b) = v.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Convert exactly one document and print one machine-readable line. The
/// directory driver invokes this in a child process so a pathological
/// document cannot take the corpus run with it.
fn run_one(file: &str, base: Option<&str>, net: bool) -> ! {
    // The child reads lossily for the same reason, and an unreadable file
    // becomes an empty document rather than a silent success: `unwrap_or_default`
    // here turned a gzip blob into a clean conversion of nothing.
    let src = fs::read(file).map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    // PRERENDER_FETCHD="<program> <args…>" routes EVERY fetch of this
    // conversion — script loading and the page's own network — through one
    // navigator-fetchd (backend §8.1), typically
    //   "portcullis exec --daemon --instance conv-{instance} org.atrium.navigator.fetcher"
    // so the network is held by a jailed, capability-mode process and never
    // by the converter. `{instance}` becomes this process's pid, one fetcher
    // jail per conversion. Unset: the measurement instrument's curl + cache.
    let fetchd = if net { std::env::var("PRERENDER_FETCHD").ok() } else { None }.map(|cmd| {
        let pid = std::process::id().to_string();
        let words: Vec<String> = cmd.split_whitespace().map(|w| w.replace("{instance}", &pid)).collect();
        let f = navigator_prerender::fetch::FetchdFetcher::spawn(&words[0], &words[1..])
            .unwrap_or_else(|e| { eprintln!("PRERENDER_FETCHD {cmd:?}: {e}"); std::process::exit(1) });
        navigator_prerender::fetch::SharedFetcher(std::rc::Rc::new(std::cell::RefCell::new(f)))
    });
    let mut http = HttpFetcher::new(std::env::temp_dir().join("prerender-jscache"));
    let mut nonet = NoNetwork;
    let mut shared = fetchd.clone();
    let fetcher: &mut dyn Fetcher = match (&mut shared, net) {
        (Some(f), _) => f,
        (None, true) => &mut http,
        (None, false) => &mut nonet,
    };
    // The page's own network is separate from the one that loads its code,
    // and is only wired when the run is networked at all.
    let mut eng = BoaEngine {
        explore: std::env::var("PRERENDER_EXPLORE").ok().as_deref() == Some("1"),
        // PRERENDER_PROBES=<per handler>,<per document>: override the
        // exploration bounds, for measuring them (§5.4.5.6a step 2).
        probe_bounds: probe_bounds_env(),
        same_site_network: std::env::var("PRERENDER_SAME_SITE").ok().as_deref() == Some("1"),
        page_fetcher: match (&fetchd, net) {
            (Some(f), _) => Some(Box::new(f.clone()) as Box<dyn Fetcher>),
            (None, true) => Some(Box::new(HttpFetcher::new(std::env::temp_dir().join("prerender-pagecache")))),
            (None, false) => None,
        },
        ..Default::default()
    };
    let c = convert_with(&src, base, &mut eng, fetcher);
    // PRERENDER_EMIT_DIR writes the publishable recording — the chosen
    // document plus the transition table — one JSON file per input.
    // ★ A challenge page is the ORIGIN refusing, not the site (§5.4.1b). The
    // batch report already said so — and the recording was written anyway,
    // because the emit path never read the verdict: four "Just a moment…"
    // pages were published as if they were Stack Overflow and B&H.
    let emit_dir = std::env::var("PRERENDER_EMIT_DIR").ok();
    if let (Some(_), Some(why)) = (&emit_dir, c.origin_refusal) {
        eprintln!("emit: {file}: refused by origin ({why}); no recording written");
    }
    if let Some(dir) = emit_dir.filter(|_| c.origin_refusal.is_none()) {
        let policy = navigator_prerender::TierPolicy::default();
        let json = navigator_prerender::artifact::emit(base, &c, &policy);
        let stem = std::path::Path::new(file).file_stem()
            .map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "doc".into());
        // ★ Not `let _ =`. A silently-dropped write is how two recordings
        // went missing from a corpus run and looked like a converter bug.
        let path = std::path::Path::new(&dir).join(format!("{stem}.json"));
        if let Err(e) = fs::create_dir_all(&dir) {
            eprintln!("emit: cannot create {dir}: {e}");
        } else if let Err(e) = fs::write(&path, json) {
            eprintln!("emit: cannot write {}: {e}", path.display());
        }
    }
    // PRERENDER_DUMP=1 prints the converted artifact itself, for inspecting
    // a single document by hand. The parent never sets it.
    if std::env::var("PRERENDER_DUMP").ok().as_deref() == Some("1") {
        eprintln!("{}", c.html);
    }
    print!("{}", report(&c, fetchd.as_ref().map(|f| { let f = f.0.borrow(); (f.requests, f.bytes_fetched()) })));
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
    // ★ No arguments is the JAIL ENTRY: a one-shot jail runs its entry bare,
    // and everything else arrives on stdin. A person typing `prerender` at
    // a terminal gets the usage instead of a silent wait.
    if args.is_empty() {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            eprintln!("usage: prerender <file-or-dir>...   (no arguments + a pipe on stdin = serve one conversion)");
            std::process::exit(2);
        }
        serve();
    }
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
    let driver = Driver::from_env();
    if let Err(e) = driver.check(net) { eprintln!("prerender: REFUSED — {e}"); std::process::exit(1) }
    match &driver.launcher {
        Some((p, a)) => println!("confinement: LAUNCHER {p} {}{}", a.join(" "),
                                 if driver.require { "  (required; each child verifies it is jailed)" } else { "" }),
        None => println!("confinement: none — UNCONFINED, for testing: process isolation only, NOT a jail"),
    }
    let mut walls: Vec<f64> = vec![];
    let (mut with_js, mut js_ok, mut js_fail, mut mutated) = (0usize, 0usize, 0usize, 0usize);
    let (mut ext_total, mut ext_ok, mut ext_fail) = (0usize, 0usize, 0usize);
    let mut timed_out_names: Vec<String> = vec![];
    let mut budget_refusals: Vec<String> = vec![];
    let mut pulled: Vec<u64> = vec![];
    let deadline = std::env::var("PRERENDER_DEADLINE").ok().and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_secs).unwrap_or(DEADLINE);
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
    let mut unreadable: Vec<String> = vec![];
    let mut lossy: Vec<String> = vec![];
    let mut gains: Vec<(i64, String)> = vec![];
    let mut demoted: Vec<String> = vec![];
    let mut refused: Vec<String> = vec![];
    let mut viol: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut empty: Vec<String> = vec![];
    let (mut inter, mut trans, mut anch, mut trans_docs) = (0u32, 0u32, 0u32, 0usize);
    let (mut attr_only, mut trunc) = (0u32, 0u32);
    let mut tier2_kept = 0usize;

    for f in &files {
        // ★ NOT a silent skip. `read_to_string` fails on any document that
        // is not valid UTF-8, and `else { continue }` dropped two of this
        // corpus's 104 files without a word: one gzip-encoded, one Latin-1.
        // Every measurement in this crate's history was therefore over 102
        // documents while the report said 104.
        //
        // A Latin-1 page is real content and converts fine lossily; a binary
        // one is not a document at all. Both are COUNTED and named.
        let Ok(bytes) = fs::read(f) else { unreadable.push(name_of(f)); continue };
        if bytes.starts_with(&[0x1f, 0x8b]) || bytes.starts_with(b"\x89PNG") {
            unreadable.push(name_of(f));
            continue;
        }
        let src = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => {
                lossy.push(name_of(f));
                String::from_utf8_lossy(e.as_bytes()).into_owned()
            }
        };
        let _ = &src;
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        let b = base.get(&name).cloned().unwrap_or_default();
        let c = match convert_in_child(&exe, f, &b, net, deadline, &driver) {
            Ok(c) => {
                walls.push(c.wall_ms);
                if net { pulled.push(c.fetch_pulled) }
                if let Some(r) = &c.fetch_budget_refusal { budget_refusals.push(format!("{name}  ({r})")) }
                c
            }
            Err(why) => {
                // ★ NAMED, not just counted. Four documents produced no recording
                // in one run and the count alone could not say which — the answer
                // took a single-document re-run to get back. A tally that makes
                // you reproduce the run to identify its members is half a report.
                timed_out_names.push(format!("{name}  ({why})"));
                continue;
            }
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
        if !c.origin_refusal.is_empty() {
            refused.push(format!("{name}  ({})", c.origin_refusal));
        } else if c.text_before < 200 && c.text_after < 200 {
            // ★ NOT called a refusal. An empty document is AMBIGUOUS: it is
            // either an app shell whose content is fetched, or a challenge
            // delivered entirely by script. Both leave nothing to publish,
            // but only one is the origin saying no — and claiming to know
            // which would be inventing a diagnosis.
            empty.push(name.clone());
        }
        for (ceiling, m, a) in &c.violations {
            viol.entry(ceiling.clone()).or_default().push(format!("{name} ({m} > {a})"));
        }
        elems.push(c.elements_after);
        inter += c.interactive_found; trans += c.transitions; anch += c.transitions_anchored;
        attr_only += c.transitions_attr_only; trunc += c.transitions_truncated;
        if c.transitions > 0 { trans_docs += 1; }
        if c.scripts_total > 0 {
            gains.push((c.text_after as i64 - c.text_before as i64, name.clone()));
            match c.tier.as_str() {
                "Two" => tier2_kept += 1,
                "One" => demoted.push(format!("{name}  ({})", c.tier_reason)),
                _ => {}
            }
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
    if !unreadable.is_empty() {
        println!("  NOT A DOCUMENT      {} (binary; skipped, and said so)", unreadable.len());
        for n in unreadable.iter().take(3) { println!("      {n}"); }
    }
    if !lossy.is_empty() {
        println!("  decoded LOSSILY     {} (not UTF-8; converted anyway)", lossy.len());
        for n in lossy.iter().take(3) { println!("      {n}"); }
    }
    if !walls.is_empty() {
        let mut w = walls.clone();
        w.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| w[((w.len() as f64 - 1.0) * p).round() as usize];
        println!("  conversion wall (driver's view, ms): p50 {:.0}  p90 {:.0}  max {:.0}  mean {:.0}  total {:.1}s over {}",
            q(0.5), q(0.9), w[w.len() - 1], w.iter().sum::<f64>() / w.len() as f64, w.iter().sum::<f64>() / 1000.0, w.len());
    }
    if !timed_out_names.is_empty() {
        // ★ EVERY name. Six of 27 were printed once, and the rest had to be
        // recovered by diffing the manifest against the recordings.
        println!("  TIMED OUT / crashed {}  (child killed at {deadline:?}; PRERENDER_DEADLINE=<secs> to change)", timed_out_names.len());
        for n in &timed_out_names { println!("      {n}"); }
    }
    if !pulled.is_empty() {
        let mut p = pulled.clone();
        p.sort();
        let q = |x: f64| p[((p.len() as f64 - 1.0) * x).round() as usize];
        println!("  fetched through the relay per conversion (bytes): p50 {}  p90 {}  p99 {}  max {}  (budget {})",
            q(0.5), q(0.9), q(0.99), p[p.len() - 1], driver.fetch_budget);
    }
    if !budget_refusals.is_empty() {
        // ★ Named, with the reason: a budget refusal changes what the page
        // could load, so the conversion is not comparable to an unbounded one.
        println!("  FETCH BUDGET refused requests in {} documents:", budget_refusals.len());
        for n in &budget_refusals { println!("      {n}"); }
    }
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
    if !viol.is_empty() {
        let n: usize = viol.values().map(|v| v.len()).sum();
        println!("DOCUMENT PROFILE v1 — {n} ceiling violations");
        println!("  the profile calls these normative and checked; now something checks them");
        for (ceiling, docs) in &viol {
            println!("   {ceiling}: {} docs", docs.len());
            for d in docs.iter().take(2) { println!("      {d}"); }
        }
    }
    if !refused.is_empty() {
        println!("REFUSED BY ORIGIN (a challenge page is not the site — spec §5.4.1b)");
        println!("  {} documents; converting one would publish an artifact that looks", refused.len());
        println!("  like a successful conversion of a page saying \"Just a moment...\"");
        for n in refused.iter().take(4) { println!("     {n}"); }
    }
    if !empty.is_empty() {
        println!("NO DOCUMENT TO PUBLISH ({} docs: app shell, or a challenge", empty.len());
        println!("  delivered by script — indistinguishable from outside, and not");
        println!("  claimed as a refusal because only one of them is the origin saying no)");
        for n in empty.iter().take(4) { println!("     {n}"); }
    }
    if inter > 0 {
        println!("STATE RECORDING (JS run as an ORACLE; every probe reverted)");
        println!("  interactive elements found {inter}");
        println!("  transitions recorded {trans} in {trans_docs} docs; \
                  {anch} anchored to the tier 1 document");
        println!("  of those, {attr_only} are ATTRIBUTE-ONLY — the content they");
        println!("  reveal is already in tier 1, so they carry nothing; {trunc} truncated");
    }
    if tier2_kept > 0 || !demoted.is_empty() {
        println!("TIER POLICY (never emit an artifact worse than the input)");
        println!("  the converter MEASURES; this policy DECIDES. Defaults: keep");
        println!("  tier 2 unless it retained under 80% of the visible text.");
        println!("   tier 2 published {tier2_kept}   DEMOTED to tier 1 {}", demoted.len());
        for n in demoted.iter().take(5) { println!("     tier 1  {n}"); }
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
        // ★ NOT THE HEADLINE, AND LABELLED SO. This counts documents whose
        // scripts all RAN. It was the headline for most of this crate's life
        // and it is the wrong thing to decide on: measured against content,
        // only 19 of 85 documents gain anything from running them and the
        // median gains nothing. A number that reads as an endorsement while
        // the thing it endorses does nothing for three quarters of the corpus
        // should not be the first thing anyone sees.
        println!("  -> scripts completed on: {lo}% to {hi}%  ({}/{scripted} known, +{unknown} unattributed)",
            clean + browser);
        println!("     ^ NOT a measure of artifact quality — see CONTENT GAIN above,");
        println!("       which is the number that decides whether tier 2 earns its cost.");
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

/// Run one document in a child process, killing it at the deadline. The
/// error says which of the ways a child can fail to answer this was: a
/// timeout and a crash need different next steps.
/// How the driver runs each conversion: read once, from the environment.
struct Driver {
    /// `PRERENDER_LAUNCHER="<program> <args…>"`, with `{instance}` replaced
    /// per conversion — typically
    ///   `portcullis exec --daemon --instance conv{instance} org.atrium.navigator.converter`
    /// ★ `None` is UNCONFINED: the serve-mode child is this same binary as
    /// an ordinary process. Process isolation only, NOT a jail — for tests
    /// and for the byte-for-byte comparison against the jailed run.
    launcher: Option<(String, Vec<String>)>,
    /// `PRERENDER_FETCHD`, as before, but now started by THIS process and
    /// relayed to the child: the child cannot start anything.
    fetchd: Option<String>,
    /// `PRERENDER_REQUIRE_CONFINEMENT=1`: refuse to run unconfined, refuse a
    /// networked run without fetchd, and have each child verify with the
    /// kernel that it is jailed.
    require: bool,
    explore: bool,
    same_site: bool,
    protect: bool,
    dump: bool,
    emit_dir: Option<PathBuf>,
    blob_dir: Option<PathBuf>,
    mem_mb: u64,
    /// Total bytes one conversion may pull through the relay
    /// (`PRERENDER_FETCH_BUDGET`, bytes; default FetchBudget::DEFAULT).
    fetch_budget: u64,
}

impl Driver {
    fn from_env() -> Driver {
        let on = |k: &str| std::env::var(k).ok().as_deref() == Some("1");
        let words = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Driver {
            launcher: words("PRERENDER_LAUNCHER").map(|v| {
                let w: Vec<String> = v.split_whitespace().map(String::from).collect();
                (w[0].clone(), w[1..].to_vec())
            }),
            fetchd: words("PRERENDER_FETCHD"),
            require: on("PRERENDER_REQUIRE_CONFINEMENT"),
            explore: on("PRERENDER_EXPLORE"),
            same_site: on("PRERENDER_SAME_SITE"),
            protect: on("PRERENDER_PROTECT"),
            dump: on("PRERENDER_DUMP"),
            emit_dir: std::env::var_os("PRERENDER_EMIT_DIR").map(PathBuf::from),
            blob_dir: std::env::var_os("PRERENDER_BLOB_DIR").map(PathBuf::from),
            mem_mb: std::env::var("PRERENDER_MEM_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(4096),
            fetch_budget: std::env::var("PRERENDER_FETCH_BUDGET").ok().and_then(|v| v.parse().ok())
                .unwrap_or(navigator_prerender::fetch::FetchBudget::DEFAULT),
        }
    }

    /// ★ The require-confinement switch. A production conversion refuses
    /// to start rather than run hostile JavaScript as the user.
    fn check(&self, net: bool) -> Result<(), String> {
        if !self.require { return Ok(()) }
        if self.launcher.is_none() {
            return Err("PRERENDER_REQUIRE_CONFINEMENT=1 but no PRERENDER_LAUNCHER: refusing to run page \
                        JavaScript unconfined".into());
        }
        if net && self.fetchd.is_none() {
            return Err("PRERENDER_REQUIRE_CONFINEMENT=1 with PRERENDER_NET=1 but no PRERENDER_FETCHD: the \
                        network would be curl in this process, without the address rules (threat model §2.1)".into());
        }
        Ok(())
    }
}

/// Where a relayed fetch goes.
enum Upstream {
    Fetchd(navigator_prerender::fetch::FetchdFetcher),
    /// The measurement instrument (unconfined runs only; see HttpFetcher).
    Curl(HttpFetcher),
    None,
}

impl Upstream {
    fn reply(&mut self, url: &str) -> navigator_prerender::fetch::Reply {
        use navigator_prerender::fetch::Reply;
        match self {
            Upstream::Fetchd(f) => f.exchange(url).unwrap_or_else(Reply::Err),
            Upstream::Curl(h) => match h.get_bytes(url) {
                Ok(b) => Reply::Ok { status: 200, rest: format!("0 {} {url}", b.len()), body: b },
                Err(e) => Reply::Err(e),
            },
            Upstream::None => Reply::Err("no network".into()),
        }
    }
}

static INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long a child gets past its own deadline before its input is closed,
/// and then again before it is killed. A jailed child normally exits on its
/// own watchdog; the kill is the backstop, because a killed launcher leaks
/// its jail (backend §4.7a bug 2).
const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

fn convert_in_child(
    exe: &Path, file: &Path, base: &str, net: bool, deadline: std::time::Duration, d: &Driver,
) -> Result<Child, String> {
    use navigator_prerender::serve::{self, Job};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    // ★ An unreadable file is an EMPTY document, counted, not a skip — as
    // the child did before it lost the filesystem.
    let doc = fs::read(file).unwrap_or_default();
    let job = Job {
        base: (!base.is_empty()).then(|| base.to_string()),
        net, explore: d.explore, same_site: d.same_site, protect: d.protect,
        emit: d.emit_dir.is_some(), blobs: d.blob_dir.is_some(), require_confined: d.require,
        deadline_s: deadline.as_secs().max(1), mem_mb: d.mem_mb, len: doc.len(),
    };
    let header = job.header()?;
    let instance = format!("{}{:05}", std::process::id(), INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let fill = |a: &String| a.replace("{instance}", &instance);
    let mut cmd = match &d.launcher {
        None => Command::new(exe),  // no arguments = serve mode
        Some((p, args)) => { let mut c = Command::new(p); c.args(args.iter().map(fill)); c }
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut ch = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let stdin: Arc<Mutex<Option<Box<dyn std::io::Write + Send>>>> =
        Arc::new(Mutex::new(Some(Box::new(ch.stdin.take().expect("piped")))));
    let stdout = ch.stdout.take().expect("piped");
    // ★ stderr is DRAINED, on its own thread, while the child runs: piped
    // and unread, a chatty child blocks forever on a full pipe (§4.7a bug 3).
    let mut stderr = ch.stderr.take().expect("piped");
    let err_t = std::thread::spawn(move || { let mut v = vec![]; let _ = std::io::Read::read_to_end(&mut stderr, &mut v); v });
    let start = std::time::Instant::now();

    let (fetchd, blob_dir, stdin_d, emit) = (d.fetchd.clone(), d.blob_dir.clone(), stdin.clone(), d.emit_dir.is_some());
    let fetch_instance = instance.clone();
    let budget_limit = d.fetch_budget;
    let budget = Arc::new(Mutex::new(navigator_prerender::fetch::FetchBudget::new(budget_limit)));
    let budget_d = budget.clone();
    let drive_t = std::thread::spawn(move || -> Result<serve::Outcome, String> {
        {
            let mut g = stdin_d.lock().unwrap();
            let w = g.as_mut().expect("open");
            w.write_all(header.as_bytes()).and_then(|_| w.write_all(&doc)).and_then(|_| w.flush())
                .map_err(|e| format!("could not hand the document to the converter: {e}"))?;
        }
        // The fetcher is started only for a networked run, per conversion,
        // so its pins (threat model §2.1) are this conversion's alone.
        let mut up = match (net, fetchd) {
            (false, _) => Upstream::None,
            (true, Some(cmd)) => {
                let w: Vec<String> = cmd.split_whitespace().map(|x| x.replace("{instance}", &fetch_instance)).collect();
                Upstream::Fetchd(navigator_prerender::fetch::FetchdFetcher::spawn(&w[0], &w[1..])
                    .map_err(|e| format!("PRERENDER_FETCHD {cmd:?}: {e}"))?)
            }
            (true, None) => Upstream::Curl(HttpFetcher::new(std::env::temp_dir().join("prerender-relaycache"))),
        };
        let mut out = std::io::BufReader::new(stdout);
        // ★ The per-conversion byte budget is applied HERE, in the relay,
        // where the converter cannot reach it.
        let r = serve::drive(&mut out, &stdin_d, &mut |u| {
            budget_d.lock().unwrap().admit(u, |u| up.reply(u)).to_frame()
        }, &mut |addr, bytes| {
            let (Some(dir), Some(hex)) = (&blob_dir, addr.strip_prefix("blake3:")) else { return };
            let _ = fs::create_dir_all(dir);
            let p = dir.join(hex);
            if !p.exists() { let _ = fs::write(p, bytes); }
        });
        // ★ End of input first: the child (and through it its launcher) is
        // asked to exit, never killed while it still could.
        drop(stdin_d.lock().unwrap().take());
        let _ = emit;
        r
    });

    let status = loop {
        match ch.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if start.elapsed() > deadline + GRACE => {
                drop(stdin.lock().unwrap().take());
                let t = std::time::Instant::now();
                let mut st = None;
                while t.elapsed() < GRACE {
                    if let Ok(Some(s)) = ch.try_wait() { st = Some(s); break }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                if st.is_none() { let _ = ch.kill(); let _ = ch.wait(); }
                break None;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(e) => return Err(format!("wait failed: {e}")),
        }
    };
    let outcome = drive_t.join().unwrap_or_else(|_| Err("driver thread panicked".into()));
    let stderr = err_t.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr);
    let Some(status) = status else { return Err(format!("timed out at {}s (child killed)", deadline.as_secs())) };
    let o = outcome.map_err(|e| format!("converter protocol: {e}"))?;
    if o.timed_out { return Err(format!("timed out at {}s", deadline.as_secs())) }
    if !o.ended { return Err(crash_reason(start.elapsed().as_secs_f64(), &status.to_string(), &stderr)) }
    if d.dump { eprint!("{stderr}") }
    if let (Some(dir), Some(rec)) = (&d.emit_dir, &o.recording) {
        let stem = file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "doc".into());
        let path = dir.join(format!("{stem}.json"));
        if let Err(e) = fs::create_dir_all(dir).and_then(|_| fs::write(&path, rec)) {
            eprintln!("emit: cannot write {}: {e}", path.display());
        }
    }
    // A refusal the child reported (origin refusal) is on its stderr.
    for l in stderr.lines().filter(|l| l.starts_with("emit: ")) { eprintln!("{l}") }
    // PRERENDER_COVERAGE=<file>: append every K line, prefixed by the document.
    if let Some(path) = std::env::var_os("PRERENDER_COVERAGE") {
        let name = file.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let mut lines = String::new();
        for l in o.report.lines().filter(|l| l.starts_with("K\t")) { lines.push_str(&format!("{name}\t{}\n", &l[2..])) }
        if lines.is_empty() { lines = format!("{name}\t-\n") }
        // Budget accounting: handlers, probes, cut, rounds, microseconds,
        // probes per handler.
        for l in o.report.lines().filter(|l| l.starts_with("U\t")) { lines.push_str(&format!("{name}\t#stats\t{}\n", &l[2..])) }
        // Relayed fetches (requests, bytes): what a per-conversion byte
        // budget is chosen from (threat model §2.2, P2).
        for l in o.report.lines().filter(|l| l.starts_with("G\t")) { lines.push_str(&format!("{name}\t#fetch\t{}\n", &l[2..])) }
        use std::io::Write;
        if let Err(e) = fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| f.write_all(lines.as_bytes())) {
            eprintln!("coverage: cannot write {}: {e}", std::path::Path::new(&path).display());
        }
    }
    let mut c = parse_child(&o.report).ok_or_else(|| crash_reason(start.elapsed().as_secs_f64(), &status.to_string(), &stderr))?;
    c.wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    {
        let b = budget.lock().unwrap();
        c.fetch_pulled = b.pulled;
        c.fetch_budget_refusal = b.reason();
    }
    Ok(c)
}

/// The jail entry: one conversion over stdin/stdout (src/serve.rs). No
/// arguments, no environment, no filesystem.
fn serve() -> ! {
    use navigator_prerender::fetch::{FetchdFetcher, SharedFetcher};
    use navigator_prerender::serve as sv;
    use std::io::Write;
    let (job, doc) = {
        let stdin = std::io::stdin();
        let mut l = stdin.lock();
        match sv::read_job(&mut l) { Ok(x) => x, Err(e) => { eprintln!("prerender serve: {e}"); std::process::exit(2) } }
    };
    if job.require_confined {
        match sv::kernel_says_jailed() {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("prerender serve: REFUSED — confinement is required and the kernel says this process is not in a jail");
                std::process::exit(3)
            }
            Err(e) => { eprintln!("prerender serve: REFUSED — cannot ask the kernel whether this is a jail: {e}"); std::process::exit(3) }
        }
    }
    // ★ Before a byte of the page is parsed.
    if let Err(e) = sv::limit_self(job.deadline_s + 5, job.mem_mb) {
        eprintln!("prerender serve: REFUSED — {e}");
        std::process::exit(3)
    }
    let deadline = std::time::Duration::from_secs(job.deadline_s);
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        let mut o = std::io::stdout().lock();
        let _ = o.write_all(b"TIMEOUT\n");
        let _ = o.flush();
        std::process::exit(124);
    });
    navigator_prerender::subresource::install_blob_sink();
    let src = String::from_utf8_lossy(&doc).into_owned();
    let relay = job.net.then(|| SharedFetcher(std::rc::Rc::new(std::cell::RefCell::new(
        FetchdFetcher::over(Box::new(std::io::stdout()), Box::new(std::io::stdin()), "F ")))));
    let mut nonet = NoNetwork;
    let mut shared = relay.clone();
    let fetcher: &mut dyn Fetcher = match &mut shared { Some(f) => f, None => &mut nonet };
    let mut eng = BoaEngine {
        explore: job.explore,
        probe_bounds: probe_bounds_env(),
        same_site_network: job.same_site,
        page_fetcher: relay.clone().map(|f| Box::new(f) as Box<dyn Fetcher>),
        ..Default::default()
    };
    let c = navigator_prerender::convert_with_opts(&src, job.base.as_deref(), &mut eng, fetcher, job.protect);
    let rep = report(&c, relay.as_ref().map(|f| { let f = f.0.borrow(); (f.requests, f.bytes_fetched()) }));
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(rep.as_bytes());
    let blobs = navigator_prerender::subresource::take_blobs();
    if job.blobs {
        for (a, b) in &blobs {
            let _ = write!(out, "BLOB {a} {}\n", b.len());
            let _ = out.write_all(b);
        }
    }
    if job.emit {
        if let Some(why) = c.origin_refusal {
            eprintln!("emit: refused by origin ({why}); no recording written");
        } else {
            let json = navigator_prerender::artifact::emit(job.base.as_deref(), &c, &navigator_prerender::TierPolicy::default());
            let _ = write!(out, "RECORDING {}\n", json.len());
            let _ = out.write_all(json.as_bytes());
        }
    }
    let _ = out.write_all(b"END\n");
    let _ = out.flush();
    std::process::exit(0)
}

/// Why a child produced no result: how long it ran, how it exited, and the
/// last lines it wrote to stderr — a panic's message and location, which
/// is the whole diagnosis ("crashed after 4.2s (signal 6)" alone was not).
fn crash_reason(secs: f64, status: &str, stderr: &str) -> String {
    let tail: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()
        && !l.starts_with("note: run with `RUST_BACKTRACE")).collect();
    let tail = &tail[tail.len().saturating_sub(3)..];
    if tail.is_empty() { format!("crashed after {secs:.1}s ({status})") }
    else { format!("crashed after {secs:.1}s ({status}): {}", tail.join(" | ")) }
}

#[cfg(test)]
mod tests {
    use super::crash_reason;

    #[test]
    fn a_crash_reason_carries_the_last_stderr_lines() {
        let err = "warning: one\n\nthread 'main' panicked at boa_engine/src/module/mod.rs:666:14:\n\
            `then` cannot fail for a native `JsPromise`: object is not a Promise\n\
            note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert_eq!(crash_reason(4.25, "signal: 6", err),
            "crashed after 4.2s (signal: 6): warning: one | thread 'main' panicked at boa_engine/src/module/mod.rs:666:14: | \
             `then` cannot fail for a native `JsPromise`: object is not a Promise");
        assert_eq!(crash_reason(1.0, "exit status: 101", ""), "crashed after 1.0s (exit status: 101)");
    }
}

/// What the parent needs back from a child run.
pub struct Child {
    /// Bytes the relay pulled for this conversion, and the budget's refusal
    /// if it refused anything (threat model §2.2, P2).
    pub fetch_pulled: u64,
    pub fetch_budget_refusal: Option<String>,
    /// Wall time of the whole conversion as the DRIVER saw it: spawn,
    /// launcher, jail, convert, teardown. What a jail costs shows up here.
    pub wall_ms: f64,
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
    pub removals_refused: u32,
    pub interactive_found: u32,
    pub transitions: u32,
    pub transitions_anchored: u32,
    pub transitions_attr_only: u32,
    pub transitions_truncated: u32,
    pub origin_refusal: String,
    pub violations: Vec<(String, usize, usize)>,
    pub tier: String,
    pub tier_reason: String,
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
        removals_refused: 0, wall_ms: 0.0, fetch_pulled: 0, fetch_budget_refusal: None,
        interactive_found: 0, transitions: 0, transitions_anchored: 0,
        transitions_attr_only: 0, transitions_truncated: 0,
        origin_refusal: String::new(), violations: vec![],
        tier: String::new(), tier_reason: String::new(),
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
            Some(&"Z") if f.len() >= 2 => c.removals_refused = f[1].parse().unwrap_or(0),
            Some(&"S") if f.len() >= 6 => {
                c.interactive_found = f[1].parse().unwrap_or(0);
                c.transitions = f[2].parse().unwrap_or(0);
                c.transitions_anchored = f[3].parse().unwrap_or(0);
                c.transitions_attr_only = f[4].parse().unwrap_or(0);
                c.transitions_truncated = f[5].parse().unwrap_or(0);
            }
            Some(&"Q") if f.len() >= 2 => c.origin_refusal = f[1].to_string(),
            Some(&"V2") if f.len() >= 4 => c.violations.push((f[1].to_string(),
                f[2].parse().unwrap_or(0), f[3].parse().unwrap_or(0))),
            Some(&"Y") if f.len() >= 3 => {
                c.tier = f[1].to_string();
                c.tier_reason = f[2].to_string();
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
