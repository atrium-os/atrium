//! `.xlsm` macros kept inert (docs/spec/officina.md §5.3): the VBA project
//! (`xl/vbaProject.bin`) and its signatures are carried byte for byte, with the
//! workbook's and sheets' VBA code names they bind to. Nothing here parses VBA.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::Arc;

use officina_grid::macros::{path_ok, MacroFamily, MacroPart, Macros, MAX_PART, MAX_TOTAL};
use officina_grid::Workbook;
use quick_xml::events::Event;

use super::read::{attr, rels, xml, XlsxError};
use crate::report::{Kind, Report};
use crate::zip::Archive;

pub const VBA: &str = "application/vnd.ms-office.vbaProject";
/// Signature media types and the relationship types that point at them.
pub const SIGNATURES: [(&str, &str); 3] = [
    ("application/vnd.ms-office.vbaProjectSignature", "http://schemas.microsoft.com/office/2006/relationships/vbaProjectSignature"),
    ("application/vnd.ms-office.vbaProjectSignatureAgile", "http://schemas.microsoft.com/office/2014/relationships/vbaProjectSignatureAgile"),
    ("application/vnd.ms-office.vbaProjectSignatureV3", "http://schemas.microsoft.com/office/2020/07/relationships/vbaProjectSignatureV3"),
];
pub const VBA_REL: &str = "http://schemas.microsoft.com/office/2006/relationships/vbaProject";
pub const MAIN_MACRO: &str = "application/vnd.ms-excel.sheet.macroEnabled.main+xml";

/// `[Content_Types].xml`: part name (no leading `/`) → media type, and
/// extension → media type.
fn content_types(z: &Archive) -> Result<(HashMap<String, String>, HashMap<String, String>), XlsxError> {
    let (mut over, mut def) = (HashMap::new(), HashMap::new());
    let Some(data) = z.read_name("[Content_Types].xml")? else { return Ok((over, def)) };
    let mut r = xml(&data);
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf)? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"Override" => {
                    if let (Some(p), Some(t)) = (attr(&e, b"PartName")?, attr(&e, b"ContentType")?) {
                        over.insert(p.trim_start_matches('/').to_ascii_lowercase(), t);
                    }
                }
                b"Default" => {
                    if let (Some(x), Some(t)) = (attr(&e, b"Extension")?, attr(&e, b"ContentType")?) {
                        def.insert(x.to_ascii_lowercase(), t);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok((over, def))
}

/// The `codeName` on the first `elem` (`workbookPr`, `sheetPr`) before `stop`.
fn code_name(data: &[u8], elem: &[u8], stop: &[u8]) -> Option<String> {
    let mut r = xml(data);
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf).ok()? {
            Event::Start(e) | Event::Empty(e) => {
                let n = e.local_name();
                if n.as_ref() == elem {
                    return attr(&e, b"codeName").ok().flatten();
                }
                if n.as_ref() == stop {
                    return None;
                }
            }
            Event::Eof => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Keeps the package's VBA project, if it has one, on `wb`.
pub(crate) fn read(z: &Archive, wb: &mut Workbook, wb_xml: &[u8], wb_rels: &HashMap<String, (String, String)>, sheet_parts: &[String], rep: &mut Report) -> Result<(), XlsxError> {
    let Some(vba) = wb_rels.values().find(|(t, _)| t.ends_with("/vbaProject")).map(|(_, p)| p.clone()) else {
        if z.entries.iter().any(|e| e.name.to_ascii_lowercase().ends_with("vbaproject.bin")) {
            rep.note(Kind::Dropped, "macros the workbook doesn't link to");
        }
        return Ok(());
    };
    let (over, def) = content_types(z)?;
    let media = |p: &str| over.get(&p.to_ascii_lowercase()).cloned().or_else(|| p.rsplit_once('.').and_then(|(_, x)| def.get(&x.to_ascii_lowercase()).cloned()));
    let mut parts = Vec::new();
    let mut total = 0usize;
    let mut take = |p: &str, ty: &str, parts: &mut Vec<MacroPart>| -> Result<bool, XlsxError> {
        let Some(e) = z.find(p) else { return Ok(false) };
        if !path_ok(p) || e.size as usize > MAX_PART || total + e.size as usize > MAX_TOTAL {
            return Ok(false);
        }
        let data = z.read(&e.clone())?;
        total += data.len();
        parts.push(MacroPart { path: p.into(), media_type: ty.into(), data: data.into() });
        Ok(true)
    };
    if !take(&vba, VBA, &mut parts)? {
        rep.note(Kind::Dropped, "macros (the VBA project is missing or too large)");
        return Ok(());
    }
    // Its signatures, by the relationships of the project part.
    for (ty, p) in rels(z, &vba)?.into_values() {
        let Some((m, _)) = SIGNATURES.iter().find(|(_, rt)| *rt == ty) else { continue };
        let m = media(&p).filter(|x| x == m).unwrap_or_else(|| m.to_string());
        take(&p, &m, &mut parts)?;
    }
    let workbook_code = code_name(wb_xml, b"workbookPr", b"sheets").filter(|c| !c.is_empty()).map(Arc::from);
    let mut sheet_codes = Vec::new();
    for (i, p) in sheet_parts.iter().enumerate() {
        if let Some(d) = z.read_name(p)? {
            if let Some(c) = code_name(&d, b"sheetPr", b"sheetData") {
                sheet_codes.push((i as u16, Arc::from(c.as_str())));
            }
        }
    }
    let mut m = Macros { family: MacroFamily::Ooxml, parts, workbook_code, sheet_codes };
    if wb.set_macros(m.clone()).is_err() {
        // Code names this engine refuses: keep the project without them.
        m.workbook_code = None;
        m.sheet_codes.clear();
        if wb.set_macros(m).is_err() {
            rep.note(Kind::Dropped, "macros (a malformed VBA project package)");
            return Ok(());
        }
    }
    rep.note(Kind::Inert, "macros (kept as they are for saving as .xlsm; never run)");
    Ok(())
}

/// What the writer adds for macros: the parts it will write (all directly in `xl/`,
/// ending `.bin`, one VBA project), or `None` to write none.
pub(crate) fn writable(wb: &Workbook) -> Option<&Macros> {
    let m = wb.macros().filter(|m| m.family == MacroFamily::Ooxml)?;
    let flat = |p: &MacroPart| p.path.strip_prefix("xl/").is_some_and(|f| !f.contains('/') && f.to_ascii_lowercase().ends_with(".bin") && path_ok(f));
    let known = |p: &MacroPart| &*p.media_type == VBA || SIGNATURES.iter().any(|(t, _)| *t == &*p.media_type);
    if !m.parts.iter().all(|p| flat(p) && known(p)) || m.parts.iter().filter(|p| &*p.media_type == VBA).count() != 1 {
        return None;
    }
    Some(m)
}

/// Content-type overrides for the parts.
pub(crate) fn content_type_xml(m: &Macros) -> String {
    let mut o = String::new();
    for p in &m.parts {
        o.push_str(&format!(r#"<Override PartName="/{}" ContentType="{}"/>"#, p.path, p.media_type));
    }
    o
}

/// Writes the parts and the project's relationships to its signatures.
pub(crate) fn write_parts<W: std::io::Write>(z: &mut crate::zip::ZipWriter<W>, m: &Macros) -> std::io::Result<()> {
    let vba = m.parts.iter().find(|p| &*p.media_type == VBA).expect("writable checked");
    let mut r: Vec<u8> = Vec::new();
    let _ = write!(r, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    let mut k = 0;
    for p in &m.parts {
        z.add(&p.path, &p.data)?;
        if let Some((_, rt)) = SIGNATURES.iter().find(|(t, _)| *t == &*p.media_type) {
            k += 1;
            let _ = write!(r, r#"<Relationship Id="rId{k}" Type="{rt}" Target="{}"/>"#, &p.path[3..]);
        }
    }
    r.extend_from_slice(b"</Relationships>");
    if k > 0 {
        z.add(&format!("xl/_rels/{}.rels", &vba.path[3..]), &r)?;
    }
    Ok(())
}
