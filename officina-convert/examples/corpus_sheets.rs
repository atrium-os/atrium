//! The Sheets real-workbook sweep (docs/spec/officina.md §16.167): every
//! `.xlsx`, `.xlsm`, `.xls` and `.ods` under the given directories is read by
//! our reader, written as `.xlsx` and `.ods`, and read back.
//!
//! cargo run --example corpus_sheets -- DIR [DIR ...] > report.tsv
//!
//! Per file it records: read or refused (with the reason), a panic (a failure
//! of the gate), the time, the sheet count, and for each written format
//! whether the file reads back with the same sheets and the same shown text in
//! every non-empty cell (up to `MAX_CELLS` per file). Volatile formulas
//! (RAND, NOW, TODAY…) and cells whose shown text follows the current date
//! are left out of the comparison. The corpus itself is test data and stays
//! out of the repository.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use officina_convert::{ods, xls, xlsx};
use officina_grid::addr::Rect;
use officina_grid::{CellAddr, Workbook};

/// Cells compared per file, at most.
const MAX_CELLS: usize = 200_000;
/// How long one file may take, read and both round trips.
const LIMIT: Duration = Duration::from_secs(120);

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files(&p, out);
        } else if matches!(ext(&p).as_str(), "xlsx" | "xlsm" | "xls" | "ods") {
            out.push(p);
        }
    }
}

fn ext(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn read(kind: &str, data: &[u8]) -> Result<Workbook, String> {
    match kind {
        "xlsx" | "xlsm" => xlsx::read(data).map(|r| r.0).map_err(|e| format!("{e:?}")),
        "xls" => xls::read(data, None).map(|r| r.0).map_err(|e| format!("{e:?}")),
        "ods" => ods::read(data).map(|r| r.0).map_err(|e| format!("{e:?}")),
        _ => Err("unknown kind".into()),
    }
}

fn volatile(wb: &Workbook, a: CellAddr) -> bool {
    wb.formula_text(a).is_some_and(|f| {
        let f = f.to_ascii_uppercase();
        ["RAND", "NOW(", "TODAY(", "INFO(", "CELL(", "OFFSET(", "INDIRECT(", "RANDBETWEEN", "RANDARRAY"].iter().any(|v| f.contains(v))
    })
}

/// Each sheet's name and its non-empty cells' shown text, in order.
fn cells(wb: &Workbook) -> Vec<(String, Vec<(u32, u32, String)>)> {
    let mut left = MAX_CELLS;
    let mut out = Vec::new();
    for s in 0..wb.sheet_count() as u16 {
        let (er, ec) = wb.extent(s);
        let mut v = Vec::new();
        if er > 0 && ec > 0 && left > 0 {
            let mut at = Vec::new();
            wb.each(&Rect { sheet: s, r0: 0, c0: 0, r1: er - 1, c1: ec - 1 }, &mut |r, c, val| {
                if !val.is_empty() && at.len() < left {
                    at.push((r, c));
                }
            });
            left -= at.len();
            for (r, c) in at {
                let a = CellAddr::new(s, r, c);
                if volatile(wb, a) {
                    continue;
                }
                v.push((r, c, wb.formatted(a).text.to_string()));
            }
        }
        out.push((wb.sheet_name(s).to_string(), v));
    }
    out
}

/// "same", or what differs first, with a count.
fn compare(a: &[(String, Vec<(u32, u32, String)>)], b: &[(String, Vec<(u32, u32, String)>)]) -> String {
    if a.len() != b.len() {
        return format!("sheets {} vs {}", a.len(), b.len());
    }
    let mut diffs = 0usize;
    let mut first = String::new();
    for ((na, ca), (nb, cb)) in a.iter().zip(b) {
        if na != nb {
            diffs += 1;
            if first.is_empty() {
                first = format!("sheet name {na:?} vs {nb:?}");
            }
        }
        let mb: std::collections::HashMap<(u32, u32), &str> = cb.iter().map(|(r, c, t)| ((*r, *c), t.as_str())).collect();
        for (r, c, t) in ca {
            let got = mb.get(&(*r, *c)).copied().unwrap_or("");
            if got != t {
                diffs += 1;
                if first.is_empty() {
                    first = format!("{na}!{}: {t:?} vs {got:?}", officina_grid::addr::a1(*r, *c));
                }
            }
        }
        let ma: std::collections::HashSet<(u32, u32)> = ca.iter().map(|(r, c, _)| (*r, *c)).collect();
        for (r, c, t) in cb {
            if !ma.contains(&(*r, *c)) && !t.is_empty() {
                diffs += 1;
                if first.is_empty() {
                    first = format!("{na}!{}: new {t:?}", officina_grid::addr::a1(*r, *c));
                }
            }
        }
    }
    if diffs == 0 { "same".into() } else { format!("{diffs} differ; first {}", first.replace(['\t', '\n'], " ")) }
}

fn one(path: PathBuf) -> String {
    let kind = ext(&path);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => return format!("unreadable\t{e}"),
    };
    let t = Instant::now();
    let wb = match read(&kind, &data) {
        Ok(wb) => wb,
        Err(e) => return format!("refused\t{}\t{}", t.elapsed().as_millis(), e.replace(['\t', '\n'], " ")),
    };
    let read_ms = t.elapsed().as_millis();
    let want = cells(&wb);
    let mut row = format!("read\t{read_ms}\t{}", wb.sheet_count());
    for (to, write) in [("xlsx", xlsx::write::<Vec<u8>> as fn(&Workbook, Vec<u8>) -> std::io::Result<Vec<u8>>), ("ods", ods::write::<Vec<u8>>)] {
        let verdict = match write(&wb, Vec::new()) {
            Err(e) => format!("not written: {}", e.to_string().replace(['\t', '\n'], " ")),
            Ok(bytes) => match read(to, &bytes) {
                Err(e) => format!("not read back: {}", e.replace(['\t', '\n'], " ")),
                Ok(back) => compare(&want, &cells(&back)),
            },
        };
        row.push('\t');
        row.push_str(&verdict);
    }
    row
}

fn main() {
    let mut list = Vec::new();
    for d in std::env::args().skip(1) {
        files(Path::new(&d), &mut list);
    }
    eprintln!("{} files", list.len());
    println!("file\tresult\tms\tsheets\txlsx\tods");
    for p in list {
        let (tx, rx) = mpsc::channel();
        let name = p.display().to_string();
        std::thread::Builder::new().stack_size(64 << 20).spawn(move || {
            let r = std::panic::catch_unwind(|| one(p)).unwrap_or_else(|e| {
                let msg = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                format!("PANIC\t\t\t{}", msg.replace(['\t', '\n'], " "))
            });
            let _ = tx.send(r);
        }).expect("thread");
        let r = rx.recv_timeout(LIMIT).unwrap_or_else(|_| "TIMEOUT".into());
        println!("{name}\t{r}");
    }
}
