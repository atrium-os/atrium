//! What a file is, by its name, and reading and writing each kind.

use crate::doc::Doc;
use officina_convert::crypt::CryptError;
use officina_convert::launch::ConvertError;
use officina_convert::sealed::{self, Protection};

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
    officina_convert::launch::convert(kind, data, None).map_err(|e| e.to_string())
}

/// A document from a file's bytes.
pub fn load(name: &str, data: &[u8]) -> Result<Doc, String> {
    load_with(name, data, None).map_err(|e| match e {
        ConvertError::Failed(w) => w,
        e => format!("“{name}” couldn't be opened: {e}."),
    })
}

/// [`load`] with the password of a protected file. `NeedsPassword` and
/// `WrongPassword` say to ask (again); anything else is in words.
pub fn load_with(name: &str, data: &[u8], password: Option<&str>) -> Result<Doc, ConvertError> {
    let failed = ConvertError::Failed;
    match format_of(name) {
        Format::Native if sealed::is_sealed(data) => {
            let Some(pw) = password else { return Err(ConvertError::NeedsPassword) };
            let (plain, sealer) = sealed::unseal(data, pw).map_err(|e| match e {
                CryptError::WrongPassword => ConvertError::WrongPassword,
                e => failed(format!("“{name}” couldn't be opened: {e}")),
            })?;
            let mut d = Doc::from_native(name, &plain).map_err(|e| failed(format!("“{name}” isn't a document this app can read: {e}")))?;
            d.protection = Some(Protection { password: pw.to_string(), sealer });
            d.status = format!("Opened “{name}”, protected with a password.");
            Ok(d)
        }
        Format::Native => Doc::from_native(name, data).map_err(|e| failed(format!("“{name}” isn't a document this app can read: {e}"))),
        f @ (Format::Docx | Format::Odt) => {
            let (native, notes) = officina_convert::launch::convert(if f == Format::Docx { "docx" } else { "odt" }, data, password).map_err(|e| match e {
                ConvertError::Failed(w) => failed(format!("“{name}” couldn't be opened: {w}")),
                e => e,
            })?;
            let mut d = Doc::from_native(name, &native).map_err(|e| failed(format!("“{name}”: the converter's output didn't load: {e}")))?;
            if let Some(pw) = password {
                d.protection = Some(Protection::new(pw));
            }
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
            let mut d = Doc::from_text(name, data).map_err(|e| failed(format!("“{name}” couldn't be read: {e}")))?;
            d.status = format!("Opened “{name}” as plain text.");
            Ok(d)
        }
    }
}

/// The file's bytes in format `f`; a protected document's encrypted with
/// its password (plain text has no way to carry one and is written as is).
pub fn encode(d: &Doc, f: Format) -> Vec<u8> {
    let pw = d.protection.as_ref().map(|p| p.password.as_str());
    let protect = |kind: &str, b: Vec<u8>| match pw {
        Some(pw) if !b.is_empty() => officina_convert::launch::protect(kind, &b, pw).unwrap_or_default(),
        _ => b,
    };
    match f {
        Format::Native => d.stored_native(),
        Format::Docx => protect("docx", officina_convert::docx::write(&d.ed.borrow().doc, Vec::new()).unwrap_or_default()),
        Format::Odt => protect("odt", officina_convert::odt::write(&d.ed.borrow().doc, Vec::new()).unwrap_or_default()),
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
