//! Files: what a name says a file is, opening one and writing one.
//!
//! PowerPoint and OpenDocument decks are read by the converter process
//! (`officina-convert pptx|odp`, sandboxed) into `.oslides`; Slides writes
//! them itself.

use crate::doc::Doc;

/// What a file is, by its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Native,
    Pptx,
    Odp,
    Other,
}

pub fn format_of(name: &str) -> Format {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".oslides") {
        Format::Native
    } else if n.ends_with(".pptx") || n.ends_with(".pptm") || n.ends_with(".ppsx") {
        Format::Pptx
    } else if n.ends_with(".odp") {
        Format::Odp
    } else {
        Format::Other
    }
}

/// The name without its extension.
pub fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// A picture's MIME type from its first bytes, for the pictures Slides takes.
pub fn picture_mime(b: &[u8]) -> Option<&'static str> {
    use navigator_image::image::{sniff, Format as F};
    Some(match sniff(b).ok()? {
        F::Png => "image/png",
        F::Jpeg => "image/jpeg",
        F::Gif => "image/gif",
        F::Webp => "image/webp",
        F::Svg => "image/svg+xml",
    })
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

/// The deck as a file of `f` (`Other` writes `.oslides`).
pub fn encode(d: &mut Doc, f: Format) -> std::io::Result<Vec<u8>> {
    match f {
        Format::Pptx => officina_convert::pptx::write(&mut d.deck, Vec::new()),
        Format::Odp => officina_convert::odp::write(&mut d.deck, Vec::new()),
        Format::Native | Format::Other => d.write_native(Vec::new()),
    }
}

/// Opens the file `f`, named `name`: a native deck is mapped (§9.3),
/// anything else is read (up to `max` bytes) and converted.
pub fn load_file(name: &str, f: &mut std::fs::File, max: u64) -> Result<Doc, String> {
    use std::io::{Read, Seek, SeekFrom};
    let native = |f: &std::fs::File| Doc::from_file(name, f);
    match format_of(name) {
        Format::Native => native(f).map_err(|e| format!("“{name}” didn't open: {e}")),
        Format::Other if native(f).is_ok() => native(f),
        _ => {
            let mut data = Vec::new();
            f.seek(SeekFrom::Start(0)).map_err(|e| format!("Couldn't read “{name}”: {e}"))?;
            f.take(max + 1).read_to_end(&mut data).map_err(|e| format!("Couldn't read “{name}”: {e}"))?;
            if data.len() as u64 > max {
                return Err(format!("Couldn't read “{name}”: the file is over {} MB", max >> 20));
            }
            load(name, data)
        }
    }
}

/// Opens `data`, named `name`, as a document.
pub fn load(name: &str, data: Vec<u8>) -> Result<Doc, String> {
    match format_of(name) {
        Format::Native => Doc::from_native(name, data).map_err(|e| format!("“{name}” didn't open: {e}")),
        f @ (Format::Pptx | Format::Odp) => {
            let (native, notes) = convert(if f == Format::Pptx { "pptx" } else { "odp" }, &data).map_err(|e| format!("“{name}” couldn't be opened: {e}"))?;
            let mut d = Doc::from_native(name, native).map_err(|e| format!("“{name}”: the converter's output didn't load: {e}"))?;
            d.status = match notes.len() {
                0 => format!("Opened “{name}”."),
                1 => format!("Opened “{name}”. {}", notes[0]),
                n => format!("Opened “{name}” with {n} notes. {} …", notes[0]),
            };
            for n in &notes {
                eprintln!("atrium-slides: {name}: {n}");
            }
            d.notes = notes;
            Ok(d)
        }
        Format::Other => {
            // A native file under another name still opens.
            Doc::from_native(name, data).map_err(|_| format!("“{name}” isn't a deck Slides can open."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_say_the_format() {
        assert_eq!(format_of("Talk.OSLIDES"), Format::Native);
        assert_eq!(format_of("a.pptx"), Format::Pptx);
        assert_eq!(format_of("a.odp"), Format::Odp);
        assert_eq!(stem_of("Talk.oslides"), "Talk");
        assert_eq!(stem_of(".hidden"), ".hidden");
        assert_eq!(picture_mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert!(load("x.oslides", b"nonsense".to_vec()).is_err());
    }
}
