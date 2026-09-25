//! wpt-reftest <wpt-root> <dir>... [--limit SECS]
//! wpt-reftest --one <wpt-root> <test-file>
//!
//! ★ THE ORACLE. The Web Platform Tests' CSS reftests are pairs: a test page
//! and a REFERENCE page, written with simpler CSS, that must render
//! identically. That comparison needs no browser — only this lane, run on
//! both — so it is a conformance oracle for the whole path a real page takes:
//! normalizer → profile renderer → pixels, at WPT's 800×600 viewport.
//!
//! A test is SKIPPED, with its reason, when the oracle cannot be fair to it:
//! it needs the Ahem test font (not in the pinned set), it runs script, or
//! its reference is not an HTML page. Every other test PASSes or FAILs, and
//! a FAIL lists what the normalizer had to drop from the test but not from
//! the reference — the features the failure most likely hangs on.
//!
//! Each test runs in a CHILD with a time limit: a renderer hang is a
//! finding, and must not stall the other few thousand.

use navigator_normalize::{normalize_and_measure, Inputs};
use navigator_render::fontset::FontSet;
use navigator_style::cascade::Env;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const W: usize = 800;
const H: usize = 600;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(String::as_str) == Some("--one") {
        let (root, test) = (PathBuf::from(&a[2]), PathBuf::from(&a[3]));
        let save = a.iter().position(|x| x == "--save").and_then(|i| a.get(i + 1)).map(PathBuf::from);
        let (status, detail) = one(&root, &test, save.as_deref());
        println!("R\t{status}\t{detail}");
        return;
    }
    let Some(root) = a.get(1).map(PathBuf::from) else {
        eprintln!("usage: wpt-reftest <wpt-root> <dir>... [--limit SECS]"); std::process::exit(2)
    };
    let limit: u64 = a.iter().position(|x| x == "--limit").and_then(|i| a.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(30);
    let dirs: Vec<&String> = a[2..].iter().take_while(|x| !x.starts_with("--")).collect();
    let mut tests = vec![];
    for d in &dirs { walk(&root.join(d), &mut tests) }
    tests.sort();
    let exe = std::env::current_exe().expect("exe");
    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut fail_reasons: BTreeMap<String, usize> = BTreeMap::new();
    for t in &tests {
        let rel = t.strip_prefix(&root).unwrap_or(t).display().to_string();
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--one").arg(&root).arg(t);
        if let Some(i) = a.iter().position(|x| x == "--save") { cmd.arg("--save").arg(&a[i + 1]); }
        let mut child = cmd
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().expect("spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(limit);
        let status = loop {
            if let Some(st) = child.try_wait().expect("wait") { break Some(st) }
            if std::time::Instant::now() >= deadline { let _ = child.kill(); let _ = child.wait(); break None }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let mut out = String::new();
        if let Some(mut so) = child.stdout.take() { use std::io::Read; let _ = so.read_to_string(&mut out); }
        let (st, detail) = match status {
            None => ("HANG".to_string(), format!("killed after {limit} s")),
            Some(s) if !s.success() => ("CRASH".to_string(), String::new()),
            Some(_) => match out.lines().find_map(|l| l.strip_prefix("R\t")) {
                Some(r) => { let mut p = r.splitn(2, '\t'); (p.next().unwrap_or("?").to_string(), p.next().unwrap_or("").to_string()) }
                None => ("CRASH".to_string(), "no result line".to_string()),
            },
        };
        if st == "FAIL" {
            for r in detail.split(" | ").skip(1).filter(|r| !r.is_empty()) { *fail_reasons.entry(r.to_string()).or_default() += 1 }
        }
        *by_status.entry(st.split(':').next().unwrap_or("?").to_string()).or_default() += 1;
        println!("{st}\t{rel}\t{detail}");
    }
    println!("\n{} reftests", tests.len());
    for (k, v) in &by_status { println!("  {k:<8} {v}") }
    let mut fr: Vec<_> = fail_reasons.into_iter().collect();
    fr.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("what failing tests had to drop (and their references did not), by number of tests:");
    for (k, n) in fr.iter().take(30) { println!("  {n:>5}  {k}") }
}

/// Every file that names a reference.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if p.is_dir() {
            if !matches!(name.as_str(), "reference" | "support" | "resources") { walk(&p, out) }
            continue;
        }
        if !(name.ends_with(".html") || name.ends_with(".htm") || name.ends_with(".xht") || name.ends_with(".xhtml")) { continue }
        if name.contains("-ref.") || name.contains("-notref.") { continue }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let l = text.to_ascii_lowercase();
        if l.contains("rel=\"match\"") || l.contains("rel='match'") || l.contains("rel=match")
            || l.contains("rel=\"mismatch\"") || l.contains("rel='mismatch'") || l.contains("rel=mismatch") {
            out.push(p);
        }
    }
}

/// `href` resolved against a document or stylesheet at `from`.
fn resolve(root: &Path, from: &Path, href: &str) -> Option<PathBuf> {
    let href = href.split(['#', '?']).next()?.trim();
    if href.is_empty() || href.contains("://") || href.starts_with("data:") { return None }
    Some(match href.strip_prefix('/') { Some(r) => root.join(r), None => from.parent()?.join(href) })
}

struct Loaded {
    html: String,
    inputs: Inputs,
    subs: navigator_render::Subresources,
    assets: BTreeMap<String, PathBuf>,
    /// Every stylesheet's text, for the skip checks.
    css: String,
    /// Files the document references that are not in the checkout (the
    /// clone is sparse). A missing image paints nothing on EITHER side, so
    /// comparing would measure the checkout: a blank test "matched" a blank
    /// reference whose black square was never fetched.
    missing: Vec<String>,
}

/// A document and everything it references, as a recording would carry it.
fn load(root: &Path, file: &Path) -> Option<Loaded> {
    let mut html = std::fs::read_to_string(file).ok()?;
    let mut missing: Vec<String> = vec![];
    // ★ WPT serves `.xht` as XHTML, where `<![CDATA[ … ]]>` inside <style> is
    // plain text; parsed as HTML the markers corrupt the first rule, and a
    // reference's only rule vanished. The lane has no XHTML mode (a
    // converter gap); for the oracle the markers are unwrapped.
    if file.extension().is_some_and(|e| e == "xht" || e == "xhtml") {
        html = html.replace("<![CDATA[", "").replace("]]>", "");
    }
    let dom = navigator_dom::parse(&html);
    let mut inputs = Inputs::default();
    let mut css = String::new();
    let mut urls: Vec<(String, PathBuf)> = vec![];
    let url_re = |text: &str, base: &Path, urls: &mut Vec<(String, PathBuf)>| {
        let mut rest = text;
        while let Some(i) = rest.find("url(") {
            rest = &rest[i + 4..];
            let end = rest.find(')').unwrap_or(rest.len());
            let raw = rest[..end].trim().trim_matches(['"', '\'']).to_string();
            if let Some(p) = resolve(root, base, &raw) { urls.push((raw, p)) }
            rest = &rest[end..];
        }
    };
    for h in dom.by_tag_anywhere("link") {
        let rel = dom.attr(h, "rel").unwrap_or("").to_ascii_lowercase();
        if !rel.split_whitespace().any(|r| r == "stylesheet") { continue }
        let Some(href) = dom.attr(h, "href") else { continue };
        let Some(path) = resolve(root, file, href) else { continue };
        let Ok(text) = std::fs::read_to_string(&path) else { missing.push(href.to_string()); continue };
        url_re(&text, &path, &mut urls);
        css.push_str(&text);
        inputs.stylesheets.insert(href.to_string(), text);
    }
    for h in dom.by_tag_anywhere("style") { let t = dom.text_content(h); url_re(&t, file, &mut urls); css.push_str(&t) }
    for h in dom.by_tag_anywhere("img") {
        if let Some(src) = dom.attr(h, "src") { if let Some(p) = resolve(root, file, src) { urls.push((src.to_string(), p)) } }
    }
    for n in &dom.nodes {
        if let Some((_, v)) = n.attrs.iter().find(|(k, _)| k == "style") { url_re(v, file, &mut urls) }
    }
    let mut subs = navigator_render::Subresources::new();
    let mut assets = BTreeMap::new();
    for (raw, path) in urls {
        let Ok(bytes) = std::fs::read(&path) else { missing.push(raw); continue };
        let Some(n) = navigator_prerender::subresource::natural_size(&bytes) else { continue };
        let address = format!("blake3:{}", blake3::hash(&bytes).to_hex());
        inputs.images.insert(raw.clone(), navigator_normalize::ImageSize { width: n.width, height: n.height, ratio: n.ratio });
        subs.insert(raw, (address.clone(), n.width.unwrap_or(0) as i64 * 64, n.height.unwrap_or(0) as i64 * 64));
        assets.insert(address, path);
    }
    Some(Loaded { html, inputs, subs, assets, css, missing })
}

/// Normalize, render and paint one document; the 800×600 viewport as RGB,
/// and what the normalizer dropped.
fn pixels(l: &Loaded, fonts: &FontSet) -> (Vec<u8>, BTreeMap<String, usize>, usize, String) {
    let env = Env::default();
    let (doc, report) = normalize_and_measure(&l.html, &l.inputs, fonts, &env);
    let o = navigator_render::html::render_html_with(&doc, fonts, &env, &l.subs);
    let cv = navigator_render::raster::paint(&o.scene, fonts, &l.assets);
    let rgb = cv.rgb8();
    // The viewport: WPT compares an 800×600 screenshot.
    let mut out = vec![255u8; W * H * 3];
    for y in 0..H.min(cv.h) {
        let n = W.min(cv.w) * 3;
        out[y * W * 3..y * W * 3 + n].copy_from_slice(&rgb[y * cv.w * 3..y * cv.w * 3 + n]);
    }
    (out, report.dropped, o.diagnostics.len(), doc)
}

/// `<meta name=fuzzy content="maxDifference=0-2;totalPixels=0-100">`, in its
/// long and short forms: the tolerance a test declares for itself.
fn fuzzy(html: &str) -> (u8, usize) {
    let dom = navigator_dom::parse(html);
    for h in dom.by_tag_anywhere("meta") {
        if dom.attr(h, "name") != Some("fuzzy") { continue }
        let c = dom.attr(h, "content").unwrap_or("");
        let c = c.rsplit(':').next().unwrap_or(c);
        let parts: Vec<&str> = c.split(';').collect();
        let hi = |s: &str| s.split('=').last().unwrap_or(s).split('-').last().unwrap_or("0").trim().parse::<usize>().unwrap_or(0);
        if parts.len() >= 2 { return (hi(parts[0]).min(255) as u8, hi(parts[1])) }
    }
    (0, 0)
}

fn save_png(path: &Path, rgb: &[u8]) {
    let Ok(f) = std::fs::File::create(path) else { return };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), W as u32, H as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    if let Ok(mut w) = enc.write_header() { let _ = w.write_image_data(rgb); }
}

fn one(root: &Path, test: &Path, save: Option<&Path>) -> (String, String) {
    let Some(t) = load(root, test) else { return ("ERROR".into(), "unreadable".into()) };
    let dom = navigator_dom::parse(&t.html);
    let mut refs = vec![];
    for h in dom.by_tag_anywhere("link") {
        let rel = dom.attr(h, "rel").unwrap_or("").to_ascii_lowercase();
        if rel == "match" || rel == "mismatch" {
            if let Some(href) = dom.attr(h, "href") { refs.push((rel == "match", href.to_string())) }
        }
    }
    let Some((is_match, href)) = refs.first().cloned() else { return ("SKIP:no-ref".into(), String::new()) };
    if !(href.ends_with(".html") || href.ends_with(".htm") || href.ends_with(".xht") || href.ends_with(".xhtml")) {
        return ("SKIP:non-html-ref".into(), href);
    }
    let Some(rpath) = resolve(root, test, &href) else { return ("SKIP:no-ref".into(), href) };
    let Some(r) = load(root, &rpath) else { return ("ERROR".into(), format!("reference unreadable: {href}")) };
    for (what, doc) in [("test", &t), ("reference", &r)] {
        if let Some(m) = doc.missing.first() { return ("SKIP:missing-file".into(), format!("{what}: {m}")) }
        // The REVIEW rasterizer decodes PNG only; an SVG, GIF or JPEG image
        // would be laid out but never painted, and the comparison would
        // measure the tool. Skipped, and labelled as the tool's limit.
        if doc.assets.values().any(|p| !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))) {
            return ("SKIP:non-png-image".into(), what.into());
        }
        let all = format!("{}{}", doc.html, doc.css);
        if all.contains("Ahem") || all.contains("ahem") { return ("SKIP:ahem".into(), what.into()) }
        if doc.html.to_ascii_lowercase().contains("<script") { return ("SKIP:script".into(), what.into()) }
    }
    let fonts = FontSet::load().expect("pinned font set");
    let (a, drops_t, ref_t, doc_t) = pixels(&t, &fonts);
    let (b, drops_r, ref_r, doc_r) = pixels(&r, &fonts);
    let (maxd_ok, total_ok) = fuzzy(&t.html);
    let (mut differ, mut maxd) = (0usize, 0u8);
    for (p, q) in a.chunks(3).zip(b.chunks(3)) {
        let d = p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
        if d > 0 { differ += 1; maxd = maxd.max(d) }
    }
    let same = differ == 0 || (maxd <= maxd_ok && differ <= total_ok);
    let pass = same == is_match;
    // `--save DIR`: a failure's two renders, to look at.
    if let (Some(dir), false) = (save, pass) {
        let stem = test.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let _ = std::fs::create_dir_all(dir);
        save_png(&dir.join(format!("{stem}.test.png")), &a);
        save_png(&dir.join(format!("{stem}.ref.png")), &b);
        let _ = std::fs::write(dir.join(format!("{stem}.test.html")), &doc_t);
        let _ = std::fs::write(dir.join(format!("{stem}.ref.html")), &doc_r);
    }
    // What only the test lost — the likeliest reasons for a failure.
    let mut only: Vec<String> = drops_t.keys().filter(|k| !drops_r.contains_key(*k)).cloned().collect();
    only.truncate(6);
    let refused = if ref_t + ref_r > 0 { format!(" refused={}", ref_t + ref_r) } else { String::new() };
    let detail = format!("{}px maxdiff={maxd}{refused} | {}", differ, only.join(" | "));
    ((if pass { "PASS" } else { "FAIL" }).into(), detail)
}
