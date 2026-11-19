//! Cell comments: `xl/commentsN.xml` (who wrote it and the text) and the VML
//! drawing Excel needs to show them (`xl/drawings/vmlDrawingN.vml`, a
//! hidden note box per comment). Excel's own notes begin with the author's
//! name in bold followed by a colon; that prefix is taken off when read.

use std::collections::{BTreeMap, HashMap};
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
    comments_xml_with_threads(wb, s, &BTreeMap::new())
}

pub(super) fn comments_xml_with_threads(wb: &Workbook, s: u16, threads: &BTreeMap<(u32, u32), Vec<u128>>) -> String {
    let cm = wb.comments(s);
    let mut authors = Vec::new();
    let mut author_ids = HashMap::new();
    let mut notes = Vec::new();
    for (&cell, k) in cm {
        let root = threads.get(&cell).map(|ids| super::threaded::guid(ids[0]));
        let who = root.as_ref().map_or_else(|| k.author.to_string(), |id| format!("tc={id}"));
        let id = *author_ids.entry(who.clone()).or_insert_with(|| {
            authors.push(who);
            authors.len() - 1
        });
        notes.push((cell, k, id, root));
    }
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main""#);
    if !threads.is_empty() {
        o.push_str(r#" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:xr="http://schemas.microsoft.com/office/spreadsheetml/2014/revision" mc:Ignorable="xr""#);
    }
    o.push_str("><authors>");
    for a in &authors {
        let _ = write!(o, "<author>{}</author>", esc(a));
    }
    o.push_str("</authors><commentList>");
    for ((r, c), k, id, root) in notes {
        let _ = write!(o, r#"<comment ref="{}" authorId="{id}""#, a1(r, c));
        if let Some(root) = root {
            let _ = write!(o, r#" xr:uid="{root}"><text><t xml:space="preserve">[Threaded comment]&#10;&#10;{}</t></text></comment>"#, esc(&k.text));
            continue;
        }
        o.push_str("><text>");
        for (text, style) in k.segments() {
            o.push_str("<r>");
            if style.mask() != 0 {
                o.push_str("<rPr>");
                for (on, tag) in [(style.bold, "<b/>"), (style.italic, "<i/>"), (style.strike, "<strike/>"), (style.underline, "<u/>")] {
                    if on {
                        o.push_str(tag);
                    }
                }
                o.push_str("</rPr>");
            }
            let _ = write!(o, r#"<t xml:space="preserve">{}</t></r>"#, esc(text));
        }
        if k.text.is_empty() {
            o.push_str("<t/>");
        }
        o.push_str("</text></comment>");
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
pub(super) use read::read_sheet_with_people;
#[cfg(feature = "read")]
pub use read::read_sheet;

#[cfg(feature = "read")]
mod read {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;

    use officina_grid::comments::{Comment, CommentRun, CommentStyle, ThreadPerson, MAX_COMMENTS, MAX_TEXT};
    use officina_grid::Workbook;
    use quick_xml::events::Event;

    use super::super::read::{attr, parse_ref, rels, xml};
    use crate::report::{Kind, Report};
    use crate::xlsx::{threaded, XlsxError};
    use crate::zip::Archive;

    /// Read the sheet's comments from a package with the standard workbook path.
    pub fn read_sheet(wb: &mut Workbook, s: u16, z: &Archive, part: &str, rep: &mut Report) -> Result<(), XlsxError> {
        let relationships = rels(z, "xl/workbook.xml")?;
        let path = relationships.values().find(|(kind, _)| kind == threaded::PERSON_REL)
            .map(|(_, path)| path.as_str());
        let mut people = threaded::persons(z, path, rep)?;
        read_sheet_with_people(wb, s, z, part, &mut people, rep)
    }

    /// Merge real conversations with ordinary notes and compatibility placeholders.
    pub(in crate::xlsx) fn read_sheet_with_people(wb: &mut Workbook, s: u16, z: &Archive, part: &str, people: &mut HashMap<u128, Arc<ThreadPerson>>, rep: &mut Report) -> Result<(), XlsxError> {
        let relationships = rels(z, part)?;
        let mut notes = BTreeMap::new();
        if let Some((_, path)) = relationships.values().find(|(t, _)| t.ends_with("/comments")) {
            if let Some(data) = z.read_name(path)? {
                notes = legacy(&data)?;
            }
        }
        if let Some((_, path)) = relationships.values().find(|(t, _)| t == threaded::THREAD_REL) {
            if let Some(data) = z.read_name(path)? {
                let placeholders = notes.iter().filter_map(|(&cell, note)| {
                    let id = threaded::parse_guid(note.author.strip_prefix("tc=")?)?;
                    Some((id, cell))
                }).collect();
                notes.extend(threaded::sheet(&data, people, &placeholders, rep)?);
            }
        }
        if notes.values().any(|k| k.thread().is_empty() && k.text.len() >= MAX_TEXT) {
            rep.note(Kind::Approximate, "comments cut to 32 KB");
        }
        wb.set_comments(s, notes.into_iter().map(|((row, col), note)| (row, col, Some(note))).collect());
        Ok(())
    }

    fn legacy(data: &[u8]) -> Result<BTreeMap<(u32, u32), Comment>, XlsxError> {
        let mut r = xml(data);
        let mut buf = Vec::new();
        let (mut authors, mut author) = (Vec::<String>::new(), None::<String>);
        // The comment being read: cell, author, text, the first run's text and
        // whether that run is bold.
        let mut cur: Option<((u32, u32), usize, String)> = None;
        let mut in_t = false;
        let mut in_run = false;
        let mut run_count = 0;
        let mut first_run_end = 0;
        let mut style = CommentStyle::default();
        let mut runs = Vec::new();
        let mut set = BTreeMap::new();
        loop {
            buf.clear();
            match r.read_event_into(&mut buf)? {
                Event::Start(e) => match e.local_name().as_ref() {
                    b"author" => author = Some(String::new()),
                    b"comment" => {
                        let at = attr(&e, b"ref")?.as_deref().and_then(parse_ref).map(|(r0, c0, _, _)| (r0, c0));
                        let id = attr(&e, b"authorId")?.and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
                        cur = at.map(|a| (a, id, String::new()));
                        run_count = 0;
                        first_run_end = 0;
                        in_run = false;
                        in_t = false;
                        runs.clear();
                    }
                    b"r" => {
                        run_count += 1;
                        in_run = true;
                        style = CommentStyle::default();
                    }
                    b"b" | b"i" | b"u" | b"strike" if in_run => read_style(&e, &mut style)?,
                    b"t" => in_t = true,
                    _ => {}
                },
                Event::Empty(e) if e.local_name().as_ref() == b"author" => authors.push(String::new()),
                Event::Empty(e) if in_run => read_style(&e, &mut style)?,
                Event::Text(t) => {
                    let s = t.unescape().map_err(|e| XlsxError::Xml(e.to_string()))?;
                    if let Some(a) = author.as_mut() {
                        a.push_str(&s);
                    } else if let (true, Some(c)) = (in_t, cur.as_mut()) {
                        let start = c.2.len();
                        if start < MAX_TEXT {
                            c.2.push_str(&s.replace("\r\n", "\n"));
                            if in_run && style.mask() != 0 {
                                runs.push(CommentRun { start, end: c.2.len(), style });
                            }
                        }
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"author" => authors.extend(author.take()),
                    b"t" => in_t = false,
                    b"r" => {
                        in_run = false;
                        if run_count == 1 {
                            first_run_end = cur.as_ref().map_or(0, |c| c.2.len());
                        }
                    }
                    b"comment" => {
                        if let Some(((row, col), id, text)) = cur.take() {
                            let who = authors.get(id).map_or("", |a| a.trim());
                            // Excel may prepend a synthetic bold author run.
                            let mut body = text.as_str();
                            if run_count > 1 && !who.is_empty()
                                && runs.first().is_some_and(|run| run.start == 0 && run.style.bold)
                                && text[..first_run_end].trim_end().strip_suffix(':').is_some_and(|name| name.trim() == who)
                            {
                                body = text[first_run_end..].trim_start_matches(['\n', '\r']);
                            }
                            let offset = text.len() - body.len();
                            if set.len() < MAX_COMMENTS {
                                let styles = runs.drain(..).filter(|run| run.end > offset).map(|mut run| {
                                    run.start = run.start.saturating_sub(offset);
                                    run.end -= offset;
                                    run
                                });
                                set.insert((row, col), Comment::with_runs(who, body, styles));
                            }
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(set)
    }

    fn read_style(e: &quick_xml::events::BytesStart<'_>, style: &mut CommentStyle) -> Result<(), XlsxError> {
        let slot = match e.local_name().as_ref() {
            b"b" => &mut style.bold,
            b"i" => &mut style.italic,
            b"u" => &mut style.underline,
            b"strike" => &mut style.strike,
            _ => return Ok(()),
        };
        *slot = !matches!(attr(e, b"val")?.as_deref(), Some("0" | "false" | "none"));
        Ok(())
    }
}
