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
    /// The language of East Asian text (BCP 47, "ja-JP", "zh-TW"): which
    /// forms a Han character takes (Word's `w:lang w:eastAsia`).
    pub lang_ea: Option<Arc<str>>,
    /// Capitals or small capitals, whatever was typed.
    pub caps: Option<Caps>,
    /// Hidden text: shown only while hidden text is shown, never printed.
    pub hidden: Option<bool>,
    /// Struck through with two lines.
    pub dstrike: Option<bool>,
    /// Extra room after each letter, twips (negative: condensed), within
    /// [`MAX_SPACING`].
    pub spacing: Option<i16>,
    /// Letters' width, percent of their own (100: as designed), within
    /// [`SCALE_RANGE`].
    pub scale: Option<u16>,
    /// Text effects, a mask of [`EFFECT_OUTLINE`], [`EFFECT_SHADOW`],
    /// [`EFFECT_EMBOSS`] and [`EFFECT_ENGRAVE`].
    pub effects: Option<u8>,
}

/// Most letter spacing either way, twips (Word's 1,584: 79.2 pt).
pub const MAX_SPACING: i16 = 1584;
/// Letter widths allowed, percent (Word's 1 to 600).
pub const SCALE_RANGE: std::ops::RangeInclusive<u16> = 1..=600;
pub const EFFECT_OUTLINE: u8 = 1;
pub const EFFECT_SHADOW: u8 = 2;
pub const EFFECT_EMBOSS: u8 = 4;
pub const EFFECT_ENGRAVE: u8 = 8;
/// Every effect bit.
pub const EFFECTS: u8 = 15;

/// How letters are cased when drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Caps {
    /// As typed.
    #[default]
    Normal,
    /// Every letter a capital.
    All,
    /// Lower-case letters as smaller capitals.
    Small,
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
    /// A drawn shape: `image` is its index in [`TextDoc::shapes`].
    pub shape: bool,
}

pub use officina_shapes::{Geom, Path as ShapePath, PathCmd};

/// Whether `name` is a preset outline we draw.
pub fn is_preset(name: &str) -> bool {
    officina_shapes::presets::preset(name).is_some()
}

/// How a shape's outline is dashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dot,
    Dash,
    DashDot,
    LongDash,
}

/// A shape's outline: its colour (RGB), width in twips (0: a hairline) and
/// dashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShapeLine {
    pub color: u32,
    pub width: i32,
    pub dash: Dash,
    /// Ends of an open outline: at its start and at its end.
    pub head: ArrowEnd,
    pub tail: ArrowEnd,
}

/// What ends an open line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ArrowEnd {
    #[default]
    None,
    Triangle,
    Open,
    Stealth,
    Diamond,
    Oval,
}

/// Where a shape's text sits when it is shorter than the shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TextAnchor {
    #[default]
    Top,
    Middle,
    Bottom,
}

/// A drawn shape (a rectangle, an arrow, a callout, a text box): its outline
/// in its frame, filled and outlined, turned and flipped about its middle,
/// with paragraphs of its own inside. Its frame and place are the
/// [`ObjectRef`]'s, as a picture's are.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeData {
    pub geom: Geom,
    /// The fill (RGB); none shows what is behind.
    pub fill: Option<u32>,
    pub line: Option<ShapeLine>,
    /// Clockwise, in 60,000ths of a degree.
    pub rot: i32,
    pub flip_h: bool,
    pub flip_v: bool,
    /// Its text, laid out inside the frame less `inset`.
    pub paras: Vec<Para>,
    /// Room between the frame and the text: start, top, end, bottom (twips).
    pub inset: [i32; 4],
    pub anchor: TextAnchor,
    /// A group's shapes, drawn in order over its own outline.
    pub kids: Vec<ShapeKid>,
}

/// A shape in a group: its frame in the group's (hundred-thousandths of
/// the group's width and height from its top-left corner; it may reach
/// outside).
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeKid {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub shape: ShapeData,
}

/// Groups in groups, at most this deep; shapes in one shape's groups, at
/// most this many.
pub const MAX_GROUP_DEPTH: usize = 8;
pub const MAX_GROUP_SHAPES: usize = 1024;

/// Word's text box insets: a tenth of an inch across, a twentieth down.
pub const DEFAULT_INSET: [i32; 4] = [144, 72, 144, 72];
/// Most shapes a document holds, and paragraphs one shape holds.
pub const MAX_SHAPES: usize = 1 << 16;
pub const MAX_SHAPE_PARAS: usize = 256;

impl ShapeData {
    /// A shape of `geom` with no fill, outline or text.
    pub fn new(geom: Geom) -> ShapeData {
        ShapeData { geom, fill: None, line: None, rot: 0, flip_h: false, flip_v: false, paras: Vec::new(), inset: DEFAULT_INSET, anchor: TextAnchor::Top, kids: Vec::new() }
    }

    /// Its paragraphs and its group's shapes', in order.
    pub fn all_paras(&self) -> Vec<&Para> {
        let mut v: Vec<&Para> = self.paras.iter().collect();
        for k in &self.kids {
            v.extend(k.shape.all_paras());
        }
        v
    }

    /// Within the limits: groups at most [`MAX_GROUP_DEPTH`] deep and
    /// [`MAX_GROUP_SHAPES`] in all, each shape's paragraphs at most
    /// [`MAX_SHAPE_PARAS`].
    pub fn fits(&self) -> bool {
        fn walk(s: &ShapeData, depth: usize, n: &mut usize) -> bool {
            *n += 1;
            depth <= MAX_GROUP_DEPTH && *n <= MAX_GROUP_SHAPES && s.paras.len() <= MAX_SHAPE_PARAS && s.kids.iter().all(|k| walk(&k.shape, depth + 1, n))
        }
        walk(self, 0, &mut 0)
    }

    /// The shape down `path` of group indices (itself for an empty path).
    pub fn kid_mut(&mut self, path: &[usize]) -> Option<&mut ShapeData> {
        match path.split_first() {
            None => Some(self),
            Some((&i, rest)) => self.kids.get_mut(i)?.shape.kid_mut(rest),
        }
    }
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
            lang_ea: self.lang_ea.clone().or_else(|| under.lang_ea.clone()),
            caps: self.caps.or(under.caps),
            hidden: self.hidden.or(under.hidden),
            dstrike: self.dstrike.or(under.dstrike),
            spacing: self.spacing.or(under.spacing),
            scale: self.scale.or(under.scale),
            effects: self.effects.or(under.effects),
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
    /// See [`CharAttrs::lang_ea`].
    pub lang_ea: Option<Arc<str>>,
    pub caps: Caps,
    pub hidden: bool,
    pub dstrike: bool,
    /// Twips after each letter.
    pub spacing: i16,
    /// Percent.
    pub scale: u16,
    pub effects: u8,
}

impl Default for CharFormat {
    fn default() -> Self {
        CharFormat { bold: false, italic: false, underline: false, strike: false, size: 22, color: 0x000000, highlight: None, family: "IBM Plex Sans".into(), link: None, object: None, vert: VertAlign::Baseline, note: None, comments: None, rev: None, deleted: None, fmt: None, conflict: None, field: None, lang_ea: None, caps: Caps::Normal, hidden: false, dstrike: false, spacing: 0, scale: 100, effects: 0 }
    }
}

impl CharFormat {
    fn apply(&mut self, a: &CharAttrs) {
        if a.lang_ea.is_some() {
            self.lang_ea = a.lang_ea.clone();
        }
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
        if let Some(v) = a.caps {
            self.caps = v;
        }
        if let Some(v) = a.hidden {
            self.hidden = v;
        }
        if let Some(v) = a.dstrike {
            self.dstrike = v;
        }
        if let Some(v) = a.spacing {
            self.spacing = v.clamp(-MAX_SPACING, MAX_SPACING);
        }
        if let Some(v) = a.scale {
            self.scale = v.clamp(*SCALE_RANGE.start(), *SCALE_RANGE.end());
        }
        if let Some(v) = a.effects {
            self.effects = v & EFFECTS;
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

    /// Entries whose character style is `id`.
    pub fn with_style(&self, id: StyleId) -> Vec<AttrId> {
        (0..self.list.len()).filter(|&i| self.list[i].style == Some(id)).map(|i| i as AttrId).collect()
    }

    /// Each entry's character style through `f`, and entries `set` to
    /// style `to` after; the lookup is rebuilt (the first of equal entries
    /// wins).
    pub fn remap_styles(&mut self, f: &dyn Fn(StyleId) -> StyleId, set: &[AttrId], to: Option<StyleId>) {
        for a in &mut self.list {
            a.style = a.style.map(f);
        }
        for &i in set {
            if let Some(a) = self.list.get_mut(i as usize) {
                a.style = to;
            }
        }
        self.ids.clear();
        for (i, a) in self.list.iter().enumerate() {
            self.ids.entry(a.clone()).or_insert(i as AttrId);
        }
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

/// A border line's look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BorderStyle {
    #[default]
    Single,
    Double,
    Dotted,
    Dashed,
    /// A single line drawn thick (Word's thick and the heavier styles).
    Thick,
}

/// One side's border: its look, width (eighths of a point), colour (sRGB;
/// `None` is automatic, black) and the room between it and the text
/// (points).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Border {
    pub style: BorderStyle,
    pub width: u16,
    pub color: Option<u32>,
    pub space: u16,
}

impl Default for Border {
    fn default() -> Self {
        Border { style: BorderStyle::Single, width: 4, color: None, space: 1 }
    }
}

/// Widest border, eighths of a point (12 pt), and the most room beside one (31 pt).
pub const MAX_BORDER_WIDTH: u16 = 96;
pub const MAX_BORDER_SPACE: u16 = 31;

impl Border {
    /// Bounded to what a file can say.
    pub fn bounded(self) -> Border {
        Border { width: self.width.clamp(1, MAX_BORDER_WIDTH), space: self.space.min(MAX_BORDER_SPACE), ..self }
    }

    /// Its drawn thickness, twips (a double line takes three widths).
    pub fn thickness(&self) -> i32 {
        let w = (self.width as i32 * 20 / 8).max(1);
        match self.style {
            BorderStyle::Double => w * 3,
            BorderStyle::Thick => w.max(30),
            _ => w,
        }
    }
}

/// A paragraph's borders. Consecutive paragraphs with the same borders are
/// one box: the top border above the first, the bottom below the last, and
/// `between` (when set) between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ParaBorders {
    pub top: Option<Border>,
    pub bottom: Option<Border>,
    pub start: Option<Border>,
    pub end: Option<Border>,
    pub between: Option<Border>,
}

impl ParaBorders {
    pub fn is_empty(&self) -> bool {
        [self.top, self.bottom, self.start, self.end, self.between].iter().all(|b| b.is_none())
    }

    pub fn sides(&self) -> [Option<Border>; 5] {
        [self.top, self.bottom, self.start, self.end, self.between]
    }
}

/// A dropped capital: the paragraph's first letter as tall as `lines`
/// lines (2–10), `distance` twips from the text beside it; `margin` puts
/// it in the margin, before the text's start edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DropCap {
    pub lines: u8,
    pub distance: i32,
    pub margin: bool,
}

pub const DROP_CAP_LINES: std::ops::RangeInclusive<u8> = 2..=10;

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
    /// Right to left (`true`) or left to right; unset, the paragraph's first
    /// strong character decides (UAX #9).
    pub rtl: Option<bool>,
    /// Borders round the paragraph (`Some` of an empty set takes the
    /// style's off).
    pub borders: Option<ParaBorders>,
    /// Background colour (sRGB); `Some(None)` takes the style's off.
    pub shading: Option<Option<u32>>,
    /// The first letter dropped; `Some(None)` takes the style's off.
    pub drop_cap: Option<Option<DropCap>>,
    /// A list item that starts its list (at its level) again at this number.
    pub list_start: Option<u32>,
    /// `false`: the paragraph's lines are left out of line numbering.
    pub line_numbers: Option<bool>,
    /// `false`: the paragraph's words are never hyphenated.
    pub hyphenate: Option<bool>,
    /// Its cell in a table inside the cell `cell` (one level deep: a table
    /// further in is read into this one).
    pub inner: Option<CellRef>,
}

impl ParaProps {
    /// `under` with the fields `self` sets replaced.
    pub fn over(&self, under: &ParaProps) -> ParaProps {
        ParaProps {
            align: self.align.or(under.align),
            indent_start: self.indent_start.or(under.indent_start),
            indent_end: self.indent_end.or(under.indent_end),
            first_line: self.first_line.or(under.first_line),
            space_before: self.space_before.or(under.space_before),
            space_after: self.space_after.or(under.space_after),
            line_spacing: self.line_spacing.or(under.line_spacing),
            keep_with_next: self.keep_with_next.or(under.keep_with_next),
            keep_together: self.keep_together.or(under.keep_together),
            page_break_before: self.page_break_before.or(under.page_break_before),
            widows: self.widows.or(under.widows),
            outline_level: self.outline_level.or(under.outline_level),
            list: self.list.or(under.list),
            cell: self.cell.or(under.cell),
            rev: self.rev.or(under.rev),
            tabs: self.tabs.clone().or_else(|| under.tabs.clone()),
            toc: self.toc.or(under.toc),
            section: self.section.or(under.section),
            rtl: self.rtl.or(under.rtl),
            borders: self.borders.or(under.borders),
            shading: self.shading.or(under.shading),
            drop_cap: self.drop_cap.or(under.drop_cap),
            list_start: self.list_start.or(under.list_start),
            line_numbers: self.line_numbers.or(under.line_numbers),
            hyphenate: self.hyphenate.or(under.hyphenate),
            inner: self.inner.or(under.inner),
        }
    }
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
    /// The cell's own look (over its table's).
    pub look: CellLook,
    /// Its row's (each cell of a row carries the same).
    pub row_look: RowLook,
    /// It continues the cell above it (merged down): its text, if any, is
    /// drawn with that cell's, and no rule runs between them.
    pub merged_up: bool,
    /// Grid columns it spans (merged across), for the writers' grid.
    pub span: u8,
}

impl Default for CellRef {
    fn default() -> Self {
        CellRef { table: 0, row: 0, col: 0, width: 1440, look: CellLook::default(), row_look: RowLook::default(), merged_up: false, span: 1 }
    }
}

/// Where a cell's text sits down it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VAlign {
    #[default]
    Top,
    Center,
    Bottom,
}

/// Space inside a cell's border, twips (each within `MAX_CELL_PAD`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellPad {
    pub top: u16,
    pub bottom: u16,
    pub start: u16,
    pub end: u16,
}

pub const MAX_CELL_PAD: u16 = 1440;

impl CellPad {
    pub fn bounded(self) -> CellPad {
        let b = |v: u16| v.min(MAX_CELL_PAD);
        CellPad { top: b(self.top), bottom: b(self.bottom), start: b(self.start), end: b(self.end) }
    }
}

impl Default for CellPad {
    fn default() -> Self {
        CellPad { top: CELL_PAD as u16, bottom: CELL_PAD as u16, start: CELL_PAD as u16, end: CELL_PAD as u16 }
    }
}

/// A cell's own borders: `None` takes the table's side, `Some(None)` has
/// none there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellBorders {
    pub top: Option<Option<Border>>,
    pub bottom: Option<Option<Border>>,
    pub start: Option<Option<Border>>,
    pub end: Option<Option<Border>>,
}

impl CellBorders {
    pub fn is_empty(&self) -> bool {
        *self == CellBorders::default()
    }
}

/// A cell's own look; what it leaves unset comes from its table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellLook {
    pub borders: CellBorders,
    /// Background (`Some(None)`: none, over the table's).
    pub shading: Option<Option<u32>>,
    pub valign: VAlign,
    pub pad: Option<CellPad>,
}

impl CellLook {
    pub fn is_default(&self) -> bool {
        *self == CellLook::default()
    }
}

/// A row's look: repeated at the top of each page as a header, kept on one
/// page, and its height (twips; 0 is as its text needs, else at least
/// that, or exactly with `exact`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RowLook {
    pub header: bool,
    pub cant_split: bool,
    pub height: u16,
    pub exact: bool,
}

/// Borders of a whole table: its four sides and the rules between rows
/// and between columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TableBorders {
    pub top: Option<Border>,
    pub bottom: Option<Border>,
    pub start: Option<Border>,
    pub end: Option<Border>,
    pub inside_h: Option<Border>,
    pub inside_v: Option<Border>,
}

impl TableBorders {
    /// Single half-point rules everywhere (Word's Table Grid).
    pub fn grid() -> TableBorders {
        let b = Some(Border::default());
        TableBorders { top: b, bottom: b, start: b, end: b, inside_h: b, inside_v: b }
    }
    pub fn sides(&self) -> [Option<Border>; 6] {
        [self.top, self.bottom, self.start, self.end, self.inside_h, self.inside_v]
    }
}

/// Which parts of a table its style marks out (Word's Table Style Options).
pub const LOOK_HEADER: u8 = 1;
pub const LOOK_TOTAL: u8 = 2;
pub const LOOK_BANDED_ROWS: u8 = 4;
pub const LOOK_FIRST_COLUMN: u8 = 8;
pub const LOOK_LAST_COLUMN: u8 = 16;
pub const LOOK_BANDED_COLUMNS: u8 = 32;

/// A whole table's look: its style, borders (over the style's), cell
/// padding, place across the column, and how its columns size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableLook {
    /// A table style by name (in [`StyleSheet::tables`]).
    pub style: Option<Arc<str>>,
    /// Its own borders; `None` takes the style's (or none).
    pub borders: Option<TableBorders>,
    pub shading: Option<u32>,
    pub pad: Option<CellPad>,
    pub align: HAlign,
    /// From the column's start edge, twips.
    pub indent: i32,
    /// `LOOK_*` bits.
    pub look: u8,
    /// Columns size to their contents (else the widths it has).
    pub autofit: bool,
}

impl Default for TableLook {
    fn default() -> Self {
        TableLook { style: None, borders: None, shading: None, pad: None, align: HAlign::Start, indent: 0, look: LOOK_HEADER | LOOK_BANDED_ROWS | LOOK_FIRST_COLUMN, autofit: false }
    }
}

impl TableLook {
    /// Bounded to what the file format holds.
    pub fn bounded(mut self) -> TableLook {
        self.indent = self.indent.clamp(-MAX_TWIPS_TABLE, MAX_TWIPS_TABLE);
        self.pad = self.pad.map(CellPad::bounded);
        self.look &= 63;
        self
    }
}

/// The furthest a table is set in or out, twips.
pub const MAX_TWIPS_TABLE: i32 = 31_680;

/// A part of a table a style gives its own look: a background, its text
/// bold, its text's colour, and rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RegionLook {
    pub shading: Option<u32>,
    pub bold: bool,
    pub color: Option<u32>,
    /// A rule under it (the header) or over it (the total row).
    pub rule: Option<Border>,
}

/// A table style: its borders and background, and its header row, total
/// row, first column and banded rows' looks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TableStyle {
    pub borders: TableBorders,
    pub shading: Option<u32>,
    pub header: RegionLook,
    pub total: RegionLook,
    pub first_col: RegionLook,
    pub last_col: RegionLook,
    /// Odd rows' and odd columns' backgrounds when banded.
    pub band_row: Option<u32>,
    pub band_col: Option<u32>,
}

/// The table styles the table style gallery offers: a name, then the
/// style (added to a document when first used).
pub fn builtin_table_styles() -> Vec<(&'static str, TableStyle)> {
    let rule = |color: u32, width: u16| Border { color: Some(color), width, space: 0, ..Border::default() };
    let accent = |name: &'static str, dark: u32, light: u32| {
        (name, TableStyle {
            borders: TableBorders { top: Some(rule(dark, 4)), bottom: Some(rule(dark, 4)), start: Some(rule(dark, 4)), end: Some(rule(dark, 4)), inside_h: Some(rule(dark, 4)), inside_v: None },
            header: RegionLook { shading: Some(dark), bold: true, color: Some(0xFFFFFF), rule: None },
            total: RegionLook { bold: true, rule: Some(Border { style: BorderStyle::Double, ..rule(dark, 4) }), ..RegionLook::default() },
            first_col: RegionLook { bold: true, ..RegionLook::default() },
            band_row: Some(light),
            ..TableStyle::default()
        })
    };
    vec![
        ("Table Grid", TableStyle { borders: TableBorders::grid(), ..TableStyle::default() }),
        ("Plain Table", TableStyle { header: RegionLook { bold: true, rule: Some(rule(0x7F7F7F, 4)), ..RegionLook::default() }, first_col: RegionLook { bold: true, ..RegionLook::default() }, band_row: Some(0xF2F2F2), ..TableStyle::default() }),
        ("Light List", TableStyle { borders: TableBorders { top: Some(rule(0x000000, 4)), bottom: Some(rule(0x000000, 4)), ..TableBorders::default() }, header: RegionLook { bold: true, rule: Some(rule(0x000000, 4)), ..RegionLook::default() }, total: RegionLook { bold: true, rule: Some(rule(0x000000, 4)), ..RegionLook::default() }, ..TableStyle::default() }),
        accent("Blue Table", 0x4472C4, 0xD9E2F3),
        accent("Orange Table", 0xED7D31, 0xFBE4D5),
        accent("Green Table", 0x70AD47, 0xE2EFD9),
        accent("Grey Table", 0x7F7F7F, 0xEDEDED),
    ]
}

/// Most table styles and tables a document keeps looks for.
pub const MAX_TABLE_STYLES: usize = 1024;
pub const MAX_TABLES: usize = 65_536;

/// A cell's look as drawn: its four rules and background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellPaint {
    pub top: Option<Border>,
    pub bottom: Option<Border>,
    pub start: Option<Border>,
    pub end: Option<Border>,
    pub shading: Option<u32>,
    pub valign: VAlign,
    /// Its text bold or coloured by the table style.
    pub bold: bool,
    pub color: Option<u32>,
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

/// How a list level counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumFormat {
    Decimal,
    /// 01, 02, …
    DecimalZero,
    LowerLetter,
    UpperLetter,
    LowerRoman,
    UpperRoman,
    /// The level's text is its bullet.
    Bullet,
    /// No number: the level's text alone.
    None,
}

/// One level of a list's definition: how it counts, its label (`%1` to
/// `%9` stand for the numbers of levels 1 to 9, so "%1.%2." labels a
/// second-level item 3.2.; a bullet level's text is its bullet), the
/// number it starts at, and where its text and label sit (twips).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListLevel {
    pub format: NumFormat,
    pub text: Arc<str>,
    pub start: u32,
    pub indent: i32,
    pub hang: i32,
}

/// Longest level label, bytes, and the largest start number.
pub const MAX_LEVEL_TEXT: usize = 64;
pub const MAX_LIST_START: u32 = 32_767;
/// Lists a document may define.
pub const MAX_LIST_DEFS: usize = 4096;

impl ListLevel {
    /// The level the built-in lists give `level` of `kind`.
    pub fn builtin(kind: ListKind, level: u8) -> ListLevel {
        let l = level.min(MAX_LIST_LEVEL);
        let indent = LIST_INDENT * (l as i32 + 1);
        match kind {
            ListKind::Bullet => ListLevel { format: NumFormat::Bullet, text: ["•", "◦", "▪"][l as usize % 3].into(), start: 1, indent, hang: LIST_HANG },
            ListKind::Numbered => {
                let format = [NumFormat::Decimal, NumFormat::LowerLetter, NumFormat::LowerRoman][l as usize % 3];
                ListLevel { format, text: format!("%{}.", l + 1).into(), start: 1, indent, hang: LIST_HANG }
            }
        }
    }

    /// Bounded to what the file format holds.
    pub fn bounded(mut self) -> ListLevel {
        if self.text.len() > MAX_LEVEL_TEXT {
            let mut e = MAX_LEVEL_TEXT;
            while !self.text.is_char_boundary(e) {
                e -= 1;
            }
            self.text = self.text[..e].into();
        }
        self.start = self.start.min(MAX_LIST_START);
        self.indent = self.indent.clamp(-31_680, 31_680);
        self.hang = self.hang.clamp(0, 31_680);
        self
    }
}

/// A list's own definition: its nine levels.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ListDef {
    pub levels: Vec<ListLevel>,
}

impl ListDef {
    /// The built-in list of `kind`, every level.
    pub fn builtin(kind: ListKind) -> ListDef {
        ListDef { levels: (0..=MAX_LIST_LEVEL).map(|l| ListLevel::builtin(kind, l)).collect() }
    }

    pub fn level(&self, level: u8) -> Option<&ListLevel> {
        self.levels.get(level as usize)
    }
}

/// Where line numbers start again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LineRestart {
    #[default]
    Page,
    Section,
    Continuous,
}

/// Numbers beside the lines, in the start margin: from `start`, every
/// `count_by`th line shown, `distance` twips from the text (0: a quarter inch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineNumbering {
    pub start: u32,
    pub count_by: u16,
    pub distance: i32,
    pub restart: LineRestart,
}

impl Default for LineNumbering {
    fn default() -> Self {
        LineNumbering { start: 1, count_by: 1, distance: 0, restart: LineRestart::Page }
    }
}

/// The document's settings: line numbers, hyphenation, and the look of
/// every page (its colour, a border round it, a watermark behind the text).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Settings {
    pub line_numbers: Option<LineNumbering>,
    /// Words are hyphenated at line ends.
    pub hyphenate: bool,
    /// The sheet's colour (RGB) behind everything.
    pub page_color: Option<u32>,
    pub page_borders: Option<PageBorders>,
    pub watermark: Option<Watermark>,
}

/// Rules round the page: each side's, `space` points from the text (or,
/// `from_page`, from the sheet's edge).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PageBorders {
    pub top: Option<Border>,
    pub bottom: Option<Border>,
    pub start: Option<Border>,
    pub end: Option<Border>,
    pub from_page: bool,
}

/// Words or a picture drawn faintly behind the text of every page.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Watermark {
    /// Words across the page, as large as fit: on the diagonal or level.
    Text { text: Arc<str>, font: Option<Arc<str>>, color: u32, diagonal: bool },
    /// A picture from the document's images, centred, its width a share of
    /// the content box's (percent), washed out (faint) or as it is.
    Picture { image: u32, scale: u16, washout: bool },
}

/// Longest watermark text, bytes.
pub const MAX_WATERMARK_TEXT: usize = 256;

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
    /// Direction; `None`: from the text.
    pub rtl: Option<bool>,
    pub borders: ParaBorders,
    pub shading: Option<u32>,
    pub drop_cap: Option<DropCap>,
    pub line_numbers: bool,
    pub hyphenate: bool,
}

impl Default for ParaFormat {
    fn default() -> Self {
        ParaFormat { align: Align::Start, indent_start: 0, indent_end: 0, first_line: 0, space_before: 0, space_after: 160, line_spacing: LineSpacing::Multiple(115), keep_with_next: false, keep_together: false, page_break_before: false, widows: true, outline_level: None, list: None, tabs: Vec::new(), rtl: None, borders: ParaBorders::default(), shading: None, drop_cap: None, line_numbers: true, hyphenate: true }
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
        if p.rtl.is_some() {
            self.rtl = p.rtl;
        }
        if let Some(t) = &p.tabs {
            self.tabs = t.iter().take(MAX_TABS).copied().collect();
        }
        if let Some(b) = p.borders {
            let f = |x: Option<Border>| x.map(Border::bounded);
            self.borders = ParaBorders { top: f(b.top), bottom: f(b.bottom), start: f(b.start), end: f(b.end), between: f(b.between) };
        }
        if let Some(v) = p.shading {
            self.shading = v.map(|c| c & 0xff_ffff);
        }
        if let Some(d) = p.drop_cap {
            self.drop_cap = d.filter(|d| DROP_CAP_LINES.contains(&d.lines)).map(|d| DropCap { distance: d.distance.clamp(0, 1440 * 22), ..d });
        }
        if let Some(v) = p.line_numbers {
            self.line_numbers = v;
        }
        if let Some(v) = p.hyphenate {
            self.hyphenate = v;
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

/// Where a style is used ([`TextDoc::style_uses`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StyleUses {
    pub body: bool,
    pub elsewhere: bool,
    pub styles: bool,
}

/// The document's named styles. Style 0 is the default paragraph style ("Normal").
#[derive(Debug, Clone)]
pub struct StyleSheet {
    pub styles: Vec<Style>,
    pub default_char: CharFormat,
    /// Lists' own definitions by list id; a list without one numbers as
    /// the built-in list of its kind.
    pub lists: std::collections::BTreeMap<u32, ListDef>,
    /// Table styles by name: the file's own and those picked from the
    /// gallery.
    pub tables: std::collections::BTreeMap<Arc<str>, TableStyle>,
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
            lists: Default::default(),
            tables: Default::default(),
        }
    }
}

impl StyleSheet {
    pub fn by_name(&self, name: &str) -> Option<StyleId> {
        self.styles.iter().position(|s| &*s.name == name).map(|i| i as StyleId)
    }

    /// Whether paragraph style `para` or character style `chars` (or
    /// what they are based on) sets a text colour.
    pub fn sets_color(&self, para: StyleId, chars: Option<StyleId>) -> bool {
        self.chain(para).iter().chain(chars.map(|c| self.chain(c)).unwrap_or_default().iter()).any(|s| s.chars.color.is_some())
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
            let lv = self.list_level(l);
            if direct.indent_start.is_none() {
                pf.indent_start = lv.indent;
            }
            if direct.first_line.is_none() {
                pf.first_line = -lv.hang;
            }
        }
        (pf, cf)
    }

    /// The level a list item's label and indents come from: its list's own
    /// definition, else the built-in list of its kind.
    pub fn list_level(&self, l: ListRef) -> ListLevel {
        self.lists.get(&l.id).and_then(|d| d.level(l.level)).cloned().unwrap_or_else(|| ListLevel::builtin(l.kind, l.level))
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
    /// This paragraph with its text changed to `new`, keeping the format of
    /// the unchanged start and end; new text takes the format before it
    /// (`plain` when that is a picture or other object).
    pub fn retext(&self, new: &str, attrs: &AttrTable, plain: AttrId) -> Para {
        let old = self.text.as_str();
        let mut pre = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
        while !old.is_char_boundary(pre) || !new.is_char_boundary(pre) {
            pre -= 1;
        }
        let room = old.len().min(new.len()) - pre;
        let mut suf = old.bytes().rev().zip(new.bytes().rev()).take(room).take_while(|(a, b)| a == b).count();
        while !old.is_char_boundary(old.len() - suf) || !new.is_char_boundary(new.len() - suf) {
            suf -= 1;
        }
        let (old_end, new_end) = ((old.len() - suf) as u32, (new.len() - suf) as u32);
        let delta = new_end as i64 - old_end as i64;
        let mid = {
            let a = self.attr_at((pre as u32).saturating_sub(1));
            if attrs.get(a).object.is_some() || attrs.get(a).note.is_some() || attrs.get(a).field.is_some() { plain } else { a }
        };
        let mut runs = Vec::with_capacity(self.runs.len() + 2);
        for &(at, a) in &self.runs {
            if (at as usize) < pre {
                runs.push((at, a));
            }
        }
        runs.push((pre as u32, mid));
        runs.push((new_end, self.attr_at(old_end)));
        for &(at, a) in &self.runs {
            if at > old_end {
                runs.push(((at as i64 + delta) as u32, a));
            }
        }
        let mut p = Para { text: new.to_string(), runs, ..self.clone() };
        p.normalize();
        p
    }

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

/// A header and footer pair for some pages (the first, or the even ones).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Margins {
    pub header: Option<HeaderFooter>,
    pub footer: Option<HeaderFooter>,
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
/// cells apart by tabs that stop where the cells are, each as its cell's
/// text is aligned (headers lay out no tables; a cell a paragraph would
/// stack a three-column row three lines high).
pub fn rows_as_lines(paras: Vec<Para>) -> Vec<Para> {
    // A cell's text sits this far in from its edges (Word's default).
    const PAD: i32 = 108;
    let stop = |x: i32, w: i32, align: Option<Align>| match align {
        Some(Align::Center) => TabStop { pos: x + w / 2, align: TabAlign::Center, leader: Leader::None },
        Some(Align::End) => TabStop { pos: (x + w - PAD).max(x), align: TabAlign::End, leader: Leader::None },
        _ => TabStop { pos: x + PAD.min(w / 2), align: TabAlign::Start, leader: Leader::None },
    };
    let mut out: Vec<Para> = Vec::with_capacity(paras.len());
    let mut last: Option<CellRef> = None;
    // Where the current cell starts across its row, twips.
    let mut x = 0i32;
    for mut p in paras {
        let cell = p.props.cell.take();
        match (cell, last, out.last_mut()) {
            (Some(c), Some(l), Some(q)) if (c.table, c.row) == (l.table, l.row) => {
                if c.col != l.col {
                    x = x.saturating_add(l.width.min(MAX_OBJECT_TWIPS) as i32);
                }
                if !p.text.is_empty() {
                    if c.col != l.col && q.props.tabs.is_none() {
                        // The row's first cell, as it was aligned.
                        let first = q.props.align;
                        q.props.align = None;
                        q.props.tabs = Some(Vec::new());
                        if matches!(first, Some(Align::Center | Align::End)) {
                            q.text.insert(0, '\t');
                            for r in q.runs.iter_mut().skip(1) {
                                r.0 += 1;
                            }
                            if let Some(t) = q.props.tabs.as_mut() {
                                t.push(stop(0, l.width.min(MAX_OBJECT_TWIPS) as i32, first));
                            }
                        }
                    }
                    let gap = if c.col != l.col { '\t' } else { ' ' };
                    if gap == '\t' {
                        if let Some(t) = q.props.tabs.as_mut().filter(|t| t.len() < MAX_TABS) {
                            t.push(stop(x, c.width.min(MAX_OBJECT_TWIPS) as i32, p.props.align));
                        }
                    }
                    let base = q.text.len() as u32 + 1;
                    let a = q.runs.last().map_or(0, |r| r.1);
                    q.text.push(gap);
                    q.runs.push((base - 1, a));
                    q.text.push_str(&p.text);
                    q.runs.extend(p.runs.iter().map(|&(at, a)| (at + base, a)));
                    q.normalize();
                }
            }
            _ => {
                x = 0;
                out.push(p);
            }
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
    /// The first page's own header and footer (either may be none: a
    /// title page without them), when it has its own.
    pub first_page: Option<Margins>,
    /// Even pages' own header and footer, when they differ from odd ones'.
    pub even_pages: Option<Margins>,
    /// The pictures placed in the text.
    pub images: Images,
    /// The drawn shapes placed in the text (as pictures are).
    pub shapes: Store<ShapeData>,
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
    /// Line numbers and hyphenation.
    pub settings: Settings,
    /// Tables' own looks by table id (a table without one has single rules
    /// everywhere).
    pub tables: std::collections::BTreeMap<u32, TableLook>,
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
        let mut d = TextDoc { blocks: BlockTree::default(), attrs: AttrTable::default(), styles: StyleSheet::default(), page: PageSetup::default(), header: None, footer: None, first_page: None, even_pages: None, images: Images::default(), shapes: Store::new(MAX_SHAPES), notes: Notes::default(), comments: Comments::default(), revisions: Store::new(MAX_REVISIONS), deletions: Store::new(MAX_DELETIONS), conflicts: Store::new(MAX_CONFLICTS), tracking: false, settings: Settings::default(), tables: Default::default(), field_values: HashMap::new(), next_id: 1, changes: None };
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
    /// The header and footer page `i` (from 0) shows.
    pub fn margins_of(&self, i: usize) -> (Option<&HeaderFooter>, Option<&HeaderFooter>) {
        let own = if i == 0 { self.first_page.as_ref() } else { None }.or(if i % 2 == 1 { self.even_pages.as_ref() } else { None });
        match own {
            Some(m) => (m.header.as_ref(), m.footer.as_ref()),
            None => (self.header.as_ref(), self.footer.as_ref()),
        }
    }

    /// Every header and footer the document has.
    pub fn all_margins(&self) -> impl Iterator<Item = &HeaderFooter> {
        let own = self.first_page.iter().chain(self.even_pages.iter()).flat_map(|m| m.header.iter().chain(m.footer.iter()));
        self.header.iter().chain(self.footer.iter()).chain(own)
    }

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

    /// Block `i`'s cell in the table inside its cell, if any.
    pub fn inner_cell(&self, i: usize) -> Option<CellRef> {
        let p = self.para(i)?;
        p.props.cell.and(p.props.inner)
    }

    /// Block `i`'s cell at a depth: its table's (false) or the one inside
    /// its cell (true).
    pub fn cell_at(&self, i: usize, inner: bool) -> Option<CellRef> {
        if inner { self.inner_cell(i) } else { self.cell(i) }
    }

    /// Whether blocks `a` and `b` are in the same cell (or both outside tables).
    pub fn same_cell(&self, a: usize, b: usize) -> bool {
        self.same_cell_at(a, b, false)
    }

    /// The blocks of the row holding block `i` (empty outside tables).
    pub fn row_range(&self, i: usize) -> std::ops::Range<usize> {
        self.row_range_at(i, false)
    }

    /// The cells of row `row` in a column `col_w` wide (see [`Self::row_cells_at`]).
    pub fn row_cells(&self, row: std::ops::Range<usize>, col_w: i64) -> Vec<(std::ops::Range<usize>, i64, i64)> {
        self.row_cells_at(row, col_w, false)
    }

    pub fn cell_pad(&self, i: usize) -> CellPad {
        self.cell_pad_at(i, false)
    }

    pub fn row_place(&self, i: usize) -> (usize, bool) {
        self.row_place_at(i, false)
    }

    pub fn row_paints(&self, row: std::ops::Range<usize>, n_cells: usize) -> Vec<CellPaint> {
        self.row_paints_at(row, n_cells, false)
    }

    /// Whether blocks `a` and `b` are in the same cell (or both outside tables).
    pub fn same_cell_at(&self, a: usize, b: usize, inner: bool) -> bool {
        match (self.cell_at(a, inner), self.cell_at(b, inner)) {
            (None, None) => true,
            (Some(x), Some(y)) => (x.table, x.row, x.col) == (y.table, y.row, y.col),
            _ => false,
        }
    }

    /// The blocks of the row holding block `i` (empty outside tables).
    pub fn row_range_at(&self, i: usize, inner: bool) -> std::ops::Range<usize> {
        let Some(c) = self.cell_at(i, inner) else { return i..i };
        let same = |k: usize| self.cell_at(k, inner).is_some_and(|d| (d.table, d.row) == (c.table, c.row));
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
    pub fn row_cells_at(&self, row: std::ops::Range<usize>, col_w: i64, inner: bool) -> Vec<(std::ops::Range<usize>, i64, i64)> {
        let mut cells: Vec<(std::ops::Range<usize>, i64)> = Vec::new();
        for k in row {
            let w = self.cell_at(k, inner).map_or(1, |c| c.width.clamp(1, 31680)) as i64;
            let n = cells.len();
            match cells.last_mut() {
                // Past the most cells a row has, the rest join the last.
                Some((r, _)) if n >= MAX_CELLS || self.same_cell_at(r.start, k, inner) => r.end = k + 1,
                _ => cells.push((k..k + 1, crate::twips(w))),
            }
        }
        let total: i64 = cells.iter().map(|c| c.1).sum::<i64>().max(1);
        let scale = |w: i64| if total > col_w { (w * col_w / total).max(crate::LU) } else { w };
        // The table's place across the column.
        let room = (col_w - total).max(0);
        let look = cells.first().and_then(|c| self.cell_at(c.0.start, inner)).and_then(|c| self.tables.get(&c.table));
        let mut x = match look.map(|t| t.align) {
            Some(HAlign::Center) => room / 2,
            Some(HAlign::End) => room,
            _ => look.map_or(0, |t| crate::twips(t.indent as i64).clamp(-col_w, room)),
        };
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
    /// its text is laid out in (a cell's, inside the padding, or the column;
    /// in a table inside a cell, its cell there).
    pub fn block_box(&self, i: usize, col_w: i64) -> (i64, i64) {
        let Some((x, w)) = self.cell_box_at(i, col_w, false) else { return (0, col_w) };
        match self.cell_box_at(i, w, true) {
            Some((x2, w2)) => (x + x2, w2),
            None => (x, w),
        }
    }

    /// The text box of block `i`'s cell at a depth, in a width `col_w`.
    fn cell_box_at(&self, i: usize, col_w: i64, inner: bool) -> Option<(i64, i64)> {
        self.cell_at(i, inner)?;
        let pad = self.cell_pad_at(i, inner);
        let (ps, pe) = (crate::twips(pad.start as i64), crate::twips(pad.end as i64));
        let row = self.row_range_at(i, inner);
        self.row_cells_at(row, col_w, inner).into_iter().find(|c| c.0.contains(&i)).map(|(_, x, w)| (x + ps, (w - ps - pe).max(crate::LU)))
    }

    /// The padding of the cell block `i` is in: its own, its table's, or
    /// the default.
    pub fn cell_pad_at(&self, i: usize, inner: bool) -> CellPad {
        let Some(c) = self.cell_at(i, inner) else { return CellPad::default() };
        c.look.pad.or_else(|| self.tables.get(&c.table).and_then(|t| t.pad)).unwrap_or_default()
    }

    /// The table style `t` names, if the document has it.
    pub fn table_style(&self, t: &TableLook) -> Option<&TableStyle> {
        t.style.as_ref().and_then(|n| self.styles.tables.get(n))
    }

    /// Where the row of block `i` sits in its table: its index among the
    /// table's rows (counted back at most `MAX_ROW_BLOCKS` blocks), and
    /// whether it is the last.
    pub fn row_place_at(&self, i: usize, inner: bool) -> (usize, bool) {
        let Some(c) = self.cell_at(i, inner) else { return (0, false) };
        let row = self.row_range_at(i, inner);
        let mut k = row.start;
        let mut index = 0;
        let mut last_row = c.row;
        while k > 0 && row.start - k < MAX_ROW_BLOCKS {
            match self.cell_at(k - 1, inner) {
                Some(d) if d.table == c.table => {
                    if d.row != last_row {
                        index += 1;
                        last_row = d.row;
                    }
                    k -= 1;
                }
                _ => break,
            }
        }
        let last = !self.cell_at(row.end, inner).is_some_and(|d| d.table == c.table);
        (index, last)
    }

    /// The blocks of the header rows at the top of block `i`'s table, when
    /// `i` is in a later row (they repeat at the top of each page).
    pub fn header_rows(&self, i: usize) -> Option<std::ops::Range<usize>> {
        let c = self.cell(i)?;
        if c.row_look.header {
            return None;
        }
        // Back to the table's first block.
        let mut start = self.row_range(i).start;
        let mut rows = 0;
        while start > 0 && rows < MAX_ROW_BLOCKS {
            match self.cell(start - 1) {
                Some(d) if d.table == c.table => {
                    start = self.row_range(start - 1).start;
                    rows += 1;
                }
                _ => break,
            }
        }
        let mut end = start;
        while self.cell(end).is_some_and(|d| d.table == c.table && d.row_look.header) && end < i {
            end = self.row_range(end).end;
        }
        (end > start).then_some(start..end)
    }

    /// How each cell of the row of blocks `row` is drawn, in order: its
    /// rules and background from its own look over its table's style
    /// regions, its table's borders and the style's.
    pub fn row_paints_at(&self, row: std::ops::Range<usize>, n_cells: usize, inner: bool) -> Vec<CellPaint> {
        let Some(first) = self.cell_at(row.start, inner) else { return Vec::new() };
        let look = self.tables.get(&first.table).cloned().unwrap_or_default();
        let style = self.table_style(&look).cloned();
        let borders = look.borders.or(style.as_ref().map(|s| s.borders)).unwrap_or_else(TableBorders::grid);
        let (index, last) = self.row_place_at(row.start, inner);
        // Header rows: those marked, or the first when the style marks it.
        let header = first.row_look.header || (index == 0 && look.look & LOOK_HEADER != 0 && style.is_some());
        let total = last && index > 0 && look.look & LOOK_TOTAL != 0 && style.is_some();
        let banded_row = look.look & LOOK_BANDED_ROWS != 0 && (index + usize::from(look.look & LOOK_HEADER == 0)) % 2 == 1;
        let mut cells: Vec<CellRef> = Vec::new();
        for k in row {
            let Some(c) = self.cell_at(k, inner) else { continue };
            if cells.last().is_none_or(|d| (d.row, d.col) != (c.row, c.col)) && cells.len() < MAX_CELLS {
                cells.push(c);
            }
        }
        let n = n_cells.max(cells.len()).max(1);
        cells
            .iter()
            .enumerate()
            .map(|(j, c)| {
                let mut p = CellPaint {
                    top: if index == 0 { borders.top } else { borders.inside_h },
                    bottom: if last { borders.bottom } else { borders.inside_h },
                    start: if j == 0 { borders.start } else { borders.inside_v },
                    end: if j + 1 == n { borders.end } else { borders.inside_v },
                    shading: look.shading.or(style.as_ref().and_then(|s| s.shading)),
                    valign: c.look.valign,
                    bold: false,
                    color: None,
                };
                if let Some(s) = &style {
                    if banded_row && !header && !total {
                        p.shading = s.band_row.or(p.shading);
                    }
                    if look.look & LOOK_BANDED_COLUMNS != 0 && j % 2 == 0 && !header {
                        p.shading = s.band_col.or(p.shading);
                    }
                    let mut region = |r: &RegionLook, under: bool| {
                        p.shading = r.shading.or(p.shading);
                        p.bold |= r.bold;
                        p.color = r.color.or(p.color);
                        if let Some(rule) = r.rule {
                            if under {
                                p.bottom = Some(rule);
                            } else {
                                p.top = Some(rule);
                            }
                        }
                    };
                    if j == 0 && look.look & LOOK_FIRST_COLUMN != 0 {
                        region(&s.first_col, true);
                    }
                    if j + 1 == n && look.look & LOOK_LAST_COLUMN != 0 {
                        region(&s.last_col, true);
                    }
                    if header {
                        region(&s.header, true);
                    }
                    if total {
                        region(&s.total, false);
                    }
                }
                let b = c.look.borders;
                p.top = b.top.unwrap_or(p.top);
                p.bottom = b.bottom.unwrap_or(p.bottom);
                p.start = b.start.unwrap_or(p.start);
                p.end = b.end.unwrap_or(p.end);
                p.shading = c.look.shading.unwrap_or(p.shading);
                if c.merged_up {
                    p.top = None;
                }
                p
            })
            .collect()
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
    /// Whether any paragraph anywhere (body, headers and footers, notes,
    /// shapes, deleted or conflicting text) or style names style `id`.
    pub fn uses_style(&self, id: StyleId) -> bool {
        let u = self.style_uses(id);
        u.body || u.elsewhere || u.styles
    }

    /// Where style `id` is used: by paragraphs or text of the body, by those
    /// anywhere else (headers and footers, notes, shapes, deleted or
    /// conflicting text), and by other styles (as their base or next style).
    pub fn style_uses(&self, id: StyleId) -> StyleUses {
        let attrs = &self.attrs;
        let runs = |r: &[(u32, AttrId)]| r.iter().any(|x| attrs.get(x.1).style == Some(id));
        let p = |p: &Para| p.style == id || runs(&p.runs);
        let hf = |h: &Option<HeaderFooter>| h.as_ref().is_some_and(|h| h.paras.iter().any(p));
        let margins = |m: &Option<Margins>| m.as_ref().is_some_and(|m| hf(&m.header) || hf(&m.footer));
        let pieces = |d: &Deletion| d.pieces.iter().any(|x| x.para.as_ref().is_some_and(|q| q.1 == id) || runs(&x.runs));
        StyleUses {
            body: self.blocks.iter().any(|b| b.as_para().is_some_and(p)),
            elsewhere: hf(&self.header)
                || hf(&self.footer)
                || margins(&self.first_page)
                || margins(&self.even_pages)
                || self.notes.iter().any(|(_, n)| n.paras.iter().any(p))
                || self.shapes.iter().any(|(_, s)| s.all_paras().into_iter().any(p))
                || self.deletions.iter().any(|(_, d)| pieces(d))
                || self.conflicts.iter().any(|(_, d)| pieces(d)),
            styles: self.styles.styles.iter().enumerate().any(|(i, s)| i != id as usize && (s.parent == Some(id) || s.next == Some(id))),
        }
    }

    /// Every style id the document holds through `f` (one-to-one on the
    /// ids in use): paragraphs anywhere, deleted paragraph breaks, styles'
    /// base and next. Character styles in runs are the attribute table's
    /// ([`AttrTable::remap_styles`]).
    pub fn remap_para_styles(&mut self, f: &dyn Fn(StyleId) -> StyleId) {
        fn shape(s: &mut ShapeData, f: &dyn Fn(StyleId) -> StyleId) {
            for p in &mut s.paras {
                p.style = f(p.style);
            }
            for k in &mut s.kids {
                shape(&mut k.shape, f);
            }
        }
        for i in 0..self.blocks.len() {
            if self.blocks.get(i).is_some_and(|b| b.as_para().is_some()) {
                self.blocks.update(i, |b| {
                    if let Some(p) = b.as_para_mut() {
                        p.style = f(p.style);
                    }
                });
            }
        }
        let hf = |h: &mut Option<HeaderFooter>| {
            if let Some(h) = h {
                for p in &mut h.paras {
                    p.style = f(p.style);
                }
            }
        };
        hf(&mut self.header);
        hf(&mut self.footer);
        for m in [&mut self.first_page, &mut self.even_pages].into_iter().flatten() {
            hf(&mut m.header);
            hf(&mut m.footer);
        }
        let ids: Vec<u32> = self.notes.iter().map(|(i, _)| i).collect();
        for i in ids {
            let mut n = (**self.notes.get(i).unwrap()).clone();
            n.paras.iter_mut().for_each(|p| p.style = f(p.style));
            self.notes.set(i, Some(n));
        }
        let ids: Vec<u32> = self.shapes.iter().map(|(i, _)| i).collect();
        for i in ids {
            let mut s = (**self.shapes.get(i).unwrap()).clone();
            shape(&mut s, f);
            self.shapes.set(i, Some(s));
        }
        for store in [&mut self.deletions, &mut self.conflicts] {
            let ids: Vec<u32> = store.iter().map(|(i, _)| i).collect();
            for i in ids {
                let mut d = (**store.get(i).unwrap()).clone();
                for x in &mut d.pieces {
                    if let Some(q) = &mut x.para {
                        q.1 = f(q.1);
                    }
                }
                store.set(i, Some(d));
            }
        }
        for st in &mut self.styles.styles {
            st.parent = st.parent.map(f);
            st.next = st.next.map(f);
        }
    }

    /// Whether `s` may be style `id` (a change, or a new one just past the
    /// last): a name of its own, references inside the table, no loop
    /// through parents, no kind change, Normal a paragraph style on nothing.
    pub fn style_fits(&self, id: StyleId, s: &Style) -> bool {
        let n = self.styles.styles.len();
        let i = id as usize;
        let len = if i == n { n + 1 } else { n };
        if i > n || len > MAX_STYLES {
            return false;
        }
        let refs_ok = [s.parent, s.next].into_iter().flatten().all(|r| (r as usize) < len) && s.parent != Some(id);
        let name_ok = !s.name.trim().is_empty() && s.name.len() <= 255 && !self.styles.styles.iter().enumerate().any(|(k, x)| k != i && x.name == s.name);
        let normal_ok = i != 0 || (s.kind == StyleKind::Para && s.parent.is_none());
        if !refs_ok || !name_ok || !normal_ok || (i < n && self.styles.styles[i].kind != s.kind) {
            return false;
        }
        let mut at = s.parent;
        for _ in 0..MAX_STYLE_DEPTH {
            match at {
                Some(q) if q == id => return false,
                Some(q) => at = self.styles.styles.get(q as usize).and_then(|x| x.parent),
                None => break,
            }
        }
        true
    }

    /// The body paragraphs whose look style `id` decides (it or a style
    /// based on it): the first and one past the last, if any.
    pub fn style_span(&self, id: StyleId) -> Option<(usize, usize)> {
        let under = |mut s: StyleId| {
            for _ in 0..MAX_STYLE_DEPTH {
                if s == id {
                    return true;
                }
                match self.styles.styles.get(s as usize).and_then(|x| x.parent) {
                    Some(q) => s = q,
                    None => return false,
                }
            }
            false
        };
        let hit = |b: &Block| b.as_para().is_some_and(|p| under(p.style));
        let mut span = None;
        for (i, b) in self.blocks.iter().enumerate() {
            if hit(b) {
                span = Some(span.map_or((i, i + 1), |(f, _)| (f, i + 1)));
            }
        }
        span
    }

    /// The block whose text places shape `id`.
    pub fn shape_anchor(&self, id: u32) -> Option<usize> {
        self.blocks.iter().position(|b| b.as_para().is_some_and(|p| p.text.contains(OBJECT_CHAR) && p.runs.iter().any(|r| self.attrs.get(r.1).object.is_some_and(|o| o.shape && o.image == id))))
    }

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
