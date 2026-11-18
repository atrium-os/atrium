//! Cell comments: `xl/commentsN.xml` (who wrote it and the text) and the VML
//! drawing Excel needs to show them (`xl/drawings/vmlDrawingN.vml`, a
//! hidden note box per comment). Excel's own notes begin with the author's
//! name in bold followed by a colon; that prefix is taken off when read.

use std::fmt::Write as _;

use officina_grid::addr::a1;
use officina_grid::Workbook;

use crate::xml::esc;

pub const COMMENTS_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments";
pub const VML_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/vmlDrawing";
pub const COMMENTS_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml";
pub const VML_TYPE: &str = "application/vnd.openxmlformats-officedocument.vmlDrawing";

/// `xl/commentsN.xml` for sheet `s`.
pub fn comments_xml(wb: &Workbook, s: u16) -> String {
    let cm = wb.comments(s);
    let mut authors: Vec<&str> = Vec::new();
    for k in cm.values() {
        if !authors.contains(&&*k.author) {
            authors.push(&k.author);
        }
    }
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><authors>"#);
    for a in &authors {
        let _ = write!(o, "<author>{}</author>", esc(a));
    }
    o.push_str("</authors><commentList>");
    for (&(r, c), k) in cm {
        let id = authors.iter().position(|a| *a == &*k.author).unwrap_or(0);
        let _ = write!(o, r#"<comment ref="{}" authorId="{id}"><text><r><t xml:space="preserve">{}</t></r></text></comment>"#, a1(r, c), esc(&k.text));
    }
    o.push_str("</commentList></comments>");
    o
}

/// `xl/drawings/vmlDrawingN.vml` for sheet `s`: a hidden note box beside each
/// commented cell, as Excel writes them.
pub fn vml(wb: &Workbook, s: u16) -> String {
    let mut o = String::from(r##"<xml xmlns:v="urn:schemas-microsoft-com:vml" xmlns:o="urn:schemas-microsoft-com:office:office" xmlns:x="urn:schemas-microsoft-com:office:excel"><o:shapelayout v:ext="edit"><o:idmap v:ext="edit" data="1"/></o:shapelayout><v:shapetype id="_x0000_t202" coordsize="21600,21600" o:spt="202" path="m,l,21600r21600,l21600,xe"><v:stroke joinstyle="miter"/><v:path gradientshapeok="t" o:connecttype="rect"/></v:shapetype>"##);
    for (i, (&(r, c), _)) in wb.comments(s).iter().enumerate() {
        let _ = write!(
            o,
            r##"<v:shape id="_x0000_s{}" type="#_x0000_t202" style="position:absolute;margin-left:59.25pt;margin-top:1.5pt;width:108pt;height:59.25pt;z-index:{};visibility:hidden" fillcolor="#ffffe1" o:insetmode="auto"><v:fill color2="#ffffe1"/><v:shadow on="t" color="black" obscured="t"/><v:path o:connecttype="none"/><v:textbox style="mso-direction-alt:auto"><div style="text-align:left"></div></v:textbox><x:ClientData ObjectType="Note"><x:MoveWithCells/><x:SizeWithCells/><x:Anchor>{}, 15, {}, 2, {}, 15, {}, 16</x:Anchor><x:AutoFill>False</x:AutoFill><x:Row>{r}</x:Row><x:Column>{c}</x:Column></x:ClientData></v:shape>"##,
            1025 + i,
            i + 1,
            c + 1,
            r.saturating_sub(1),
            c + 3,
            r + 3,
        );
    }
    o.push_str("</xml>");
    o
}

#[cfg(feature = "read")]
pub use read::read_sheet;

#[cfg(feature = "read")]
mod read {
    use quick_xml::events::Event;

    use super::super::read::{attr, parse_ref, rels, xml};
    use crate::report::{Kind, Report};
    use crate::zip::Archive;
    use crate::xlsx::XlsxError;
    use officina_grid::comments::{Comment, MAX_COMMENTS, MAX_TEXT};
    use officina_grid::Workbook;

    /// The comments of the sheet at `part`, onto sheet `s`.
    pub fn read_sheet(wb: &mut Workbook, s: u16, z: &Archive, part: &str, rep: &mut Report) -> Result<(), XlsxError> {
        let Some(path) = rels(z, part)?.into_values().find(|(t, _)| t.ends_with("/comments")).map(|(_, p)| p) else { return Ok(()) };
        let Some(data) = z.read_name(&path)? else { return Ok(()) };
        let mut r = xml(&data);
        let mut buf = Vec::new();
        let (mut authors, mut author) = (Vec::<String>::new(), None::<String>);
        // The comment being read: cell, author, text, the first run's text and
        // whether that run is bold.
        let mut cur: Option<((u32, u32), usize, String)> = None;
        let (mut in_t, mut runs, mut bold_first, mut first_run) = (false, 0usize, false, String::new());
        let mut set = Vec::new();
        loop {
            buf.clear();
            match r.read_event_into(&mut buf)? {
                Event::Start(e) => match e.local_name().as_ref() {
                    b"author" => author = Some(String::new()),
                    b"comment" => {
                        let at = attr(&e, b"ref")?.as_deref().and_then(parse_ref).map(|(r0, c0, _, _)| (r0, c0));
                        let id = attr(&e, b"authorId")?.and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
                        cur = at.map(|a| (a, id, String::new()));
                        (runs, bold_first) = (0, false);
                        first_run.clear();
                    }
                    b"r" => runs += 1,
                    b"t" => in_t = true,
                    _ => {}
                },
                Event::Empty(e) if e.local_name().as_ref() == b"b" && runs == 1 => bold_first = true,
                Event::Text(t) => {
                    let s = t.unescape().map_err(|e| XlsxError::Xml(e.to_string()))?;
                    if let Some(a) = author.as_mut() {
                        a.push_str(&s);
                    } else if let (true, Some(c)) = (in_t, cur.as_mut()) {
                        if c.2.len() < MAX_TEXT {
                            c.2.push_str(&s);
                        }
                        if runs == 1 {
                            first_run.push_str(&s);
                        }
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"author" => authors.extend(author.take()),
                    b"t" => in_t = false,
                    b"comment" => {
                        if let Some(((row, col), id, mut text)) = cur.take() {
                            let who = authors.get(id).cloned().unwrap_or_default();
                            // Excel's "Author:" in bold before the note.
                            if bold_first && runs > 1 && first_run.trim_end().ends_with(':') {
                                text = text[first_run.len()..].trim_start_matches(['\n', '\r']).to_string();
                            }
                            let text = text.replace("\r\n", "\n");
                            if set.len() < MAX_COMMENTS {
                                set.push((row, col, Some(Comment::new(who.trim(), &text))));
                            }
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
        // Excel 365's threaded comments: their legacy note above is only a
        // "[Threaded comment] Your version of Excel..." stand-in.
        let threads = threaded(z, part)?;
        if !threads.is_empty() {
            let replies = threads.iter().any(|t| t.3);
            for ((row, col), who, text, _) in threads {
                let k = Some(Comment::new(who.trim(), &text));
                match set.iter().position(|x| (x.0, x.1) == (row, col)) {
                    Some(i) => set[i].2 = k,
                    None if set.len() < MAX_COMMENTS => set.push((row, col, k)),
                    None => {}
                }
            }
            if replies {
                rep.note(Kind::Approximate, "threaded comments kept as notes, replies as lines under them");
            }
        }
        if set.iter().any(|x| x.2.as_ref().is_some_and(|k| k.text.len() >= MAX_TEXT)) {
            rep.note(Kind::Approximate, "comments cut to 32 KB");
        }
        if !set.is_empty() {
            wb.set_comments(s, set);
        }
        Ok(())
    }

    /// The sheet's threaded comments, one per cell: (cell, first author, the
    /// first message then "Name: reply" paragraphs, whether it had replies).
    fn threaded(z: &Archive, part: &str) -> Result<Vec<((u32, u32), String, String, bool)>, XlsxError> {
        let Some(path) = rels(z, part)?.into_values().find(|(t, _)| t.ends_with("/threadedComment")).map(|(_, p)| p) else { return Ok(Vec::new()) };
        let Some(data) = z.read_name(&path)? else { return Ok(Vec::new()) };
        let people = persons(z)?;
        let name = |id: &str| people.iter().find(|(i, _)| i.eq_ignore_ascii_case(id)).map(|(_, n)| n.clone()).unwrap_or_default();
        let mut r = xml(&data);
        let mut buf = Vec::new();
        let mut out: Vec<((u32, u32), String, String, bool)> = Vec::new();
        // The message being read: cell, person, whether it is a reply, text.
        let mut cur: Option<((u32, u32), String, bool, String)> = None;
        let mut in_text = false;
        loop {
            buf.clear();
            match r.read_event_into(&mut buf)? {
                Event::Start(e) => match e.local_name().as_ref() {
                    b"threadedComment" => {
                        let at = attr(&e, b"ref")?.as_deref().and_then(parse_ref).map(|(r0, c0, _, _)| (r0, c0));
                        let who = attr(&e, b"personId")?.unwrap_or_default();
                        let reply = attr(&e, b"parentId")?.is_some();
                        cur = at.map(|a| (a, who, reply, String::new()));
                    }
                    b"text" => in_text = true,
                    _ => {}
                },
                Event::Text(t) if in_text => {
                    if let Some(c) = cur.as_mut() {
                        if c.3.len() < MAX_TEXT {
                            c.3.push_str(&t.unescape().map_err(|e| XlsxError::Xml(e.to_string()))?);
                        }
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"text" => in_text = false,
                    b"threadedComment" => {
                        let Some((at, who, reply, text)) = cur.take() else { continue };
                        let text = text.replace("\r\n", "\n");
                        let text = text.trim();
                        match out.iter().position(|t| t.0 == at) {
                            Some(i) if out[i].2.len() < MAX_TEXT => {
                                let n = name(&who);
                                let t = &mut out[i];
                                t.2.push_str("\n\n");
                                if !n.trim().is_empty() {
                                    t.2.push_str(n.trim());
                                    t.2.push_str(": ");
                                }
                                t.2.push_str(text);
                                t.3 = true;
                            }
                            Some(_) => {}
                            None if out.len() < MAX_COMMENTS => out.push((at, name(&who), text.to_string(), reply)),
                            None => {}
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }

    /// The workbook's people who wrote threaded comments: (id, display name).
    fn persons(z: &Archive) -> Result<Vec<(String, String)>, XlsxError> {
        let Some(path) = rels(z, "xl/workbook.xml")?.into_values().find(|(t, _)| t.ends_with("/person")).map(|(_, p)| p) else { return Ok(Vec::new()) };
        let Some(data) = z.read_name(&path)? else { return Ok(Vec::new()) };
        let mut r = xml(&data);
        let mut buf = Vec::new();
        let mut out = Vec::new();
        loop {
            buf.clear();
            match r.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"person" => {
                    if let (Some(id), Some(n)) = (attr(&e, b"id")?, attr(&e, b"displayName")?) {
                        if out.len() < MAX_COMMENTS {
                            out.push((id, n));
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }
}
