//! `xl/theme/theme1.xml`: the workbook theme's name, twelve colours and
//! heading and body fonts. Cells hold resolved colours, so the theme part
//! only carries the theme itself (for Page Layout > Themes and for Excel).

use std::fmt::Write as _;

use officina_grid::look::{Theme, COLOR_NAMES};

use crate::xml::esc;

pub(crate) const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme";
pub(crate) const TYPE: &str = "application/vnd.openxmlformats-officedocument.theme+xml";

// The format scheme DrawingML requires: three of each, plain.
const FMT: &str = r#"<a:fmtScheme name="Office"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme>"#;

/// The theme part for `t`.
pub(crate) fn write(t: &Theme) -> String {
    let mut o = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="{}"><a:themeElements><a:clrScheme name="{}">"#, esc(&t.name), esc(&t.name));
    for (n, c) in COLOR_NAMES.iter().zip(t.colors) {
        let _ = write!(o, r#"<a:{n}><a:srgbClr val="{c:06X}"/></a:{n}>"#);
    }
    let _ = write!(o, r#"</a:clrScheme><a:fontScheme name="{}"><a:majorFont><a:latin typeface="{}"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="{}"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme>{FMT}</a:themeElements></a:theme>"#,
        esc(&t.name), esc(&t.heading), esc(&t.body));
    o
}

#[cfg(feature = "read")]
/// The theme a part describes: its name, colours (unswapped, `dk1` first) and
/// fonts, each falling back to Office's. Excel's own default reads as ours.
pub(crate) fn read(xml: &[u8]) -> Theme {
    use quick_xml::events::Event;
    let mut colors = super::styles::theme_colors(xml);
    colors.swap(0, 1);
    colors.swap(2, 3);
    let mut t = Theme::default();
    if let Ok(c) = <[u32; 12]>::try_from(colors) {
        t.colors = c;
    }
    let ok = |s: &str| !s.is_empty() && s.len() <= officina_grid::look::MAX_NAME && !s.chars().any(char::is_control);
    let mut r = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut font = 0u8;
    let mut name = None;
    loop {
        buf.clear();
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let a = super::styles::attrs(&e);
                match e.local_name().as_ref() {
                    b"theme" => name = a.get(b"name".as_slice()).filter(|n| ok(n)).cloned(),
                    b"majorFont" => font = 1,
                    b"minorFont" => font = 2,
                    b"latin" => {
                        if let Some(f) = a.get(b"typeface".as_slice()).filter(|n| ok(n)) {
                            match font {
                                1 => t.heading = f.as_str().into(),
                                2 => t.body = f.as_str().into(),
                                _ => {}
                            }
                        }
                        font = 0;
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"fontScheme" => break,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    let office = Theme::default();
    // Excel names its default "Office Theme"; any theme with Office's colours
    // and fonts reads as ours, so a plain file carries no look.
    if (t.colors, &*t.heading, &*t.body) != (office.colors, &*office.heading, &*office.body) {
        if let Some(n) = name {
            t.name = n.into();
        }
    }
    t
}
