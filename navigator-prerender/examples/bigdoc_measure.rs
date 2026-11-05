//! bigdoc_measure <recording.json> [--stage ingest|normalize|layout|scene]:
//! the worker's side of a big document (backend §4.8, measured). Runs the
//! display path up to `--stage` (default `scene`) in THIS process, as the
//! per-document worker does, and prints one TSV line:
//!
//! `stage  doc_bytes  elements  violations  ingest_ms  normalize_ms  layout_ms  scene_ms
//!  runs  rects  images  nodes  extent_px  nsg_bytes  full_raster_mib  peak_rss_mib`
//!
//! - `violations`: the profile ceilings the document breaks (profile.rs), `-` if none;
//! - `nodes`: every node in the scene's draw order; `extent_px`: its height at 800 px;
//! - `nsg_bytes`: the scene as NSG text, what the worker would hand Fresco whole;
//! - `full_raster_mib`: 800 × extent × 4 bytes, the page as one RGBA surface;
//! - `peak_rss_mib`: this process's peak resident set (getrusage), so run one
//!   process per stage to read each stage's peak.

fn peak_rss_mib() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    // macOS reports bytes, FreeBSD and Linux kilobytes.
    let b = if cfg!(target_os = "macos") { u.ru_maxrss as f64 } else { u.ru_maxrss as f64 * 1024.0 };
    b / (1024.0 * 1024.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().expect("recording.json");
    let stage = args.iter().position(|a| a == "--stage").and_then(|i| args.get(i + 1)).map(String::as_str).unwrap_or("scene").to_string();
    let rank = |s: &str| ["ingest", "normalize", "layout", "scene"].iter().position(|x| *x == s).expect("stage");
    let want = rank(&stage);
    let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3;
    let json = std::fs::read_to_string(path).expect("read");
    let fonts = navigator_render::fontset::FontSet::load().expect("fonts");
    let env = navigator_style::cascade::Env::default();
    let t = std::time::Instant::now();
    let r = navigator_normalize::recording::parse(&json).expect("recording");
    let dom = navigator_dom::parse(&r.document);
    let violations = navigator_dom::profile::check(&dom, r.document.len());
    let elements = dom.element_count();
    drop(dom);
    let ingest = ms(t);
    let (mut normalize, mut layout, mut scene_ms) = (0.0, 0.0, 0.0);
    let (mut runs, mut rects, mut images, mut nodes, mut extent, mut nsg_bytes) = (0, 0, 0, 0, 0u64, 0usize);
    if want >= 1 {
        let t = std::time::Instant::now();
        let (normalized, _) = navigator_normalize::normalize_and_measure(&r.document, &r.inputs, &fonts, &env);
        normalize = ms(t);
        if want >= 2 {
            let t = std::time::Instant::now();
            let subs = navigator_render::conformance::subresources_from_recording_json(&json);
            let o = navigator_render::html::render_html_with(&normalized, &fonts, &env, &subs);
            layout = ms(t);
            runs = o.scene.runs.len(); rects = o.scene.rects.len(); images = o.scene.images.len();
            nodes = o.scene.order.len(); extent = (o.scene.height / 64) as u64;
            if want >= 3 {
                let t = std::time::Instant::now();
                nsg_bytes = navigator_render::nsg::write(&o.scene, &fonts).len();
                scene_ms = ms(t);
            }
        }
    }
    let v = if violations.is_empty() { "-".to_string() } else { violations.iter().map(|v| v.ceiling).collect::<Vec<_>>().join(",") };
    println!("{stage}\t{}\t{elements}\t{v}\t{ingest:.0}\t{normalize:.0}\t{layout:.0}\t{scene_ms:.0}\t{runs}\t{rects}\t{images}\t{nodes}\t{extent}\t{nsg_bytes}\t{:.1}\t{:.1}",
             r.document.len(), 800.0 * extent as f64 * 4.0 / 1048576.0, peak_rss_mib());
}
