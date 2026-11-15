//! The O6c corpus run (docs/spec/officina.md §9.2): every `.pptx` (or
//! `.odp`) under the given directories is read by our reader and compared with
//! LibreOffice's reading of the same file: slide count and words.
//!
//! cargo run --release --example corpus_slides -- SRC[=LO] [SRC[=LO] ...]
//!
//! `LO` holds LibreOffice's `.odp` of each file, made beforehand with
//! `soffice --headless --convert-to odp --outdir LO SRC/*.pptx`; without it
//! only our reading is checked (read or refused, a panic fails the gate). Per
//! file: time, slides, shapes, words found in LibreOffice's text (recall), and
//! whether our writer and reader give the same slides back. The corpus is test
//! data and stays out of the repository.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use officina_convert::xml::{self, El};
use officina_convert::zip::Archive;
use officina_slides::model::{Deck, Shape, ShapeKind};

fn words(t: &str) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for w in t.split(|c: char| c.is_whitespace() || c == '\u{2028}') {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        if w.is_empty() || w.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        *m.entry(w.to_string()).or_default() += 1;
    }
    m
}

fn shape_text(s: &Shape, out: &mut String) {
    if let Some(t) = s.text() {
        out.push_str(&t.text());
        out.push('\n');
    }
    if let ShapeKind::Group { shapes, .. } = &s.kind {
        for k in shapes {
            shape_text(k, out);
        }
    }
}

/// The slides' own text (not layouts, masters or notes).
fn deck_text(d: &mut Deck) -> String {
    let mut out = String::new();
    for i in 0..d.slide_count() {
        if let Ok(s) = d.slide(i) {
            for sh in &s.shapes {
                shape_text(sh, &mut out);
            }
        }
    }
    out
}

/// LibreOffice's `.odp`: its page count and the pages' text (notes left out).
fn lo_odp(path: &Path) -> Option<(usize, String)> {
    let data = std::fs::read(path).ok()?;
    let a = Archive::open(&data).ok()?;
    let content = xml::parse(&a.read_name("content.xml").ok()??).ok()?;
    let pres = content.find("office:presentation")?;
    let mut text = String::new();
    fn walk(e: &El, out: &mut String) {
        for k in e.els() {
            if k.name == "presentation:notes" {
                continue;
            }
            if k.name == "text:p" || k.name == "text:h" {
                odf_text(k, out);
                out.push('\n');
            } else {
                walk(k, out);
            }
        }
    }
    let pages: Vec<&El> = pres.children("draw:page").collect();
    for p in &pages {
        walk(p, &mut text);
    }
    Some((pages.len(), text))
}

/// An ODF paragraph's text: `text:s` spaces, tabs and line breaks as text.
fn odf_text(e: &El, out: &mut String) {
    for k in &e.kids {
        match k {
            xml::Node::Text(t) => out.push_str(t),
            xml::Node::El(c) => match c.name.as_str() {
                "text:s" => out.push_str(&" ".repeat(c.attr("text:c").and_then(|v| v.parse().ok()).unwrap_or(1usize).min(1000))),
                "text:tab" => out.push('\t'),
                "text:line-break" => out.push('\n'),
                _ => odf_text(c, out),
            },
        }
    }
}

fn main() {
    let mut files: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    for a in std::env::args().skip(1) {
        let (src, lo) = match a.split_once('=') {
            Some((s, l)) => (PathBuf::from(s), Some(PathBuf::from(l))),
            None => (PathBuf::from(&a), None),
        };
        let mut v: Vec<PathBuf> = std::fs::read_dir(&src).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        v.sort();
        for f in v {
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if ext == "pptx" || ext == "odp" {
                let lo_file = lo.as_ref().map(|l| l.join(f.with_extension("odp").file_name().unwrap()));
                files.push((f, lo_file));
            }
        }
    }
    let (mut read, mut refused, mut panics) = (0, 0, 0);
    let (mut slides_same, mut slides_compared) = (0, 0);
    let (mut have, mut want) = (0usize, 0usize);
    let mut rt_same = 0;
    let mut cross_same = 0;
    let mut notes: HashMap<String, u64> = HashMap::new();
    let mut worst: Vec<(f64, String)> = Vec::new();
    let mut slowest = (0.0f64, String::new());
    for (f, lo) in &files {
        let data = match std::fs::read(f) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let name = f.display().to_string();
        let odp = f.extension().is_some_and(|e| e.eq_ignore_ascii_case("odp"));
        let t = Instant::now();
        let got = std::panic::catch_unwind(|| if odp { officina_convert::odp::read(&data).map_err(|e| e.to_string()) } else { officina_convert::pptx::read(&data).map_err(|e| e.to_string()) });
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if ms > slowest.0 {
            slowest = (ms, name.clone());
        }
        let (mut deck, loaded) = match got {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                refused += 1;
                println!("refused\t{name}\t{e}");
                continue;
            }
            Err(_) => {
                panics += 1;
                println!("PANIC\t{name}");
                continue;
            }
        };
        read += 1;
        for l in loaded.report.lines() {
            let f: Vec<&str> = l.split('\t').collect();
            let what = format!("{} {}", f.first().unwrap_or(&""), f.get(2).unwrap_or(&""));
            *notes.entry(what).or_default() += 1;
        }
        let ours = deck_text(&mut deck);
        // Our writer and reader give the same slides back.
        let back = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out = Vec::new();
            let w = if odp { officina_convert::odp::write(&mut deck, &mut out) } else { officina_convert::pptx::write(&mut deck, &mut out) };
            w.ok()?;
            let again = if odp { officina_convert::odp::read(&out).ok() } else { officina_convert::pptx::read(&out).ok() };
            again.map(|(mut d, _)| (d.slide_count(), deck_text(&mut d)))
        }));
        // And through the other format.
        let cross = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut out = Vec::new();
            let w = if odp { officina_convert::pptx::write(&mut deck, &mut out) } else { officina_convert::odp::write(&mut deck, &mut out) };
            w.ok()?;
            let again = if odp { officina_convert::pptx::read(&out).map_err(|e| e.to_string()) } else { officina_convert::odp::read(&out).map_err(|e| e.to_string()) };
            match again {
                Ok((mut d, _)) => Some((d.slide_count(), deck_text(&mut d))),
                Err(e) => {
                    println!("cross read failed\t{name}\t{e}");
                    None
                }
            }
        }));
        match cross {
            Ok(Some((n, t))) if n == deck.slide_count() && words(&t) == words(&ours) => cross_same += 1,
            Ok(Some((n, t))) => println!("cross differs\t{name}\tslides {} -> {n}\twords {} -> {}", deck.slide_count(), words(&ours).values().sum::<usize>(), words(&t).values().sum::<usize>()),
            Ok(None) => println!("cross failed\t{name}"),
            Err(_) => println!("CROSS PANIC\t{name}"),
        }
        match back {
            Ok(Some((n, t))) if n == deck.slide_count() && words(&t) == words(&ours) => rt_same += 1,
            Ok(Some((n, t))) => println!("round trip differs\t{name}\tslides {} -> {n}\twords {} -> {}", deck.slide_count(), words(&ours).values().sum::<usize>(), words(&t).values().sum::<usize>()),
            Ok(None) => println!("round trip failed\t{name}"),
            Err(_) => println!("ROUND TRIP PANIC\t{name}"),
        }
        let Some((pages, lot)) = lo.as_ref().and_then(|l| lo_odp(l)) else { continue };
        slides_compared += 1;
        if pages == deck.slide_count() {
            slides_same += 1;
        } else {
            println!("slides differ\t{name}\tours {} LibreOffice {pages}", deck.slide_count());
        }
        let (a, b) = (words(&ours), words(&lot));
        let total: usize = b.values().sum();
        let got: usize = b.iter().map(|(w, n)| (*n).min(*a.get(w).unwrap_or(&0))).sum();
        have += got;
        want += total;
        if total > 0 && got * 2 < total && std::env::var("SHOW").is_ok() {
            println!("ours\t{name}\t{:?}\nLibreOffice\t{:?}", ours.chars().take(300).collect::<String>(), lot.chars().take(300).collect::<String>());
        }
        if total > 0 {
            worst.push((got as f64 / total as f64, name.clone()));
        }
    }
    worst.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (r, n) in worst.iter().take(15) {
        println!("low recall\t{:.1} %\t{n}", r * 100.0);
    }
    let mut nv: Vec<_> = notes.into_iter().collect();
    nv.sort_by(|a, b| b.1.cmp(&a.1));
    for (w, n) in nv.iter().take(30) {
        println!("note\t{n} files\t{w}");
    }
    println!("files {}  read {read}  refused {refused}  panics {panics}  round trip same {rt_same}/{read}  other format same {cross_same}/{read}", files.len());
    if slides_compared > 0 {
        println!("slide count same as LibreOffice {slides_same}/{slides_compared}; words of LibreOffice's we have {:.2} % ({have}/{want})", 100.0 * have as f64 / want.max(1) as f64);
    }
    println!("slowest {:.0} ms {}", slowest.0, slowest.1);
    if panics > 0 {
        std::process::exit(1);
    }
}
