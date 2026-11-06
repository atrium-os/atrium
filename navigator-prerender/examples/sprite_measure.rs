//! sprite_measure <cache-dir> <recording.json>…: external SVG sprites on
//! real documents (2026-11-06). For each recording whose inline icons name
//! another file (`<use href="file.svg#id">`): the files fetched now, from
//! the live site, through the converter's `HttpFetcher` (cached in
//! <cache-dir>), and each inline icon made standalone without and with them,
//! then held to the worker's SVG profile and vector conversion. One TSV line
//! per document:
//!
//! `stem  files  fetched  failed  bytes  fetch_ms  icons_ext  drawn_before  drawn_after  vector_after  refused_after`
//!
//! - `icons_ext`: inline icons with at least one external `<use>`;
//! - `drawn_*`: of those, how many the profile accepts (drawn as an image);
//! - `vector_after`: how many also convert to vector shapes;
//! - `refused_after`: the profile's reasons for the rest, and why a drawn
//!   icon is not vector (drawn as an image instead), counted.

use navigator_prerender::fetch::{Fetcher, HttpFetcher};
use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut f = HttpFetcher::new(std::path::PathBuf::from(&args[0]));
    for path in &args[1..] {
        let json = std::fs::read_to_string(path).unwrap();
        let stem = std::path::Path::new(path).file_name().unwrap().to_string_lossy().trim_end_matches(".json").to_string();
        let Ok(r) = navigator_normalize::recording::parse(&json) else { continue };
        let dom = navigator_dom::parse(&r.document);
        // The files, as the converter finds them.
        let mut files: Vec<String> = vec![];
        for h in dom.by_tag_anywhere("use") {
            let Some(v) = dom.attr(h, "href").or_else(|| dom.attr(h, "xlink:href")) else { continue };
            let Some((file, id)) = v.split_once('#') else { continue };
            if !file.is_empty() && !id.is_empty() && !files.iter().any(|x| x == file) { files.push(file.to_string()) }
        }
        if files.is_empty() { continue }
        let base = url::Url::parse(&r.url).ok();
        let t0 = std::time::Instant::now();
        let mut inputs = navigator_normalize::recording::parse(&json).expect("parsed above").inputs;
        let (mut fetched, mut failed, mut bytes) = (0, 0, 0usize);
        for file in files.iter().take(navigator_normalize::MAX_SPRITE_FILES) {
            let Some(abs) = base.as_ref().and_then(|b| b.join(file).ok()) else { failed += 1; continue };
            match f.get(abs.as_str()) {
                Ok(text) if text.len() <= navigator_normalize::MAX_SPRITE_BYTES => { bytes += text.len(); fetched += 1; inputs.sprites.insert(file.clone(), text); }
                _ => failed += 1,
            }
        }
        let fetch_ms = t0.elapsed().as_secs_f64() * 1e3;
        let before = navigator_normalize::svg_inline::standalone(&r.document, &r.inputs);
        let after = navigator_normalize::svg_inline::standalone(&r.document, &inputs);
        let ext = |s: &str| files.iter().any(|file| s.contains(&format!("\"{file}#")));
        let (mut icons, mut drawn_b, mut drawn_a, mut vector_a) = (0, 0, 0, 0);
        let mut refused: BTreeMap<String, usize> = BTreeMap::new();
        for (b, a) in before.iter().zip(&after) {
            if !ext(b) { continue }
            icons += 1;
            if navigator_svg::load(b.as_bytes()).is_ok() { drawn_b += 1 }
            match navigator_svg::load(a.as_bytes()) {
                Ok((tree, _)) => {
                    drawn_a += 1;
                    match navigator_svg::vector::fills(&tree) {
                        Ok(v) if !v.is_empty() => vector_a += 1,
                        Ok(_) => {
                            // The page's own hiding (`display: none` or
                            // `visibility: hidden` resolved onto the svg) draws
                            // nothing in a browser either.
                            let root = a.split('>').next().unwrap_or("");
                            let why = if root.contains("display=\"none\"") || root.contains("visibility=\"hidden\"") { "hidden by the page (draws nothing, as in a browser)" } else { "vector: no shapes" };
                            *refused.entry(why.into()).or_default() += 1;
                            if let Some(d) = std::env::var_os("SPRITE_DUMP") { let _ = std::fs::write(std::path::Path::new(&d).join(format!("{stem}.noshape.svg")), a); }
                        }
                        Err(e) => *refused.entry(format!("vector: {e}")).or_default() += 1,
                    }
                }
                Err(e) => *refused.entry(format!("{e:?}")).or_default() += 1,
            }
        }
        let why: Vec<String> = refused.iter().map(|(k, n)| format!("{n}x {k}")).collect();
        println!("{stem}\t{}\t{fetched}\t{failed}\t{bytes}\t{fetch_ms:.0}\t{icons}\t{drawn_b}\t{drawn_a}\t{vector_a}\t{}", files.len(), why.join("; "));
    }
}
