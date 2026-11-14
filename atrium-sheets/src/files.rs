//! Files: what a name says a file is, and opening a foreign one through the converter
//! process.

use std::io::{Read, Write};
use std::path::PathBuf;

use crate::doc::Doc;

/// What a file is, by its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Native,
    Csv,
    Xlsx,
    /// `.xlsm`: an `.xlsx` that may carry macros (kept inert, never run).
    Xlsm,
    Ods,
}

pub fn format_of(name: &str) -> Format {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".osheet") {
        Format::Native
    } else if n.ends_with(".xlsx") {
        Format::Xlsx
    } else if n.ends_with(".xlsm") {
        Format::Xlsm
    } else if n.ends_with(".ods") {
        Format::Ods
    } else {
        Format::Csv
    }
}

pub fn is_native(name: &str) -> bool {
    format_of(name) == Format::Native
}

/// Runs the converter process on `data` (docs/spec/officina.md §5.2): the foreign
/// file is parsed there, sandboxed, never here. Returns the native file and the
/// fidelity notes.
pub fn convert(kind: &str, data: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    use std::process::{Command, Stdio};
    let exe = std::env::var_os("OFFICINA_CONVERT").map(PathBuf::from).unwrap_or_else(|| {
        let here = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("officina-convert")));
        here.filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from("officina-convert"))
    });
    let mut child = Command::new(&exe)
        .arg(kind)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("the converter ({}) didn't start: {e}", exe.display()))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    std::thread::scope(|sc| {
        sc.spawn(move || {
            let _ = stdin.write_all(data);
        });
        let e = sc.spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        });
        let mut out = Vec::new();
        let r = child.stdout.take().unwrap().read_to_end(&mut out);
        let status = child.wait();
        let errs = e.join().unwrap_or_default();
        let mut notes = Vec::new();
        let mut failure = None;
        for l in errs.lines() {
            if let Some(n) = l.strip_prefix("note\t") {
                if let Some((kind, count, what, ex)) = officina_convert::report::Report::parse_line(n) {
                    let ex = if ex.is_empty() { String::new() } else { format!(" (e.g. {ex})") };
                    notes.push(if count == 1 { format!("{}: {what}{ex}", kind.word()) } else { format!("{}: {count} × {what}{ex}", kind.word()) });
                }
            } else if let Some(w) = l.strip_prefix("error\t") {
                failure = Some(w.to_string());
            }
        }
        match (r, status) {
            (Ok(_), Ok(st)) if st.success() => Ok((out, notes)),
            _ => Err(failure.unwrap_or_else(|| "the converter stopped".into())),
        }
    })
}

/// A document from a file's bytes, by its name's extension.
pub fn load(name: &str, data: &[u8]) -> Result<Doc, String> {
    if is_native(name) {
        return Doc::from_native(name, data).map_err(|e| format!("“{name}” isn't a sheet this app can read: {e}"));
    }
    let foreign = match format_of(name) {
        Format::Xlsx | Format::Xlsm => Some("xlsx"),
        Format::Ods => Some("ods"),
        _ => None,
    };
    if let Some(kind) = foreign {
        let (native, notes) = convert(kind, data).map_err(|e| format!("“{name}” couldn't be opened: {e}"))?;
        let mut d = Doc::from_native(name, &native).map_err(|e| format!("“{name}”: the converter's output didn't load: {e}"))?;
        d.status = match notes.len() {
            0 => format!("Opened “{name}”."),
            1 => format!("Opened “{name}”. {}", notes[0]),
            n => format!("Opened “{name}” with {n} notes. {} …", notes[0]),
        };
        for n in &notes {
            eprintln!("atrium-sheets: {name}: {n}");
        }
        d.notes = notes;
        return Ok(d);
    }
    let (mut d, st) = Doc::from_csv(name, data).map_err(|e| format!("“{name}” couldn't be read as CSV: {e:?}"))?;
    let mut notes = vec![format!("Opened “{name}”: {} rows × {} columns", st.rows, st.cols)];
    if st.latin {
        notes.push("read as Windows-1252 (not UTF-8)".into());
    }
    if st.cut > 0 {
        notes.push(format!("{} over-long cells cut", st.cut));
    }
    d.status = notes.join("; ") + ".";
    Ok(d)
}


/// The name without its extension.
pub fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}
