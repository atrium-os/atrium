//! The comment bar edits the original text and its inline marks. The balloon
//! wraps the same marked runs, measuring them in the style they are drawn in.

use std::ops::Range;

use officina_grid::comments::{Comment, CommentRun, CommentStyle};
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
