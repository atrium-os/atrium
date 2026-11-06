//! layout_peak <normalized.html> [--first PX]: a worker that starts from an
//! already-normalized document — parse, cascade, layout, scene — and its peak
//! resident set (getrusage). With `--first`, the first-screen layout only.
fn peak_mib() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let b = if cfg!(target_os = "macos") { u.ru_maxrss as f64 } else { u.ru_maxrss as f64 * 1024.0 };
    b / (1024.0 * 1024.0)
}
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let html = std::fs::read_to_string(&a[0]).unwrap();
    let fonts = navigator_render::fontset::FontSet::load().unwrap();
    let env = navigator_style::cascade::Env::default();
    let before = peak_mib();
    let t = std::time::Instant::now();
    let nodes = match a.iter().position(|x| x == "--first") {
        Some(i) => navigator_render::html::render_html_first(&html, &fonts, &env, &Default::default(), a[i + 1].parse::<i64>().unwrap() * 64).0.scene.order.len(),
        None => navigator_render::html::render_html(&html, &fonts, &env).scene.order.len(),
    };
    println!("{}\tms {:.0}\tnodes {nodes}\tpeak_mib {:.0}\t(after fonts {:.0})", a[0].rsplit('/').next().unwrap(), t.elapsed().as_secs_f64() * 1e3, peak_mib(), before);
}
