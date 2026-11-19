//! XLSX threaded comments and their workbook-wide author directory (MS-XLSX
//! 2.1.17–18 and 2.3.7). Ordinary notes are handled by the comments module.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use officina_grid::addr::a1;
use officina_grid::comments::ThreadPerson;
use officina_grid::Workbook;

pub(super) const THREAD_REL: &str = "http://schemas.microsoft.com/office/2017/10/relationships/threadedComment";
pub(super) const PERSON_REL: &str = "http://schemas.microsoft.com/office/2017/10/relationships/person";
pub(super) const THREAD_TYPE: &str = "application/vnd.ms-excel.threadedcomments+xml";
pub(super) const PERSON_TYPE: &str = "application/vnd.ms-excel.person+xml";
const NS: &str = "http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments";
const XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;

type Cell = (u32, u32);
type SheetIds = BTreeMap<Cell, Vec<u128>>;

pub(super) fn guid(id: u128) -> String {
    let hex = format!("{id:032X}");
    format!("{{{}-{}-{}-{}-{}}}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

fn xstring(value: &str) -> String {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, ch) in value.char_indices() {
        let escape: &[u8] = match ch {
            '\t' => b"&#9;",
            '\n' => b"&#10;",
            '\r' => b"_x000D_",
            '\u{FFFE}' => b"_xFFFE_",
            '\u{FFFF}' => b"_xFFFF_",
            _ => continue,
        };
        super::write::esc(&mut out, &value[start..i]);
        out.extend_from_slice(escape);
        start = i + ch.len_utf8();
    }
    super::write::esc(&mut out, &value[start..]);
    String::from_utf8(out).expect("XML escape writes UTF-8")
}

/// Copies of a cell can carry the same original IDs. The first occurrence
/// keeps its ID, and later occurrences get a fresh ID and updated parent links.
pub(super) struct Export<'a> {
    pub(super) sheets: Vec<SheetIds>,
    people: HashMap<&'a ThreadPerson, u128>,
}

impl<'a> Export<'a> {
    pub(super) fn new(wb: &'a Workbook) -> Self {
        let threads = (0..wb.sheet_count() as u16).flat_map(|s| wb.comments(s).values()).flat_map(|k| k.thread());
        let mut reserved: HashSet<_> = threads.flat_map(|m| [m.id, m.author.id]).collect();
        let mut seen_people = HashSet::new();
        let mut seen_messages = HashSet::new();
        let mut next = 0x4000_8000_0000_0000_0000_u128;
        let mut allocate = |seen: &mut HashSet<_>, original| {
            if seen.insert(original) { return original; }
            loop {
                next += 1;
                if reserved.insert(next) { return next; }
            }
        };
        let mut people = HashMap::new();
        let sheets = (0..wb.sheet_count() as u16).map(|s| {
            wb.comments(s).iter().filter(|(_, k)| !k.thread().is_empty()).map(|(&cell, k)| {
                let ids = k.thread().iter().map(|m| {
                    people.entry(m.author.as_ref()).or_insert_with(|| allocate(&mut seen_people, m.author.id));
                    allocate(&mut seen_messages, m.id)
                }).collect();
                (cell, ids)
            }).collect()
        }).collect();
        Self { sheets, people }
    }

    pub(super) fn has_people(&self) -> bool {
        !self.people.is_empty()
    }

    pub(super) fn persons_xml(&self) -> String {
        let mut out = format!(r#"{XML}<personList xmlns="{NS}">"#);
        let mut people: Vec<_> = self.people.iter().collect();
        people.sort_unstable_by_key(|(_, id)| **id);
        for (p, &id) in people {
            let _ = write!(out, r#"<person displayName="{}" id="{}" userId="{}""#, xstring(&p.name), guid(id), xstring(&p.user_id));
            if !p.provider_id.is_empty() {
                let _ = write!(out, r#" providerId="{}""#, xstring(&p.provider_id));
            }
            out.push_str("/>");
        }
        out.push_str("</personList>");
        out
    }

    pub(super) fn sheet_xml(&self, wb: &Workbook, s: u16) -> String {
        let mut out = format!(r#"{XML}<ThreadedComments xmlns="{NS}">"#);
        for (&(r, c), ids) in &self.sheets[s as usize] {
            let messages = wb.comments(s)[&(r, c)].thread();
            let mapped: HashMap<_, _> = messages.iter().zip(ids).map(|(m, &id)| (m.id, id)).collect();
            let cell = a1(r, c);
            for (m, &id) in messages.iter().zip(ids) {
                let _ = write!(out, r#"<threadedComment ref="{cell}" personId="{}" id="{}""#, guid(self.people[m.author.as_ref()]), guid(id));
                if let Some(parent) = m.parent {
                    let _ = write!(out, r#" parentId="{}""#, guid(mapped[&parent]));
                }
                if !m.timestamp.is_empty() {
                    let _ = write!(out, r#" dT="{}""#, crate::xml::esc(&m.timestamp));
                }
                if m.done { out.push_str(r#" done="1""#); }
                let _ = write!(out, "><text>{}</text></threadedComment>", xstring(&m.text));
            }
        }
        out.push_str("</ThreadedComments>");
        out
    }
}

#[cfg(feature = "read")]
pub(super) use read::{parse_guid, persons, sheet};

#[cfg(feature = "read")]
mod read {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use officina_grid::comments::{Comment, ThreadMessage, MAX_COMMENTS, MAX_TEXT, MAX_THREAD_FIELD, MAX_THREAD_MESSAGES};
    use quick_xml::events::{BytesStart, Event};

    use super::*;
    use crate::report::{Kind, Report};
    use crate::xlsx::read::{attr, decode_escapes, parse_ref, xml};
    use crate::xlsx::XlsxError;
    use crate::zip::Archive;

    pub(in crate::xlsx) fn parse_guid(s: &str) -> Option<u128> {
        let hex = s.strip_prefix('{')?.strip_suffix('}')?;
        if hex.len() != 36 || [8, 13, 18, 23].into_iter().any(|i| hex.as_bytes()[i] != b'-') {
            return None;
        }
        let digits = hex.replace('-', "");
        if digits.len() != 32 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) { return None; }
        u128::from_str_radix(&digits, 16).ok()
    }

    type People = HashMap<u128, Arc<ThreadPerson>>;

    pub(in crate::xlsx) fn persons(z: &Archive, path: Option<&str>, rep: &mut Report) -> Result<People, XlsxError> {
        let Some(data) = path.map(|p| z.read_name(p)).transpose()?.flatten() else { return Ok(HashMap::new()); };
        let mut reader = xml(&data);
        let mut out = HashMap::new();
        loop {
            match reader.read_event()? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"person" => {
                    let Some(id) = attr(&e, b"id")?.as_deref().and_then(parse_guid) else { continue; };
                    if out.len() >= MAX_COMMENTS { break; }
                    out.entry(id).or_insert(Arc::new(ThreadPerson {
                        id,
                        name: field(&e, b"displayName", rep)?,
                        user_id: field(&e, b"userId", rep)?,
                        provider_id: field(&e, b"providerId", rep)?,
                    }));
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(out)
    }

    fn field(e: &BytesStart<'_>, key: &[u8], rep: &mut Report) -> Result<Arc<str>, XlsxError> {
        let value = attr(e, key)?.unwrap_or_default();
        let decoded = decode_escapes(&value);
        Ok(bounded(&decoded, MAX_THREAD_FIELD, rep).into())
    }

    fn bounded<'a>(s: &'a str, max: usize, rep: &mut Report) -> &'a str {
        if s.len() > max {
            rep.note(Kind::Approximate, "threaded comment text or metadata exceeded import limits");
            &s[..s.floor_char_boundary(max)]
        } else {
            s
        }
    }

    struct Pending {
        cell: Option<Cell>,
        message: ThreadMessage,
        text: String,
    }

    fn start(e: &BytesStart<'_>, people: &mut People, rep: &mut Report) -> Result<Option<Pending>, XlsxError> {
        let id = attr(e, b"id")?.as_deref().and_then(parse_guid);
        let author = attr(e, b"personId")?.as_deref().and_then(parse_guid);
        let parent = attr(e, b"parentId")?;
        let parent_id = parent.as_deref().and_then(parse_guid);
        let (Some(id), Some(author_id), true) = (id, author, parent.is_none() || parent_id.is_some()) else {
            rep.note(Kind::Dropped, "threaded comments with invalid IDs");
            return Ok(None);
        };
        let author = people.entry(author_id).or_insert_with(|| {
            rep.note(Kind::Approximate, "threaded comments with missing author records");
            Arc::new(ThreadPerson { id: author_id, name: "".into(), user_id: "".into(), provider_id: "".into() })
        }).clone();
        let cell = attr(e, b"ref")?.as_deref().and_then(parse_ref)
            .and_then(|(r, c, r1, c1)| ((r, c) == (r1, c1)).then_some((r, c)));
        Ok(Some(Pending {
            cell,
            message: ThreadMessage {
                id, parent: parent_id, author, timestamp: field(e, b"dT", rep)?,
                done: attr(e, b"done")?.is_some_and(|v| matches!(v.as_str(), "1" | "true")),
                text: "".into(),
            },
            text: String::new(),
        }))
    }

    fn append(pending: &mut Pending, text: &str, rep: &mut Report) {
        // Each escaped character can use seven bytes before Xstring decoding.
        let room = 7 * MAX_TEXT - pending.text.len();
        pending.text.push_str(bounded(text, room, rep));
    }

    fn finish(mut pending: Pending, rep: &mut Report) -> (Option<Cell>, ThreadMessage) {
        let normal = pending.text.replace("\r\n", "\n").replace('\r', "\n");
        let text = decode_escapes(&normal);
        pending.message.text = bounded(&text, MAX_TEXT, rep).into();
        (pending.cell, pending.message)
    }

    pub(in crate::xlsx) fn sheet(data: &[u8], people: &mut People, placeholders: &HashMap<u128, Cell>, rep: &mut Report) -> Result<BTreeMap<Cell, Comment>, XlsxError> {
        let mut reader = xml(data);
        let mut pending = None;
        let mut in_text = false;
        let mut messages = Vec::new();
        loop {
            match reader.read_event()? {
                Event::Start(e) if e.local_name().as_ref() == b"threadedComment" => {
                    pending = start(&e, people, rep)?;
                    in_text = false;
                }
                Event::Empty(e) if e.local_name().as_ref() == b"threadedComment" => {
                    messages.extend(start(&e, people, rep)?.map(|p| finish(p, rep)));
                }
                Event::Start(e) if e.local_name().as_ref() == b"text" => in_text = true,
                Event::Start(e) if e.local_name().as_ref() == b"mentions" && pending.is_some() => {
                    rep.note(Kind::Dropped, "threaded comment mention metadata");
                }
                Event::Text(t) if in_text => {
                    if let Some(p) = pending.as_mut() {
                        let text = t.unescape().map_err(|e| XlsxError::Xml(e.to_string()))?;
                        append(p, &text, rep);
                    }
                }
                Event::CData(t) if in_text => {
                    if let Some(p) = pending.as_mut() {
                        let text = t.decode().map_err(|e| XlsxError::Xml(e.to_string()))?;
                        append(p, &text, rep);
                    }
                }
                Event::End(e) if e.local_name().as_ref() == b"text" => in_text = false,
                Event::End(e) if e.local_name().as_ref() == b"threadedComment" => {
                    messages.extend(pending.take().map(|p| finish(p, rep)));
                }
                Event::Eof => break,
                _ => {}
            }
            if messages.len() >= MAX_COMMENTS {
                rep.note(Kind::Dropped, "threaded comments exceeded the worksheet import limit");
                break;
            }
        }

        let mut children: HashMap<u128, Vec<usize>> = HashMap::new();
        let mut ids = HashSet::new();
        let total = messages.len();
        messages.retain(|(_, m)| ids.insert(m.id));
        if total != messages.len() {
            rep.note(Kind::Dropped, "duplicate threaded comment IDs");
        }
        for (i, (_, m)) in messages.iter().enumerate() {
            if let Some(parent) = m.parent { children.entry(parent).or_default().push(i); }
        }
        let mut out = BTreeMap::new();
        let mut kept = 0;
        for (i, (cell, m)) in messages.iter().enumerate() {
            if m.parent.is_some() { continue; }
            let Some(cell) = placeholders.get(&m.id).copied().or(*cell) else { continue; };
            if out.contains_key(&cell) { continue; }
            let mut queue = BTreeSet::from([i]);
            let mut thread = Vec::new();
            while thread.len() < MAX_THREAD_MESSAGES {
                let Some(index) = queue.pop_first() else { break; };
                let m = &messages[index].1;
                thread.push(m.clone());
                if let Some(next) = children.remove(&m.id) {
                    queue.extend(next);
                }
            }
            if let Some(comment) = Comment::threaded(thread) {
                kept += comment.thread().len();
                out.insert(cell, comment);
            }
        }
        if kept != ids.len() {
            rep.note(Kind::Dropped, "unattached or oversized threaded comment conversations");
        }
        Ok(out)
    }
}
