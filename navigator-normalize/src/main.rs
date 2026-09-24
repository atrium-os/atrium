//! navigator-normalize <file.html>...    profile-conformant HTML on stdout
//! navigator-normalize --dir <in> <out>  normalize a whole corpus

use navigator_normalize::{normalize_and_measure, Inputs};

/// `<stem>.links.tsv`: `href<TAB>local-file`. `<stem>.images.tsv`:
/// `src<TAB>width<TAB>height`.
fn sidecars(doc: &std::path::Path) -> Inputs {
    let mut inputs = Inputs::default();
    if let Ok(t) = std::fs::read_to_string(doc.with_extension("links.tsv")) {
        for line in t.lines() {
            let mut it = line.split('\t');
            if let (Some(href), Some(file)) = (it.next(), it.next()) {
                let path = doc.parent().unwrap_or(std::path::Path::new(".")).join(file);
                if let Ok(css) = std::fs::read_to_string(&path) { inputs.stylesheets.insert(href.to_string(), css); }
            }
        }
    }
    if let Ok(t) = std::fs::read_to_string(doc.with_extension("images.tsv")) {
        for line in t.lines() {
            let mut it = line.split('\t');
            if let (Some(src), Some(w), Some(h)) = (it.next(), it.next(), it.next()) {
                if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) { inputs.images.insert(src.to_string(), (w, h).into()); }
            }
        }
    }
    inputs
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    // ★ The whole lane, from one file: converter recording in, profile
    // document out. The normalizer fetches nothing — everything it needs
    // travelled in the recording.
    if a.len() >= 4 && a[1] == "--recordings" {
        let (src, dst) = (std::path::Path::new(&a[2]), std::path::Path::new(&a[3]));
        std::fs::create_dir_all(dst).expect("output directory");
        let fonts = navigator_render::fontset::FontSet::load().expect("pinned font set");
        let env = navigator_style::cascade::Env::default();
        let mut files: Vec<_> = std::fs::read_dir(src).expect("input directory").flatten()
            .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        let mut total = std::collections::BTreeMap::<String, usize>::new();
        let (mut ok, mut sheets, mut images) = (0usize, 0usize, 0usize);
        for f in &files {
            let json = std::fs::read_to_string(f).unwrap_or_default();
            let r = match navigator_normalize::recording::parse(&json) {
                Ok(r) => r,
                Err(e) => { println!("{}: {e}", f.display()); continue }
            };
            sheets += r.inputs.stylesheets.len();
            images += r.inputs.images.len();
            let (out, rep) = normalize_and_measure(&r.document, &r.inputs, &fonts, &env);
            let name = f.file_stem().expect("stem").to_string_lossy().to_string();
            std::fs::write(dst.join(format!("{name}.html")), out).expect("write");
            println!("{:<22} {:>3} sheets {:>4} images {:>5} rules in {:>5} out {:>4} columns",
                name, r.inputs.stylesheets.len(), r.inputs.images.len(), rep.rules_in, rep.rules_out, rep.columns_measured);
            for (k, v) in rep.dropped { *total.entry(k).or_default() += v }
            ok += 1;
        }
        println!("\n{ok} recordings: {sheets} stylesheets, {images} measured images");
        let mut v: Vec<_> = total.into_iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        println!("dropped, by reason:");
        for (k, n) in v.iter().take(if std::env::var_os("NORMALIZE_ALL_DROPS").is_some() { usize::MAX } else { 20 }) { println!("  {n:>7}  {k}") }
        return;
    }
    let fonts = navigator_render::fontset::FontSet::load().expect("pinned font set");
    let env = navigator_style::cascade::Env::default();
    if a.len() >= 4 && a[1] == "--dir" {
        let (src, dst) = (std::path::Path::new(&a[2]), std::path::Path::new(&a[3]));
        std::fs::create_dir_all(dst).expect("output directory");
        let mut files: Vec<_> = std::fs::read_dir(src).expect("input directory").flatten()
            .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "html")).collect();
        files.sort();
        let mut total = std::collections::BTreeMap::<String, usize>::new();
        for f in &files {
            let html = std::fs::read_to_string(f).unwrap_or_default();
            // ★ The normalizer has NO capabilities (§6): the stylesheets and
            // the measured image sizes are handed to it. Here they come from
            // sidecars the fetch step wrote; in the real lane they travel in
            // the converter's recording.
            let inputs = sidecars(f);
            let (out, rep) = normalize_and_measure(&html, &inputs, &fonts, &env);
            std::fs::write(dst.join(f.file_name().expect("name")), out).expect("write");
            println!("{:<22} {:>5} rules in {:>5} out {:>6} styled {:>4} columns measured",
                f.file_name().unwrap().to_string_lossy(), rep.rules_in, rep.rules_out, rep.elements_styled, rep.columns_measured);
            for (k, v) in rep.dropped { *total.entry(k).or_default() += v }
        }
        println!("\ndropped, by reason:");
        let mut v: Vec<_> = total.into_iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        for (k, n) in v.iter().take(30) { println!("  {n:>7}  {k}") }
        return;
    }
    for f in a.iter().skip(1) {
        let html = std::fs::read_to_string(f).expect("readable");
        let (out, rep) = normalize_and_measure(&html, &Inputs::default(), &fonts, &env);
        eprintln!("{rep:?}");
        print!("{out}");
    }
}
