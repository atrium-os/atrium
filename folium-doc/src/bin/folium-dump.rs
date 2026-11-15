//! folium-dump: what Folium reads from a PDF, for checking the syntax and
//! document layers against real files (folium.md §11, P1).
//!
//!   folium-dump [--password PW] [--pages] [--outline] [--streams] FILE…
//!
//! Prints the summary always; `--pages` one line per page, `--outline`
//! the whole outline, `--streams` decodes every stream in the file and
//! reports failures and time. Exit status 1 if any file didn't open.

use std::collections::BTreeMap;
use std::time::Instant;

use folium_doc::{Doc, OutlineItem, Target};
use folium_syntax::{Object, Ref};

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut pages, mut outline, mut streams) = (false, false, false);
    let mut pw = Vec::new();
    let mut files = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pages" => pages = true,
            "--outline" => outline = true,
            "--streams" => streams = true,
            "--password" => pw = args.next().unwrap_or_default().into_bytes(),
            _ => files.push(a),
        }
    }
    let mut bad = false;
    for f in files {
        if !dump(&f, &pw, pages, outline, streams) {
            bad = true;
        }
    }
    std::process::exit(bad as i32);
}

fn dump(path: &str, pw: &[u8], pages: bool, outline: bool, streams: bool) -> bool {
    println!("== {path}");
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            println!("  open: {e}");
            return false;
        }
    };
    let t = Instant::now();
    let doc = match Doc::open(Box::new(file), pw) {
        Ok(d) => d,
        Err(e) => {
            println!("  error: {e}");
            return false;
        }
    };
    let t_open = t.elapsed();
    let s = &doc.syntax;
    println!(
        "  PDF {}.{}, {} objects, {} pages, opened in {:.1} ms{}{}",
        s.version.0,
        s.version.1,
        s.object_count(),
        doc.page_count(),
        t_open.as_secs_f64() * 1e3,
        if s.repaired() { ", REPAIRED" } else { "" },
        if doc.tree_damaged { ", page tree damaged" } else { "" }
    );
    if let Some(h) = s.security() {
        println!("  encrypted: revision {}, permissions {:#x}, opened as {}", h.revision, h.permissions, if h.owner { "owner" } else { "user" });
    }
    let info = doc.info();
    for (k, v) in [("title", &info.title), ("author", &info.author), ("creator", &info.creator), ("producer", &info.producer)] {
        if let Some(v) = v {
            println!("  {k}: {v}");
        }
    }
    // Page sizes, grouped.
    let t = Instant::now();
    let mut sizes: BTreeMap<String, usize> = BTreeMap::new();
    let mut errs = 0;
    for i in 0..doc.page_count() {
        match doc.page(i) {
            Ok(p) => {
                let (w, h) = p.size();
                *sizes.entry(format!("{w:.0}x{h:.0}{}", if p.rotate != 0 { format!(" rot {}", p.rotate) } else { String::new() })).or_default() += 1;
                if pages {
                    let c = doc.contents(&p).map(|v| v.len()).unwrap_or(0);
                    println!("  page {:>5}: {w:.1} x {h:.1} pt, rotate {}, {} content stream(s), {} resource kinds", i + 1, p.rotate, c, p.resources.len());
                }
            }
            Err(e) => {
                errs += 1;
                if pages {
                    println!("  page {:>5}: error {e}", i + 1);
                }
            }
        }
    }
    let sz: Vec<String> = sizes.iter().map(|(k, n)| format!("{k} pt ×{n}")).collect();
    println!("  page sizes: {} ({} errors, all pages read in {:.1} ms)", sz.join(", "), errs, t.elapsed().as_secs_f64() * 1e3);
    if let Some(l) = doc.page_labels() {
        let show: Vec<&str> = l.iter().take(6).map(String::as_str).collect();
        println!("  page labels: {} … {}", show.join(", "), l.last().map(String::as_str).unwrap_or(""));
    }
    let t = Instant::now();
    let ol = doc.outline();
    let (n, depth, unresolved) = count(&ol, 1);
    println!("  outline: {n} items, depth {depth}, {unresolved} without a page target, read in {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    if outline {
        print_outline(&ol, 2);
    } else {
        for it in ol.iter().take(5) {
            println!("    {} -> {}", it.title, target(&it.target));
        }
    }
    if streams {
        stream_scan(&doc);
    }
    true
}

fn count(items: &[OutlineItem], d: usize) -> (usize, usize, usize) {
    let mut n = 0;
    let mut depth = if items.is_empty() { 0 } else { d };
    let mut un = 0;
    for it in items {
        n += 1;
        if !matches!(it.target, Some(Target::Page { .. })) {
            un += 1;
        }
        let (a, b, c) = count(&it.children, d + 1);
        n += a;
        depth = depth.max(b);
        un += c;
    }
    (n, depth, un)
}

fn target(t: &Option<Target>) -> String {
    match t {
        Some(Target::Page { index, .. }) => format!("page {}", index + 1),
        Some(Target::Uri(u)) => u.clone(),
        Some(Target::Other(o)) => format!("({o})"),
        None => "-".into(),
    }
}

fn print_outline(items: &[OutlineItem], ind: usize) {
    for it in items {
        println!("{:ind$}{} -> {}", "", it.title, target(&it.target));
        print_outline(&it.children, ind + 2);
    }
}

fn stream_scan(doc: &Doc) {
    let s = &doc.syntax;
    let t = Instant::now();
    let (mut objs, mut nstreams, mut bytes, mut failed) = (0usize, 0usize, 0usize, 0usize);
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut codecs: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    for n in 1..s.object_count() as u32 {
        let o = match s.get(Ref::new(n, 0)) {
            Ok(o) => o,
            Err(e) => {
                *errors.entry(format!("object: {e}")).or_default() += 1;
                continue;
            }
        };
        if o.is_null() {
            continue;
        }
        objs += 1;
        let ty = o
            .as_dict()
            .and_then(|d| d.name("Subtype").or(d.name("Type")))
            .map(|n| String::from_utf8_lossy(n.as_bytes()).into_owned())
            .unwrap_or_else(|| kind(&o).into());
        *kinds.entry(ty).or_default() += 1;
        if let Object::Stream(st) = &*o {
            nstreams += 1;
            match s.decode(st) {
                Ok(d) => {
                    bytes += d.data.len();
                    if let Some(c) = d.rest.first() {
                        *codecs.entry(String::from_utf8_lossy(c.name.as_bytes()).into_owned()).or_default() += 1;
                    }
                }
                Err(e) => {
                    failed += 1;
                    *errors.entry(format!("stream: {e}")).or_default() += 1;
                }
            }
        }
    }
    println!(
        "  streams: {objs} objects, {nstreams} streams, {:.1} MB decoded, {failed} failed, in {:.0} ms",
        bytes as f64 / 1e6,
        t.elapsed().as_secs_f64() * 1e3
    );
    let top: Vec<String> = {
        let mut v: Vec<_> = kinds.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.into_iter().take(10).map(|(k, n)| format!("{k} {n}")).collect()
    };
    println!("  object kinds: {}", top.join(", "));
    if !codecs.is_empty() {
        let c: Vec<String> = codecs.iter().map(|(k, n)| format!("{k} {n}")).collect();
        println!("  left for image decoders: {}", c.join(", "));
    }
    for (e, n) in errors {
        println!("  ERROR ×{n}: {e}");
    }
}

fn kind(o: &Object) -> &'static str {
    match o {
        Object::Null => "null",
        Object::Bool(_) => "bool",
        Object::Int(_) | Object::Real(_) => "number",
        Object::String(_) => "string",
        Object::Name(_) => "name",
        Object::Array(_) => "array",
        Object::Dict(_) => "dict",
        Object::Stream(_) => "stream",
        Object::Ref(_) => "ref",
    }
}
