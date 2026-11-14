//! The document model: blocks, paragraphs with attribute runs, styles.
//!
//! Character formatting is interned ([`AttrTable`]): a paragraph's runs name an
//! [`AttrId`], so a run is 8 bytes whatever its formatting and comparing two runs'
//! formatting is comparing two integers. Paragraph and character styles are named
//! and inherit from a parent; what a paragraph shows is its style chain, then its
//! direct properties, then each run's direct attributes.

use std::collections::HashMap;
use std::sync::Arc;

use crate::tree::BlockTree;

/// A block's stable id: it never changes while the block exists, so layout
/// caches, anchors, links and cross-references key on it.
pub type BlockId = u64;
pub type AttrId = u32;
pub type StyleId = u16;

/// Direct character formatting; `None` takes the style's value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct CharAttrs {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    /// In half points (Word's unit): 22 is 11 pt.
    pub size: Option<u16>,
    /// sRGB as 0xRRGGBB.
    pub color: Option<u32>,
    pub highlight: Option<u32>,
    pub family: Option<Arc<str>>,
    /// A named character style under the direct formatting.
    pub style: Option<StyleId>,
    pub link: Option<Arc<str>>,
    /// The character (U+FFFC) is a picture drawn in line with the text.
    pub object: Option<ObjectRef>,
}

/// The object replacement character: one inline picture in the text.
pub const OBJECT_CHAR: char = '\u{FFFC}';

/// An inline picture: an image of the document's [`Images`] and the frame it is
/// drawn in, in twips. Layout needs only the frame; the pixels are decoded when
/// a page showing it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectRef {
    pub image: u32,
    pub width: u32,
    pub height: u32,
}

/// Largest picture frame side, twips (22 in).
pub const MAX_OBJECT_TWIPS: u32 = 31680;

/// An image's encoded bytes (PNG, JPEG, GIF or WebP, as given) and its size in
/// pixels, as read from its header when it was added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    pub bytes: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
}

/// The pictures a document uses, each stored once however often it is placed.
/// Append-only while the document is open (undo keeps indices valid).
#[derive(Debug, Clone, Default)]
pub struct Images {
    list: Vec<Arc<ImageData>>,
    by_hash: HashMap<u64, Vec<u32>>,
}

impl Images {
    fn hash(b: &[u8]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        b.hash(&mut h);
        h.finish()
    }
    /// Adds an image (or finds the same bytes already here); returns its index.
    pub fn add(&mut self, img: ImageData) -> u32 {
        let h = Self::hash(&img.bytes);
        if let Some(&i) = self.by_hash.get(&h).and_then(|v| v.iter().find(|&&i| self.list[i as usize].bytes == img.bytes)) {
            return i;
        }
        let i = self.list.len() as u32;
        self.list.push(Arc::new(img));
        self.by_hash.entry(h).or_default().push(i);
        i
    }
    pub fn get(&self, i: u32) -> Option<&Arc<ImageData>> {
        self.list.get(i as usize)
    }
    pub fn len(&self) -> usize {
        self.list.len()
    }
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Arc<ImageData>> {
        self.list.iter()
    }
}

impl CharAttrs {
    /// `self` over `under`: each field this one sets wins.
    pub fn over(&self, under: &CharAttrs) -> CharAttrs {
        CharAttrs {
            bold: self.bold.or(under.bold),
            italic: self.italic.or(under.italic),
            underline: self.underline.or(under.underline),
            strike: self.strike.or(under.strike),
            size: self.size.or(under.size),
            color: self.color.or(under.color),
            highlight: self.highlight.or(under.highlight),
            family: self.family.clone().or_else(|| under.family.clone()),
            style: self.style.or(under.style),
            link: self.link.clone().or_else(|| under.link.clone()),
            object: self.object.or(under.object),
        }
    }
}

/// Character formatting with every field decided, as layout uses it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CharFormat {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Half points.
    pub size: u16,
    pub color: u32,
    pub highlight: Option<u32>,
    pub family: Arc<str>,
    pub link: Option<Arc<str>>,
    pub object: Option<ObjectRef>,
}

impl Default for CharFormat {
    fn default() -> Self {
        CharFormat { bold: false, italic: false, underline: false, strike: false, size: 22, color: 0x000000, highlight: None, family: "IBM Plex Sans".into(), link: None, object: None }
    }
}

impl CharFormat {
    fn apply(&mut self, a: &CharAttrs) {
        if let Some(v) = a.bold {
            self.bold = v;
        }
        if let Some(v) = a.italic {
            self.italic = v;
        }
        if let Some(v) = a.underline {
            self.underline = v;
        }
        if let Some(v) = a.strike {
            self.strike = v;
        }
        if let Some(v) = a.size {
            self.size = v.clamp(2, 3276);
        }
        if let Some(v) = a.color {
            self.color = v & 0xFF_FFFF;
        }
        if a.highlight.is_some() {
            self.highlight = a.highlight;
        }
        if a.object.is_some() {
            self.object = a.object;
        }
        if let Some(v) = &a.family {
            self.family = v.clone();
        }
        if a.link.is_some() {
            self.link = a.link.clone();
        }
    }
}

/// The interned character attributes; id 0 is "no direct formatting".
#[derive(Debug, Clone)]
pub struct AttrTable {
    list: Vec<CharAttrs>,
    ids: HashMap<CharAttrs, AttrId>,
}

impl Default for AttrTable {
    fn default() -> Self {
        let d = CharAttrs::default();
        AttrTable { ids: HashMap::from([(d.clone(), 0)]), list: vec![d] }
    }
}

impl AttrTable {
    pub fn intern(&mut self, a: CharAttrs) -> AttrId {
        if let Some(&id) = self.ids.get(&a) {
            return id;
        }
        let id = self.list.len() as AttrId;
        self.list.push(a.clone());
        self.ids.insert(a, id);
        id
    }
    pub fn get(&self, id: AttrId) -> &CharAttrs {
        self.list.get(id as usize).unwrap_or(&self.list[0])
    }
    pub fn len(&self) -> usize {
        self.list.len()
    }
    pub fn is_empty(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Justify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineSpacing {
    /// A multiple of the font's line height, in hundredths (100 = single).
    Multiple(u16),
    /// Exactly this, twips.
    Exact(i32),
    /// At least this, twips.
    AtLeast(i32),
}

/// Direct paragraph properties; `None` takes the style's value. Lengths in twips.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ParaProps {
    pub align: Option<Align>,
    pub indent_start: Option<i32>,
    pub indent_end: Option<i32>,
    /// The first line's indent relative to the start indent (negative hangs).
    pub first_line: Option<i32>,
    pub space_before: Option<i32>,
    pub space_after: Option<i32>,
    pub line_spacing: Option<LineSpacing>,
    pub keep_with_next: Option<bool>,
    pub keep_together: Option<bool>,
    pub page_break_before: Option<bool>,
    /// Widow and orphan control (two lines).
    pub widows: Option<bool>,
    /// A heading's level, 1 to 9.
    pub outline_level: Option<u8>,
    /// The list the paragraph is an item of; `Some(None)` takes it out of a
    /// list its style puts it in.
    pub list: Option<Option<ListRef>>,
}

/// How a list's items are marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListKind {
    /// • ◦ ▪ by level.
    Bullet,
    /// 1. a. i. by level, then again.
    Numbered,
}

/// A paragraph's place in a list: which list (items of one list number on
/// from each other, across paragraphs between), its kind, and its level (0–8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ListRef {
    pub id: u32,
    pub kind: ListKind,
    pub level: u8,
}

/// Deepest list level.
pub const MAX_LIST_LEVEL: u8 = 8;
/// A list level's indent step and its label's hang, twips.
pub const LIST_INDENT: i32 = 720;
pub const LIST_HANG: i32 = 360;

/// Paragraph properties with every field decided.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParaFormat {
    pub align: Align,
    pub indent_start: i32,
    pub indent_end: i32,
    pub first_line: i32,
    pub space_before: i32,
    pub space_after: i32,
    pub line_spacing: LineSpacing,
    pub keep_with_next: bool,
    pub keep_together: bool,
    pub page_break_before: bool,
    pub widows: bool,
    pub outline_level: Option<u8>,
    pub list: Option<ListRef>,
}

impl Default for ParaFormat {
    fn default() -> Self {
        ParaFormat { align: Align::Start, indent_start: 0, indent_end: 0, first_line: 0, space_before: 0, space_after: 160, line_spacing: LineSpacing::Multiple(115), keep_with_next: false, keep_together: false, page_break_before: false, widows: true, outline_level: None, list: None }
    }
}

impl ParaFormat {
    fn apply(&mut self, p: &ParaProps) {
        macro_rules! set {
            ($($f:ident),*) => { $( if let Some(v) = p.$f { self.$f = v; } )* };
        }
        set!(align, indent_start, indent_end, first_line, space_before, space_after, line_spacing, keep_with_next, keep_together, page_break_before, widows);
        if p.outline_level.is_some() {
            self.outline_level = p.outline_level.filter(|l| (1..=9).contains(l));
        }
        if let Some(l) = p.list {
            self.list = l.map(|mut l| {
                l.level = l.level.min(MAX_LIST_LEVEL);
                l
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StyleKind {
    Para,
    Char,
}

/// A named style.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub name: Arc<str>,
    pub kind: StyleKind,
    pub parent: Option<StyleId>,
    /// The style a new paragraph after one of this style takes (Enter after a
    /// heading gives body text).
    pub next: Option<StyleId>,
    pub para: ParaProps,
    pub chars: CharAttrs,
}

/// Inheritance chains at most this deep; a longer (or cyclic) chain stops here.
pub const MAX_STYLE_DEPTH: usize = 16;
pub const MAX_STYLES: usize = 4096;

/// The document's named styles. Style 0 is the default paragraph style ("Normal").
#[derive(Debug, Clone)]
pub struct StyleSheet {
    pub styles: Vec<Style>,
    pub default_char: CharFormat,
}

impl Default for StyleSheet {
    fn default() -> Self {
        let p = |name: &str, parent: Option<StyleId>, para: ParaProps, chars: CharAttrs| Style { name: name.into(), kind: StyleKind::Para, parent, next: Some(0), para, chars };
        let h = |lvl: u8, size: u16, before: i32| {
            p(&format!("Heading {lvl}"), Some(0), ParaProps { outline_level: Some(lvl), space_before: Some(before), space_after: Some(80), keep_with_next: Some(true), keep_together: Some(true), ..Default::default() }, CharAttrs { bold: Some(true), size: Some(size), ..Default::default() })
        };
        StyleSheet {
            styles: vec![
                p("Normal", None, ParaProps::default(), CharAttrs::default()),
                h(1, 32, 480),
                h(2, 26, 360),
                h(3, 24, 280),
                p("Title", Some(0), ParaProps { space_after: Some(240), ..Default::default() }, CharAttrs { size: Some(52), ..Default::default() }),
            ],
            default_char: CharFormat::default(),
        }
    }
}

impl StyleSheet {
    pub fn by_name(&self, name: &str) -> Option<StyleId> {
        self.styles.iter().position(|s| &*s.name == name).map(|i| i as StyleId)
    }

    /// The chain from the root down to `id` (at most [`MAX_STYLE_DEPTH`]).
    fn chain(&self, id: StyleId) -> Vec<&Style> {
        let mut v = Vec::new();
        let mut at = Some(id);
        while let Some(i) = at {
            let Some(s) = self.styles.get(i as usize) else { break };
            if v.len() >= MAX_STYLE_DEPTH {
                break;
            }
            v.push(s);
            at = s.parent;
        }
        v.reverse();
        v
    }

    /// A paragraph style's paragraph format and character format.
    pub fn para_format(&self, id: StyleId, direct: &ParaProps) -> (ParaFormat, CharFormat) {
        let mut pf = ParaFormat::default();
        let mut cf = self.default_char.clone();
        for s in self.chain(id) {
            pf.apply(&s.para);
            cf.apply(&s.chars);
        }
        pf.apply(direct);
        // A list item hangs its label at its level's indent, unless the
        // paragraph sets its own indents.
        if let Some(l) = pf.list {
            if direct.indent_start.is_none() {
                pf.indent_start = LIST_INDENT * (l.level as i32 + 1);
            }
            if direct.first_line.is_none() {
                pf.first_line = -LIST_HANG;
            }
        }
        (pf, cf)
    }

    /// What a run shows: its paragraph's character format, then its character
    /// style's chain, then its direct attributes.
    pub fn char_format(&self, para: &CharFormat, a: &CharAttrs) -> CharFormat {
        let mut cf = para.clone();
        if let Some(cs) = a.style {
            for s in self.chain(cs) {
                cf.apply(&s.chars);
            }
        }
        cf.apply(a);
        cf
    }
}

/// A paragraph: text, attribute runs over it, a style, direct properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Para {
    pub id: BlockId,
    pub text: String,
    /// (start byte, attributes), sorted, the first at 0, starts on char boundaries;
    /// adjacent runs differ. An empty paragraph has one run (its "typing" format).
    pub runs: Vec<(u32, AttrId)>,
    pub style: StyleId,
    pub props: ParaProps,
}

impl Para {
    pub fn new(id: BlockId, text: &str, style: StyleId) -> Para {
        Para { id, text: text.to_string(), runs: vec![(0, 0)], style, props: ParaProps::default() }
    }

    /// The attributes at byte `at` (the run it falls in; the end takes the last run).
    pub fn attr_at(&self, at: u32) -> AttrId {
        let i = self.runs.partition_point(|r| r.0 <= at).saturating_sub(1);
        self.runs[i].1
    }

    /// Runs as (start, end, attr).
    pub fn spans(&self) -> impl Iterator<Item = (u32, u32, AttrId)> + '_ {
        let n = self.text.len() as u32;
        self.runs.iter().enumerate().map(move |(i, &(s, a))| (s, self.runs.get(i + 1).map_or(n, |r| r.0), a))
    }

    /// Merges equal neighbours and drops empty runs (keeping one).
    pub fn normalize(&mut self) {
        let n = self.text.len() as u32;
        let mut v: Vec<(u32, AttrId)> = Vec::with_capacity(self.runs.len());
        for &(s, a) in &self.runs {
            let s = s.min(n);
            // A run starting at the end covers nothing.
            if s == n && !v.is_empty() {
                continue;
            }
            if let Some(last) = v.last_mut() {
                if last.0 == s {
                    // An empty run before this one: this one wins, unless at the end.
                    if s < n {
                        last.1 = a;
                    }
                    continue;
                }
                if last.1 == a {
                    continue;
                }
            }
            v.push((s, a));
        }
        if v.is_empty() {
            v.push((0, 0));
        }
        v[0].0 = 0;
        self.runs = v;
    }

    /// Characters (not bytes).
    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// A block of the document. Tables and images come with O3d.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Para(Para),
}

impl Block {
    pub fn id(&self) -> BlockId {
        match self {
            Block::Para(p) => p.id,
        }
    }
    pub fn as_para(&self) -> Option<&Para> {
        match self {
            Block::Para(p) => Some(p),
        }
    }
    pub fn as_para_mut(&mut self) -> Option<&mut Para> {
        match self {
            Block::Para(p) => Some(p),
        }
    }
}

/// Page geometry, twips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageSetup {
    pub width: i32,
    pub height: i32,
    pub margin_top: i32,
    pub margin_bottom: i32,
    pub margin_start: i32,
    pub margin_end: i32,
}

impl Default for PageSetup {
    /// A4 with 2.54 cm margins.
    fn default() -> Self {
        PageSetup { width: 11906, height: 16838, margin_top: 1440, margin_bottom: 1440, margin_start: 1440, margin_end: 1440 }
    }
}

/// A page header or footer: one line of text, where `{page}` and `{pages}`
/// stand for the page number and count, drawn in the margin, half an inch
/// from the page's edge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HeaderFooter {
    pub text: String,
    pub align: Align,
}

impl HeaderFooter {
    /// The text for page `page` (1-based) of `pages`.
    pub fn resolve(&self, page: usize, pages: &str) -> String {
        self.text.replace("{page}", &page.to_string()).replace("{pages}", pages)
    }
}

/// Distance of a header or footer's line from the page edge, twips.
pub const HEADER_FROM_EDGE: i32 = 720;

/// One text document.
#[derive(Debug, Clone)]
pub struct TextDoc {
    pub blocks: BlockTree,
    pub attrs: AttrTable,
    pub styles: StyleSheet,
    pub page: PageSetup,
    /// Text in the top and bottom margins of every page.
    pub header: Option<HeaderFooter>,
    pub footer: Option<HeaderFooter>,
    /// The pictures placed in the text.
    pub images: Images,
    next_id: BlockId,
    /// What ops changed since [`TextDoc::take_changes`] last ran.
    changes: Option<Edit>,
}

/// Which blocks an edit touched: in the new document, blocks `first..last`
/// are new or changed and every block from `last` on is block `i - delta` of
/// the old document, unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit {
    pub first: usize,
    pub last: usize,
    pub delta: isize,
}

impl Edit {
    /// This edit followed by `e` (given in the numbering `self` left), as one.
    pub fn then(self, e: Edit) -> Edit {
        let last1 = if self.last as isize >= e.last as isize - e.delta {
            (self.last as isize + e.delta) as usize
        } else if self.last <= e.first {
            self.last
        } else {
            e.last
        };
        Edit { first: self.first.min(e.first), last: last1.max(e.last), delta: self.delta + e.delta }
    }
}

impl Default for TextDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl TextDoc {
    /// One empty Normal paragraph.
    pub fn new() -> TextDoc {
        let mut d = TextDoc { blocks: BlockTree::default(), attrs: AttrTable::default(), styles: StyleSheet::default(), page: PageSetup::default(), header: None, footer: None, images: Images::default(), next_id: 1, changes: None };
        let id = d.new_id();
        d.blocks.insert(0, Block::Para(Para::new(id, "", 0)));
        d
    }

    /// Notes that an op changed blocks (ops call this).
    pub fn note(&mut self, e: Edit) {
        self.changes = Some(match self.changes {
            Some(c) => c.then(e),
            None => e,
        });
    }

    /// What changed since the last call, as one edit (for the paginator).
    pub fn take_changes(&mut self) -> Option<Edit> {
        self.changes.take()
    }

    /// A fresh block id.
    pub fn new_id(&mut self) -> BlockId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// The next id that will be handed out (for files and journals).
    pub fn id_watermark(&self) -> BlockId {
        self.next_id
    }
    pub fn raise_watermark(&mut self, to: BlockId) {
        self.next_id = self.next_id.max(to);
    }

    pub fn para(&self, i: usize) -> Option<&Para> {
        self.blocks.get(i).and_then(Block::as_para)
    }

    /// Appends a paragraph (building documents in code and in converters).
    pub fn push_para(&mut self, text: &str, style: StyleId, runs: Vec<(u32, AttrId)>) -> usize {
        let id = self.new_id();
        let mut p = Para::new(id, text, style);
        if !runs.is_empty() {
            p.runs = runs;
        }
        p.normalize();
        let i = self.blocks.len();
        self.blocks.insert(i, Block::Para(p));
        i
    }

    /// All the text, paragraphs joined by '\n' (tests, word count, export).
    pub fn plain_text(&self) -> String {
        let mut s = String::new();
        for (i, b) in self.blocks.iter().enumerate() {
            if i > 0 {
                s.push('\n');
            }
            if let Some(p) = b.as_para() {
                s.push_str(&p.text);
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_inherit_and_direct_wins() {
        let ss = StyleSheet::default();
        let h1 = ss.by_name("Heading 1").unwrap();
        let (pf, cf) = ss.para_format(h1, &ParaProps::default());
        assert_eq!(pf.outline_level, Some(1));
        assert!(pf.keep_with_next && cf.bold);
        assert_eq!(cf.size, 32);
        assert_eq!(pf.line_spacing, LineSpacing::Multiple(115), "from Normal");
        let (pf, _) = ss.para_format(h1, &ParaProps { align: Some(Align::Center), ..Default::default() });
        assert_eq!(pf.align, Align::Center);
        let run = ss.char_format(&cf, &CharAttrs { bold: Some(false), italic: Some(true), ..Default::default() });
        assert!(!run.bold && run.italic && run.size == 32);
    }

    #[test]
    fn cyclic_styles_stop() {
        let mut ss = StyleSheet::default();
        ss.styles[0].parent = Some(1);
        ss.styles[1].parent = Some(0);
        let _ = ss.para_format(1, &ParaProps::default());
    }

    #[test]
    fn runs_normalize() {
        let mut p = Para::new(1, "abcdef", 0);
        p.runs = vec![(0, 3), (2, 3), (2, 4), (4, 0), (6, 9)];
        p.normalize();
        assert_eq!(p.runs, vec![(0, 3), (2, 4), (4, 0)]);
        assert_eq!(p.attr_at(3), 4);
        assert_eq!(p.attr_at(6), 0);
        assert_eq!(p.spans().collect::<Vec<_>>(), vec![(0, 2, 3), (2, 4, 4), (4, 6, 0)]);
    }
}
