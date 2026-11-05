//! svg_corpus DIR...: every distinct SVG blob in content-addressed blob
//! directories, drawn by the worker's SVG path (src/svg.rs) at its own size
//! (or 300 × 150), and counted by outcome: drawn, or refused / failed by
//! reason. The measurement behind threat model §2.4's SVG rows.
use navigator_backend::image::{decode_unaddressed, sniff, Ceilings, DecodeError, Format};
fn main() {
    let mut seen = std::collections::HashSet::new();
    let mut by: std::collections::BTreeMap<String, usize> = Default::default();
    for d in std::env::args().skip(1) {
        for e in std::fs::read_dir(&d).expect("dir").flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !seen.insert(name) { continue }
            let Ok(b) = std::fs::read(e.path()) else { continue };
            if sniff(&b) != Ok(Format::Svg) { continue }
            let k = match decode_unaddressed(&b, Ceilings::default()) {
                Ok(_) => "drawn".to_string(),
                Err(DecodeError::Refused { why, .. }) => format!("refused: {why}"),
                Err(e) => format!("{}: {}", e.cause(), e.to_string().chars().take(70).collect::<String>()),
            };
            *by.entry(k).or_default() += 1;
        }
    }
    let n: usize = by.values().sum();
    println!("{n} distinct SVG blobs");
    for (k, v) in &by { println!("  {v:5}  {:5.1}%  {k}", 100.0 * *v as f64 / n as f64) }
}
