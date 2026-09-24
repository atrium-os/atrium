//! nsg-flag <lane-dir> [--recordings <dir>] [--detail N] [--limit SECS]
//!
//! ★ Each document renders in a CHILD process with a time limit (default
//! 60 s). A renderer that hangs on one page is a finding — FT's error page
//! spun the grid placement forever — and it must not stall the other 77.
//!
//! REVIEW TOOLING. Renders every normalized document in `<lane-dir>` through
//! the profile renderer and flags the geometric symptoms that every layout
//! bug in the reviewed corpus showed (see `navigator_render::flag`): text
//! over text, text past the canvas edge, text sliced by a clip, all text in
//! a strip of the page, and far less text painted than the document holds.
//!
//! One line per document, then up to N examples of each finding with page
//! coordinates in px — enough to crop the render and look — then a summary.
//! It turns a stack of screenshots into a short list of places to look; it
//! does not decide what is a bug.

use navigator_render::{flag, fontset::FontSet, PX};
use navigator_style::cascade::Env;
use std::collections::BTreeMap;

/// One document: render, flag, print its line and details, then a
/// machine-readable `V<TAB>verdicts` line for the parent to aggregate.
fn one(f: &std::path::Path, recordings: Option<&String>, detail: usize) {
    let fonts = FontSet::load().expect("pinned font set");
    let env = Env::default();
    let name = f.file_stem().unwrap().to_string_lossy().to_string();
    let html = std::fs::read_to_string(f).unwrap_or_default();
    let subs = recordings.map(|r| navigator_render::conformance::subresources_from_recording(
        &std::path::Path::new(r).join(format!("{name}.json")))).unwrap_or_default();
    let o = navigator_render::html::render_html_with(&html, &fonts, &env, &subs);
    let fl = flag::flag(&o.scene, &fonts, flag::doc_text_chars(&html));
    let mut v = fl.verdicts();
    if !o.diagnostics.is_empty() { v.insert(0, "REFUSED") }
    println!("{:<44} {:>4} {:>6} {:>5} {:>5} {:>5} {:>5.0} {:>5.0}  {}", &name[..name.len().min(44)],
        o.diagnostics.len(), fl.visible, fl.overlaps.len(), fl.offcanvas.len(), fl.sliced.len(),
        fl.coverage * 100.0, fl.text_ratio * 100.0, v.join(","));
    let text_of = |run: usize| fl.vis.iter().find(|x| x.run == run).map(|x| x.text.as_str()).unwrap_or("?");
    let at = |run: usize| fl.vis.iter().find(|x| x.run == run).map(|x| (x.x0 / PX, x.y0 / PX)).unwrap_or((0, 0));
    for (a, b, (x, y, _, _)) in fl.overlaps.iter().take(detail) {
        println!("    overlap  at ({}, {})  {:?} × {:?}", x / PX, y / PX, text_of(*a), text_of(*b));
    }
    for r in fl.offcanvas.iter().take(detail) {
        let (x, y) = at(*r);
        println!("    offcanvas at ({x}, {y})  {:?}", text_of(*r));
    }
    if fl.sliced.len() >= 3 {
        for r in fl.sliced.iter().take(detail) {
            let run = &o.scene.runs[*r];
            println!("    sliced   at ({}, {})  {:?}", run.x / PX, run.y / PX, run.text);
        }
    }
    for d in o.diagnostics.iter().take(detail) { println!("    refused  {}: {}", d.code, d.msg) }
    println!("V\t{}", v.join(","));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--one") {
        let rec = args.iter().position(|a| a == "--recordings").and_then(|i| args.get(i + 1)).cloned();
        let detail = args.iter().position(|a| a == "--detail").and_then(|i| args.get(i + 1)).and_then(|d| d.parse().ok()).unwrap_or(3);
        one(std::path::Path::new(&args[2]), rec.as_ref(), detail);
        return;
    }
    let Some(dir) = args.get(1) else { eprintln!("usage: nsg-flag <lane-dir> [--recordings <dir>] [--detail N]"); std::process::exit(2) };
    let opt = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let recordings = opt("--recordings");
    let detail: usize = opt("--detail").and_then(|d| d.parse().ok()).unwrap_or(3);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir).expect("readable dir").flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "html")).collect();
    files.sort();

    let mut by_verdict: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
    let mut refused_docs = 0usize;
    println!("{:<44} {:>4} {:>6} {:>5} {:>5} {:>5} {:>5} {:>5}  {}", "document", "ref", "vis", "ovl", "off", "slc", "cov%", "txt%", "flags");
    let limit: u64 = opt("--limit").and_then(|d| d.parse().ok()).unwrap_or(60);
    let exe = std::env::current_exe().expect("exe");
    for f in &files {
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--one").arg(f).arg("--detail").arg(detail.to_string());
        if let Some(r) = &recordings { cmd.arg("--recordings").arg(r); }
        let mut child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
            .spawn().expect("spawn child");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(limit);
        let status = loop {
            if let Some(st) = child.try_wait().expect("wait") { break Some(st) }
            if std::time::Instant::now() >= deadline { let _ = child.kill(); let _ = child.wait(); break None }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let mut out = String::new();
        if let Some(mut so) = child.stdout.take() { use std::io::Read; let _ = so.read_to_string(&mut out); }
        match status {
            Some(st) if st.success() => {
                for line in out.lines() {
                    match line.strip_prefix("V\t") {
                        Some(vs) => for v in vs.split(',').filter(|v| !v.is_empty()) {
                            if v == "REFUSED" { refused_docs += 1 }
                            by_verdict.entry(match v { "REFUSED" => "REFUSED", "OVERLAP" => "OVERLAP", "OFFCANVAS" => "OFFCANVAS",
                                "SLICED" => "SLICED", "NARROW" => "NARROW", "TEXTLOSS" => "TEXTLOSS", _ => "UNKNOWN-VERDICT" }).or_default().push(name.clone())
                        },
                        None => println!("{line}"),
                    }
                }
            }
            // ★ Not a skip: a crash or a hang is the most serious finding.
            Some(_) => { println!("{:<44} CRASH", &name[..name.len().min(44)]); by_verdict.entry("CRASH").or_default().push(name) }
            None => { println!("{:<44} HANG (killed after {limit} s)", &name[..name.len().min(44)]); by_verdict.entry("HANG").or_default().push(name) }
        }
    }
    println!("\n{} documents; {} refused", files.len(), refused_docs);
    for (k, docs) in &by_verdict {
        println!("  {:<9} {:>3}  {}", k, docs.len(), docs.join(" "));
    }
}
