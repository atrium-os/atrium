//! What a file is, by its name, and reading and writing each kind.

use crate::doc::Doc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// `.owrite`, the native file.
    Native,
    /// Anything else: plain text, a paragraph per line.
    Text,
}

pub fn format_of(name: &str) -> Format {
    if name.to_ascii_lowercase().ends_with(".owrite") {
        Format::Native
    } else {
        Format::Text
    }
}

/// A document from a file's bytes.
pub fn load(name: &str, data: &[u8]) -> Result<Doc, String> {
    match format_of(name) {
        Format::Native => Doc::from_native(name, data).map_err(|e| format!("“{name}” isn't a document this app can read: {e}")),
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
