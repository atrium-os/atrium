#![no_main]
//! Any page the writers accept reads back identical, in both formats.
use libfuzzer_sys::fuzz_target;
use prelum_raster::{read_pwg, read_urf, ColorSpace, Duplex, PageHeader, PwgWriter, UrfWriter};

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let color = if data[0] & 1 == 0 { ColorSpace::SGray8 } else { ColorSpace::SRgb8 };
    let width = (data[1] as u32 % 64) + 1;
    let pixels = &data[3..];
    let bpl = width as usize * color.bytes_per_pixel();
    // Repeat the input rows to get long line repeats too.
    let rows: Vec<&[u8]> = pixels.chunks_exact(bpl).collect();
    if rows.is_empty() {
        return;
    }
    let reps = (data[2] as usize % 4) * 100 + 1;
    let height = (rows.len() * reps) as u32;
    if height > 100_000 {
        return; // past the writers' page limit, refused by design
    }
    let h = PageHeader { width, height, dpi: (300, 300), color, duplex: Duplex::Simplex, quality: 0, media_name: String::new(), media_type: String::new(), total_pages: 1, transform: (1, 1) };
    let mut expect = Vec::new();
    for r in &rows {
        for _ in 0..reps {
            expect.extend_from_slice(r);
        }
    }
    let mut p = PwgWriter::new(Vec::new()).unwrap();
    let mut u = UrfWriter::new(Vec::new(), 1).unwrap();
    p.begin_page(&h).unwrap();
    u.begin_page(&h).unwrap();
    for row in expect.chunks_exact(bpl) {
        p.write_row(row).unwrap();
        u.write_row(row).unwrap();
    }
    p.end_page().unwrap();
    u.end_page().unwrap();
    assert_eq!(read_pwg(&p.finish().unwrap(), 1 << 26).unwrap()[0].pixels, expect);
    assert_eq!(read_urf(&u.finish().unwrap(), 1 << 26).unwrap()[0].pixels, expect);
});
