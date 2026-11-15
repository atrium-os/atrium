//! Files: what a name says a file is, opening one and writing one.
//!
//! PowerPoint and OpenDocument decks are read by the converter process
//! (`officina-convert pptx|odp`, sandboxed) into `.oslides`; Slides writes
//! them itself.

use crate::doc::Doc;
use officina_convert::crypt::CryptError;
use officina_convert::launch::ConvertError;
use officina_convert::sealed::{self, Protection};

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
    officina_convert::launch::convert(kind, data, None).map_err(|e| e.to_string())
}

/// The deck as a file of `f` (`Other` writes `.oslides`).
pub fn encode(d: &mut Doc, f: Format) -> std::io::Result<Vec<u8>> {
    let b = match f {
        Format::Pptx => officina_convert::pptx::write(&mut d.deck, Vec::new())?,
        Format::Odp => officina_convert::odp::write(&mut d.deck, Vec::new())?,
        Format::Native | Format::Other => {
            let b = d.write_native(Vec::new())?;
            return Ok(match &d.protection {
                Some(p) => p.sealer.seal(&b),
                None => b,
            });
        }
    };
    match &d.protection {
        Some(p) => officina_convert::launch::protect(if f == Format::Pptx { "pptx" } else { "odp" }, &b, &p.password).map_err(|e| std::io::Error::other(e.to_string())),
        None => Ok(b),
    }
}

/// Opens the file `f`, named `name`: a native deck is mapped (§9.3),
/// anything else is read (up to `max` bytes) and converted.
pub fn load_file(name: &str, f: &mut std::fs::File, max: u64) -> Result<Doc, String> {
    load_file_with(name, f, max, None).map_err(|e| in_words(name, e))
}

fn in_words(name: &str, e: ConvertError) -> String {
    match e {
        ConvertError::Failed(w) => w,
        e => format!("“{name}” couldn't be opened: {e}."),
    }
}

/// [`load_file`] with the password of a protected file. `NeedsPassword` and
/// `WrongPassword` say to ask (again); anything else is in words.
pub fn load_file_with(name: &str, f: &mut std::fs::File, max: u64, password: Option<&str>) -> Result<Doc, ConvertError> {
    use std::io::{Read, Seek, SeekFrom};
    // A sealed deck is read and opened in memory, never mapped.
    let mut head = [0u8; 128];
    let sealed_file = f.seek(SeekFrom::Start(0)).is_ok() && f.read_exact(&mut head).is_ok() && sealed::is_sealed(&head);
    if sealed_file {
        let mut data = Vec::new();
        f.seek(SeekFrom::Start(0)).and_then(|_| f.take(max + 1).read_to_end(&mut data)).map_err(|e| ConvertError::Failed(format!("Couldn't read “{name}”: {e}")))?;
        if data.len() as u64 > max {
            return Err(ConvertError::Failed(format!("Couldn't read “{name}”: the file is over {} MB", max >> 20)));
        }
        return load_with(name, data, password);
    }
    let native = |f: &std::fs::File| Doc::from_file(name, f).map_err(ConvertError::Failed);
    match format_of(name) {
        Format::Native => native(f).map_err(|e| ConvertError::Failed(format!("“{name}” didn't open: {e}"))),
        Format::Other if native(f).is_ok() => native(f),
        _ => {
            let mut data = Vec::new();
            let failed = |e: std::io::Error| ConvertError::Failed(format!("Couldn't read “{name}”: {e}"));
            f.seek(SeekFrom::Start(0)).map_err(failed)?;
            f.take(max + 1).read_to_end(&mut data).map_err(failed)?;
            if data.len() as u64 > max {
                return Err(ConvertError::Failed(format!("Couldn't read “{name}”: the file is over {} MB", max >> 20)));
            }
            load_with(name, data, password)
        }
    }
}

/// Opens `data`, named `name`, as a document.
pub fn load(name: &str, data: Vec<u8>) -> Result<Doc, String> {
    load_with(name, data, None).map_err(|e| in_words(name, e))
}

/// [`load`] with the password of a protected file.
pub fn load_with(name: &str, data: Vec<u8>, password: Option<&str>) -> Result<Doc, ConvertError> {
    let failed = ConvertError::Failed;
    if sealed::is_sealed(&data) {
        let Some(pw) = password else { return Err(ConvertError::NeedsPassword) };
        let (plain, sealer) = sealed::unseal(&data, pw).map_err(|e| match e {
            CryptError::WrongPassword => ConvertError::WrongPassword,
            e => failed(format!("“{name}” couldn't be opened: {e}")),
        })?;
        let mut d = Doc::from_native(name, plain).map_err(|e| failed(format!("“{name}” didn't open: {e}")))?;
        d.protection = Some(Protection { password: pw.to_string(), sealer });
        d.status = format!("Opened “{name}”, protected with a password.");
        return Ok(d);
    }
    match format_of(name) {
        Format::Native => Doc::from_native(name, data).map_err(|e| failed(format!("“{name}” didn't open: {e}"))),
        f @ (Format::Pptx | Format::Odp) => {
            let (native, notes) = officina_convert::launch::convert(if f == Format::Pptx { "pptx" } else { "odp" }, &data, password).map_err(|e| match e {
                ConvertError::Failed(w) => failed(format!("“{name}” couldn't be opened: {w}")),
                e => e,
            })?;
            let mut d = Doc::from_native(name, native).map_err(|e| failed(format!("“{name}”: the converter's output didn't load: {e}")))?;
            if let Some(pw) = password {
                d.protection = Some(Protection::new(pw));
            }
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
            Doc::from_native(name, data).map_err(|_| failed(format!("“{name}” isn't a deck Slides can open.")))
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
