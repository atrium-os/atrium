//! The comment bar edits the original text and its inline marks. The balloon
//! wraps the same marked runs, measuring them in the style they are drawn in.

use std::ops::Range;
use std::sync::Arc;

use officina_grid::comments::{Comment, CommentRun, CommentStyle, ThreadMention, ThreadPerson, MAX_MENTIONS, MAX_TEXT, MAX_THREAD_FIELD};
use pergola::text_edit::TextEdit;
use pergola::TextStyle;

pub fn edit(comment: Option<&Comment>) -> TextEdit {
    let text = comment.map_or("", |comment| comment.text.as_ref());
    let runs = comment.into_iter().flat_map(|comment| comment.runs())
        .map(|run| (run.start..run.end, run.style.mask()));
    TextEdit::rich(text, runs)
}

pub fn save(author: &str, edit: &TextEdit) -> Option<Comment> {
    let text = edit.text();
    if text.is_empty() { return None; }
    let runs = edit.inline_runs().filter(|(_, mask)| *mask != 0)
        .map(|(range, mask)| CommentRun {
            start: range.start,
            end: range.end,
            style: CommentStyle::from_mask(mask),
        });
    Some(Comment::with_runs(author, text, runs))
}

#[derive(Clone)]
pub(crate) struct PendingMention {
    id: u128,
    person: Arc<ThreadPerson>,
}

/// Snapshot of the thread being edited, so a stale bar cannot replace edits
/// made by undo, a row operation, or another command.
#[derive(Clone)]
pub(crate) struct ThreadEdit {
    pub original: Option<Comment>,
    pub index: usize,
    pub added_mentions: Vec<PendingMention>,
}

impl ThreadEdit {
    pub fn messages(&self) -> &[officina_grid::comments::ThreadMessage] {
        self.original.as_ref().map_or(&[], Comment::thread)
    }

    pub fn open(&self) -> TextEdit {
        let Some(message) = self.messages().get(self.index) else { return TextEdit::marked_lines(""); };
        TextEdit::marked_lines(&message.text).with_tracked_ranges(
            message.mentions.iter().enumerate().map(|(i, m)| (i, m.start..m.end)),
        )
    }

    pub fn unchanged(&self, edit: &TextEdit) -> bool {
        let Some(message) = self.messages().get(self.index) else { return edit.text().is_empty(); };
        message.text.as_ref() == edit.text() && message.mentions.iter().enumerate()
            .map(|(i, m)| (i, m.start..m.end)).eq(edit.tracked_ranges().iter().cloned())
    }

    fn original_mentions(&self) -> &[ThreadMention] {
        self.messages().get(self.index).map_or(&[], |message| &message.mentions)
    }

    pub fn mention(&mut self, edit: &mut TextEdit, person: Arc<ThreadPerson>) -> bool {
        let name = if person.name.trim().is_empty() { &person.user_id } else { &person.name };
        let label = name.split_whitespace().collect::<Vec<_>>().join(" ");
        let index = self.original_mentions().len() + self.added_mentions.len();
        if label.is_empty() || self.added_mentions.len() >= MAX_MENTIONS * 4
            || !edit.insert_tracked(index, &format!("@{label}"), " ")
            || edit.text().len() > MAX_TEXT || edit.tracked_ranges().len() > MAX_MENTIONS
        {
            return false;
        }
        self.added_mentions.push(PendingMention { id: uuid::Uuid::new_v4().as_u128(), person });
        true
    }

    pub fn save(&self, edit: &TextEdit) -> Option<Comment> {
        use officina_grid::comments::ThreadMessage;

        let text = edit.text();
        if text.trim().is_empty() || text.len() > MAX_TEXT { return None; }
        let original = self.original_mentions();
        let mentions: Arc<[ThreadMention]> = edit.tracked_ranges().iter().filter_map(|(i, range)| {
            let (id, person) = if *i < original.len() {
                let mention = &original[*i];
                (mention.id, &mention.person)
            } else {
                let mention = self.added_mentions.get(*i - original.len())?;
                (mention.id, &mention.person)
            };
            Some(ThreadMention { id, person: person.clone(), start: range.start, end: range.end })
        }).collect();
        let mut messages = self.messages().to_vec();
        if let Some(message) = messages.get_mut(self.index) {
            message.text = text.into();
            message.mentions = mentions;
        } else {
            messages.push(ThreadMessage {
                id: uuid::Uuid::new_v4().as_u128(),
                parent: messages.first().map(|m| m.id),
                author: local_author(),
                timestamp: timestamp().into(),
                done: false,
                text: text.into(),
                mentions,
            });
        }
        Comment::threaded(messages)
    }
}

/// An unlinked @prefix next to the caret. Email addresses already in prose
/// and portions of existing mention tokens are left alone.
pub(crate) fn mention_prefix(edit: &TextEdit) -> Option<(Range<usize>, &str)> {
    if edit.has_selection() { return None; }
    let end = edit.caret();
    let before = &edit.text()[..end];
    let start = before.rfind(|c: char| !(c.is_alphanumeric() || matches!(c, '@' | '.' | '-' | '_' | '+')))
        .map_or(0, |i| i + before[i..].chars().next().unwrap().len_utf8());
    let query = before[start..].strip_prefix('@')?;
    if edit.tracked_ranges().iter().any(|(_, r)| r.start < end && start < r.end) { return None; }
    Some((start..end, query))
}

pub(crate) struct People(Vec<Arc<ThreadPerson>>);

impl People {
    pub fn from_workbook(wb: &officina_grid::Workbook, draft: &[PendingMention]) -> Self {
        const MAX_PEOPLE: usize = 4096;
        let mut ids = std::collections::HashSet::new();
        let mut people = Vec::new();
        let candidates = (0..wb.sheet_count() as u16).flat_map(|sheet| wb.comments(sheet).values())
            .flat_map(Comment::thread)
            .flat_map(|m| std::iter::once(&m.author).chain(m.mentions.iter().map(|mention| &mention.person)));
        for person in draft.iter().map(|mention| &mention.person).chain(candidates) {
            if (person.name.is_empty() && person.user_id.is_empty()) || !ids.insert(person.id) { continue; }
            people.push(person.clone());
            if people.len() == MAX_PEOPLE { break; }
        }
        people.sort_by_cached_key(|p| (p.name.to_lowercase(), p.user_id.to_lowercase(), p.id));
        Self(people)
    }

    pub fn matches(&self, query: &str) -> impl Iterator<Item = &Arc<ThreadPerson>> {
        let query = query.trim().to_lowercase();
        self.0.iter().filter(move |p| {
            p.name.to_lowercase().contains(&query) || p.user_id.to_lowercase().contains(&query)
        }).take(5)
    }

    pub fn get(&self, id: u128) -> Option<Arc<ThreadPerson>> {
        self.0.iter().find(|person| person.id == id).cloned()
    }

    pub fn email(&self, query: &str) -> Option<Arc<ThreadPerson>> {
        let email = email_address(query)?;
        if let Some(existing) = self.0.iter().find(|p| p.user_id.eq_ignore_ascii_case(email)) {
            return Some(existing.clone());
        }
        Some(Arc::new(ThreadPerson {
            id: uuid::Uuid::new_v4().as_u128(), name: email.into(), user_id: email.into(),
            provider_id: "PeoplePicker".into(),
        }))
    }
}

pub(crate) fn email_address(query: &str) -> Option<&str> {
    let query = query.trim();
    let (local, domain) = query.split_once('@')?;
    if query.len() > 254 || local.is_empty() || domain.is_empty() || !domain.contains('.')
        || local.len() > 64 || local.starts_with('.') || local.ends_with('.') || local.contains("..")
        || !local.chars().all(|c| c.is_alphanumeric() || ".!#$%&'*+-/=?^_`{|}~".contains(c))
        || !domain.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        || query.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || domain.split('.').any(|part| part.is_empty() || part.starts_with('-') || part.ends_with('-'))
    {
        return None;
    }
    Some(query)
}

fn local_author() -> std::sync::Arc<officina_grid::comments::ThreadPerson> {
    use std::sync::OnceLock;

    static AUTHOR: OnceLock<Arc<ThreadPerson>> = OnceLock::new();
    AUTHOR.get_or_init(|| {
        let name = super::app::comment_author();
        let name = &name[..name.floor_char_boundary(MAX_THREAD_FIELD.min(name.len()))];
        Arc::new(ThreadPerson {
            id: uuid::Uuid::new_v4().as_u128(),
            name: name.into(),
            user_id: name.into(),
            provider_id: "None".into(),
        })
    }).clone()
}

fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let Ok(elapsed) = SystemTime::now().duration_since(UNIX_EPOCH) else { return String::new(); };
    let Ok(seconds) = libc::time_t::try_from(elapsed.as_secs()) else { return String::new(); };
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // gmtime_r initializes tm on success.
    if unsafe { libc::gmtime_r(&seconds, tm.as_mut_ptr()) }.is_null() { return String::new(); }
    let tm = unsafe { tm.assume_init() };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", tm.tm_year + 1900, tm.tm_mon + 1,
        tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub type Line<'a> = Vec<(&'a str, u8)>;

pub fn wrap<'a>(comment: &'a Comment, base: &TextStyle, width: f32, max: usize) -> Vec<Line<'a>> {
    let text = &*comment.text;
    let mut end = 0;
    let spans: Vec<_> = comment.segments().map(|(text, style)| {
        let start = end;
        end += text.len();
        (start..end, style.mask())
    }).collect();
    let segments = |range: Range<usize>| {
        let first = spans.partition_point(|(span, _)| span.end <= range.start);
        spans[first..].iter().take_while(move |(span, _)| span.start < range.end)
            .filter_map(move |(span, mask)| {
                let a = span.start.max(range.start);
                let b = span.end.min(range.end);
                (a < b).then_some((&text[a..b], *mask))
            })
    };
    let measure = |range| segments(range).map(|(text, mask)| width_of(text, base, mask)).sum::<f32>();
    let mut lines = Vec::new();
    let mut offset = 0;
    'paragraphs: for para in text.split('\n') {
        let mut start = offset;
        let mut end = offset;
        for word in para.split_inclusive([' ', '\t']) {
            let next = end + word.len();
            if measure(start..next) > width {
                if end > start {
                    lines.push(start..end);
                    start = end;
                    if lines.len() > max { break 'paragraphs; }
                    if measure(start..next) <= width {
                        end = next;
                        continue;
                    }
                }
                for (at, ch) in word.char_indices() {
                    let before = end + at;
                    if before > start && measure(start..before + ch.len_utf8()) > width {
                        lines.push(start..before);
                        start = before;
                        if lines.len() > max { break 'paragraphs; }
                    }
                }
            }
            end = next;
            if lines.len() > max { break 'paragraphs; }
        }
        lines.push(start..end);
        if lines.len() > max { break; }
        offset += para.len() + 1;
    }
    let truncated = lines.len() > max;
    lines.truncate(max);
    if truncated {
        if let Some(last) = lines.last_mut() {
            let room = width - width_of("…", base, 0);
            while last.end > last.start && measure(last.clone()) > room {
                last.end -= text[..last.end].chars().next_back().unwrap().len_utf8();
            }
        }
    }
    let mut lines: Vec<Line<'a>> = lines.into_iter().map(|range| segments(range).collect()).collect();
    if truncated {
        if let Some(last) = lines.last_mut() {
            last.push(("…", 0));
        }
    }
    lines
}

pub fn shown(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\t') { text.replace('\t', "    ").into() } else { text.into() }
}

fn width_of(text: &str, base: &TextStyle, mask: u8) -> f32 {
    pergola::text::measure(&shown(text), &pergola::inline::style(base, mask)).w
}
