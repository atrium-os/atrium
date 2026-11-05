//! decode_digest DIR...: every distinct blob decoded by the worker's decoder
//! at its natural size, one line each: `<blob>  <outcome or blake3 of the
//! pixels>`. Built at two commits and diffed, it says whether a change to
//! the decoder moved any output (the SVG work: rasters must not move).
use navigator_backend::image::{decode_unaddressed, Ceilings};
fn main() {
    let mut names: Vec<(String, std::path::PathBuf)> = vec![];
    let mut seen = std::collections::HashSet::new();
    for d in std::env::args().skip(1) {
        for e in std::fs::read_dir(&d).expect("dir").flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if seen.insert(n.clone()) { names.push((n, e.path())) }
        }
    }
    names.sort();
    for (n, p) in names {
        let Ok(b) = std::fs::read(&p) else { continue };
        let out = match decode_unaddressed(&b, Ceilings::default()) {
            Ok(px) => format!("{}x{} {}", px.width, px.height, blake3::hash(&px.rgba).to_hex()),
            Err(e) => format!("error {}", e.cause()),
        };
        println!("{n}\t{out}");
    }
}
