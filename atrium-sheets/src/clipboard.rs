//! Sheets sends Tabula a native range with HTML and plain text alternatives.
//! The native range contains typed values, styles and comments. The source document can
//! match its own offer to use the original formulas and one-shot cut state.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use officina_convert::html_clipboard::{self, MAX_HTML_BYTES, MAX_NATIVE_BYTES, MIME as HTML};
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
    let html = html_clipboard::write(&doc.wb, clip.from);
    if let Some(html) = &html {
        formats.push((HTML, html.as_bytes()));
    }
    match clipboard::copy_formats(&formats, false) {
        Ok(()) => doc.keep_system_clip(clip, rich),
        Err(e) => doc.status = format!("Couldn't copy: {e:?}"),
    }
}

pub fn paste(doc: &mut crate::doc::Doc, area: Rect, plain: bool) {
    if plain {
        doc.html_paste = None;
    } else if doc.html_paste.is_some() {
        doc.status = "An HTML clipboard paste is already being prepared.".into();
        return;
    }
    let accept: &[&str] = if plain { &[TEXT] } else { &[CELLS, HTML, TEXT] };
    match clipboard::paste_format(accept) {
        Ok(Some((mime, bytes))) if mime == CELLS => doc.paste_system(area, &bytes, MAX_FORMAT_BYTES),
        Ok(Some((mime, bytes))) if mime == HTML => start_html(doc, area, bytes),
        Ok(Some((mime, bytes))) if mime == TEXT => {
            doc.paste_tsv(area.r0, area.c0, &String::from_utf8_lossy(&bytes));
        }
        Ok(_) => doc.status = "The clipboard holds no cells or text.".into(),
        Err(e) => doc.status = format!("Couldn't paste: {e:?}"),
    }
}

pub(crate) struct PendingHtml {
    area: Rect,
    edits: u64,
    rx: Receiver<Result<Vec<u8>, String>>,
}

fn start_html(doc: &mut crate::doc::Doc, area: Rect, bytes: Vec<u8>) {
    if bytes.len() > MAX_HTML_BYTES {
        doc.status = "HTML clipboard data exceeds 1 MiB. Ctrl+Shift+V pastes plain text.".into();
        return;
    }
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::Builder::new().name("paste-html".into()).spawn(move || {
        let _ = tx.send(crate::files::convert(html_clipboard::WORKER_KIND, &bytes).map(|(range, _)| range));
    });
    match worker {
        Ok(_) => {
            doc.html_paste = Some(PendingHtml { area, edits: doc.edits, rx });
            doc.status = "Reading the clipboard table…".into();
        }
        Err(e) => doc.status = format!("Couldn't start HTML paste: {e}"),
    }
}

/// Finish the pending paste at the original selection only if its sheet and
/// edit revision still match. The activation was already spent on Tabula GET.
pub(crate) fn poll_html(doc: &mut crate::doc::Doc) -> bool {
    let Some(pending) = &doc.html_paste else { return false; };
    let got = match pending.rx.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("Clipboard conversion stopped.".into()),
    };
    let (area, edits) = (pending.area, pending.edits);
    doc.html_paste = None;
    if doc.sheet != area.sheet || doc.edits != edits {
        doc.status = "Clipboard paste canceled because the sheet changed.".into();
    } else {
        match got {
            Ok(bytes) => doc.paste_external(area, &bytes, MAX_NATIVE_BYTES),
            Err(e) => doc.status = format!("{e} Ctrl+Shift+V pastes plain text."),
        }
    }
    true
}
