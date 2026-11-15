//! Paginates every corpus document with floating pictures (pinned fonts):
//! no panics, the time, and how many pictures landed on a page.
//!
//! cargo run --release --example float_pages DIR...

use officina_text::layout::{LayoutCache, PinnedFonts};
use officina_text::paginate::Paginator;

fn main() {
    let fonts = PinnedFonts::new().expect("fonts");
    let (mut docs, mut floats, mut placed, mut pages) = (0, 0, 0, 0);
    let mut slowest = (std::time::Duration::ZERO, String::new());
    for dir in std::env::args().skip(1) {
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            let Ok(bytes) = std::fs::read(&f) else { continue };
            let d = match ext.as_str() {
                "docx" => officina_convert::docx::read(&bytes).map(|x| x.0).ok(),
                "odt" => officina_convert::odt::read(&bytes).map(|x| x.0).ok(),
                _ => None,
            };
            let Some(d) = d else { continue };
            let n = d.blocks.iter().filter_map(|b| b.as_para()).flat_map(|p| p.runs.iter().map(|r| r.1)).filter(|&a| d.attrs.get(a).object.is_some_and(|o| o.float.is_some())).count();
            if n == 0 {
                continue;
            }
            docs += 1;
            floats += n;
            let t = std::time::Instant::now();
            let mut p = Paginator::new(&d);
            let mut c = LayoutCache::default();
            p.run(&d, &mut c, &fonts, usize::MAX);
            let e = t.elapsed();
            if e > slowest.0 {
                slowest = (e, f.display().to_string());
            }
            pages += p.pages().len();
            placed += p.pages().iter().map(|pg| pg.floats.len()).sum::<usize>();
        }
    }
    println!("{docs} documents with {floats} floating picture runs: {pages} pages, {placed} pictures placed; slowest {} in {:.2?}", slowest.1, slowest.0);
}
