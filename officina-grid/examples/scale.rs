//! The O1 scale gate: a 100-million-cell workbook in memory, a 10-million-row formula
//! fill recalculated in full, and edits with one and with a million dependents.
//!
//!     cargo run --release --example scale [rows-per-column]
//!
//! Prints resident memory (VmRSS) and timings.

use officina_grid::{native, CellAddr, Workbook};
use std::time::Instant;

fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    s.lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|k| k.parse::<f64>().ok())
        .map_or(0.0, |k| k / 1024.0)
}

fn main() {
    let rows: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10_000_000);
    let cols = 10u32;
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    println!("threads {}, base RSS {:.0} MB", wb.threads, rss_mb());

    let t = Instant::now();
    let mut col = vec![0f64; rows as usize];
    for c in 0..cols {
        for (r, x) in col.iter_mut().enumerate() {
            *x = ((r as u64 * 2654435761 + c as u64 * 97) % 100_000) as f64 / 100.0;
        }
        wb.set_numbers(0, c, 0, &col);
    }
    drop(col);
    wb.recalc();
    let cells = rows as u64 * cols as u64;
    println!(
        "{} cells loaded in {:.2} s; RSS {:.0} MB; storage {:.0} MB ({:.2} bytes/cell)",
        cells,
        t.elapsed().as_secs_f64(),
        rss_mb(),
        wb.storage_bytes() as f64 / 1048576.0,
        wb.storage_bytes() as f64 / cells as f64
    );

    // A formula filled down every row: one group, one AST.
    let t = Instant::now();
    wb.fill_formula(0, 10, 0, rows - 1, "=A1*2+B1-C1/4").unwrap();
    wb.set_input(CellAddr::new(0, 0, 11), "=SUM(K:K)").unwrap();
    wb.set_input(CellAddr::new(0, 1, 11), "=AVERAGE(A:A)").unwrap();
    let st = wb.recalc();
    println!(
        "fill K1:K{rows} + SUM(K:K) + AVERAGE(A:A): full recalc {:.0} ms ({} cells, {} vectorised); groups {}; RSS {:.0} MB",
        st.millis,
        st.cells,
        st.fast_cells,
        wb.groups().count(),
        rss_mb()
    );
    let _ = t;

    // An edit with few dependents.
    wb.set_input(CellAddr::new(0, rows / 2, 0), "12345").unwrap();
    let st = wb.recalc();
    println!("edit A{} (dependents: K{} and the SUM): {:.2} ms, {} cells", rows / 2 + 1, rows / 2 + 1, st.millis, st.cells);

    // An edit with a million dependents.
    let m = 1_000_000.min(rows);
    wb.fill_formula(0, 12, 0, m - 1, "=$A$1*D1").unwrap();
    wb.recalc();
    wb.set_input(CellAddr::new(0, 0, 0), "3").unwrap();
    let st = wb.recalc();
    println!("edit A1 with {m} dependents in M (plus K1 and the SUM): {:.0} ms, {} cells, {} vectorised", st.millis, st.cells, st.fast_cells);

    // Lookups over the big column: first is an index build, then cached.
    wb.set_input(CellAddr::new(0, 2, 11), &format!("=MATCH(K{},K:K,0)", rows - 7)).unwrap();
    let st = wb.recalc();
    let first = st.millis;
    wb.set_input(CellAddr::new(0, 3, 11), &format!("=MATCH(K{},K:K,0)", rows - 9)).unwrap();
    let st = wb.recalc();
    println!("MATCH over {rows} rows: {:.0} ms with index build, {:.2} ms cached; RSS {:.0} MB", first, st.millis, rss_mb());

    // Save and load (in memory; disk speed is not what is measured).
    let t = Instant::now();
    let bytes = native::save_to_vec(&wb);
    let save = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let back = native::load(&bytes).unwrap();
    let load = t.elapsed().as_secs_f64();
    println!("native file {:.0} MB: save {:.2} s, load {:.2} s; RSS {:.0} MB (two copies plus the file)", bytes.len() as f64 / 1048576.0, save, load, rss_mb());
    assert_eq!(back.get(CellAddr::new(0, 0, 11)), wb.get(CellAddr::new(0, 0, 11)));
    println!("SUM(K:K) = {}", wb.get(CellAddr::new(0, 0, 11)).display());
}
