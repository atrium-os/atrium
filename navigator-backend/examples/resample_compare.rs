//! Choosing the downsampling filter by MEASUREMENT (threat model §2.4).
//!
//!     resample_compare prepare PAINTED.tsv OUT [EVERY]
//!     (then: sips and headless Chrome over OUT, see OUT/run.sh)
//!     resample_compare compare OUT
//!
//! `PAINTED.tsv` is `doc hex blobpath tw th uses` (the largest tile each
//! image is painted at in a scene, CSS px). `prepare` decodes every EVERY-th
//! image that is painted SMALLER than it is (a reduction on both axes),
//! writes the decoded natural frame as a lossless PNG (`src/`), each filter's
//! reduction to exactly the painted size (`ours/<filter>/`), and HTML pages
//! that place the same natural PNG at that size for Chrome.
//!
//! ★ The references are fed OUR decoded frame, not the original bytes, so
//! the comparison measures the RESAMPLER and not three JPEG decoders'
//! IDCTs (whose differences, measured on the same corpus, are a mean of up
//! to 2.2 levels — the size of the effect being measured).
//!
//! `compare` composites everything on white (as the page is) and reports,
//! per filter, the mean absolute difference per channel and PSNR against
//! Chrome's screenshot and against `sips -z`.

use navigator_backend::image::{decode_unaddressed, Ceilings};
use navigator_backend::resample::{resample, Filter};
use std::io::Write;

fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) {
    let f = std::fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(rgba).unwrap();
}

fn read_png(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    let mut d = png::Decoder::new(std::fs::File::open(path).ok()?);
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width, info.height);
    let n = info.color_type.samples();
    buf.truncate(info.buffer_size());
    let rgba: Vec<u8> = match n {
        4 => buf,
        3 => buf.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        2 => buf.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        _ => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
    };
    Some((w, h, rgba))
}

/// RGB over white, straight alpha.
fn on_white(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4).flat_map(|p| {
        let a = p[3] as u32;
        let c = |v: u8| ((v as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        [c(p[0]), c(p[1]), c(p[2])]
    }).collect()
}

fn diff(a: &[u8], b: &[u8]) -> (f64, f64) {
    assert_eq!(a.len(), b.len());
    let (mut s, mut sq) = (0f64, 0f64);
    for (x, y) in a.iter().zip(b) { let d = (*x as f64 - *y as f64).abs(); s += d; sq += d * d }
    let n = a.len() as f64;
    let mse = sq / n;
    (s / n, if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() })
}

const PAGE_H: u32 = 6000;
const GAP: u32 = 8;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("prepare") => prepare(&a[1], &a[2], a.get(3).map(|s| s.parse().unwrap()).unwrap_or(1)),
        Some("compare") => compare(&a[1]),
        _ => panic!("usage: resample_compare prepare PAINTED.tsv OUT [EVERY] | compare OUT"),
    }
}

fn prepare(tsv: &str, out: &str, every: usize) {
    for d in ["src", "sips", "chrome"].iter().map(|s| s.to_string()).chain(Filter::ALL.iter().map(|f| format!("ours/{}", f.name()))) {
        std::fs::create_dir_all(format!("{out}/{d}")).unwrap();
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut list = std::fs::File::create(format!("{out}/list.tsv")).unwrap();
    let mut run = std::fs::File::create(format!("{out}/run.sh")).unwrap();
    let (mut pages, mut y, mut page): (Vec<String>, u32, String) = (vec![], 0, String::new());
    let mut n = 0usize;
    for (k, line) in std::fs::read_to_string(tsv).unwrap().lines().enumerate() {
        if k % every != 0 { continue }
        let f: Vec<&str> = line.split('\t').collect();
        let (hex, path, tw, th) = (f[1], f[2], f[3].parse::<u32>().unwrap(), f[4].parse::<u32>().unwrap());
        if !seen.insert(hex.to_string()) || tw > 800 || th > 2000 || tw < 2 || th < 2 { continue }
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Ok(px) = decode_unaddressed(&bytes, Ceilings::default()) else { continue };
        // A reduction on both axes, by at least 1.25 on one of them.
        if tw > px.width || th > px.height || (tw * 5 > px.width * 4 && th * 5 > px.height * 4) { continue }
        write_png(&format!("{out}/src/{n}.png"), px.width, px.height, &px.rgba);
        for flt in Filter::ALL {
            let r = resample(&px.rgba, px.width, px.height, tw, th, flt);
            write_png(&format!("{out}/ours/{}/{n}.png", flt.name()), tw, th, &r);
        }
        writeln!(run, "sips -z {th} {tw} {out}/src/{n}.png --out {out}/sips/{n}.png >/dev/null 2>&1").unwrap();
        if y + th > PAGE_H { pages.push(std::mem::take(&mut page)); y = 0 }
        let p = pages.len();
        page.push_str(&format!("<img src=\"src/{n}.png\" style=\"position:absolute;left:0;top:{y}px;width:{tw}px;height:{th}px\">\n"));
        writeln!(list, "{n}\t{hex}\t{}\t{}\t{tw}\t{th}\t{p}\t{y}", px.width, px.height).unwrap();
        y += th + GAP;
        // ★ A CONTROL every 8th image: the natural frame placed at its own
        // size, so the harness's floor (Chrome's colour handling, the crop)
        // is measured rather than assumed to be zero.
        if n % 8 == 0 && px.width <= 800 && px.height <= 2000 {
            if y + px.height > PAGE_H { pages.push(std::mem::take(&mut page)); y = 0 }
            let p = pages.len();
            page.push_str(&format!("<img src=\"src/{n}.png\" style=\"position:absolute;left:0;top:{y}px;width:{}px;height:{}px\">\n", px.width, px.height));
            writeln!(list, "c{n}\t{hex}\t{}\t{}\t{}\t{}\t{p}\t{y}", px.width, px.height, px.width, px.height).unwrap();
            y += px.height + GAP;
        }
        n += 1;
    }
    pages.push(page);
    for (i, p) in pages.iter().enumerate() {
        std::fs::write(format!("{out}/page{i}.html"),
            format!("<!DOCTYPE html><meta charset=utf-8><style>html,body{{margin:0;background:#fff}}</style>\n{p}")).unwrap();
        writeln!(run, "timeout 120 \"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\" --headless --disable-gpu \
            --force-device-scale-factor=1 --allow-file-access-from-files --user-data-dir=$(mktemp -d) --hide-scrollbars \
            --screenshot={out}/chrome/page{i}.png --window-size=800,{PAGE_H} file://{out}/page{i}.html >/dev/null 2>&1").unwrap();
    }
    println!("{n} images, {} pages; run: bash {out}/run.sh", pages.len());
}

fn compare(out: &str) {
    let mut pages: std::collections::BTreeMap<usize, (u32, u32, Vec<u8>)> = Default::default();
    // filter -> (vs chrome MAD, PSNR), (vs sips MAD, PSNR) lists
    let mut stats: Vec<(Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>)> = vec![Default::default(); Filter::ALL.len()];
    let (mut sips_vs_chrome, mut wins_c, mut wins_s) = (vec![], vec![0usize; Filter::ALL.len()], vec![0usize; Filter::ALL.len()]);
    let mut n = 0;
    let mut control: Vec<f64> = vec![];
    // band (reduction <2, 2-4, >=4) -> filter -> MAD vs Chrome
    let mut bands: Vec<Vec<Vec<f64>>> = vec![vec![vec![]; Filter::ALL.len()]; 3];
    for line in std::fs::read_to_string(format!("{out}/list.tsv")).unwrap().lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let id = f[0];
        let (tw, th, p, y): (u32, u32, usize, u32) = (f[4].parse().unwrap(), f[5].parse().unwrap(), f[6].parse().unwrap(), f[7].parse().unwrap());
        let page = pages.entry(p).or_insert_with(|| read_png(&format!("{out}/chrome/page{p}.png")).expect("chrome page"));
        if y + th > page.1 { continue }
        let chrome: Vec<u8> = (0..th).flat_map(|r| {
            let s = (((y + r) * page.0) * 4) as usize;
            page.2[s..s + (tw * 4) as usize].to_vec()
        }).collect();
        let chrome = on_white(&chrome);
        if let Some(src) = id.strip_prefix('c') {
            let (_, _, s) = read_png(&format!("{out}/src/{src}.png")).unwrap();
            control.push(diff(&on_white(&s), &chrome).0);
            continue
        }
        let (nw, nh): (u32, u32) = (f[2].parse().unwrap(), f[3].parse().unwrap());
        let factor = (nw as f64 / tw as f64).max(nh as f64 / th as f64);
        let band = if factor < 2.0 { 0 } else if factor < 4.0 { 1 } else { 2 };
        let Some((sw, sh, sips)) = read_png(&format!("{out}/sips/{id}.png")) else { continue };
        if (sw, sh) != (tw, th) { continue }
        let sips = on_white(&sips);
        sips_vs_chrome.push(diff(&sips, &chrome).0);
        let mut best = (f64::MAX, 0, f64::MAX, 0);
        for (i, flt) in Filter::ALL.iter().enumerate() {
            let (_, _, ours) = read_png(&format!("{out}/ours/{}/{id}.png", flt.name())).unwrap();
            let ours = on_white(&ours);
            let (mc, pc) = diff(&ours, &chrome);
            let (ms, ps) = diff(&ours, &sips);
            bands[band][i].push(mc);
            stats[i].0.push(mc); stats[i].1.push(pc); stats[i].2.push(ms); stats[i].3.push(ps);
            if mc < best.0 { best.0 = mc; best.1 = i }
            if ms < best.2 { best.2 = ms; best.3 = i }
        }
        wins_c[best.1] += 1; wins_s[best.3] += 1;
        n += 1;
    }
    let med = |v: &Vec<f64>| { let mut v = v.clone(); v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] };
    let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
    let p90 = |v: &Vec<f64>| { let mut v = v.clone(); v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[(v.len() * 9) / 10] };
    println!("{n} images compared at their painted size (composited on white)");
    println!("{:<12} {:>22} {:>14} {:>8} | {:>22} {:>14} {:>8}", "filter", "vs Chrome MAD mean/med/p90", "PSNR mean", "closest", "vs sips MAD mean/med/p90", "PSNR mean", "closest");
    for (i, flt) in Filter::ALL.iter().enumerate() {
        let s = &stats[i];
        println!("{:<12} {:>8.2} {:>6.2} {:>6.2} {:>14.2} {:>8} | {:>8.2} {:>6.2} {:>6.2} {:>14.2} {:>8}", flt.name(),
            mean(&s.0), med(&s.0), p90(&s.0), mean(&s.1), wins_c[i], mean(&s.2), med(&s.2), p90(&s.2), mean(&s.3), wins_s[i]);
    }
    for (b, name) in ["reduction < 2x", "2x to 4x", ">= 4x"].iter().enumerate() {
        if bands[b][0].is_empty() { continue }
        let row: Vec<String> = Filter::ALL.iter().enumerate().map(|(i, f)| format!("{} {:.2}", f.name(), mean(&bands[b][i]))).collect();
        println!("  vs Chrome, {name} (n={}): {}", bands[b][0].len(), row.join(", "));
    }
    if !control.is_empty() { println!("CONTROL natural size through Chrome (n={}): MAD mean {:.3} max {:.3}", control.len(), mean(&control), control.iter().cloned().fold(0.0, f64::max)) }
    println!("sips vs Chrome MAD mean {:.2} median {:.2}", mean(&sips_vs_chrome), med(&sips_vs_chrome));
}
