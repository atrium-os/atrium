//! What a file is, by its name, and reading and writing each kind.

use crate::doc::Doc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// `.owrite`, the native file.
    Native,
    /// Word's `.docx`: read by the converter process, written here.
    Docx,
    /// OpenDocument `.odt`: likewise.
    Odt,
    /// Anything else: plain text, a paragraph per line.
    Text,
}

pub fn format_of(name: &str) -> Format {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".owrite") {
        Format::Native
    } else if n.ends_with(".docx") {
        Format::Docx
    } else if n.ends_with(".odt") {
        Format::Odt
    } else {
        Format::Text
    }
}

/// Runs the converter process (`officina-convert KIND`, sandboxed, next to
/// this program or at `OFFICINA_CONVERT`) over `data`: the native file and the
/// fidelity notes.
pub fn convert(kind: &str, data: &[u8]) -> Result<(Vec<u8>, Vec<String>), String> {
    use std::io::{Read, Write};
    use std::path::PathBuf;
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
    let mut stdin = child.stdin.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
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
        let r = stdout.read_to_end(&mut out);
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

/// A document from a file's bytes.
pub fn load(name: &str, data: &[u8]) -> Result<Doc, String> {
    match format_of(name) {
        Format::Native => Doc::from_native(name, data).map_err(|e| format!("“{name}” isn't a document this app can read: {e}")),
        f @ (Format::Docx | Format::Odt) => {
            let (native, notes) = convert(if f == Format::Docx { "docx" } else { "odt" }, data).map_err(|e| format!("“{name}” couldn't be opened: {e}"))?;
            let mut d = Doc::from_native(name, &native).map_err(|e| format!("“{name}”: the converter's output didn't load: {e}"))?;
            d.status = match notes.len() {
                0 => format!("Opened “{name}”."),
                1 => format!("Opened “{name}”. {}", notes[0]),
                n => format!("Opened “{name}” with {n} notes. {} …", notes[0]),
            };
            for n in &notes {
                eprintln!("atrium-write: {name}: {n}");
            }
            Ok(d)
        }
        Format::Text => {
            let mut d = Doc::from_text(name, data).map_err(|e| format!("“{name}” couldn't be read: {e}"))?;
            d.status = format!("Opened “{name}” as plain text.");
            Ok(d)
        }
    }
}

pub fn encode(d: &Doc, f: Format) -> Vec<u8> {
    match f {
        Format::Native => d.native_bytes(),
        Format::Docx => officina_convert::docx::write(&d.ed.borrow().doc, Vec::new()).unwrap_or_default(),
        Format::Odt => officina_convert::odt::write(&d.ed.borrow().doc, Vec::new()).unwrap_or_default(),
        Format::Text => d.plain_text().into_bytes(),
    }
}

/// The name without its extension.
pub fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}
