//! display_profile <parts-dir>… [--ext first]: where a first display's time
//! goes once its artifact exists (legacy-web §5.4.5.7, cut B). For every
//! `<stem>.<ext>.json` (default `first`), the display path as the driver
//! times it — ingest, normalize (by phase, `navigator_normalize::phases`),
//! render at 800 px — `--runs N` times (default 3, the median kept), one TSV
//! line per document, then the phases' share of the summed time. Fonts are
//! loaded once, not counted. `--ext any` takes every `*.json` (a recordings
//! directory). With `--out <dir>` the normalized documents and their
//! reports are written there, to check a change leaves them byte-identical.

use std::collections::BTreeMap;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let ext = opt("--ext").unwrap_or_else(|| "first".into());
    let runs: usize = opt("--runs").and_then(|v| v.parse().ok()).unwrap_or(3);
    let out = opt("--out");
    let skip: Vec<String> = ["--ext", "--runs", "--out"].iter().filter_map(|k| opt(k)).collect();
    let dirs: Vec<&String> = args.iter().filter(|a| !a.starts_with("--") && !skip.contains(a)).collect();
    let fonts = navigator_render::fontset::FontSet::load().expect("fonts");
    let env = navigator_style::cascade::Env::default();
    let mut sums: BTreeMap<String, f64> = BTreeMap::new();
    let mut order: Vec<String> = vec![];
    println!("doc\tingest\tnormalize\trender\tphases");
    for dir in dirs {
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("dir").flatten()
            .filter(|e| { let n = e.file_name().to_string_lossy().to_string();
                          if ext == "any" { n.ends_with(".json") } else { n.ends_with(&format!(".{ext}.json")) } }).collect();
        files.sort_by_key(|e| e.file_name());
        for e in files {
            let json = std::fs::read_to_string(e.path()).unwrap();
            let name = e.file_name().to_string_lossy().to_string();
            let stem = name.strip_suffix(".json").unwrap_or(&name).to_string();
            let mut samples = vec![];
            for _ in 0..runs.max(1) {
                let t0 = Instant::now();
                let r = navigator_normalize::recording::parse(&json).expect("recording");
                let t1 = Instant::now();
                navigator_normalize::phases::start();
                let (normalized, report) = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, &fonts, &env);
                let laps = navigator_normalize::phases::take();
                let t2 = Instant::now();
                let subs = navigator_render::conformance::subresources_from_recording_json(&json);
                let _ = navigator_render::html::render_html_with(&normalized, &fonts, &env, &subs);
                let t3 = Instant::now();
                let ms = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1e3;
                if let Some(o) = &out {
                    let _ = std::fs::create_dir_all(o);
                    let _ = std::fs::write(format!("{o}/{stem}.html"), &normalized);
                    let _ = std::fs::write(format!("{o}/{stem}.report.txt"), format!("{report:#?}"));
                }
                samples.push((ms(t1, t2), ms(t0, t1), ms(t2, t3), laps));
            }
            samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            let (norm, ingest, render, laps) = &samples[samples.len() / 2];
            let ph: Vec<String> = laps.iter().map(|(n, d)| format!("{n}={:.1}", d.as_secs_f64() * 1e3)).collect();
            println!("{stem}\t{ingest:.1}\t{norm:.1}\t{render:.1}\t{}", ph.join(" "));
            for (k, v) in [("ingest", *ingest), ("render", *render)] { *sums.entry(k.into()).or_default() += v }
            for (n, d) in laps {
                if !order.iter().any(|o| o == n) { order.push(n.to_string()) }
                *sums.entry(format!("n.{n}")).or_default() += d.as_secs_f64() * 1e3;
            }
        }
    }
    let total: f64 = sums.values().sum();
    eprintln!("summed: {total:.0} ms");
    for k in ["ingest".to_string()].into_iter().chain(order.iter().map(|o| format!("n.{o}"))).chain(["render".to_string()]) {
        let v = sums.get(&k).copied().unwrap_or(0.0);
        eprintln!("  {k:16} {v:9.0} ms  {:5.1}%", 100.0 * v / total);
    }
}
