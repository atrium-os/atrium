//! Draw characters' colour glyphs in a row, as raw RGBA for a look.
//!
//!   cargo run --example sheet FONT SIZE TEXT OUT.rgba   (prints width height)
//!   TEXT may be `#12,345`: glyph ids, for a shaped sequence.

use skrifa::{FontRef, MetadataProvider};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).expect("font");
    let size: f32 = a[2].parse().expect("size");
    let font = FontRef::new(&data).expect("a font");
    let cmap = font.charmap();
    // TEXT is characters, or `#gid,gid` for glyph ids (a shaped sequence).
    let gids: Vec<u32> = match a[3].strip_prefix('#') {
        Some(list) => list.split(',').filter_map(|g| g.parse().ok()).collect(),
        None => a[3].chars().filter_map(|c| cmap.map(c)).map(|g| g.to_u32()).collect(),
    };
    let bms: Vec<atrium_colr::Bitmap> = gids.into_iter().filter_map(|g| atrium_colr::render(&data, g, size, [0.1, 0.1, 0.1, 1.0])).collect();
    let (w, h) = (bms.iter().map(|b| b.width + 4).sum::<u32>().max(1), bms.iter().map(|b| b.height).max().unwrap_or(1));
    let mut out = vec![255u8; (w * h * 4) as usize];
    let mut x0 = 0;
    for b in &bms {
        for y in 0..b.height {
            for x in 0..b.width {
                let s = ((y * b.width + x) * 4) as usize;
                let d = ((y * w + x0 + x) * 4) as usize;
                let al = b.rgba[s + 3] as u32;
                for k in 0..3 {
                    out[d + k] = ((b.rgba[s + k] as u32 * al + out[d + k] as u32 * (255 - al)) / 255) as u8;
                }
            }
        }
        x0 += b.width + 4;
    }
    std::fs::write(&a[4], out).expect("write");
    println!("{w} {h} {}", bms.len());
}
