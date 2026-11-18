//! `settings.xml`: the view settings LibreOffice keeps per sheet. Frozen panes
//! are the only ones read and written (a split position with mode 2).
//! LibreOffice ignores the file's view settings unless the root declares the
//! `xlink` and `ooo` namespaces as its own do.

use std::fmt::Write as _;

use officina_grid::addr::{MAX_COLS, MAX_ROWS};
use officina_grid::Workbook;

/// `settings.xml` for `wb`'s frozen panes; None when no sheet has any.
pub fn settings_xml(wb: &Workbook) -> Option<String> {
    let frozen: Vec<(u16, (u32, u32))> = (0..wb.sheet_count() as u16).map(|s| (s, wb.sheet(s).freeze)).filter(|x| x.1 != (0, 0)).collect();
    if frozen.is_empty() {
        return None;
    }
    let item = |o: &mut String, name: &str, ty: &str, v: u32| {
        let _ = write!(o, r#"<config:config-item config:name="{name}" config:type="{ty}">{v}</config:config-item>"#);
    };
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-settings xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:ooo="http://openoffice.org/2004/office" office:version="1.3"><office:settings><config:config-item-set config:name="ooo:view-settings"><config:config-item-map-indexed config:name="Views"><config:config-item-map-entry><config:config-item config:name="ViewId" config:type="string">view1</config:config-item><config:config-item-map-named config:name="Tables">"#);
    for (s, (rows, cols)) in frozen {
        let _ = write!(o, r#"<config:config-item-map-entry config:name="{}">"#, crate::xml::esc(&wb.sheet(s).name));
        item(&mut o, "HorizontalSplitMode", "short", if cols > 0 { 2 } else { 0 });
        item(&mut o, "VerticalSplitMode", "short", if rows > 0 { 2 } else { 0 });
        item(&mut o, "HorizontalSplitPosition", "int", cols);
        item(&mut o, "VerticalSplitPosition", "int", rows);
        // The pane the cursor is in: 1 top right, 2 bottom left, 3 bottom right.
        item(&mut o, "ActiveSplitRange", "short", if rows > 0 { 2 + (cols > 0) as u32 } else { 1 });
        item(&mut o, "PositionLeft", "int", 0);
        item(&mut o, "PositionRight", "int", cols);
        item(&mut o, "PositionTop", "int", 0);
        item(&mut o, "PositionBottom", "int", rows);
        o.push_str("</config:config-item-map-entry>");
    }
    o.push_str("</config:config-item-map-named></config:config-item-map-entry></config:config-item-map-indexed></config:config-item-set></office:settings></office:document-settings>");
    Some(o)
}

/// The frozen panes in `data` (a `settings.xml`), onto `wb`'s sheets by name.
#[cfg(feature = "read")]
pub fn read(wb: &mut Workbook, data: &[u8]) {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(data);
    let mut buf = Vec::new();
    // Inside <config-item-map-named name="Tables">: its depth; the sheet whose
    // entry is open; the item open, and its text.
    let mut tables: Option<usize> = None;
    let mut depth = 0usize;
    let mut sheet: Option<(String, [Option<u32>; 4])> = None;
    let mut item: Option<(usize, String)> = None;
    let name = |e: &quick_xml::events::BytesStart| e.attributes().with_checks(false).flatten().find(|a| a.key.local_name().as_ref() == b"name").and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()));
    let keys = ["HorizontalSplitMode", "VerticalSplitMode", "HorizontalSplitPosition", "VerticalSplitPosition"];
    loop {
        buf.clear();
        let Ok(ev) = r.read_event_into(&mut buf) else { return };
        match ev {
            Event::Start(e) => {
                depth += 1;
                match e.local_name().as_ref() {
                    b"config-item-map-named" if tables.is_none() && name(&e).as_deref() == Some("Tables") => tables = Some(depth),
                    b"config-item-map-entry" if tables == Some(depth - 1) => sheet = name(&e).map(|n| (n, [None; 4])),
                    b"config-item" if sheet.is_some() => item = name(&e).and_then(|n| keys.iter().position(|k| *k == n)).map(|i| (i, String::new())),
                    _ => {}
                }
            }
            Event::Text(t) => {
                if let Some((_, v)) = item.as_mut() {
                    if v.len() < 16 {
                        v.push_str(&String::from_utf8_lossy(&t));
                    }
                }
            }
            Event::End(e) => {
                match e.local_name().as_ref() {
                    b"config-item" => {
                        if let (Some((i, v)), Some((_, vals))) = (item.take(), sheet.as_mut()) {
                            vals[i] = v.trim().parse().ok();
                        }
                    }
                    b"config-item-map-entry" if tables == Some(depth - 1) => {
                        if let Some((n, [hm, vm, hp, vp])) = sheet.take() {
                            let cols = if hm == Some(2) { hp.unwrap_or(0).min(MAX_COLS - 1) } else { 0 };
                            let rows = if vm == Some(2) { vp.unwrap_or(0).min(MAX_ROWS - 1) } else { 0 };
                            if let (Some(s), true) = (wb.sheet_id(&n), (rows, cols) != (0, 0)) {
                                wb.sheet_mut(s).freeze = (rows, cols);
                            }
                        }
                    }
                    b"config-item-map-named" if tables == Some(depth) => tables = None,
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => return,
            _ => {}
        }
    }
}
