//! band_check <normalized.html>… [--band PX] [--sample N] [--no-pixels]:
//! bands phase 2 on real documents. Each document is laid out whole, and
//! band by band with checkpoints (`bands::Banded`, bands cut at the first
//! checkpoint below `--band` px, default 600); then every band (or `--sample`
//! of them, evenly spaced, default 40) is laid out AGAIN from its checkpoint
//! and drawn as a host draws it — its group, then the overlay's leaves for
//! it — and its whole pixel rows are compared with the whole layout's at 8
//! bits per channel.
//!
//! It also counts what fresco-scene-groups.md §6 asked the Navigator thread
//! to measure, on the whole layout:
//! - opacity groups whose leaves overlap (multiplied alpha would be wrong);
//! - rotated or skewed text and images (need a worker-drawn bitmap);
//! - leaves painted outside their own band (the overlay's sources), and of
//!   those reaching other bands, how many the group holds in place (painted
//!   under something there) and how many the overlay draws over;
//! - scene groups per band (one per run of leaves with the same clip,
//!   transform and opacity group, plus the band's own), against 4,096.
//!
//! One TSV line per document:
//! `stem nodes full_ms banded_ms bands whole suppressed reruns cross under over
//!  opacity_groups overlapping_groups rotated_text rotated_images max_groups_per_band
//!  checked differing first_diff`.
use navigator_render::{bands::{self, Banded}, fontset::FontSet, html::render_html, raster, Scene, U};
use navigator_style::cascade::Env;
use std::time::Instant;

const PX: i64 = 64;

fn rows(scene: &Scene, fonts: &FontSet, y0: i64, y1: i64) -> Vec<[u8; 4]> {
    let mut s = bands::translated(scene, y0 * PX);
    s.height = (y1 - y0) * PX;
    raster::paint(&s, fonts, &Default::default()).px.iter().map(|p| p.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)).collect()
}

/// A leaf's box, page units, through its transform's translation only
/// (enough to tell overlap of axis-aligned leaves).
fn leaf_box(s: &Scene, kind: u8, i: usize) -> Option<(U, U, U, U)> {
    let (y0, y1) = bands::extent(s, kind, i)?;
    let (x0, x1) = match kind {
        0 => (s.rects[i].x, s.rects[i].x + s.rects[i].w),
        1 => { let r = &s.runs[i]; (r.x, r.x + r.glyphs.last().map(|g| g.1).unwrap_or(0) + r.size) }
        3 => (s.shadows[i].x, s.shadows[i].x + s.shadows[i].w),
        4 => (s.grads[i].area.x, s.grads[i].area.x + s.grads[i].area.w),
        5 => (s.images[i].area.x, s.images[i].area.x + s.images[i].area.w),
        _ => return None,
    };
    (x1 > x0).then_some((x0, y0, x1, y1))
}

fn attrs(s: &Scene, kind: u8, i: usize) -> navigator_render::Attrs {
    match kind { 0 => s.rect_attrs[i], 1 => s.run_attrs[i], 2 => s.link_attrs[i], 3 => s.shadow_attrs[i],
                 4 => s.grad_attrs[i], 5 => s.image_attrs[i], _ => s.path_attrs[i] }
}

/// (opacity groups, groups whose leaves overlap one another).
fn overlapping_groups(s: &Scene) -> (usize, usize) {
    let mut by_group: std::collections::BTreeMap<u32, Vec<(U, U, U, U)>> = Default::default();
    for &(k, i) in &s.order {
        if let Some(g) = attrs(s, k, i).1 {
            if k == 2 { continue } // a link paints nothing
            if let Some(b) = leaf_box(s, k, i) { by_group.entry(g).or_default().push(b) }
        }
    }
    let mut n = 0;
    for boxes in by_group.values_mut() {
        boxes.sort_by_key(|b| b.1);
        let mut hit = false;
        'outer: for a in 0..boxes.len() {
            for b in a + 1..boxes.len() {
                if boxes[b].1 >= boxes[a].3 { break }
                let (p, q) = (boxes[a], boxes[b]);
                if p.0 < q.2 && q.0 < p.2 && p.1 < q.3 && q.1 < p.3 { hit = true; break 'outer }
            }
        }
        n += hit as usize;
    }
    (by_group.len(), n)
}

fn rotated(s: &Scene) -> (usize, usize) {
    let rot = |a: navigator_render::Attrs| a.2.and_then(|x| s.xforms.get(x as usize)).is_some_and(|m| m[1] != 0 || m[2] != 0);
    (s.run_attrs.iter().filter(|a| rot(**a)).count(), s.image_attrs.iter().filter(|a| rot(**a)).count())
}

/// Scene groups a band needs: one per run of consecutive leaves with the
/// same (clip, transform, opacity group), and the band's own.
fn groups_in(s: &Scene) -> usize {
    let mut n = 1;
    let mut last = None;
    for &(k, i) in &s.order {
        let a = attrs(s, k, i);
        if last != Some(a) { n += 1; last = Some(a) }
    }
    n
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).map(|v| v.parse::<i64>().unwrap());
    let band_px = arg("--band").unwrap_or(600);
    let sample = arg("--sample").unwrap_or(40) as usize;
    let pixels = !args.iter().any(|a| a == "--no-pixels");
    let fonts = FontSet::load().unwrap();
    let env = Env::default();
    for p in args.iter().filter(|a| a.ends_with(".html")) {
        let html = std::fs::read_to_string(p).unwrap();
        let stem = std::path::Path::new(p).file_stem().unwrap().to_string_lossy().to_string();
        let t = Instant::now();
        let full = render_html(&html, &fonts, &env).scene;
        let full_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let mut b = Banded::new(&html, &fonts, &env, &Default::default(), band_px * PX);
        b.settle();
        let banded_ms = t.elapsed().as_secs_f64() * 1e3;
        let n = b.laid_out();
        if let Ok(y) = std::env::var("BAND_PROBE") {
            let y: i64 = y.parse::<i64>().unwrap() * PX;
            let k = b.band_at(y).unwrap();
            for kk in k.saturating_sub(1)..=k + 1 {
                let r = b.bands[kk].range;
                eprintln!("band {kk}: {}..{} checkpoint {:?} {}", r.0 / PX, r.1 / PX, b.checkpoints.get(kk).map(|c| (c.at, c.y / PX)),
                          b.checkpoints.get(kk).and_then(|c| c.at).map(|h| b.describe(h)).unwrap_or_default());
            }
            b.ensure(k);
            let c = b.composite(k).unwrap();
            for run in &c.runs { if (run.y - y).abs() < 40 * PX { eprintln!("  run {:?} at {} {}", run.text, run.x / PX, run.y / PX) } }
            for run in &full.runs { if (run.y - y).abs() < 40 * PX { eprintln!("  whole run {:?} at {} {}", run.text, run.x / PX, run.y / PX) } }
        }
        let (ogroups, overlap) = overlapping_groups(&full);
        let (rtext, rimg) = rotated(&full);
        let idx = bands::index(&full, 64 * PX);
        let picks: Vec<usize> = if n <= sample { (0..n).collect() } else {
            let mut v: Vec<usize> = (0..sample).map(|i| i * (n - 1) / (sample - 1)).collect(); v.dedup(); v
        };
        let (mut checked, mut differing, mut first_diff, mut under, mut over, mut maxg) = (0, 0, String::new(), 0, 0, 0);
        for &k in &picks {
            b.ensure(k);
            let (u, o) = b.foreign_counts(k);
            under += u; over += o;
            let c = b.composite(k).unwrap();
            if let Ok(t) = std::env::var("BAND_FIND") {
                let r = b.bands[k].range;
                for (n, run) in c.runs.iter().enumerate() { if run.text == t { eprintln!("band {k} ({}..{}) run {t:?} at {} {} (#{n})", r.0 / PX, r.1 / PX, run.x / PX, run.y / PX) } }
                if k == picks[0] { for run in &full.runs { if run.text == t { eprintln!("whole: run {t:?} at {} {}", run.x / PX, run.y / PX) } } }
            }
            maxg = maxg.max(groups_in(&b.group(k).unwrap()));
            if pixels {
                let (y0, y1) = b.bands[k].range;
                // (At most 2,400 rows of a band: a one-band document is the whole page.)
                let (y0, y1) = ((y0.max(0) + PX - 1) / PX, y1.min(full.height) / PX);
                let y1 = y1.min(y0 + 2400);
                if y0 < y1 {
                    checked += 1;
                    let ci = bands::index(&c, 64 * PX);
                    let mine = rows(&bands::scene_of(&c, &ci.nodes_in(y0 * PX, y1 * PX)), &fonts, y0, y1);
                    let theirs = rows(&bands::scene_of(&full, &idx.nodes_in(y0 * PX, y1 * PX)), &fonts, y0, y1);
                    if mine != theirs {
                        differing += 1;
                        if first_diff.is_empty() {
                            let w = (full.width / PX) as usize;
                            let i = mine.iter().zip(&theirs).position(|(a, c)| a != c).unwrap();
                            let nd = mine.iter().zip(&theirs).filter(|(a, c)| a != c).count();
                            let md = mine.iter().zip(&theirs).map(|(a, c)| (0..4).map(|j| a[j].abs_diff(c[j])).max().unwrap()).max().unwrap();
                            first_diff = format!("band {k} x={} y={} px {nd} maxlev {md}", i % w, y0 + (i / w) as i64);
                            if std::env::var_os("BAND_DIFF").is_some() {
                                let yy = (y0 + (i / w) as i64) * PX;
                                let xx = (i % w) as i64 * PX;
                                let full_here = bands::scene_of(&full, &idx.nodes_in(yy, yy + PX));
                                for (name, sc) in [("banded", &c), ("whole", &full_here)] {
                                    eprintln!("{stem} {name} @ x={} y={} ({:?} vs {:?}):", xx / PX, yy / PX, mine[i], theirs[i]);
                                    for &(kk, ii) in &sc.order {
                                        let Some((a, bb)) = bands::extent(sc, kk, ii) else { eprintln!("   (no extent) kind {kk}"); continue };
                                        if a > yy + 20 * PX || bb <= yy - 20 * PX { continue }
                                        if let Some((x0, _, x1, _)) = leaf_box(sc, kk, ii) { if xx < x0 - 40 * PX || xx >= x1 + 40 * PX { continue } }
                                        let at = attrs(sc, kk, ii);
                                        let d = match kk {
                                            0 => { let r = &sc.rects[ii]; format!("rect {} {} {} {} {:08x} ring{}", r.x / PX, r.y / PX, r.w / PX, r.h / PX, r.rgba, r.ring) }
                                            1 => { let r = &sc.runs[ii]; format!("run {} {} {:?}", r.x / PX, r.y / PX, r.text) }
                                            3 => { let r = &sc.shadows[ii]; format!("shadow {} {} {} {}", r.x / PX, r.y / PX, r.w / PX, r.h / PX) }
                                            5 => { let a = &sc.images[ii].area; format!("image {} {} {} {}", a.x / PX, a.y / PX, a.w / PX, a.h / PX) }
                                            6 => { let p = &sc.paths[ii]; format!("path pts{} t{:?} clip{:?}", p.points.len(), p.transform, p.clip) }
                                            k => format!("kind {k}"),
                                        };
                                        eprintln!("   {d}  clip{:?} g{:?} x{:?}", at.0.map(|c| sc.clips[c as usize]).map(|c| (c.0 / PX, c.1 / PX, c.2 / PX, c.3 / PX)),
                                                  at.1.map(|g| sc.groups[g as usize]), at.2.map(|x| sc.xforms[x as usize]));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            b.evict(k);
        }
        let st = b.stats;
        println!("{stem}\t{}\t{full_ms:.0}\t{banded_ms:.0}\t{n}\t{}\t{}\t{}\t{}\t{under}\t{over}\t{ogroups}\t{overlap}\t{rtext}\t{rimg}\t{maxg}\t{checked}\t{differing}\t{first_diff}",
                 full.order.len(), st.whole as u8, st.suppressed, st.reruns, st.cross_leaves);
    }
}
