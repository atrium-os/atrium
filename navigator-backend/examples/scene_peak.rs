//! scene_peak <recording.json> [--cached normalized.html | --emit normalized.html] [--nsg out] [--limit PX]:
//! the worker's display path (`display::first_screen`) on one recording, in
//! its own process, so the arm's peak resident set is its own: normalizing
//! (the first worker to show a recording) or from a stored normalized
//! document (every later one).
fn peak_mib() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let b = if cfg!(target_os = "macos") { u.ru_maxrss as f64 } else { u.ru_maxrss as f64 * 1024.0 };
    b / (1024.0 * 1024.0)
}
fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let arg = |k: &str| a.iter().position(|x| x == k).map(|i| a[i + 1].clone());
    let rec = std::fs::read(&a[0]).unwrap();
    let cached = arg("--cached").map(|p| std::fs::read(p).unwrap());
    let limit: u32 = arg("--limit").map(|l| l.parse().unwrap()).unwrap_or(1200);
    let before = peak_mib();
    let t = std::time::Instant::now();
    let ans = navigator_backend::display::first_screen(&rec, None, cached.as_deref(), limit).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1e3;
    if let Some(p) = arg("--nsg") { std::fs::write(p, &ans.nsg).unwrap() }
    if let (Some(p), Some(n)) = (arg("--emit"), &ans.normalized) { std::fs::write(p, n).unwrap() }
    println!("{}\t{}\tms {ms:.0}\tnsg_bytes {}\tpeak_mib {:.0}\t(at start {before:.0})",
        a[0].rsplit('/').next().unwrap(), if cached.is_some() { "cached" } else { "normalize" }, ans.nsg.len(), peak_mib());
}
