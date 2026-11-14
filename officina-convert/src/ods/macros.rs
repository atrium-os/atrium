//! `.ods` macros kept inert (docs/spec/officina.md §5.3): the `Basic/`, `Dialogs/`
//! and `Scripts/` folders are carried byte for byte with their manifest media types,
//! never parsed or run. Bindings of macros to events and buttons (in `content.xml`)
//! are not kept, so a saved file has its macros but nothing starts them by itself.

use std::collections::HashMap;
use std::fmt::Write as _;

use officina_grid::macros::{path_ok, MacroFamily, MacroPart, Macros, MAX_PART, MAX_PARTS, MAX_TOTAL};
use officina_grid::Workbook;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::report::{Kind, Report};
use crate::zip::Archive;

const FOLDERS: [&str; 3] = ["Basic/", "Dialogs/", "Scripts/"];

pub(crate) fn is_macro_path(p: &str) -> bool {
    FOLDERS.iter().any(|f| p.starts_with(f)) && path_ok(p)
}

/// `META-INF/manifest.xml`: path → media type.
fn manifest(z: &Archive) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(Some(data)) = z.read_name("META-INF/manifest.xml") else { return out };
    let mut r = Reader::from_reader(&data[..]);
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.local_name().as_ref() == b"file-entry" => {
                let (mut p, mut t) = (None, None);
                for a in e.attributes().with_checks(false).flatten() {
                    let v = a.unescape_value().map(|v| v.into_owned()).ok();
                    match a.key.local_name().as_ref() {
                        b"full-path" => p = v,
                        b"media-type" => t = v,
                        _ => {}
                    }
                }
                if let (Some(p), Some(t)) = (p, t) {
                    if out.len() < 100_000 {
                        out.insert(p, t);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Keeps the package's macro folders, if it has any, on `wb`.
pub(crate) fn read(z: &Archive, wb: &mut Workbook, rep: &mut Report) -> Result<(), crate::zip::ZipError> {
    let entries: Vec<_> = z.entries.iter().filter(|e| FOLDERS.iter().any(|f| e.name.starts_with(f)) && !e.name.ends_with('/')).cloned().collect();
    if entries.is_empty() {
        return Ok(());
    }
    let man = manifest(z);
    let mut parts = Vec::new();
    let mut total = 0usize;
    let mut lost = false;
    for e in entries {
        if !path_ok(&e.name) || e.size as usize > MAX_PART || total + e.size as usize > MAX_TOTAL || parts.len() >= MAX_PARTS {
            lost = true;
            continue;
        }
        let data = z.read(&e)?;
        total += data.len();
        let media = man.get(&e.name).cloned().unwrap_or_default();
        let media = if media.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'<' | b'>' | b'&' | b'\'')) && media.len() <= 255 { media } else { String::new() };
        parts.push(MacroPart { path: e.name.as_str().into(), media_type: media.into(), data: data.into() });
    }
    if parts.is_empty() {
        rep.note(Kind::Dropped, "macros (malformed or too large)");
        return Ok(());
    }
    let m = Macros { family: MacroFamily::Odf, parts, workbook_code: None, sheet_codes: Vec::new() };
    if wb.set_macros(m).is_err() {
        rep.note(Kind::Dropped, "macros (malformed or too large)");
        return Ok(());
    }
    if lost {
        rep.note(Kind::Dropped, "macro files with odd names or too large");
    }
    rep.note(Kind::Inert, "macros (kept as they are for saving as .ods; never run)");
    Ok(())
}

/// The parts the writer will write: an ODF family's, in the macro folders.
pub(crate) fn writable(wb: &Workbook) -> Vec<&MacroPart> {
    match wb.macros() {
        Some(m) if m.family == MacroFamily::Odf => m.parts.iter().filter(|p| is_macro_path(&p.path)).collect(),
        _ => Vec::new(),
    }
}

/// Manifest entries for the parts (media types are checked plain by the grid).
pub(crate) fn manifest_xml(parts: &[&MacroPart]) -> String {
    let mut o = String::new();
    for p in parts {
        let _ = write!(o, r#"<manifest:file-entry manifest:full-path="{}" manifest:media-type="{}"/>"#, p.path, p.media_type);
    }
    o
}
