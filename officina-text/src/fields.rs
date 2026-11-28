//! Fields: values shown in the text and worked out from the rest of the
//! document after pagination (§7.4): page and text references to a paragraph,
//! and the table of contents built from them.
//!
//! A field is one U+FFFC with [`CharAttrs::field`] set, showing a label the way
//! a note anchor shows its number. What it shows lives in
//! [`TextDoc::field_values`], derived and never saved. When pagination is done,
//! [`resolve`] works every field out again and returns the paragraphs whose
//! fields now show something else; the editor lays those out again and
//! paginates once more, at most [`MAX_ROUNDS`] times (a page number that moves
//! its own paragraph onto another page can't loop forever).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::model::{AttrId, Block, BlockId, CaptionKind, CharAttrs, Field, Leader, Para, ParaProps, StyleId, TabAlign, TabStop, TextDoc, MAX_REF_TEXT, OBJECT_CHAR};
use crate::ops::TextOp;
use crate::paginate::Page;

/// Rounds of resolve-and-repaginate after an edit before fields stay as they are.
pub const MAX_ROUNDS: u32 = 3;

/// Most entries a table of contents gets.
pub const MAX_TOC_ENTRIES: usize = 10_000;

/// Each TOC level's extra start indent, twips.
pub const TOC_INDENT: i32 = 360;

impl TextDoc {
    pub fn caption_number(&self, id: BlockId, at: u32) -> String {
        self.blocks.caption_number(&self.attrs, id, at).map_or_else(|| "?".into(), |n| n.to_string())
    }

    /// What field `f` shows: its value as last worked out, or, before that, a
    /// text reference's text and "?" for a page.
    pub fn field_text(&self, f: Field) -> String {
        if let Some(v) = self.field_values.get(&f) {
            return v.to_string();
        }
        match f {
            Field::TextRef(b) => self.index_of(b).and_then(|i| self.para(i)).map_or_else(|| "?".to_string(), |p| self.ref_text(p)),
            Field::PageRef(_) => "?".to_string(),
        }
    }

    /// Index of the block with id `id` (a scan; fields resolve with a map).
    pub fn index_of(&self, id: BlockId) -> Option<usize> {
        self.blocks.iter().position(|b| b.id() == id)
    }

    /// A paragraph's text as a reference shows it: what is on the page (no
    /// tombstones, pictures, anchors or fields), trimmed, at most
    /// [`MAX_REF_TEXT`] chars.
    pub fn ref_text(&self, p: &Para) -> String {
        let text = self.caption_text(p);
        match text.char_indices().nth(MAX_REF_TEXT) {
            Some((i, _)) => format!("{}…", text[..i].trim_end()),
            None => text,
        }
    }

    pub fn caption_text(&self, p: &Para) -> String {
        let mut s = String::new();
        for (a, b, at) in p.spans() {
            let x = self.attrs.get(at);
            let t = &p.text[a as usize..b as usize];
            if x.deleted.is_some() || x.conflict.is_some() {
                continue;
            }
            for (off, ch) in t.char_indices() {
                if ch == OBJECT_CHAR {
                    if x.sequence.is_some() {
                        s.push_str(&self.caption_number(p.id, a + off as u32));
                    }
                    continue;
                }
                s.push(if ch == '\t' || ch == '\u{2028}' { ' ' } else { ch });
            }
        }
        s.trim().to_string()
    }

    fn caption_kinds(&self, p: &Para) -> [bool; 3] {
        let mut found = [false; 3];
        if p.props.figure_list.is_some() || p.props.toc.is_some() || !p.text.contains(OBJECT_CHAR) {
            return found;
        }
        for (start, end, attr) in p.spans() {
            let a = self.attrs.get(attr);
            if let Some(kind) = a.sequence.filter(|_| a.deleted.is_none() && a.conflict.is_none()) {
                found[kind as usize] |= p.text[start as usize..end as usize].contains(OBJECT_CHAR);
            }
        }
        found
    }

    /// The caption a generated entry's page field links to.
    pub fn figure_list_target(&self, p: &Para) -> Option<BlockId> {
        p.props.figure_list?;
        self.fields_in(p).into_iter().find_map(|(_, f)| match f {
            Field::PageRef(target) => Some(target),
            _ => None,
        })
    }

    /// The fields in paragraph `p`: (byte, field).
    pub fn fields_in(&self, p: &Para) -> Vec<(u32, Field)> {
        if !p.text.contains(OBJECT_CHAR) {
            return Vec::new();
        }
        let mut v = Vec::new();
        for (at, ch) in p.text.char_indices() {
            if ch == OBJECT_CHAR {
                let x = self.attrs.get(p.attr_at(at as u32));
                if let (Some(f), None) = (x.field, x.deleted) {
                    v.push((at as u32, f));
                }
            }
        }
        v
    }
}

/// Works every field out against `pages` (complete) and stores the values;
/// returns the blocks, in order, whose fields now show something else.
pub fn resolve(doc: &mut TextDoc, pages: &[Page]) -> Vec<usize> {
    let mut changed = Vec::new();
    let mut sequences = HashMap::new();
    // Which blocks hold fields, and what they refer to.
    let mut holders: Vec<(usize, Vec<Field>)> = Vec::new();
    let mut wanted: HashSet<BlockId> = HashSet::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        let Some(p) = b.as_para() else { continue };
        let mut renumbered = false;
        for (start, end, attr) in p.spans() {
            if doc.attrs.get(attr).sequence.is_none() { continue; }
            for (offset, _) in p.text[start as usize..end as usize].match_indices(OBJECT_CHAR) {
                let key = (p.id, start + offset as u32);
                if let Some(n) = doc.blocks.caption_number(&doc.attrs, key.0, key.1) {
                    renumbered |= doc.sequence_values.get(&key) != Some(&n);
                    sequences.insert(key, n);
                }
            }
        }
        if renumbered { changed.push(i); }
        let f: Vec<Field> = doc.fields_in(p).into_iter().map(|x| x.1).collect();
        if !f.is_empty() {
            wanted.extend(f.iter().map(|x| x.target()));
            holders.push((i, f));
        }
    }
    doc.sequence_values = sequences;
    if holders.is_empty() {
        doc.field_values.clear();
        return changed;
    }
    let mut at: HashMap<BlockId, usize> = HashMap::new();
    for (i, b) in doc.blocks.iter().enumerate() {
        if wanted.contains(&b.id()) {
            at.insert(b.id(), i);
        }
    }
    let page_of = |i: usize| pages.partition_point(|p| p.start.block < i || (p.start.block == i && p.start.line == 0)).saturating_sub(1);
    let mut values: HashMap<Field, Arc<str>> = HashMap::new();
    for (i, fs) in &holders {
        let mut differs = false;
        for &f in fs {
            if !values.contains_key(&f) {
                let v: Arc<str> = match (f, at.get(&f.target())) {
                    (_, None) => "?".into(),
                    (Field::PageRef(_), Some(&k)) => (page_of(k) + 1).to_string().into(),
                    (Field::TextRef(_), Some(&k)) => doc.para(k).map_or_else(|| "?".to_string(), |p| doc.ref_text(p)).into(),
                };
                values.insert(f, v);
            }
            differs |= doc.field_text(f) != *values[&f];
        }
        if differs {
            changed.push(*i);
        }
    }
    doc.field_values = values;
    changed.sort_unstable();
    changed.dedup();
    changed
}

/// Readers: a field read before its bookmark is known refers to name `k` of
/// their list as target `UNBOUND | k`, until [`bind`].
pub const UNBOUND: BlockId = 1 << 63;

/// Readers: points each unbound field at the paragraph its bookmark name
/// marks (`marks`: name → paragraph id) and keeps what the file showed for it
/// (`shown`, by the unbound field) as its value until pagination works it
/// out. A field whose bookmark the file doesn't have becomes the text it
/// showed. Returns how many became text.
pub fn bind(doc: &mut TextDoc, names: &[String], marks: &HashMap<String, BlockId>, shown: &HashMap<Field, String>) -> usize {
    let rebind = |f: Field| -> Option<Field> {
        let t = f.target();
        if t & UNBOUND == 0 {
            return Some(f);
        }
        let id = *names.get((t & !UNBOUND) as usize).and_then(|n| marks.get(n))?;
        Some(match f {
            Field::PageRef(_) => Field::PageRef(id),
            Field::TextRef(_) => Field::TextRef(id),
        })
    };
    let mut lost = 0;
    let mut map: HashMap<AttrId, (AttrId, Option<Field>, Option<Field>)> = HashMap::new();
    for i in 0..doc.blocks.len() {
        let Some(p) = doc.para(i) else { continue };
        if !p.runs.iter().any(|r| doc.attrs.get(r.1).field.is_some_and(|f| f.target() & UNBOUND != 0)) {
            continue;
        }
        let p = p.clone();
        let mut text = String::new();
        let mut runs: Vec<(u32, AttrId)> = Vec::new();
        for (s, e, a) in p.spans() {
            let (na, was, now) = *map.entry(a).or_insert_with(|| {
                let x = doc.attrs.get(a).clone();
                match x.field {
                    Some(f) if f.target() & UNBOUND != 0 => {
                        let now = rebind(f);
                        let mut y = x.clone();
                        y.field = now;
                        (doc.attrs.intern(y), Some(f), now)
                    }
                    _ => (a, None, None),
                }
            });
            let push = |t: &str, text: &mut String, runs: &mut Vec<(u32, AttrId)>| {
                if t.is_empty() {
                    return;
                }
                if runs.last().map(|r| r.1) != Some(na) {
                    runs.push((text.len() as u32, na));
                }
                text.push_str(t);
            };
            let t = &p.text[s as usize..e as usize];
            match (was, now) {
                (Some(f), None) => {
                    // No bookmark: what the file showed, as text.
                    for ch in t.chars() {
                        if ch == OBJECT_CHAR {
                            lost += 1;
                            push(shown.get(&f).map_or("", |v| v.as_str()), &mut text, &mut runs);
                        } else {
                            push(ch.encode_utf8(&mut [0; 4]), &mut text, &mut runs);
                        }
                    }
                }
                _ => push(t, &mut text, &mut runs),
            }
            if let (Some(f), Some(g)) = (was, now) {
                if let Some(v) = shown.get(&f).filter(|v| !v.trim().is_empty()) {
                    doc.field_values.insert(g, v.trim().chars().take(MAX_REF_TEXT).collect::<String>().into());
                }
            }
        }
        if runs.is_empty() {
            // An empty cached result leaves the field's typing format.
            let mut typing = doc.attrs.get(p.attr_at(0)).clone();
            typing.field = None;
            runs.push((0, doc.attrs.intern(typing)));
        }
        doc.blocks.update(i, |b| {
            if let Some(q) = b.as_para_mut() {
                if text.is_empty() {
                    q.props.toc = None;
                    q.props.figure_list = None;
                }
                q.text = text;
                q.runs = runs;
                q.normalize();
            }
        });
    }
    lost
}

/// Readers: a contents entry that came with its page number as text (a
/// producer that flattened the field) gets a page reference again, to the
/// heading with the entry's text: the next such heading after the last one
/// matched, so repeated titles pair in order. Returns how many were linked.
pub fn relink_toc(doc: &mut TextDoc) -> usize {
    let mut heads: Vec<(BlockId, String)> = Vec::new();
    let mut captions = HashMap::new();
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    for b in doc.blocks.iter() {
        let Some(p) = b.as_para() else { continue };
        let kinds = doc.caption_kinds(p);
        if kinds.contains(&true) {
            let title = norm(&doc.caption_text(p));
            for kind in CaptionKind::ALL.into_iter().filter(|&k| kinds[k as usize]) {
                captions.entry((kind, title.clone())).or_insert(p.id);
            }
        }
        if p.props.toc.is_none() && p.props.figure_list.is_none() && p.props.cell.is_none() && doc.styles.para_format(p.style, &p.props).0.outline_level.is_some() {
            heads.push((p.id, doc.ref_text(p)));
        }
    }
    let mut from = 0;
    let mut n = 0;
    for i in 0..doc.blocks.len() {
        let Some(p) = doc.para(i) else { continue };
        if (p.props.toc.is_none() && p.props.figure_list.is_none()) || p.text.contains(OBJECT_CHAR) {
            continue;
        }
        let Some(tab) = p.text.rfind('\t') else { continue };
        let num = p.text[tab + 1..].trim();
        if num.is_empty() || num.chars().count() > 12 || num.contains(char::is_whitespace) {
            continue;
        }
        let title = norm(p.text[..tab].trim());
        let target = if let Some(kind) = p.props.figure_list {
            let Some(&id) = captions.get(&(kind, title)) else { continue };
            id
        } else {
            let Some(k) = (from..heads.len()).chain(0..from).find(|&k| norm(&heads[k].1) == title) else { continue };
            from = k + 1;
            heads[k].0
        };
        let num = num.to_string();
        let at = (tab + 1) as u32;
        let mut a = doc.attrs.get(p.attr_at(at)).clone();
        a.field = Some(Field::PageRef(target));
        let fa = doc.attrs.intern(a);
        doc.blocks.update(i, |b| {
            if let Some(q) = b.as_para_mut() {
                q.text.truncate(at as usize);
                q.runs.retain(|r| r.0 < at);
                q.runs.push((at, fa));
                q.text.push(OBJECT_CHAR);
                q.normalize();
            }
        });
        doc.field_values.entry(Field::PageRef(target)).or_insert_with(|| num.as_str().into());
        n += 1;
    }
    n
}

/// One entry of a table of contents: heading level, text, heading id.
#[derive(Debug, Clone, PartialEq)]
pub struct TocEntry {
    pub level: u8,
    pub text: String,
    pub target: BlockId,
}

/// The headings a table of contents lists (levels 1..=`max_level`), in order,
/// leaving out its own entries and empty headings.
pub fn toc_entries(doc: &TextDoc, max_level: u8) -> Vec<TocEntry> {
    let mut v = Vec::new();
    for b in doc.blocks.iter() {
        let Some(p) = b.as_para() else { continue };
        if p.props.toc.is_some() || p.props.figure_list.is_some() || p.props.cell.is_some() {
            continue;
        }
        let Some(level) = doc.styles.para_format(p.style, &p.props).0.outline_level.filter(|l| *l <= max_level) else { continue };
        let text = doc.ref_text(p);
        if text.is_empty() {
            continue;
        }
        v.push(TocEntry { level, text, target: p.id });
        if v.len() == MAX_TOC_ENTRIES {
            break;
        }
    }
    v
}

/// The table of contents' blocks: `first..end`, the run of entries holding
/// block `near` or, if none does, the first run in the document.
pub fn toc_range(doc: &TextDoc, near: Option<usize>) -> Option<(usize, usize)> {
    index_range(doc, near, None)
}

pub fn index_range(doc: &TextDoc, near: Option<usize>, figures: Option<CaptionKind>) -> Option<(usize, usize)> {
    let is = |i: usize| doc.para(i).is_some_and(|p| match figures {
        Some(k) => p.props.figure_list == Some(k),
        None => p.props.toc.is_some(),
    });
    let n = doc.blocks.len();
    let start = match near.filter(|&i| is(i)) {
        Some(mut i) => {
            while i > 0 && is(i - 1) {
                i -= 1;
            }
            i
        }
        None => (0..n).find(|&i| is(i))?,
    };
    let mut end = start;
    while end < n && is(end) {
        end += 1;
    }
    Some((start, end))
}

/// The paragraph props of a level-`level` entry, for a column `width` twips wide.
pub fn entry_props(level: u8, width: i32) -> ParaProps {
    ParaProps {
        toc: Some(level),
        indent_start: Some(TOC_INDENT * (level as i32 - 1)),
        space_before: Some(if level == 1 { 120 } else { 0 }),
        space_after: Some(40),
        tabs: Some(vec![TabStop { pos: width.max(1), align: TabAlign::End, leader: Leader::Dots }]),
        // The heading's text ends before its page number.
        indent_end: Some(0),
        ..Default::default()
    }
}

/// The paragraphs of a table of contents for `entries`: heading text, a
/// tab, a page reference. `ids` gives each entry's block id (reused ids
/// where an update keeps them); `attr` is the text's attributes.
pub fn entry_blocks(doc: &mut TextDoc, entries: &[TocEntry], ids: &mut dyn FnMut(&mut TextDoc) -> BlockId, style: StyleId, attr: AttrId, width: i32) -> Vec<Block> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let mut fa: CharAttrs = doc.attrs.get(attr).clone();
        fa.field = Some(Field::PageRef(e.target));
        let fa = doc.attrs.intern(fa);
        let text = format!("{}\t{}", e.text, OBJECT_CHAR);
        let tab = e.text.len() as u32 + 1;
        let mut p = Para::new(ids(doc), &text, style);
        p.runs = vec![(0, attr), (tab, fa)];
        p.props = entry_props(e.level, width);
        out.push(Block::Para(p));
    }
    out
}

/// Ops that put a table of contents (headings up to `max_level`) before block
/// `at`, or, if the document has one, that bring it up to date. An empty list
/// means it is already current (or there are no headings to list).
pub fn toc_ops(doc: &mut TextDoc, at: usize, max_level: u8) -> Vec<TextOp> {
    let entries = toc_entries(doc, max_level);
    generated_ops(doc, at, &entries, None)
}

pub fn figure_list_ops(doc: &mut TextDoc, at: usize, kind: CaptionKind) -> Vec<TextOp> {
    let mut entries = Vec::new();
    for block in doc.blocks.iter() {
        let Some(p) = block.as_para() else { continue };
        if doc.caption_kinds(p)[kind as usize] {
            entries.push(TocEntry { level: 1, text: doc.caption_text(p), target: p.id });
            if entries.len() == MAX_TOC_ENTRIES { break; }
        }
    }
    generated_ops(doc, at, &entries, Some(kind))
}

fn generated_ops(doc: &mut TextDoc, at: usize, entries: &[TocEntry], figures: Option<CaptionKind>) -> Vec<TextOp> {
    let width = (doc.page.width - doc.page.margin_start - doc.page.margin_end).max(1440);
    let existing = index_range(doc, figures.map(|_| at), figures);
    let style = 0;
    let attr = doc.attrs.intern(CharAttrs::default());
    let blocks = |d: &mut TextDoc, ids: &mut dyn FnMut(&mut TextDoc) -> BlockId| {
        let mut blocks = entry_blocks(d, entries, ids, style, attr, width);
        if let Some(kind) = figures {
            for b in &mut blocks {
                if let Some(p) = b.as_para_mut() {
                    p.props.toc = None;
                    p.props.figure_list = Some(kind);
                }
            }
        }
        blocks
    };
    match existing {
        Some((first, end)) => {
            // Same entries, same props: nothing to do (page numbers are fields).
            let olds: Vec<&Para> = (first..end).filter_map(|i| doc.para(i)).collect();
            let same = olds.len() == entries.len()
                && olds.iter().zip(entries).all(|(p, e)| (figures.is_some() || p.props.toc == Some(e.level)) && doc.fields_in(p).first().map(|x| x.1) == Some(Field::PageRef(e.target)) && p.text.strip_suffix(&format!("\t{OBJECT_CHAR}")) == Some(e.text.as_str()));
            if same {
                return Vec::new();
            }
            let old_ids: Vec<Option<BlockId>> = if figures.is_some() {
                // A caption entry retains its id when another caption moves ahead of it.
                let mut by_target: HashMap<BlockId, BlockId> = olds.iter().filter_map(|p| {
                    doc.figure_list_target(p).map(|target| (target, p.id))
                }).collect();
                entries.iter().map(|e| by_target.remove(&e.target)).collect()
            } else {
                olds.iter().map(|p| Some(p.id)).collect()
            };
            let mut ids = old_ids.into_iter();
            let blocks = blocks(doc, &mut |d| ids.next().flatten().unwrap_or_else(|| d.new_id()));
            let mut ops = vec![TextOp::Remove { at: first as u32, n: (end - first) as u32 }];
            if !blocks.is_empty() {
                ops.push(TextOp::Insert { at: first as u32, blocks });
            }
            ops
        }
        None => {
            if entries.is_empty() {
                return Vec::new();
            }
            let blocks = blocks(doc, &mut |d| d.new_id());
            vec![TextOp::Insert { at: at.min(doc.blocks.len()) as u32, blocks }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FixedFonts, LayoutCache};
    use crate::paginate::Paginator;
    use officina_core::history::Op;

    /// `n` chapters, each a heading and `body` paragraphs of filler.
    fn book(n: usize, body: usize) -> TextDoc {
        let mut d = TextDoc::new();
        let h1 = d.styles.by_name("Heading 1").unwrap();
        let h2 = d.styles.by_name("Heading 2").unwrap();
        for c in 0..n {
            d.push_para(&format!("Chapter {}", c + 1), h1, vec![]);
            d.push_para(&format!("Part {}.1", c + 1), h2, vec![]);
            for _ in 0..body {
                d.push_para(&"words and more words ".repeat(30), 0, vec![]);
            }
        }
        d
    }

    fn settle(d: &mut TextDoc, p: &mut Paginator, c: &mut LayoutCache) -> u32 {
        let mut rounds = 0;
        loop {
            assert!(p.run(d, c, &FixedFonts, usize::MAX));
            if rounds == MAX_ROUNDS {
                return rounds;
            }
            rounds += 1;
            let ch = resolve(d, p.pages());
            let (Some(&a), Some(&b)) = (ch.first(), ch.last()) else { return rounds };
            p.edit(d, c, &FixedFonts, crate::model::Edit { first: a, last: b + 1, delta: 0 });
        }
    }

    fn run(d: &mut TextDoc, ops: Vec<TextOp>) {
        for op in ops {
            op.apply(d);
        }
    }

    fn shown(d: &TextDoc) -> Vec<String> {
        (0..d.blocks.len()).filter_map(|i| d.para(i)).filter(|p| p.props.toc.is_some()).map(|p| {
            let f = d.fields_in(p);
            format!("{} {}", p.text.split('\t').next().unwrap(), d.field_text(f[0].1))
        }).collect()
    }

    #[test]
    fn caption_ranks_follow_edits_across_leaves_and_invalidate_distant_pages() {
        let mut d = TextDoc::new();
        let figure = d.attrs.intern(CharAttrs { sequence: Some(CaptionKind::Figure), ..Default::default() });
        let table = d.attrs.intern(CharAttrs { sequence: Some(CaptionKind::Table), ..Default::default() });
        for i in 0..=crate::tree::LEAF + 2 {
            d.push_para(&format!("Item {i}"), 0, vec![]);
        }
        let last = d.push_para(&format!("{OBJECT_CHAR}/{OBJECT_CHAR}"), 0, vec![(0, figure), (3, 0), (4, table)]);
        let id = d.para(last).unwrap().id;
        assert_eq!(d.caption_number(id, 0), "1");
        assert_eq!(d.caption_number(id, 4), "1");
        resolve(&mut d, &[]);
        let mut first = Para::new(d.new_id(), &format!("{OBJECT_CHAR} {OBJECT_CHAR}"), 0);
        first.runs = vec![(0, figure)];
        d.blocks.insert(0, Block::Para(first));
        assert_eq!(d.caption_number(id, 0), "3");
        assert_eq!(d.caption_number(id, 4), "1");
        assert_eq!(resolve(&mut d, &[]), [0, last + 1]);
        let moved = d.blocks.remove(0).unwrap();
        d.blocks.insert(d.blocks.len(), moved);
        assert_eq!(d.caption_number(id, 0), "1");
        assert_eq!(d.caption_number(id, 4), "1");
        assert_eq!(d.caption_text(d.para(d.blocks.len() - 1).unwrap()), "2 3");
    }

    #[test]
    fn toc_lists_headings_with_their_pages() {
        let mut d = book(4, 6);
        let ops = toc_ops(&mut d, 0, 3);
        run(&mut d, ops);
        assert_eq!(toc_range(&d, None), Some((0, 8)));
        let (mut c, mut p) = (LayoutCache::default(), Paginator::new(&d));
        settle(&mut d, &mut p, &mut c);
        let pages: Vec<usize> = (0..d.blocks.len()).filter(|&i| d.para(i).unwrap().props.outline_level.is_some() || d.styles.para_format(d.para(i).unwrap().style, &d.para(i).unwrap().props).0.outline_level.is_some()).map(|i| p.page_of(i, 0) + 1).collect();
        let want: Vec<String> = toc_entries(&d, 3).iter().zip(&pages).map(|(e, n)| format!("{} {n}", e.text)).collect();
        assert_eq!(shown(&d), want);
        assert!(pages.windows(2).all(|w| w[0] <= w[1]) && pages.last() > Some(&2), "{pages:?}");
        // Up to date: no ops.
        assert!(toc_ops(&mut d, 0, 3).is_empty());
        // A new heading: the update lists it, keeping the old entries' ids.
        let ids: Vec<BlockId> = (0..8).map(|i| d.para(i).unwrap().id).collect();
        let h1 = d.styles.by_name("Heading 1").unwrap();
        d.push_para("Appendix", h1, vec![]);
        let ops = toc_ops(&mut d, 0, 3);
        run(&mut d, ops);
        assert_eq!(toc_range(&d, None), Some((0, 9)));
        assert_eq!((0..8).map(|i| d.para(i).unwrap().id).collect::<Vec<_>>(), ids);
        settle(&mut d, &mut p, &mut c);
        assert!(shown(&d).last().unwrap().starts_with("Appendix "));
    }

    #[test]
    fn references_follow_their_target() {
        let mut d = book(3, 5);
        let target = d.para(d.blocks.len() - 7).unwrap().id; // "Chapter 3"
        let mut a = d.attrs.get(0).clone();
        a.field = Some(Field::PageRef(target));
        let pa = d.attrs.intern(a.clone());
        a.field = Some(Field::TextRef(target));
        let ta = d.attrs.intern(a);
        let text = format!("See {OBJECT_CHAR} on page {OBJECT_CHAR}.");
        let mut p0 = Para::new(d.new_id(), &text, 0);
        p0.runs = vec![(0, 0), (4, ta), (7, 0), (16, pa), (19, 0)];
        run(&mut d, vec![TextOp::Insert { at: 0, blocks: vec![Block::Para(p0)] }]);
        assert_eq!(d.field_text(Field::TextRef(target)), "Chapter 3", "text shows before pagination");
        assert_eq!(d.field_text(Field::PageRef(target)), "?");
        let (mut c, mut p) = (LayoutCache::default(), Paginator::new(&d));
        settle(&mut d, &mut p, &mut c);
        let ti = d.index_of(target).unwrap();
        assert_eq!(d.field_text(Field::PageRef(target)), (p.page_of(ti, 0) + 1).to_string());
        // Retitled: shows the new text once resolved again.
        let n = d.para(ti).unwrap().text.len() as u32;
        run(&mut d, vec![TextOp::Splice { block: ti as u32, at: 0, del: n, ins: crate::ops::Fragment { text: "Results".into(), runs: vec![(0, 0)] } }]);
        p.edit(&d, &mut c, &FixedFonts, crate::model::Edit { first: ti, last: ti + 1, delta: 0 });
        assert_eq!(d.field_text(Field::TextRef(target)), "Chapter 3");
        settle(&mut d, &mut p, &mut c);
        assert_eq!(d.field_text(Field::TextRef(target)), "Results");
        // Target gone: "?".
        run(&mut d, vec![TextOp::Remove { at: ti as u32, n: 1 }]);
        p.edit(&d, &mut c, &FixedFonts, crate::model::Edit { first: ti, last: ti, delta: -1 });
        settle(&mut d, &mut p, &mut c);
        assert_eq!(d.field_text(Field::PageRef(target)), "?");
    }

    #[test]
    fn rounds_are_bounded() {
        // Whatever fields do to pagination, settling stops within MAX_ROUNDS.
        let mut d = book(30, 4);
        let ops = toc_ops(&mut d, 0, 2);
        run(&mut d, ops);
        let (mut c, mut p) = (LayoutCache::default(), Paginator::new(&d));
        assert!(settle(&mut d, &mut p, &mut c) <= MAX_ROUNDS);
    }
}
