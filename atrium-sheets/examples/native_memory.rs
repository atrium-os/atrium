//! Build and verify native workbooks near the Sheets memory boundary.
//! Build on the host, then run individual commands on an idle Atrium guest.
//! The dense case uses the 1 GiB edit ceiling. Sparse cases use the guest's
//! actual open budget. Run each case in its own process to measure peak RSS.

use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::Path;
use std::time::Instant;

use atrium_sheets::doc::{default_memory_budget, MAKE_LIMIT};
use officina_grid::addr::MAX_ROWS;
use officina_grid::column::CHUNK_ROWS;
use officina_grid::{native, CellAddr, Value, Workbook};

const DENSE_ROWS: u32 = 1 << 20;
const DENSE_COLS: u32 = 125;
const SPARSE_ROW: u32 = MAX_ROWS - 1;
const INITIAL: f64 = 3.0;

fn points_per_column() -> u64 {
    u64::from(MAX_ROWS.div_ceil(CHUNK_ROWS)) * std::mem::size_of::<usize>() as u64
}

fn make(case: &str, path: &Path, a1: f64) -> io::Result<()> {
    let mut workbook = Workbook::new();
    workbook.add_sheet("Memory boundary").unwrap();
    let budget = match case {
        "dense" => {
            let values: Vec<_> = (0..DENSE_ROWS).map(f64::from).collect();
            for col in 0..DENSE_COLS {
                workbook.set_numbers(0, col, 0, &values);
            }
            workbook.set_value(CellAddr::new(0, 0, 0), Value::Num(a1));
            MAKE_LIMIT
        }
        "sparse" | "oversized" => {
            let budget = default_memory_budget();
            let below = ((budget - points_per_column()) / (points_per_column() + 1024)) as u32;
            let columns = if case == "sparse" { below } else { below + 8 };
            workbook.set_value(CellAddr::new(0, 0, 0), Value::Num(a1));
            for col in 0..columns {
                workbook.set_value(
                    CellAddr::new(0, SPARSE_ROW, col),
                    Value::Num(f64::from(col)),
                );
            }
            budget
        }
        _ => return Err(io::Error::other("unknown fixture")),
    };
    let memory = workbook.memory_bytes();
    if case == "oversized" {
        assert!(memory > budget);
    } else {
        assert!(
            memory <= budget && budget - memory < (16 << 20),
            "estimate {memory}, budget {budget}"
        );
    }
    native::save(&workbook, BufWriter::new(File::create_new(path)?))?
        .into_inner()
        .map_err(|err| err.into_error())?
        .sync_all()?;
    println!(
        "wrote {case}: file={} estimate={memory} budget={budget}",
        fs::metadata(path)?.len()
    );
    Ok(())
}

fn check(case: &str, path: &Path, expected: f64) -> io::Result<()> {
    let start = Instant::now();
    let data = fs::read(path)?;
    let budget = if case == "dense" {
        MAKE_LIMIT
    } else {
        default_memory_budget()
    };
    let read = start.elapsed();
    let start = Instant::now();
    let result = native::load_within(&data, budget);
    let load = start.elapsed();
    match case {
        "oversized" => {
            let Err(native::LoadError::TooBig(needed)) = result else {
                return Err(io::Error::other("expected the workbook budget refusal"));
            };
            assert!(needed > budget && needed - budget <= (8 << 20));
            println!(
                "PASS oversized file={} refused_at={needed} budget={budget} load_ms={}",
                data.len(),
                load.as_millis()
            );
        }
        "dense" | "sparse" => {
            let workbook = result.map_err(io::Error::other)?;
            let memory = workbook.memory_bytes();
            assert!(memory <= budget && budget - memory < (16 << 20));
            if case == "dense" {
                assert_eq!(workbook.extent(0), (DENSE_ROWS, DENSE_COLS));
                for col in 0..DENSE_COLS {
                    for row in [0, 65_535, DENSE_ROWS - 1] {
                        let value = if row == 0 && col == 0 {
                            expected
                        } else {
                            f64::from(row)
                        };
                        assert_eq!(workbook.value(0, row, col), Value::Num(value));
                    }
                }
            } else {
                let (rows, cols) = workbook.extent(0);
                assert_eq!(rows, MAX_ROWS);
                assert_eq!(workbook.value(0, 0, 0), Value::Num(expected));
                assert_eq!(workbook.value(0, SPARSE_ROW - 1, 0), Value::Empty);
                for col in 0..cols {
                    assert_eq!(
                        workbook.value(0, SPARSE_ROW, col),
                        Value::Num(f64::from(col))
                    );
                }
            }
            println!(
                "PASS {case} file={} estimate={memory} budget={budget} read_ms={} load_ms={}",
                data.len(),
                read.as_millis(),
                load.as_millis(),
            );
        }
        _ => return Err(io::Error::other("unknown fixture")),
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [command, case, path] | [command, case, path, _] => {
            let initial = if case == "dense" { 0.0 } else { INITIAL };
            let a1 = args
                .get(3)
                .map(|value| value.parse())
                .transpose()
                .map_err(io::Error::other)?
                .unwrap_or(initial);
            match command.as_str() {
                "make" => make(case, Path::new(path), a1),
                "check" => check(case, Path::new(path), a1),
                _ => Err(usage()),
            }
        }
        _ => Err(usage()),
    }
}

fn usage() -> io::Error {
    io::Error::other("usage: native_memory make|check dense|sparse|oversized FILE [A1]")
}
