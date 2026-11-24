//! Measure the Sheets loader, full grid recalculation, and XLSX export on a
//! private Atrium guest with officina-convert staged beside this binary.
//! Run as `excel_scale INPUT.xlsx OUTPUT_DIRECTORY` with an empty directory.
//! Each of three trials reads the same source and writes a new file. Compare
//! those files' raw OOXML caches against an independently saved Excel source.
//! Use `excel_scale check INITIAL.osheet REOPENED.osheet` to check the cached
//! results captured from the installed app before Save and after picker reopen.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use atrium_sheets::{doc::Doc, files};
use officina_grid::{native, Value, Workbook};

fn open(path: &Path) -> io::Result<Doc> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 workbook name");
    files::load(name, &fs::read(path)?).map_err(io::Error::other)
}

fn same_value(left: Value, right: Value) -> bool {
    match (left, right) {
        (Value::Num(a), Value::Num(b)) => (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0),
        (a, b) => a == b,
    }
}

fn formula_count(book: &Workbook) -> u64 {
    book.groups()
        .map(|(_, group)| u64::from(group.r1 - group.r0 + 1))
        .sum()
}

fn verify(left: &Workbook, right: &Workbook) -> io::Result<u64> {
    let count = formula_count(left);
    if count == 0 || count != formula_count(right) {
        return Err(io::Error::other(format!(
            "formula count changed: {count} -> {}",
            formula_count(right)
        )));
    }
    let names: Vec<_> = (0..left.sheet_count())
        .map(|sheet| left.sheet_name(sheet as u16))
        .collect();
    for (_, group) in left.groups() {
        let name = &names[usize::from(group.sheet)];
        let sheet = right
            .sheet_id(name)
            .ok_or_else(|| io::Error::other(format!("missing sheet {name:?}")))?;
        for row in group.r0..=group.r1 {
            if right.group_at(sheet, row, group.col).is_none()
                || !same_value(
                    left.value(group.sheet, row, group.col),
                    right.value(sheet, row, group.col),
                )
            {
                return Err(io::Error::other(format!(
                    "changed formula result at {name:?}, row {}, column {}",
                    row + 1,
                    group.col + 1
                )));
            }
        }
    }
    Ok(count)
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if let [command, initial, reopened] = args.as_slice() {
        if command == "check" {
            let read = |path| {
                native::load(&fs::read(path)?).map_err(|err| io::Error::other(format!("{err:?}")))
            };
            let initial = read(initial)?;
            let reopened = read(reopened)?;
            println!(
                "PASS {} cached formula results after picker reopen",
                verify(&initial, &reopened)?
            );
            return Ok(());
        }
    }
    let [input, out] = args.as_slice() else {
        return Err(io::Error::other(
            "usage: excel_scale INPUT.xlsx OUTPUT_DIRECTORY",
        ));
    };
    let input = Path::new(input);
    let out = Path::new(out);
    for trial in 1..=3 {
        let start = Instant::now();
        let mut doc = open(input)?;
        let open = start.elapsed();

        let start = Instant::now();
        let calc = doc.wb.recalc_all();
        let recalc = start.elapsed();
        assert!(calc.cells > 0 && !doc.wb.recalc_pending());

        let path = out.join(format!("trial-{trial}.xlsx"));
        let start = Instant::now();
        let encoded = officina_convert::xlsx::write(&doc.wb, Vec::new())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        output.write_all(&encoded)?;
        output.sync_all()?;
        let save = start.elapsed();
        drop(output);

        let start = Instant::now();
        let reopened = self::open(&path)?;
        let reopen = start.elapsed();

        println!(
            "trial={trial} open_ms={:.1} recalc_ms={:.1} save_ms={:.1} reopen_ms={:.1} evaluated={} output_bytes={}",
            open.as_secs_f64() * 1000.0, recalc.as_secs_f64() * 1000.0, save.as_secs_f64() * 1000.0,
            reopen.as_secs_f64() * 1000.0, calc.cells, encoded.len(),
        );
        println!(
            "PASS {} formula results after reopen",
            verify(&doc.wb, &reopened.wb)?
        );
    }
    Ok(())
}
