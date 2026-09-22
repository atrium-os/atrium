//! Run the strict parser over real stylesheets: it must survive them all,
//! and the refusal breakdown says how far the web is from the profile.
//!   realcss <dir>
use navigator_style::sheet::parse_sheet;
use std::collections::BTreeMap;
fn main() {
    let dir = std::env::args().nth(1).expect("dir");
    let (mut sheets, mut bytes, mut admitted, mut refused_sheets) = (0, 0usize, 0usize, 0);
    let mut codes: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut unknown: BTreeMap<String, usize> = BTreeMap::new();
    let mut slowest = (0.0f64, String::new());
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let Ok(b) = std::fs::read(e.path()) else { continue };
        let s = String::from_utf8_lossy(&b);
        if s.trim_start().starts_with('<') || s.is_empty() { continue } // HTML error pages, not CSS
        sheets += 1; bytes += s.len();
        let t = std::time::Instant::now();
        let p = parse_sheet(&s);
        let dt = t.elapsed().as_secs_f64();
        if dt > slowest.0 { slowest = (dt, e.file_name().to_string_lossy().into()) }
        if let Some(r) = &p.refused { refused_sheets += 1; println!("refused whole: {} — {} ({})", e.file_name().to_string_lossy(), r.code, r.msg) }
        admitted += p.sheet.rules.iter().map(|r| r.decls.len()).sum::<usize>();
        for d in &p.diagnostics {
            *codes.entry(d.code).or_default() += 1;
            if d.code == "property.unknown" {
                let name = d.msg.split('`').nth(1).unwrap_or("?").to_string();
                *unknown.entry(name).or_default() += 1;
            }
        }
    }
    let refusals: usize = codes.values().sum();
    println!("sheets {sheets} ({:.1} MB), refused whole {refused_sheets}; slowest {:.0} ms ({})", bytes as f64 / 1e6, slowest.0 * 1e3, slowest.1);
    println!("declarations admitted {admitted}; refusals {refusals}");
    for (c, n) in &codes { println!("  {n:>8}  {c}") }
    let mut u: Vec<_> = unknown.into_iter().collect(); u.sort_by(|a, b| b.1.cmp(&a.1));
    println!("top unknown properties: {}", u.iter().take(12).map(|(k, n)| format!("{k}({n})")).collect::<Vec<_>>().join(" "));
}
