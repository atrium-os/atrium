//! Cell comments (Excel's notes, ODF's `office:annotation`): a few lines of
//! text and who wrote them, on a cell. They stay when the cell is
//! cleared or edited, as in Excel, and move with inserted and deleted rows
//! and columns ([`crate::shift`]). [`crate::ops::GridOp::Comments`] changes
//! them, one undo step.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use officina_core::wire::{Reader, WireError, Writer};

/// The longest comment text kept, in bytes (Excel's limit is 32,767 characters).
pub const MAX_TEXT: usize = 32 << 10;
/// The longest author name kept, in bytes.
pub const MAX_AUTHOR: usize = 255;
/// The most comments a sheet keeps.
pub const MAX_COMMENTS: usize = 1 << 20;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CommentStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

impl CommentStyle {
    pub fn mask(self) -> u8 {
        u8::from(self.bold) | (u8::from(self.italic) << 1)
            | (u8::from(self.underline) << 2) | (u8::from(self.strike) << 3)
    }

    pub fn from_mask(mask: u8) -> Self {
        Self { bold: mask & 1 != 0, italic: mask & 2 != 0, underline: mask & 4 != 0, strike: mask & 8 != 0 }
    }
}

/// A styled range in the UTF-8 text, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentRun {
    pub start: usize,
    pub end: usize,
    pub style: CommentStyle,
}

/// Max messages per thread and max bytes in an external identity field.
pub const MAX_THREAD_MESSAGES: usize = 1024;
pub const MAX_THREAD_FIELD: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ThreadPerson {
    pub id: u128,
    pub name: Arc<str>,
    pub user_id: Arc<str>,
    pub provider_id: Arc<str>,
}

/// A person reference over a UTF-8 byte range of a thread message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMention {
    pub id: u128,
    pub person: Arc<ThreadPerson>,
    pub start: usize,
    pub end: usize,
}

pub const MAX_MENTIONS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMessage {
    pub id: u128,
    pub parent: Option<u128>,
    pub author: Arc<ThreadPerson>,
    pub timestamp: Arc<str>,
    pub done: bool,
    pub text: Arc<str>,
    pub mentions: Arc<[ThreadMention]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Who wrote it; may be empty.
    pub author: Arc<str>,
    /// Its text; lines split by `\n`.
    pub text: Arc<str>,
    runs: Arc<[CommentRun]>,
    thread: Arc<[ThreadMessage]>,
}

impl Comment {
    pub fn new(author: &str, text: &str) -> Comment {
        Comment { author: cut(author, MAX_AUTHOR).into(), text: cut(text, MAX_TEXT).into(), runs: Arc::default(), thread: Arc::default() }
    }

    pub fn with_runs(author: &str, text: &str, runs: impl IntoIterator<Item = CommentRun>) -> Self {
        let mut comment = Self::new(author, text);
        let mut kept: Vec<CommentRun> = Vec::new();
        for mut run in runs {
            run.end = run.end.min(comment.text.len());
            if run.start >= run.end || !comment.text.is_char_boundary(run.start)
                || !comment.text.is_char_boundary(run.end) || run.style.mask() == 0
            {
                continue;
            }
            if let Some(last) = kept.last_mut() {
                if run.start < last.end {
                    continue;
                }
                if run.start == last.end && run.style == last.style {
                    last.end = run.end;
                    continue;
                }
            }
            kept.push(run);
        }
        comment.runs = kept.into();
        comment
    }

    /// Make a cell comment from a root and its replies in parent-first order.
    /// The plain text is a bounded view for the balloon and legacy formats.
    pub fn threaded(messages: Vec<ThreadMessage>) -> Option<Self> {
        let root = messages.first()?;
        if root.parent.is_some() || messages.len() > MAX_THREAD_MESSAGES {
            return None;
        }
        let mut seen = HashSet::new();
        for (i, message) in messages.iter().enumerate() {
            if (i > 0 && !message.parent.is_some_and(|p| seen.contains(&p))) || !seen.insert(message.id)
                || message.text.len() > MAX_TEXT || message.timestamp.len() > MAX_THREAD_FIELD
                || [message.author.name.len(), message.author.user_id.len(), message.author.provider_id.len()]
                    .into_iter().any(|n| n > MAX_THREAD_FIELD)
            {
                return None;
            }
            if message.mentions.len() > MAX_MENTIONS || message.mentions.iter().any(|m| {
                m.start >= m.end || message.text.get(m.start..m.end).is_none()
                    || [&m.person.name, &m.person.user_id, &m.person.provider_id].into_iter()
                        .any(|field| field.len() > MAX_THREAD_FIELD)
            }) {
                return None;
            }
        }
        let mut view = root.text.to_string();
        for reply in &messages[1..] {
            if view.len() >= MAX_TEXT { break; }
            view.push_str("\n\n");
            if !reply.author.name.is_empty() {
                view.push_str(&reply.author.name);
                view.push_str(": ");
            }
            view.push_str(&reply.text);
        }
        let mut comment = Self::new(&root.author.name, &view);
        comment.thread = messages.into();
        Some(comment)
    }

    pub fn thread(&self) -> &[ThreadMessage] {
        &self.thread
    }

    pub(crate) fn estimated_bytes(&self) -> u64 {
        let thread = self.thread.iter().map(|m| {
            160 + m.text.len() + m.timestamp.len() + m.author.name.len()
                + m.author.user_id.len() + m.author.provider_id.len()
                + m.mentions.iter().map(|m| 96 + m.person.name.len() + m.person.user_id.len()
                    + m.person.provider_id.len()).sum::<usize>()
        }).sum::<usize>();
        (64 + self.author.len() + self.text.len() + std::mem::size_of_val(self.runs()) + thread) as u64
    }

    pub fn runs(&self) -> &[CommentRun] {
        &self.runs
    }

    /// The plain and styled segments in text order.
    pub fn segments(&self) -> impl Iterator<Item = (&str, CommentStyle)> {
        let mut start = 0;
        self.runs.iter().flat_map(move |run| {
            let plain = &self.text[start..run.start];
            start = run.end;
            [(plain, CommentStyle::default()), (&self.text[run.start..run.end], run.style)]
        }).chain(std::iter::once_with(|| {
            let end = self.runs.last().map_or(0, |run| run.end);
            (&self.text[end..], CommentStyle::default())
        })).filter(|(text, _)| !text.is_empty())
    }

}

/// `s` cut to at most `max` bytes, on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// A sheet's comments by (row, column).
pub type Comments = BTreeMap<(u32, u32), Comment>;

/// Writes changes: count, then each cell and its comment or none.
pub fn encode(w: &mut Writer, set: &[(u32, u32, Option<Comment>)]) {
    w.var(set.len() as u64);
    for (r, c, k) in set {
        encode_entry(w, *r, *c, k.as_ref());
    }
}

pub(crate) fn encode_entry(w: &mut Writer, row: u32, col: u32, comment: Option<&Comment>) {
    w.var(row as u64);
    w.var(col as u64);
    match comment {
        Some(k) if !k.thread.is_empty() => {
            let mentions = k.thread().iter().any(|m| !m.mentions.is_empty());
            w.u8(if mentions { 4 } else { 3 });
            encode_thread(w, k.thread(), mentions);
        }
        Some(k) => {
            w.u8(if k.runs.is_empty() { 1 } else { 2 });
            w.str(&k.author);
            w.str(&k.text);
            if !k.runs.is_empty() {
                w.var(k.runs.len() as u64);
                for run in k.runs() {
                    w.var(run.start as u64);
                    w.var(run.end as u64);
                    w.u8(run.style.mask());
                }
            }
        }
        None => w.u8(0),
    }
}

/// Reads what [`encode`] wrote.
pub fn decode(r: &mut Reader) -> Result<Vec<(u32, u32, Option<Comment>)>, WireError> {
    decode_in(r, MAX_ROWS, MAX_COLS, MAX_COMMENTS as u64 * 2)
}

pub(crate) fn decode_in(r: &mut Reader, rows: u32, cols: u32, max: u64) -> Result<Vec<(u32, u32, Option<Comment>)>, WireError> {
    let n = r.count("comments", max, 3)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let (row, col) = (r.var()?, r.var()?);
        if row >= rows as u64 || col >= cols as u64 {
            return Err(WireError::Invalid("comment cell"));
        }
        let k = match r.u8()? {
            0 => None,
            kind @ (1 | 2) => {
                let author = r.str("comment author", MAX_AUTHOR as u64)?;
                let text = r.str("comment", MAX_TEXT as u64)?;
                if kind == 1 {
                    Some(Comment::new(author, text))
                } else {
                    let count = r.count("comment runs", text.len() as u64, 3)?;
                    let mut runs = Vec::with_capacity(count);
                    let mut previous_end = 0;
                    for _ in 0..count {
                        let (start, end, mask) = (r.var()?, r.var()?, r.u8()?);
                        if start < previous_end || start >= end || end > text.len() as u64
                            || !text.is_char_boundary(start as usize) || !text.is_char_boundary(end as usize)
                            || !(1..=15).contains(&mask)
                        {
                            return Err(WireError::Invalid("comment run"));
                        }
                        runs.push(CommentRun { start: start as usize, end: end as usize, style: CommentStyle::from_mask(mask) });
                        previous_end = end;
                    }
                    Some(Comment::with_runs(author, text, runs))
                }
            }
            kind @ (3 | 4) => Some(decode_thread(r, kind == 4)?),
            _ => return Err(WireError::Invalid("comment")),
        };
        v.push((row as u32, col as u32, k));
    }
    Ok(v)
}

fn encode_thread(w: &mut Writer, messages: &[ThreadMessage], mentions: bool) {
    fn id(w: &mut Writer, id: u128) {
        w.u64(id as u64);
        w.u64((id >> 64) as u64);
    }
    fn person(w: &mut Writer, p: &ThreadPerson) {
        id(w, p.id);
        w.str(&p.name);
        w.str(&p.user_id);
        w.str(&p.provider_id);
    }
    w.var(messages.len() as u64);
    for message in messages {
        id(w, message.id);
        if let Some(parent) = message.parent { id(w, parent); }
        person(w, &message.author);
        w.str(&message.timestamp);
        w.u8(u8::from(message.done));
        w.str(&message.text);
        if mentions {
            w.var(message.mentions.len() as u64);
            for mention in message.mentions.iter() {
                id(w, mention.id);
                person(w, &mention.person);
                w.var(mention.start as u64);
                w.var(mention.end as u64);
            }
        }
    }
}

fn decode_thread(r: &mut Reader, has_mentions: bool) -> Result<Comment, WireError> {
    fn id(r: &mut Reader) -> Result<u128, WireError> {
        Ok(r.u64()? as u128 | ((r.u64()? as u128) << 64))
    }
    fn person(r: &mut Reader) -> Result<Arc<ThreadPerson>, WireError> {
        Ok(Arc::new(ThreadPerson {
            id: id(r)?,
            name: r.str("thread author", MAX_THREAD_FIELD as u64)?.into(),
            user_id: r.str("thread user ID", MAX_THREAD_FIELD as u64)?.into(),
            provider_id: r.str("thread provider ID", MAX_THREAD_FIELD as u64)?.into(),
        }))
    }
    let n = r.count("thread messages", MAX_THREAD_MESSAGES as u64, 38)?;
    let mut messages = Vec::with_capacity(n);
    for i in 0..n {
        let message_id = id(r)?;
        let parent = (i != 0).then(|| id(r)).transpose()?;
        let author = person(r)?;
        let timestamp = r.str("thread timestamp", MAX_THREAD_FIELD as u64)?.into();
        let done = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(WireError::Invalid("thread resolution")),
        };
        let text = r.str("thread text", MAX_TEXT as u64)?.into();
        let mut mentions = Vec::new();
        if has_mentions {
            let count = r.count("thread mentions", MAX_MENTIONS as u64, 37)?;
            for _ in 0..count {
                let id = id(r)?;
                let person = person(r)?;
                let (start, end) = (r.var()?, r.var()?);
                if start >= end || end > MAX_TEXT as u64 {
                    return Err(WireError::Invalid("thread mention range"));
                }
                mentions.push(ThreadMention { id, person, start: start as usize, end: end as usize });
            }
        }
        messages.push(ThreadMessage { id: message_id, parent, author, timestamp, done, text, mentions: mentions.into() });
    }
    Comment::threaded(messages).ok_or(WireError::Invalid("comment thread"))
}

impl Workbook {
    /// The sheet's comments, by (row, column).
    pub fn comments(&self, sheet: u16) -> &Comments {
        static NONE: Comments = BTreeMap::new();
        self.sheets.get(sheet as usize).map_or(&NONE, |s| &s.comments)
    }

    /// The comment on a cell.
    pub fn comment(&self, sheet: u16, row: u32, col: u32) -> Option<&Comment> {
        self.comments(sheet).get(&(row, col))
    }

    /// Sets (or, `None`, takes away) comments; what was there before, in the
    /// same order, so applying it undoes this. Past [`MAX_COMMENTS`] new ones
    /// are left out.
    pub fn set_comments(&mut self, sheet: u16, set: Vec<(u32, u32, Option<Comment>)>) -> Vec<(u32, u32, Option<Comment>)> {
        let Some(sh) = self.sheets.get_mut(sheet as usize) else { return Vec::new() };
        let mut old = Vec::with_capacity(set.len());
        for (r, c, k) in set {
            let before = match k {
                Some(k) if sh.comments.len() < MAX_COMMENTS || sh.comments.contains_key(&(r, c)) => sh.comments.insert((r, c), k),
                Some(_) => continue,
                None => sh.comments.remove(&(r, c)),
            };
            old.push((r, c, before));
        }
        old.reverse();
        old
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::CellAddr;
    use crate::native;
    use crate::ops::GridOp;
    use crate::shift::{shift_op, Axis};
    use crate::Value;
    use officina_core::history::Op;

    #[test]
    fn comments_undo_round_trip_survive_clearing_and_follow_rows() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_value(CellAddr::new(0, 2, 1), Value::Num(5.0));
        let op = GridOp::Comments { sheet: 0, set: vec![(2, 1, Some(Comment::with_runs("Ana", "Check this\nagainst March", [CommentRun { start: 0, end: 5, style: CommentStyle { bold: true, ..CommentStyle::default() } }]))), (0, 0, Some(Comment::new("", "Title")))] };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let inv = op.apply(&mut wb);
        assert_eq!(wb.comment(0, 2, 1).map(|k| &*k.author), Some("Ana"));
        // Kept in the native file.
        let back = native::load(&native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.comments(0), wb.comments(0));
        // Clearing the cell keeps its comment.
        let clear = GridOp::clear(crate::addr::Rect { sheet: 0, r0: 2, c0: 1, r1: 2, c1: 1 }).apply(&mut wb);
        assert!(wb.comment(0, 2, 1).is_some());
        clear.apply(&mut wb);
        // A row above moves it down; deleting its row takes it away; undo brings both back.
        let ins = shift_op(&wb, 0, Axis::Rows, 1, 1).unwrap().apply(&mut wb);
        assert!(wb.comment(0, 3, 1).is_some() && wb.comment(0, 2, 1).is_none());
        assert!(wb.comment(0, 0, 0).is_some());
        ins.apply(&mut wb);
        let del = shift_op(&wb, 0, Axis::Rows, 2, -1).unwrap().apply(&mut wb);
        assert!(wb.comment(0, 2, 1).is_none() && wb.comments(0).len() == 1);
        del.apply(&mut wb);
        assert_eq!(wb.comment(0, 2, 1).map(|k| &*k.text), Some("Check this\nagainst March"));
        inv.apply(&mut wb);
        assert!(wb.comments(0).is_empty());
    }

    #[test]
    fn long_text_is_cut_on_a_character() {
        let k = Comment::new("x", &"é".repeat(MAX_TEXT));
        assert!(k.text.len() <= MAX_TEXT && k.text.len() % 2 == 0);
    }
}
