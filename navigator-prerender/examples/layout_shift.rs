//! layout_shift <parts-dir>…: how far the first display's layout moves when
//! its images arrive (legacy-web §5.4.5.7, the image-free first display).
//!
//! For every `<stem>.first.json` with a `<stem>.first-images.json` beside it
//! (the converter's `PRERENDER_PARTS_DIR`), both are normalized and rendered
//! at the lane's reading size (800 × 600 px), and their TEXT is compared run
//! by run: a run is matched to the next run with the same text, and it moved
//! if its origin moved by more than half a pixel. One TSV line per document:
//!
//! `doc  declared  placeholder  resized  runs  moved  screen_runs  screen_moved  max_dy_px  cls  height_before  height_after  painted_after`
//!
//! - `declared` / `placeholder`: the first artifact's pending images by how
//!   their box was sized; `resized`: images whose sizing differs once they
//!   arrived (or which failed, and are gone);
//! - `screen_*`: runs in the first screen (top 600 px of the first layout),
//!   where a reader shown the first display is;
//! - `cls`: a layout-shift score in the manner of CLS, for that screen: the
//!   share of the viewport the moved runs cover (before ∪ after, on a 4 px
//!   grid) times the largest move as a share of the viewport height.

use navigator_render::{fontset::FontSet, Run};
use navigator_style::cascade::Env;

const PX: i64 = 64;

fn render(json: &str, fonts: &FontSet, env: &Env) -> navigator_render::Scene {
    let r = navigator_normalize::recording::parse(json).expect("a recording");
    let (normalized, _) = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, fonts, env);
    let subs = navigator_render::conformance::subresources_from_recording_json(json);
    navigator_render::html::render_html_with(&normalized, fonts, env, &subs).scene
}

/// A run's box in px: from its origin across its glyphs, an em above the
/// baseline and a quarter below.
fn rect(r: &Run) -> (i64, i64, i64, i64) {
    let x0 = r.x as i64 / PX;
    let w = r.glyphs.iter().map(|g| g.1 as i64).max().unwrap_or(0) / PX + (r.size as i64 / PX) / 2;
    let size = (r.size as i64 / PX).max(1);
    (x0, r.y as i64 / PX - size, w.max(1), size + size / 4)
}

fn entries(json: &str) -> Vec<(String, String)> {
    // (src, sizing) for each image line the converter wrote.
    json.lines().map(str::trim).filter(|l| l.starts_with("{ \"src\": ")).map(|l| {
        let src = l.split("\"src\": ").nth(1).and_then(|r| r.split("\", ").next()).unwrap_or("").to_string();
        let sizing = l.split("\"width\": ").nth(1).and_then(|r| r.split(", \"address\"").next()).unwrap_or("").to_string();
        (src, sizing)
    }).collect()
}

fn main() {
    let fonts = FontSet::load().expect("fonts");
    let env = Env::default();
    let (vw, vh) = (env.width_px as i64, env.height_px as i64);
    println!("doc\tdeclared\tplaceholder\tresized\truns\tmoved\tscreen_runs\tscreen_moved\tmax_dy_px\tcls\theight_before\theight_after\tpainted_after");
    for dir in std::env::args().skip(1) {
        let mut stems: Vec<String> = std::fs::read_dir(&dir).expect("parts dir").flatten()
            .filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".first-images.json").map(str::to_string)).collect();
        stems.sort();
        for stem in stems {
            let p = |ext: &str| std::path::Path::new(&dir).join(format!("{stem}.{ext}.json"));
            let (Ok(a), Ok(b)) = (std::fs::read_to_string(p("first")), std::fs::read_to_string(p("first-images"))) else { continue };
            let declared = a.matches("\"pending\": \"declared\"").count();
            let placeholder = a.matches("\"pending\": \"placeholder\"").count();
            let after: std::collections::HashMap<String, String> = entries(&b).into_iter().collect();
            let resized = entries(&a).iter().filter(|(src, sizing)| {
                let before = sizing.split(", \"pending\"").next().unwrap_or("");
                after.get(src).is_none_or(|s| s != before)
            }).count();
            let (s0, s1) = (render(&a, &fonts, &env), render(&b, &fonts, &env));
            // Match runs in order by text.
            let mut j = 0usize;
            let (mut moved, mut screen, mut screen_moved, mut max_dy) = (0usize, 0usize, 0usize, 0i64);
            const CELL: i64 = 4;
            let (gw, gh) = ((vw / CELL) as usize, (vh / CELL) as usize);
            let mut grid = vec![false; gw * gh];
            let mut mark = |(x, y, w, h): (i64, i64, i64, i64)| {
                for cy in (y.max(0) / CELL)..((y + h).min(vh) + CELL - 1) / CELL {
                    for cx in (x.max(0) / CELL)..((x + w).min(vw) + CELL - 1) / CELL {
                        if (cx as usize) < gw && (cy as usize) < gh { grid[cy as usize * gw + cx as usize] = true }
                    }
                }
            };
            for r in &s0.runs {
                let Some(k) = s1.runs[j..].iter().position(|q| q.text == r.text) else { continue };
                let q = &s1.runs[j + k];
                j += k + 1;
                let (dx, dy) = ((q.x as i64 - r.x as i64).abs(), (q.y as i64 - r.y as i64).abs());
                let on_screen = (r.y as i64) / PX < vh;
                if on_screen { screen += 1 }
                if dx * 2 > PX || dy * 2 > PX {
                    moved += 1;
                    if on_screen {
                        screen_moved += 1;
                        max_dy = max_dy.max(dx.max(dy) / PX);
                        mark(rect(r));
                        mark(rect(q));
                    }
                }
            }
            let impact = grid.iter().filter(|c| **c).count() as f64 / grid.len() as f64;
            let cls = impact * (max_dy.min(vh) as f64 / vh as f64);
            // `painted_after`: image nodes the layout with images paints —
            // the positive control that the images did arrive in it.
            println!("{stem}\t{declared}\t{placeholder}\t{resized}\t{}\t{moved}\t{screen}\t{screen_moved}\t{max_dy}\t{cls:.4}\t{}\t{}\t{}",
                s0.runs.len(), s0.height / 64, s1.height / 64, s1.images.len());
        }
    }
}
