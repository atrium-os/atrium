//! zoom_measure <dir>… [--ext readable|any] [--levels 32,48,...]: zoom and
//! text size on real documents (backend §4.9, measured). For every recording:
//! normalize once at 800 x 600 (as the cache would hold it), then re-lay it
//! out at each page zoom and each text size, and print one TSV line per
//! (document, mode, level):
//!
//! `stem  mode  level  ms  repro  same100  doc_w_px  doc_h_px  tables  over_min  anchors  kept  clamped  hidden`
//!
//! - `mode`: `page` (layout at 800/zoom x 600/zoom, `Env::zoomed`) or `text`
//!   (`text_scale_64`);
//! - `ms`: the zoom step — one layout plus scene of the cached normalized
//!   document, the work a zoom costs (normalize is not repeated);
//! - `repro`: two renders at this level gave byte-identical NSG (1/0);
//! - `same100`: at level 64 only, the NSG equals the plain default render
//!   (1/0; `-` at other levels);
//! - `doc_w_px`: the furthest right edge of any laid-out box, in CSS px of
//!   the zoomed layout, against
//!   its viewport (800/zoom for page, 800 for text): wider = the page
//!   scrolls sideways;
//! - `tables`, `over_min`: tables pre-measured, and how many need more than
//!   the available width at their min-content columns (scaled for text);
//! - `anchors`, `kept`, `clamped`: reading positions at 10%..90% of the
//!   100% layout carried through `navigator_render::anchor`; kept = the
//!   anchor's element contains the new scroll position at the same fraction;
//!   clamped = the position was pinned to the document's end; hidden = the
//!   element is not laid out at the new size (a media query hid it).
//!
//! With `--tables`, instead one line per document and text level:
//! `stem  TABLES  level  cols  diff_p50_px  diff_p90_px  diff_max_px  under  over`
//! comparing the 100% widths SCALED (nearest 1/64, halves away from zero —
//! the design the corpus refuted) with an exact re-measure at that text size
//! (`under`: scaled min-content below the re-measured one — a column that
//! would be too narrow for its longest word).

use navigator_render::{anchor, fontset::FontSet, html};
use navigator_style::cascade::Env;
use std::collections::BTreeMap;

const MARK: &str = "atrium-zk";

/// Pre-measured columns per table: `(min, max)` px, grouped by the element
/// that holds a table's declarations (a colgroup, or a CSS table's first row).
fn tables_of(doc: &str) -> Vec<Vec<(f64, f64)>> {
    let dom = navigator_dom::parse(doc);
    let mut css = String::new();
    for h in dom.by_tag_anywhere("style") { if dom.attr(h, "atrium-premeasured").is_some() { css = dom.text_content(h) } }
    let mut cols: BTreeMap<usize, (f64, f64)> = BTreeMap::new();
    for line in css.lines() {
        let Some(rest) = line.strip_prefix(".tc") else { continue };
        let k: usize = rest.split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap();
        let px = |name: &str| line.split(name).nth(1).and_then(|r| r.split("px").next()).and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
        cols.insert(k, (px("min-width:"), px("max-width:")));
    }
    let mut groups: BTreeMap<u32, Vec<(f64, f64)>> = BTreeMap::new();
    for h in 0..dom.nodes.len() as u32 {
        let Some(class) = dom.attr(h, "class") else { continue };
        for c in class.split_ascii_whitespace() {
            if let Some(k) = c.strip_prefix("tc").and_then(|k| k.parse::<usize>().ok()) {
                if let (Some(v), Some(p)) = (cols.get(&k), dom.nodes[h as usize].parent) { groups.entry(p).or_default().push(*v) }
            }
        }
    }
    groups.into_values().collect()
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() { return 0.0 }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let ext = opt("--ext").unwrap_or_else(|| "readable".into());
    let levels: Vec<u32> = opt("--levels").unwrap_or_else(|| "32,48,64,80,96,128,160,192".into())
        .split(',').map(|v| v.parse().unwrap()).collect();
    let tables_mode = args.iter().any(|a| a == "--tables");
    let skip: Vec<&String> = ["--ext", "--levels"].iter().filter_map(|n| args.iter().position(|a| a == n).map(|i| &args[i + 1])).collect();
    let fonts = FontSet::load().expect("fonts");
    let base = Env::default();
    for dir in args.iter().filter(|a| !a.starts_with("--") && !skip.contains(a)) {
        let mut files: Vec<_> = std::fs::read_dir(dir).expect("dir").flatten().map(|e| e.path())
            .filter(|p| { let n = p.file_name().unwrap().to_string_lossy().to_string();
                          if ext == "any" { n.ends_with(".json") } else { n.ends_with(&format!(".{ext}.json")) } }).collect();
        files.sort();
        for p in files {
            let json = std::fs::read_to_string(&p).unwrap();
            let stem = p.file_name().unwrap().to_string_lossy().trim_end_matches(".json").to_string();
            let Ok(r) = navigator_normalize::recording::parse(&json) else { println!("{stem}\tunparsed"); continue };
            let subs = navigator_render::conformance::subresources_from_recording_json(&json);
            if tables_mode {
                let (doc, _) = navigator_normalize::normalize(&r.document, &r.inputs);
                let (at100, n) = html::premeasure_tables(&doc, &fonts, &base);
                if n == 0 { continue }
                let t100 = tables_of(&at100);
                for &t in levels.iter().filter(|&&t| t != 64) {
                    let env = Env { text_scale_64: t, ..base.clone() };
                    let (exact, _) = html::premeasure_tables_at(&doc, &fonts, &env);
                    let te = tables_of(&exact);
                    let mut diffs = vec![];
                    let (mut under, mut over, mut cols) = (0, 0, 0);
                    for (a, b) in t100.iter().flatten().zip(te.iter().flatten()) {
                        let s = |v: f64| (((v * 64.0).round() as i64 * t as i64 + 32).div_euclid(64)) as f64 / 64.0;
                        for (scaled, real) in [(s(a.0), b.0), (s(a.1), b.1)] { diffs.push((scaled - real).abs()); }
                        if std::env::var_os("ZOOM_COLS").is_some() { println!("{stem}\tCOL\t{t}\t{}\t{}\t{}\t{}\t{}\t{}", a.0, a.1, s(a.0), s(a.1), b.0, b.1) }
                        if s(a.0) < b.0 { under += 1 }
                        if s(a.0) > b.0 { over += 1 }
                        cols += 1;
                    }
                    let (p50, p90, mx) = (pct(&mut diffs, 0.5), pct(&mut diffs, 0.9), pct(&mut diffs, 1.0));
                    println!("{stem}\tTABLES\t{t}\t{cols}\t{p50:.3}\t{p90:.3}\t{mx:.3}\t{under}\t{over}");
                }
                continue;
            }
            let (normalized, _) = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, &fonts, &base);
            let tables = tables_of(&normalized);
            let plain = navigator_render::nsg::write(&html::render_html_with(&normalized, &fonts, &base, &subs).scene, &fonts);
            // The marked copy, for geometry: one attribute per element, no
            // effect on layout (the normalizer's output selects by class).
            let marked = { let d = navigator_dom::parse(&normalized); d.serialize_marked(MARK) };
            let geo100 = html::layout_geometry(&marked, &fonts, &base, &subs, MARK);
            let vh = |z: u32| (600.0 * 64.0 / z as f64 * 64.0) as i64;
            for mode in ["page", "text"] {
                for &z in &levels {
                    let env = if mode == "page" { Env::zoomed(800.0, 600.0, z) } else { Env { text_scale_64: z, ..base.clone() } };
                    let t0 = std::time::Instant::now();
                    let o = html::render_html_with(&normalized, &fonts, &env, &subs);
                    let ms = t0.elapsed().as_secs_f64() * 1e3;
                    let a = navigator_render::nsg::write(&o.scene, &fonts);
                    let b = navigator_render::nsg::write(&html::render_html_with(&normalized, &fonts, &env, &subs).scene, &fonts);
                    let same100 = if z == 64 { if a == plain { "1" } else { "0" } } else { "-" };
                    let avail = env.width_px;
                    let ts = if mode == "text" { z as f64 / 64.0 } else { 1.0 };
                    let over_min = tables.iter().filter(|t| t.iter().map(|c| c.0 * ts).sum::<f64>() > avail).count();
                    // Anchors: 10%..90% down the 100% layout.
                    let geo = html::layout_geometry(&marked, &fonts, &env, &subs, MARK);
                    let (mut n, mut kept, mut clamped, mut hidden) = (0, 0, 0, 0);
                    let vh_new = if mode == "page" { vh(z) } else { 600 * 64 };
                    // Reading positions a reader can scroll to at 100%.
                    let max100 = (geo100.height - 600 * 64).max(0);
                    for i in 1..10 {
                        let y = max100 * i / 10;
                        let Some(an) = anchor::anchor_at(&geo100, y) else { continue };
                        n += 1;
                        let Some(sy) = anchor::scroll_for(&geo, an, vh_new) else { hidden += 1; continue };
                        let g = &geo.elements[&an.key];
                        let want = g.y + (g.h as i128 * an.frac as i128 / 65536) as i64;
                        if sy != want { clamped += 1 } else if sy >= g.y && sy < g.y + g.h.max(1) { kept += 1 }
                    }
                    // The horizontal extent: the furthest right edge laid out.
                    let right = geo.elements.values().map(|g| g.x + g.w).max().unwrap_or(0);
                    println!("{stem}\t{mode}\t{z}\t{ms:.1}\t{}\t{same100}\t{:.0}\t{:.0}\t{}\t{over_min}\t{n}\t{kept}\t{clamped}\t{hidden}",
                             (a == b) as u8, right as f64 / 64.0, geo.height as f64 / 64.0, tables.len());
                }
            }
        }
    }
}
