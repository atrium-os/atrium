//! big_xlsx [ROWS] [COLS]: the O4 open target, 1,048,576 × 20 numbers in `.xlsx`
//! (docs/spec/officina.md §15: under 5 s to first screen).
//!
//! Writes the file with our writer, then times our reader on it (unzip, parse, store,
//! recalculate) and the native save the converter process sends back, and the size of
//! each.

use std::time::Instant;

use officina_grid::Workbook;

fn main() {
    let mut a = std::env::args().skip(1);
    let rows: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(1 << 20);
    let cols: u32 = a.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let mut wb = Workbook::new();
    wb.add_sheet("Sheet1").unwrap();
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for c in 0..cols {
        let v: Vec<f64> = (0..rows)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x % 10_000_000) as f64 / 100.0
            })
            .collect();
        wb.set_numbers(0, c, 0, &v);
    }
    let t = Instant::now();
    let file = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    println!("write: {rows} × {cols} in {:.2} s, {:.1} MB", t.elapsed().as_secs_f64(), file.len() as f64 / 1e6);
    drop(wb);
    if let Ok(p) = std::env::var("SAVE") {
        std::fs::write(p, &file).unwrap();
    }
    let t = Instant::now();
    let (back, loaded) = officina_convert::xlsx::read(&file).unwrap_or_else(|e| panic!("{e}"));
    let read_s = t.elapsed().as_secs_f64();
    println!("read: {} cells in {read_s:.2} s ({:.1} M cells/s)", loaded.cells, loaded.cells as f64 / read_s / 1e6);
    let t = Instant::now();
    let native = officina_grid::native::save_to_vec(&back);
    println!("native out of the converter: {:.1} MB in {:.2} s", native.len() as f64 / 1e6, t.elapsed().as_secs_f64());
    let t = Instant::now();
    let _ = officina_grid::native::load(&native).unwrap();
    println!("native load in the document process: {:.2} s", t.elapsed().as_secs_f64());
}
