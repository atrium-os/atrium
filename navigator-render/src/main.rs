//! nsg-render <file.md>            NSG on stdout
//! nsg-render --corpus <list>      one line per file: blake3 of its NSG, then
//!                                 a digest over all of them — what the M0
//!                                 gate compares across runs and machines.
//! nsg-render --pins               canonical addresses of the font set (to pin)
//! nsg-render --corpus-html <dir> [--recordings <dir>]
//!                                 render every .html through the PROFILE
//!                                 renderer and report what each document
//!                                 refuses — M2's corpus leg. The number it
//!                                 prints is "documents that render with no
//!                                 refusal at all", which is a different and
//!                                 harder claim than the 64/64 row number.

use navigator_render::{fontset::FontSet, nsg, render, Options, Report};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--pins") {
        match FontSet::load() {
            Ok(fs) => { for f in &fs.faces { println!("{} {} {}", f.name, f.weight, f.address) } return ExitCode::SUCCESS }
            Err(e) => { println!("{e}"); return ExitCode::FAILURE }
        }
    }
    let fonts = match FontSet::load() {
        Ok(f) => f,
        Err(e) => { eprintln!("nsg-render: {e}"); return ExitCode::FAILURE }
    };
    let opts = Options::default();
    match args.get(1).map(String::as_str) {
        Some("--corpus-html") => {
            use navigator_style::cascade::Env;
            use std::collections::BTreeMap;
            let dir = args.get(2).expect("directory");
            // ★ The subresource map comes from the converter's RECORDING —
            // `src -> (content address, intrinsic size)`. Without it an
            // image lays out and reserves its space but cannot be painted,
            // because a scene node names bytes by address and nothing here
            // would know which bytes.
            let recordings = args.iter().position(|a| a == "--recordings").and_then(|i| args.get(i + 1));
            let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir).expect("readable dir")
                .flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "html")).collect();
            files.sort();
            let env = Env::default();
            let (mut clean, mut n) = (0usize, 0usize);
            let mut codes: BTreeMap<String, (usize, usize)> = BTreeMap::new(); // code -> (hits, docs)
            let mut unimpl: BTreeMap<String, (usize, usize)> = BTreeMap::new();
            let mut total = Report::default();
            let mut painted = 0usize;
            for f in &files {
                let html = std::fs::read_to_string(f).unwrap_or_default();
                let subs = recordings.map(|r| {
                    let stem = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    navigator_render::conformance::subresources_from_recording(&std::path::Path::new(r).join(format!("{stem}.json")))
                }).unwrap_or_default();
                painted += subs.len();
                let o = navigator_render::html::render_html_with(&html, &fonts, &env, &subs);
                n += 1;
                let mut per: BTreeMap<String, usize> = BTreeMap::new();
                for d in &o.diagnostics {
                    *per.entry(d.code.to_string()).or_default() += 1;
                    if std::env::var_os("NSG_CORPUS_DETAIL").is_some() { println!("D\t{}\t{}", d.code, d.msg) }
                }
                for (k, v) in &per { let e = codes.entry(k.clone()).or_default(); e.0 += v; e.1 += 1 }
                for (k, v) in &o.unimplemented { let e = unimpl.entry(k.to_string()).or_default(); e.0 += v; e.1 += 1 }
                if o.diagnostics.is_empty() { clean += 1 }
                total.notdef += o.report.notdef; total.overflow_lines += o.report.overflow_lines;
                total.em_upright += o.report.em_upright;
                println!("{:<24} {:>6} nodes  {:>4} refusals  {:>3} unimplemented  {}",
                    f.file_name().unwrap().to_string_lossy(), o.scene.order.len(), o.diagnostics.len(),
                    o.unimplemented.values().sum::<usize>(),
                    per.keys().cloned().collect::<Vec<_>>().join(","));
            }
            println!("\nCORPUS: {clean}/{n} documents render with NO refusal");
            if recordings.is_some() { println!("subresources supplied: {painted} images") }
            println!("refusals, by code (hits / documents):");
            let mut v: Vec<_> = codes.into_iter().collect();
            v.sort_by_key(|(_, (h, _))| std::cmp::Reverse(*h));
            for (k, (h, d)) in v { println!("  {h:>7} hits {d:>4} docs  {k}") }
            println!("unimplemented, by kind (hits / documents):");
            let mut v: Vec<_> = unimpl.into_iter().collect();
            v.sort_by_key(|(_, (h, _))| std::cmp::Reverse(*h));
            for (k, (h, d)) in v { println!("  {h:>7} hits {d:>4} docs  {k}") }
            eprintln!("report: {total:?}");
            return ExitCode::SUCCESS;
        }
        Some("--corpus") => {
            let list = std::fs::read_to_string(args.get(2).expect("list file")).expect("readable list");
            let mut all = blake3::Hasher::new();
            // ★ Hash the INPUTS too. The corpus is the live repo, so editing
            // any document moves the output digest; without an input digest
            // beside it, an edit is indistinguishable from a renderer
            // regression — which is exactly the mistake this prints against.
            let mut inputs = blake3::Hasher::new();
            let mut total = Report::default();
            let mut n = 0;
            for path in list.lines().filter(|l| !l.is_empty()) {
                let md = match std::fs::read(path) {
                    Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                    Err(e) => { eprintln!("{path}: {e}"); return ExitCode::FAILURE }
                };
                inputs.update(blake3::hash(md.as_bytes()).as_bytes());
                inputs.update(path.as_bytes());
                let (scene, r) = render(&md, &fonts, &opts);
                let out = nsg::write(&scene, &fonts);
                let h = blake3::hash(out.as_bytes());
                println!("{} {} {}", h.to_hex(), out.len(), path);
                all.update(h.as_bytes());
                total.notdef += r.notdef; total.html_skipped += r.html_skipped;
                total.images_as_alt += r.images_as_alt; total.overflow_lines += r.overflow_lines;
                total.em_upright += r.em_upright;
                n += 1;
            }
            // Same input digest + different output digest = the renderer
            // changed. Both different = the corpus was edited.
            println!("corpus {n} documents input {} digest {}", inputs.finalize().to_hex(), all.finalize().to_hex());
            eprintln!("report: {total:?}");
            ExitCode::SUCCESS
        }
        // A single HTML document, through the profile renderer.
        Some(path) if path.ends_with(".html") => {
            let html = std::fs::read_to_string(path).expect("readable");
            let rec = args.iter().position(|a| a == "--recording").and_then(|i| args.get(i + 1));
            let subs = rec.map(|r| navigator_render::conformance::subresources_from_recording(std::path::Path::new(r))).unwrap_or_default();
            let env = navigator_style::cascade::Env::default();
            let o = navigator_render::html::render_html_with(&html, &fonts, &env, &subs);
            for d in &o.diagnostics { eprintln!("{} {}", d.code, d.msg) }
            print!("{}", nsg::write(&o.scene, &fonts));
            return ExitCode::SUCCESS;
        }
        Some(path) => {
            let md = String::from_utf8_lossy(&std::fs::read(path).expect("readable")).into_owned();
            let (scene, r) = render(&md, &fonts, &opts);
            print!("{}", nsg::write(&scene, &fonts));
            eprintln!("report: {r:?}");
            ExitCode::SUCCESS
        }
        None => { eprintln!("usage: nsg-render <file.md> | --corpus <list> | --pins"); ExitCode::from(2) }
    }
}
