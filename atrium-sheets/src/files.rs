//! Files: what a name says a file is, and opening a foreign one through the converter
//! process.

use crate::doc::Doc;
use officina_convert::crypt::CryptError;
use officina_convert::launch::ConvertError;
use officina_convert::sealed::{self, Protection};

/// What a file is, by its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Native,
    Csv,
    Xlsx,
    /// `.xlsm`: an `.xlsx` that may carry macros (kept inert, never run).
    Xlsm,
    Ods,
    /// `.xls`, Excel 97–2003: read, never written.
    Xls,
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
    } else if n.ends_with(".xls") {
        Format::Xls
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
    officina_convert::launch::convert(kind, data, None).map_err(|e| e.to_string())
}

/// A document from a file's bytes, by its name's extension.
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
    if is_native(name) && sealed::is_sealed(data) {
        let Some(pw) = password else { return Err(ConvertError::NeedsPassword) };
        let (plain, sealer) = sealed::unseal(data, pw).map_err(|e| match e {
            CryptError::WrongPassword => ConvertError::WrongPassword,
            e => failed(format!("“{name}” couldn't be opened: {e}")),
        })?;
        let mut d = Doc::from_native(name, &plain).map_err(|e| failed(format!("“{name}” isn't a sheet this app can read: {e}")))?;
        d.protection = Some(Protection { password: pw.to_string(), sealer });
        d.status = format!("Opened “{name}”, protected with a password.");
        return Ok(d);
    }
    if is_native(name) {
        return Doc::from_native(name, data).map_err(|e| failed(format!("“{name}” isn't a sheet this app can read: {e}")));
    }
    let foreign = match format_of(name) {
        Format::Xlsx | Format::Xlsm => Some("xlsx"),
        Format::Ods => Some("ods"),
        Format::Xls => Some("xls"),
        _ => None,
    };
    if let Some(kind) = foreign {
        let (native, notes) = officina_convert::launch::convert(kind, data, password).map_err(|e| match e {
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
            eprintln!("atrium-sheets: {name}: {n}");
        }
        d.notes = notes;
        return Ok(d);
    }
    let (mut d, st) = Doc::from_csv(name, data).map_err(|e| failed(format!("“{name}” couldn't be read as CSV: {e:?}")))?;
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
