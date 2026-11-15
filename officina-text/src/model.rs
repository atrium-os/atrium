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
    /// Raised or lowered, smaller (superscript, subscript).
    pub vert: Option<VertAlign>,
    /// The character (U+FFFC) anchors this note of the document's [`Notes`],
    /// and shows the note's number.
    pub note: Option<NoteId>,
    /// The comments of the document's [`Comments`] this text is under, sorted.
    pub comments: Option<Arc<[CommentId]>>,
    /// Tracked: the text was inserted under this revision (not yet accepted).
    pub rev: Option<RevId>,
    /// Tracked: the character (U+FFFC) is a tombstone holding this deletion of
    /// the document's [`Deletions`]; it takes no room on the page.
    pub deleted: Option<DelId>,
    /// Tracked: the formatting was changed under a revision; `old` is what the
    /// text had (an earlier entry of the attribute table).
    pub fmt: Option<FmtChange>,
    /// Merge: the character (U+FFFC) is one end of this conflict of the
    /// document's [`Conflicts`] (the first marker starts it, the second ends
    /// it); it takes no room on the page.
    pub conflict: Option<ConflictId>,
    /// The character (U+FFFC) is a field: it shows a value worked out from
    /// the rest of the document (see [`TextDoc::field_text`]).
    pub field: Option<Field>,
}

/// A value shown in the text and worked out after pagination: a reference
/// to a paragraph (by its id), showing its page number or its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    PageRef(BlockId),
    TextRef(BlockId),
}

impl Field {
    pub fn target(self) -> BlockId {
        match self {
            Field::PageRef(b) | Field::TextRef(b) => b,
        }
    }
}

/// Longest text a text reference shows, in chars.
pub const MAX_REF_TEXT: usize = 120;

/// A formatting change: its revision and the attributes before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FmtChange {
    pub rev: RevId,
    pub old: AttrId,
}

/// A revision of the document's [`Revisions`].
pub type RevId = u32;
/// A merge conflict of the document's [`Conflicts`].
pub type ConflictId = u32;
/// A deletion of the document's [`Deletions`].
pub type DelId = u32;

/// Who changed the text and when: one author's tracked edits in one session (or
/// one change set a compare or merge made).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revision {
    pub author: Arc<str>,
    /// ISO 8601 UTC, when the session's first edit was made.
    pub date: Arc<str>,
    /// The change set it belongs to (revisions of one set are accepted together).
    pub set: u32,
    /// The set's name, if it has one ("Legal's pass").
    pub name: Arc<str>,
}

/// What a tracked deletion took out: pieces of text with their runs; every
/// piece after the first starts a paragraph (the one that followed the
/// deleted paragraph break), with its id, style and properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Deletion {
    pub rev: RevId,
    pub pieces: Vec<DelPiece>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DelPiece {
    /// For a piece after the first: the paragraph it began.
    pub para: Option<(BlockId, StyleId, ParaProps)>,
    pub text: String,
    /// Run starts relative to `text`.
    pub runs: Vec<(u32, AttrId)>,
}

impl Deletion {
    /// The deleted text, paragraph breaks as `\n`.
    pub fn text(&self) -> String {
        let mut s = String::new();
        for (i, p) in self.pieces.iter().enumerate() {
            if i > 0 {
                s.push('\n');
            }
            s.push_str(&p.text);
        }
        s
    }
}

/// Most revisions and deletions in a document; most bytes one deletion holds.
pub const MAX_REVISIONS: usize = 1 << 16;
pub const MAX_DELETIONS: usize = 1 << 20;
pub const MAX_DELETION_TEXT: usize = 64 << 20;

/// A store of entries by id, an id staying taken while the document is open.
#[derive(Debug, Clone)]
pub struct Store<T> {
    list: Vec<Option<Arc<T>>>,
    max: usize,
}

impl<T: Clone> Store<T> {
    pub fn new(max: usize) -> Self {
        Store { list: Vec::new(), max }
    }
    pub fn get(&self, id: u32) -> Option<&Arc<T>> {
        self.list.get(id as usize).and_then(|n| n.as_ref())
    }
    pub fn add(&mut self, c: T) -> Option<u32> {
        if self.list.len() >= self.max {
            return None;
        }
        self.list.push(Some(Arc::new(c)));
        Some((self.list.len() - 1) as u32)
    }
    /// Sets entry `id` (growing the store to it); the old one.
    pub fn set(&mut self, id: u32, c: Option<T>) -> Option<T> {
        let i = id as usize;
        if i >= self.max {
            return None;
        }
        if self.list.len() <= i {
            self.list.resize(i + 1, None);
        }
        std::mem::replace(&mut self.list[i], c.map(Arc::new)).map(|a| (*a).clone())
    }
    pub fn next_id(&self) -> u32 {
        self.list.len() as u32
    }
    pub fn is_empty(&self) -> bool {
        self.list.iter().all(|n| n.is_none())
    }
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Arc<T>)> {
        self.list.iter().enumerate().filter_map(|(i, n)| n.as_ref().map(|n| (i as u32, n)))
    }
}

pub type Revisions = Store<Revision>;
pub type Deletions = Store<Deletion>;
/// Merge conflicts: each holds the other copy's version of the text between
/// its two markers (as tracked text, under that copy's revisions), kept until
/// someone picks.
pub type Conflicts = Store<Deletion>;
pub const MAX_CONFLICTS: usize = 1 << 16;

/// A comment of the document's [`Comments`].
pub type CommentId = u32;

/// A comment: who wrote it and when, its text (a paragraph a line), and the
/// comment it answers, if it is a reply. A reply needn't cover text of its own:
/// it belongs where its first comment is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Comment {
    pub author: Arc<str>,
    pub initials: Arc<str>,
    /// ISO 8601, as the files have it ("" when unknown).
    pub date: Arc<str>,
    pub text: String,
    pub parent: Option<CommentId>,
    /// Resolved.
    pub done: bool,
}

/// Most comments in a document; longest comment text, bytes.
pub const MAX_COMMENTS: usize = 1 << 20;
pub const MAX_COMMENT_TEXT: usize = 64 << 10;

/// The document's comments by id. As with notes, an id stays taken while a
/// document is open; files keep the comments that are anchored (and their replies).
#[derive(Debug, Clone, Default)]
pub struct Comments {
    list: Vec<Option<Arc<Comment>>>,
}

impl Comments {
    pub fn get(&self, id: CommentId) -> Option<&Arc<Comment>> {
        self.list.get(id as usize).and_then(|n| n.as_ref())
    }
    pub fn add(&mut self, c: Comment) -> Option<CommentId> {
        if self.list.len() >= MAX_COMMENTS {
            return None;
        }
        self.list.push(Some(Arc::new(c)));
        Some((self.list.len() - 1) as CommentId)
    }
    /// Sets comment `id` (growing the store to it); the old one.
    pub fn set(&mut self, id: CommentId, c: Option<Comment>) -> Option<Comment> {
        let i = id as usize;
        if i >= MAX_COMMENTS {
            return None;
        }
        if self.list.len() <= i {
            self.list.resize(i + 1, None);
        }
        std::mem::replace(&mut self.list[i], c.map(Arc::new)).map(|a| (*a).clone())
    }
    /// The id the next [`Comments::add`] gives.
    pub fn next_id(&self) -> CommentId {
        self.list.len() as CommentId
    }
    pub fn is_empty(&self) -> bool {
        self.list.iter().all(|n| n.is_none())
    }
    pub fn iter(&self) -> impl Iterator<Item = (CommentId, &Arc<Comment>)> {
        self.list.iter().enumerate().filter_map(|(i, n)| n.as_ref().map(|n| (i as CommentId, n)))
    }
    /// The replies to `id`, oldest first.
    pub fn replies(&self, id: CommentId) -> Vec<CommentId> {
        self.iter().filter(|(_, c)| c.parent == Some(id)).map(|(i, _)| i).collect()
    }
}

/// `secs` since 1970 as ISO 8601 UTC ("2026-11-14T20:30:00Z").
pub fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Days to civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Superscript and subscript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VertAlign {
    #[default]
    Baseline,
    Super,
    Sub,
}

/// A note of the document's [`Notes`].
pub type NoteId = u32;

/// Footnotes go at the foot of the anchor's page; endnotes after the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NoteKind {
    #[default]
    Foot,
    End,
}

/// A note's text: paragraphs (with their own block ids, for the layout cache;
/// attributes from the document's table). Never empty.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub kind: NoteKind,
    pub paras: Vec<Para>,
}

/// Most paragraphs a note keeps (the rest are joined into the last).
pub const MAX_NOTE_PARAS: usize = 64;
/// Most notes in a document.
pub const MAX_NOTES: usize = 1 << 20;

/// The document's notes by id. An id stays taken once given (an anchor deleted
/// and brought back by undo finds its note), so the store only grows while a
/// document is open; files keep the notes that have anchors.
#[derive(Debug, Clone, Default)]
pub struct Notes {
    list: Vec<Option<Arc<Note>>>,
    /// (kind, number) by note id, in anchor order; rebuilt when anchors change.
    numbers: std::cell::RefCell<Option<Arc<HashMap<NoteId, (NoteKind, u32)>>>>,
}

impl Notes {
    pub fn get(&self, id: NoteId) -> Option<&Arc<Note>> {
        self.list.get(id as usize).and_then(|n| n.as_ref())
    }
    /// Adds a note; its id.
    pub fn add(&mut self, n: Note) -> Option<NoteId> {
        if self.list.len() >= MAX_NOTES {
            return None;
        }
        self.list.push(Some(Arc::new(n)));
        Some((self.list.len() - 1) as NoteId)
    }
    /// Sets note `id` (growing the store to it); the old note.
    pub fn set(&mut self, id: NoteId, n: Option<Note>) -> Option<Note> {
        let i = id as usize;
        if i >= MAX_NOTES {
            return None;
        }
        if self.list.len() <= i {
            self.list.resize(i + 1, None);
        }
        std::mem::replace(&mut self.list[i], n.map(Arc::new)).map(|a| (*a).clone())
    }
    pub fn len(&self) -> usize {
        self.list.len()
    }
    pub fn is_empty(&self) -> bool {
        self.list.iter().all(|n| n.is_none())
    }
    pub fn iter(&self) -> impl Iterator<Item = (NoteId, &Arc<Note>)> {
        self.list.iter().enumerate().filter_map(|(i, n)| n.as_ref().map(|n| (i as NoteId, n)))
    }
    /// Anchors moved, came or went: numbers are recounted when next asked.
    pub fn renumber(&self) {
        *self.numbers.borrow_mut() = None;
    }
}

/// A note number as shown: 1, 2, 3 for footnotes; i, ii, iii for endnotes.
pub fn note_label(kind: NoteKind, n: u32) -> String {
    match kind {
        NoteKind::Foot => n.to_string(),
        NoteKind::End => crate::lists::roman(n).to_lowercase(),
    }
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
    /// Floating: placed by position near its anchor (this character) rather
    /// than in the line, the text wrapping around it as `float.wrap` says.
    pub float: Option<Float>,
}

/// How text goes around a floating picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Wrap {
    /// Beside it, on the side with more room (Word's square).
    #[default]
    Square,
    /// Above and below it, nothing beside.
    TopBottom,
    /// Over the text, which runs on as if it weren't there; drawn behind it.
    Behind,
    /// Over the text, drawn in front of it.
    InFront,
}

/// What a floating picture's position is measured from: the text (its
/// column across, its paragraph down), the margins or the page's edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Rel {
    #[default]
    Text,
    Margin,
    Page,
}

/// Across: at the start, centre or end of what it's measured from, or `x`
/// from its start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HAlign {
    Start,
    Center,
    End,
    #[default]
    Offset,
}

/// A floating picture's place and wrap (twips).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Float {
    pub wrap: Wrap,
    pub h_rel: Rel,
    pub h_align: HAlign,
    pub x: i32,
    /// Down from the top of its paragraph, the top margin or the page.
    pub v_rel: Rel,
    pub y: i32,
    /// Room kept between it and the text.
    pub gap: i32,
}

impl Default for Float {
    fn default() -> Self {
        Float { wrap: Wrap::Square, h_rel: Rel::Text, h_align: HAlign::End, x: 0, v_rel: Rel::Text, y: 0, gap: DEFAULT_FLOAT_GAP }
    }
}

/// Room between a floating picture and the text by default (an eighth of an inch).
pub const DEFAULT_FLOAT_GAP: i32 = 180;
/// Largest offset of a floating picture from what it's measured from (22 in).
pub const MAX_FLOAT_OFFSET: i32 = 31680;

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
            vert: self.vert.or(under.vert),
            note: self.note.or(under.note),
            comments: self.comments.clone().or_else(|| under.comments.clone()),
            rev: self.rev.or(under.rev),
            deleted: self.deleted.or(under.deleted),
            fmt: self.fmt.or(under.fmt),
            conflict: self.conflict.or(under.conflict),
            field: self.field.or(under.field),
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
    pub vert: VertAlign,
    pub note: Option<NoteId>,
    pub comments: Option<Arc<[CommentId]>>,
    /// Tracked changes over the text (see [`CharAttrs`]).
    pub rev: Option<RevId>,
    pub deleted: Option<DelId>,
    pub fmt: Option<FmtChange>,
    pub conflict: Option<ConflictId>,
    pub field: Option<Field>,
}

impl Default for CharFormat {
    fn default() -> Self {
        CharFormat { bold: false, italic: false, underline: false, strike: false, size: 22, color: 0x000000, highlight: None, family: "IBM Plex Sans".into(), link: None, object: None, vert: VertAlign::Baseline, note: None, comments: None, rev: None, deleted: None, fmt: None, conflict: None, field: None }
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
        if let Some(v) = a.vert {
            self.vert = v;
        }
        if a.note.is_some() {
            self.note = a.note;
        }
        if a.comments.is_some() {
            self.comments = a.comments.clone();
        }
        if a.rev.is_some() {
            self.rev = a.rev;
        }
        if a.deleted.is_some() {
            self.deleted = a.deleted;
        }
        if a.fmt.is_some() {
            self.fmt = a.fmt;
        }
        if a.conflict.is_some() {
            self.conflict = a.conflict;
        }
        if a.field.is_some() {
            self.field = a.field;
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
    /// The table cell the paragraph is in (never from a style).
    pub cell: Option<CellRef>,
    /// Tracked: the paragraph break before this paragraph was inserted under
    /// this revision (never from a style).
    pub rev: Option<RevId>,
    /// Tab stops, in order (they replace the style's); positions from the
    /// column's start edge, twips. Past the last, a stop every half inch.
    pub tabs: Option<Vec<TabStop>>,
    /// An entry of a generated table of contents, for a heading of this level
    /// (Update Table of Contents replaces the run of them).
    pub toc: Option<u8>,
    /// The paragraph starts a section with these settings (never from a
    /// style). On the first paragraph it sets the first section's columns.
    pub section: Option<Section>,
}

/// How a section begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SectionStart {
    #[default]
    NextPage,
    /// On the same page, under the section before (whose columns are balanced).
    Continuous,
    /// On the next odd (or even) numbered page, leaving one blank if needed.
    OddPage,
    EvenPage,
}

/// A section's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Section {
    pub start: SectionStart,
    /// 1 to [`MAX_COLUMNS`] columns of equal width.
    pub columns: u8,
    /// Between columns, twips.
    pub gap: i32,
    /// A line between columns.
    pub rule: bool,
    /// The section's own page size and margins; `None`: the document's.
    /// A section with a page of its own starts on a new page.
    pub page: Option<PageSetup>,
}

impl Default for Section {
    fn default() -> Self {
        Section { start: SectionStart::NextPage, columns: 1, gap: 720, rule: false, page: None }
    }
}

pub const MAX_COLUMNS: u8 = 16;

impl Section {
    /// Columns and gap as laid out: at least one, and each column at least
    /// a quarter inch wide in `width` (twips).
    pub fn fitted(&self, width: i32) -> (u8, i32) {
        let gap = self.gap.clamp(0, 2880);
        let mut n = self.columns.clamp(1, MAX_COLUMNS);
        while n > 1 && (width - gap * (n as i32 - 1)) / (n as i32) < 360 {
            n -= 1;
        }
        (n, if n == 1 { 0 } else { gap })
    }
}

/// A tab stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabStop {
    /// From the column's start edge, twips.
    pub pos: i32,
    pub align: TabAlign,
    pub leader: Leader,
}

/// Where text after a tab goes against its stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabAlign {
    /// Starts at the stop.
    Start,
    Center,
    /// Ends at the stop.
    End,
    /// The decimal point (or the end, without one) at the stop.
    Decimal,
}

/// What fills the space a tab takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Leader {
    None,
    Dots,
    Dashes,
    Line,
}

/// Default tab stops: every half inch, twips.
pub const DEFAULT_TAB: i32 = 720;
/// Most tab stops a paragraph keeps.
pub const MAX_TABS: usize = 64;

/// A paragraph's place in a table. Consecutive paragraphs with the same
/// `table` and `row` are a row; within it, consecutive ones with the same
/// `col` are a cell, laid out side by side in the order they come. `row` and
/// `col` only tell neighbours apart: they needn't count from anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellRef {
    pub table: u32,
    pub row: u32,
    pub col: u16,
    /// The cell's width, twips (scaled down when a row is wider than the column).
    pub width: u32,
}

/// Space inside a cell's border, twips.
pub const CELL_PAD: i32 = 80;
/// Most cells in a row, and most paragraphs a row is looked through for.
pub const MAX_CELLS: usize = 63;
pub const MAX_ROW_BLOCKS: usize = 4096;

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
    pub tabs: Vec<TabStop>,
}

impl Default for ParaFormat {
    fn default() -> Self {
        ParaFormat { align: Align::Start, indent_start: 0, indent_end: 0, first_line: 0, space_before: 0, space_after: 160, line_spacing: LineSpacing::Multiple(115), keep_with_next: false, keep_together: false, page_break_before: false, widows: true, outline_level: None, list: None, tabs: Vec::new() }
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
        if let Some(t) = &p.tabs {
            self.tabs = t.iter().take(MAX_TABS).copied().collect();
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

/// A page header or footer, drawn in the margin half an inch from the
/// page's edge: one line of text (`text`, `align`), or its own paragraphs
/// (`paras`: formatted, several lines, pictures in line), `text` then
/// holding their words. In either, `{page}` and `{pages}` stand for the
/// page number and count.
#[derive(Debug, Clone, PartialEq)]
pub struct HeaderFooter {
    pub text: String,
    pub align: Align,
    pub paras: Vec<Para>,
}

/// Most paragraphs a header or footer keeps.
pub const MAX_MARGIN_PARAS: usize = 64;

impl HeaderFooter {
    /// One line of text.
    pub fn line(text: &str, align: Align) -> HeaderFooter {
        HeaderFooter { text: text.into(), align, paras: Vec::new() }
    }

    /// The text for page `page` (1-based) of `pages`.
    pub fn resolve(&self, page: usize, pages: &str) -> String {
        self.text.replace("{page}", &page.to_string()).replace("{pages}", pages)
    }
}

/// A header's or footer's paragraphs with each table row made one line, its
/// cells apart by tabs (headers lay out no tables; a cell a paragraph would
/// stack a three-column row three lines high).
pub fn rows_as_lines(paras: Vec<Para>) -> Vec<Para> {
    let mut out: Vec<Para> = Vec::with_capacity(paras.len());
    let mut last: Option<CellRef> = None;
    for mut p in paras {
        let cell = p.props.cell.take();
        match (cell, last, out.last_mut()) {
            (Some(c), Some(l), Some(q)) if (c.table, c.row) == (l.table, l.row) => {
                let gap = if c.col != l.col { '\t' } else { ' ' };
                if !p.text.is_empty() {
                    let base = q.text.len() as u32 + 1;
                    let a = q.runs.last().map_or(0, |r| r.1);
                    q.text.push(gap);
                    q.runs.push((base - 1, a));
                    q.text.push_str(&p.text);
                    q.runs.extend(p.runs.iter().map(|&(at, a)| (at + base, a)));
                    q.normalize();
                }
            }
            _ => out.push(p),
        }
        last = cell;
    }
    out
}

/// `p` with `{page}` and `{pages}` filled in, its runs moved to match.
pub fn resolve_para<'a>(p: &'a Para, page: usize, pages: &str) -> std::borrow::Cow<'a, Para> {
    if !p.text.contains("{page") {
        return std::borrow::Cow::Borrowed(p);
    }
    let page = page.to_string();
    let mut q = p.clone();
    q.text.clear();
    // Each byte of the old text and where it lands.
    let mut map = Vec::with_capacity(p.text.len() + 1);
    let mut i = 0;
    while i < p.text.len() {
        let rest = &p.text[i..];
        let (key, val) = if rest.starts_with("{pages}") {
            ("{pages}", pages)
        } else if rest.starts_with("{page}") {
            ("{page}", page.as_str())
        } else {
            let n = rest.chars().next().map_or(1, char::len_utf8);
            for _ in 0..n {
                map.push(q.text.len() as u32);
            }
            q.text.push_str(&rest[..n]);
            i += n;
            continue;
        };
        for _ in 0..key.len() {
            map.push(q.text.len() as u32);
        }
        q.text.push_str(val);
        i += key.len();
    }
    map.push(q.text.len() as u32);
    for r in q.runs.iter_mut() {
        r.0 = map.get(r.0 as usize).copied().unwrap_or(q.text.len() as u32);
    }
    q.runs.dedup_by_key(|r| r.0);
    std::borrow::Cow::Owned(q)
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
    /// Footnotes and endnotes, anchored in the text.
    pub notes: Notes,
    /// Comments on ranges of the text.
    pub comments: Comments,
    /// Tracked changes: who made them, and what deletions took out.
    pub revisions: Revisions,
    pub deletions: Deletions,
    /// Merge conflicts not yet settled.
    pub conflicts: Conflicts,
    /// Edits are tracked (a setting the file keeps).
    pub tracking: bool,
    /// What each field shows, as last worked out (derived: not saved, not
    /// part of undo); see [`crate::fields`].
    pub field_values: HashMap<Field, Arc<str>>,
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
        let mut d = TextDoc { blocks: BlockTree::default(), attrs: AttrTable::default(), styles: StyleSheet::default(), page: PageSetup::default(), header: None, footer: None, images: Images::default(), notes: Notes::default(), comments: Comments::default(), revisions: Store::new(MAX_REVISIONS), deletions: Store::new(MAX_DELETIONS), conflicts: Store::new(MAX_CONFLICTS), tracking: false, field_values: HashMap::new(), next_id: 1, changes: None };
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

    /// The cell block `i` is in.
    pub fn cell(&self, i: usize) -> Option<CellRef> {
        self.para(i)?.props.cell
    }

    /// Whether blocks `a` and `b` are in the same cell (or both outside tables).
    pub fn same_cell(&self, a: usize, b: usize) -> bool {
        match (self.cell(a), self.cell(b)) {
            (None, None) => true,
            (Some(x), Some(y)) => (x.table, x.row, x.col) == (y.table, y.row, y.col),
            _ => false,
        }
    }

    /// The blocks of the row holding block `i` (empty outside tables).
    pub fn row_range(&self, i: usize) -> std::ops::Range<usize> {
        let Some(c) = self.cell(i) else { return i..i };
        let same = |k: usize| self.cell(k).is_some_and(|d| (d.table, d.row) == (c.table, c.row));
        let mut a = i;
        while a > 0 && i - a < MAX_ROW_BLOCKS && same(a - 1) {
            a -= 1;
        }
        let mut b = i + 1;
        while b - a < MAX_ROW_BLOCKS && same(b) {
            b += 1;
        }
        a..b
    }

    /// The cells of row `row`: their blocks, x and width in layout units, in
    /// a column `col_w` wide (widths scaled down to fit it).
    pub fn row_cells(&self, row: std::ops::Range<usize>, col_w: i64) -> Vec<(std::ops::Range<usize>, i64, i64)> {
        let mut cells: Vec<(std::ops::Range<usize>, i64)> = Vec::new();
        for k in row {
            let w = self.cell(k).map_or(1, |c| c.width.clamp(1, 31680)) as i64;
            let n = cells.len();
            match cells.last_mut() {
                // Past the most cells a row has, the rest join the last.
                Some((r, _)) if n >= MAX_CELLS || self.same_cell(r.start, k) => r.end = k + 1,
                _ => cells.push((k..k + 1, crate::twips(w))),
            }
        }
        let total: i64 = cells.iter().map(|c| c.1).sum::<i64>().max(1);
        let scale = |w: i64| if total > col_w { (w * col_w / total).max(crate::LU) } else { w };
        let mut x = 0;
        cells
            .into_iter()
            .map(|(r, w)| {
                let w = scale(w);
                let c = (r, x, w);
                x += w;
                c
            })
            .collect()
    }

    /// Where block `i` sits across a column `col_w` wide: its x and the width
    /// its text is laid out in (a cell's, inside the padding, or the column).
    pub fn block_box(&self, i: usize, col_w: i64) -> (i64, i64) {
        if self.cell(i).is_none() {
            return (0, col_w);
        }
        let pad = crate::twips(CELL_PAD as i64);
        self.row_cells(self.row_range(i), col_w)
            .into_iter()
            .find(|c| c.0.contains(&i))
            .map_or((0, col_w), |(_, x, w)| (x + pad, (w - 2 * pad).max(crate::LU)))
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

    /// (kind, number) of each anchored note, footnotes and endnotes each
    /// counted from 1 in the order of their anchors.
    pub fn note_numbers(&self) -> Arc<HashMap<NoteId, (NoteKind, u32)>> {
        if let Some(m) = self.notes.numbers.borrow().as_ref() {
            return m.clone();
        }
        let mut m = HashMap::new();
        let (mut foot, mut end) = (0u32, 0u32);
        if !self.notes.is_empty() {
            for b in self.blocks.iter() {
                let Some(p) = b.as_para() else { continue };
                if !p.text.contains(OBJECT_CHAR) {
                    continue;
                }
                for (at, ch) in p.text.char_indices() {
                    if ch != OBJECT_CHAR {
                        continue;
                    }
                    if let Some(id) = self.attrs.get(p.attr_at(at as u32)).note {
                        if m.contains_key(&id) {
                            continue;
                        }
                        let Some(n) = self.notes.get(id) else { continue };
                        let k = match n.kind {
                            NoteKind::Foot => {
                                foot += 1;
                                foot
                            }
                            NoteKind::End => {
                                end += 1;
                                end
                            }
                        };
                        m.insert(id, (n.kind, k));
                    }
                }
            }
        }
        let m = Arc::new(m);
        *self.notes.numbers.borrow_mut() = Some(m.clone());
        m
    }

    /// The label note `id`'s anchor shows ("" for a note with no anchor).
    pub fn note_label(&self, id: NoteId) -> String {
        self.note_numbers().get(&id).map(|&(k, n)| note_label(k, n)).unwrap_or_default()
    }

    /// Where note `id` is anchored: (block, byte).
    pub fn anchor_of(&self, id: NoteId) -> Option<(usize, u32)> {
        for (i, b) in self.blocks.iter().enumerate() {
            let Some(p) = b.as_para() else { continue };
            if !p.text.contains(OBJECT_CHAR) {
                continue;
            }
            for (at, ch) in p.text.char_indices() {
                if ch == OBJECT_CHAR && self.attrs.get(p.attr_at(at as u32)).note == Some(id) {
                    return Some((i, at as u32));
                }
            }
        }
        None
    }

    /// Each anchored comment's first and last character: (block, byte) of its
    /// start and (block, byte) of its end, in document order of starts.
    pub fn comment_ranges(&self) -> Vec<(CommentId, (usize, u32), (usize, u32))> {
        let mut m: HashMap<CommentId, ((usize, u32), (usize, u32))> = HashMap::new();
        let mut order = Vec::new();
        if self.comments.is_empty() {
            return Vec::new();
        }
        for (i, b) in self.blocks.iter().enumerate() {
            let Some(p) = b.as_para() else { continue };
            for (s, e, a) in p.spans() {
                if s == e {
                    continue;
                }
                let Some(ids) = &self.attrs.get(a).comments else { continue };
                for &id in ids.iter() {
                    match m.get_mut(&id) {
                        Some(r) => r.1 = (i, e),
                        None => {
                            if self.comments.get(id).is_some() {
                                m.insert(id, ((i, s), (i, e)));
                                order.push(id);
                            }
                        }
                    }
                }
            }
        }
        order.into_iter().map(|id| (id, m[&id].0, m[&id].1)).collect()
    }

    /// The comments a file keeps: the anchored ones and replies to them, in
    /// document order (a reply after the comment it answers).
    pub fn live_comments(&self) -> Vec<CommentId> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (id, _, _) in self.comment_ranges() {
            let mut stack = vec![id];
            while let Some(c) = stack.pop() {
                if seen.insert(c) {
                    out.push(c);
                    let mut r = self.comments.replies(c);
                    r.reverse();
                    stack.extend(r);
                }
            }
        }
        out
    }

    /// The anchored endnotes in order.
    pub fn endnotes(&self) -> Vec<NoteId> {
        let m = self.note_numbers();
        let mut v: Vec<(u32, NoteId)> = m.iter().filter(|(_, &(k, _))| k == NoteKind::End).map(|(&id, &(_, n))| (n, id)).collect();
        v.sort_unstable();
        v.into_iter().map(|x| x.1).collect()
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
