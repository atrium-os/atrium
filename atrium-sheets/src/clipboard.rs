//! Sheets sends Tabula a formatted cell range with plain text as an alternative.
//! The native range contains typed values and styles. The source document can
//! match its own offer to use the original formulas and one-shot cut state.

use officina_grid::addr::Rect;
use officina_grid::clip::Clip;
use pergola::clipboard::{self, MAX_FORMAT_BYTES};

const CELLS: &str = "application/vnd.atrium.sheets.cells";
const TEXT: &str = "text/plain";

pub fn copy(doc: &mut crate::doc::Doc, rect: Rect, cut: bool) {
    let clip = Clip::take(&doc.wb, rect, cut);
    let Some(text) = doc.copy_tsv_bounded(clip.from, MAX_FORMAT_BYTES) else {
        doc.status = "Selection exceeds the system clipboard size limit.".into();
        return;
    };
    let mut id = [0u8; 16];
    // SAFETY: the writable buffer is exactly 16 bytes, within getentropy's limit.
    if unsafe { libc::getentropy(id.as_mut_ptr().cast(), id.len()) } != 0 {
        doc.status = "Couldn't create a clipboard copy ID.".into();
        return;
    }
    let rich = clip.encode_system(&id, MAX_FORMAT_BYTES);
    if cut && rich.is_none() {
        doc.status = "That is too large to cut. Select a smaller range.".into();
        return;
    }

    let mut formats = vec![(TEXT, text.as_bytes())];
    if let Some(ref bytes) = rich {
        formats.push((CELLS, bytes));
    }
    match clipboard::copy_formats(&formats, false) {
        Ok(()) => doc.keep_system_clip(clip, rich),
        Err(e) => doc.status = format!("Couldn't copy: {e:?}"),
    }
}

pub fn paste(doc: &mut crate::doc::Doc, area: Rect) {
    match clipboard::paste_format(&[CELLS, TEXT]) {
        Ok(Some((mime, bytes))) if mime == CELLS => doc.paste_system(area, &bytes, MAX_FORMAT_BYTES),
        Ok(Some((mime, bytes))) if mime == TEXT => {
            doc.paste_tsv(area.r0, area.c0, &String::from_utf8_lossy(&bytes));
        }
        Ok(_) => doc.status = "The clipboard holds no cells or text.".into(),
        Err(e) => doc.status = format!("Couldn't paste: {e:?}"),
    }
}
