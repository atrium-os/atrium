//! nsg-render <file.md>            NSG on stdout
//! nsg-render --corpus <list>      one line per file: blake3 of its NSG, then
//!                                 a digest over all of them — what the M0
//!                                 gate compares across runs and machines.
//! nsg-render --pins               canonical addresses of the font set (to pin)

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
        Some("--corpus") => {
            let list = std::fs::read_to_string(args.get(2).expect("list file")).expect("readable list");
            let mut all = blake3::Hasher::new();
            let mut total = Report::default();
            let mut n = 0;
            for path in list.lines().filter(|l| !l.is_empty()) {
                let md = match std::fs::read(path) {
                    Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                    Err(e) => { eprintln!("{path}: {e}"); return ExitCode::FAILURE }
                };
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
            println!("corpus {n} documents digest {}", all.finalize().to_hex());
            eprintln!("report: {total:?}");
            ExitCode::SUCCESS
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
