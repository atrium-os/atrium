//! The rich text alternative used by text fields. Version 1 pairs UTF-8
//! text with the same number of emphasis bytes. Tracked application ranges
//! carry local state and are outside this representation.

use crate::clipboard;
use crate::text_edit::TextEdit;

const INLINE: &str = "application/vnd.atrium.pergola.inline-text";
const PLAIN: &str = "text/plain";
const VERSION: u8 = 1;
const MAX_TEXT: usize = 32 << 10;

pub(crate) fn offer(edit: &TextEdit) -> Option<Vec<u8>> {
    let masks = edit.selected_masks()?;
    if masks.is_empty() || masks.len() > MAX_TEXT { return None; }
    let mut bytes = Vec::with_capacity(1 + 2 * masks.len());
    bytes.push(VERSION);
    bytes.extend_from_slice(edit.selected_text().as_bytes());
    bytes.extend_from_slice(masks);
    Some(bytes)
}

pub(crate) fn copy(text: &str, inline: Option<&[u8]>) -> Result<(), clipboard::Error> {
    match inline {
        Some(bytes) => clipboard::copy_formats(&[(PLAIN, text.as_bytes()), (INLINE, bytes)], false),
        None => clipboard::copy_text(text, false),
    }
}

pub(crate) fn paste(edit: &mut TextEdit, plain: bool) -> bool {
    if plain || !edit.is_rich() {
        return clipboard::paste_text().is_ok_and(|text| text.is_some_and(|text| edit.insert(&text)));
    }
    match clipboard::paste_format(&[INLINE, PLAIN]) {
        Ok(Some((mime, bytes))) if mime == INLINE => {
            if bytes.first() != Some(&VERSION) || (bytes.len() - 1) % 2 != 0 || bytes.len() > 1 + 2 * MAX_TEXT {
                return false;
            }
            let (text, masks) = bytes[1..].split_at((bytes.len() - 1) / 2);
            std::str::from_utf8(text).is_ok_and(|text| edit.paste_inline(text, masks))
        }
        Ok(Some((mime, bytes))) if mime == PLAIN => edit.insert(&String::from_utf8_lossy(&bytes)),
        _ => false,
    }
}
