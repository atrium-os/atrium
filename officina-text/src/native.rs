//! The native text document file, version 0 (docs/spec/officina.md §5.1, §7.4):
//! an `officina_core::container` of kind Write with these chunks.
//!
//! | kind | chunk | payload |
//! |---|---|---|
//! | 1 | DOCUMENT | page setup, the next block id |
//! | 2 | STYLES | named styles: name, kind, parent, next, paragraph and character properties |
//! | 3 | ATTRS | the character attribute table after the default (id 0), in id order |
//! | 4 | BLOCKS | up to [`BLOCKS_PER_CHUNK`] blocks from a first index: id, style, properties, text, runs |
//! | 5 | CHECKPOINTS | page starts (block, line) from the last pagination, for showing a reopened file at once |
//! | 6 | IMAGE | one picture: its index, its size in pixels, its encoded bytes (one chunk each, so a reader can load them late) |
//! | 7 | NOTES | footnotes and endnotes: id, kind, paragraphs |
//! | 8 | COMMENTS | comments: id, author, initials, date, text, the comment answered, resolved |
//! | 9 | REVISIONS | whether edits are tracked; revisions: id, author, date, change set, its name |
//! | 10 | DELETIONS | tracked deletions a tombstone holds: id, revision, pieces (paragraph, text, runs) |
//! | 11 | CONFLICTS | merge conflicts: id, then the other version as a deletion's encoding |
//! | 13 | SHAPE | one drawn shape: its index, outline (a preset's name and adjust values, or paths), fill, outline, turn and flips, text insets and anchor, its paragraphs |
//! | 12 | MARGINS | the header's then the footer's own paragraphs (count, blocks); then the first page's and even pages' own header and footer, each absent (0) or present (1: both lines, then their paragraphs) |
//!
//! Block chunks are independent, so a reader can load the chunks around the
//! reading position first (v0 loads them all). The same encodings serve the
//! autosave journal: [`encode_op`] and [`decode_op`] for `TextOp`s, and
//! [`encode_attrs`] for attribute entries an action interned.
//!
//! The reader is for hostile files: counts are bounded by what the bytes left
//! could hold, indices are checked, and nothing panics.

use std::io::{self, Write};
use std::sync::Arc;

use officina_core::container::{ContainerError, ContainerReader, ContainerWriter, DocKind, Limits};
use officina_core::wire::{Reader, WireError, Writer};

use crate::model::{CellBorders, CellLook, CellPad, RowLook, VAlign, TableBorders, TableLook, TableStyle, RegionLook, MAX_TABLES, MAX_TABLE_STYLES, MAX_CELL_PAD, MAX_TWIPS_TABLE, MAX_CELLS, Border, BorderStyle, ParaBorders, DropCap, DROP_CAP_LINES, MAX_BORDER_WIDTH, MAX_BORDER_SPACE, ListDef, ListLevel, NumFormat, MAX_LEVEL_TEXT, MAX_LIST_START, MAX_LIST_DEFS, LineNumbering, LineRestart, Settings, PageBorders, Watermark, MAX_WATERMARK_TEXT, Caps, MAX_SPACING, SCALE_RANGE, EFFECTS, Float, Wrap, Rel, HAlign, MAX_FLOAT_OFFSET, Section, SectionStart, MAX_COLUMNS, Comment, Field, CommentId, DelId, DelPiece, Deletion, FmtChange, RevId, Revision, MAX_DELETIONS, MAX_DELETION_TEXT, MAX_CONFLICTS, ConflictId, TabStop, TabAlign, Leader, MAX_TABS, MAX_REVISIONS, MAX_COMMENTS, MAX_COMMENT_TEXT, Note, NoteId, NoteKind, VertAlign, MAX_NOTES, MAX_NOTE_PARAS, MAX_MARGIN_PARAS, Align, AttrId, CellRef, HeaderFooter, ImageData, ObjectRef, MAX_OBJECT_TWIPS, ListKind, ListRef, MAX_LIST_LEVEL, Block, CharAttrs, LineSpacing, PageSetup, Para, ParaProps, Style, StyleKind, StyleSheet, TextDoc, MAX_STYLES};
use crate::ops::{Fragment, TextOp};
use crate::paginate::Checkpoint;

pub const DOCUMENT: u16 = 1;
pub const STYLES: u16 = 2;
pub const ATTRS: u16 = 3;
pub const BLOCKS: u16 = 4;
pub const CHECKPOINTS: u16 = 5;
pub const IMAGE: u16 = 6;
pub const NOTES: u16 = 7;
pub const COMMENTS: u16 = 8;
pub const REVISIONS: u16 = 9;
pub const DELETIONS: u16 = 10;
pub const CONFLICTS: u16 = 11;
pub const MARGINS: u16 = 12;
pub const SHAPE: u16 = 13;
/// Lists' own definitions.
pub const LISTS: u16 = 14;
/// Line numbers and hyphenation.
pub const SETTINGS: u16 = 15;
/// Tables' own looks, and table styles.
pub const TABLES: u16 = 16;
pub const TABLE_STYLES: u16 = 17;

/// Largest picture side recorded, pixels.
pub const MAX_IMAGE_PX: u64 = 1 << 16;
/// Largest encoded picture.
pub const MAX_IMAGE_BYTES: u64 = 48 << 20;

pub const BLOCKS_PER_CHUNK: usize = 256;
/// Most blocks a document may have.
pub const MAX_BLOCKS: u64 = 1 << 26;
/// Longest paragraph text, in bytes.
pub const MAX_TEXT: u64 = 16 << 20;
/// Most attribute table entries.
pub const MAX_ATTRS: u64 = 1 << 20;
const MAX_NAME: u64 = 255;

#[derive(Debug)]
pub enum LoadError {
    Container(ContainerError),
    Wire(WireError),
    Bad(&'static str),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Container(e) => write!(f, "{e}"),
            LoadError::Wire(e) => write!(f, "damaged file: {e}"),
            LoadError::Bad(w) => write!(f, "damaged file: {w}"),
        }
    }
}
impl std::error::Error for LoadError {}
impl From<ContainerError> for LoadError {
    fn from(e: ContainerError) -> Self {
        LoadError::Container(e)
    }
}
impl From<WireError> for LoadError {
    fn from(e: WireError) -> Self {
        LoadError::Wire(e)
    }
}

// ---------- shared encodings ----------

/// Character attributes: a mask of the fields set, then their values.
pub fn encode_attrs(w: &mut Writer, a: &CharAttrs) {
    let mut m = 0u16;
    for (i, on) in [a.bold.is_some(), a.italic.is_some(), a.underline.is_some(), a.strike.is_some(), a.size.is_some(), a.color.is_some(), a.highlight.is_some(), a.family.is_some(), a.style.is_some(), a.link.is_some(), a.object.is_some(), a.vert.is_some(), a.note.is_some(), a.comments.is_some(), a.rev.is_some()].into_iter().enumerate() {
        if on {
            m |= 1 << i;
        }
    }
    // Bit 15: a second mask follows (tracked deletions and formatting).
    let float = a.object.and_then(|o| o.float);
    let ext = a.deleted.is_some() as u16 | (a.fmt.is_some() as u16) << 1 | (a.conflict.is_some() as u16) << 2 | (a.field.is_some() as u16) << 3 | (float.is_some() as u16) << 4 | (a.object.is_some_and(|o| o.shape) as u16) << 5 | (a.lang_ea.is_some() as u16) << 6 | ((look_mask(a) != 0) as u16) << 7;
    if ext != 0 {
        m |= 1 << 15;
    }
    w.u16(m);
    if ext != 0 {
        w.u16(ext);
    }
    for b in [a.bold, a.italic, a.underline, a.strike].into_iter().flatten() {
        w.u8(b as u8);
    }
    if let Some(s) = a.size {
        w.u16(s);
    }
    if let Some(c) = a.color {
        w.u32(c);
    }
    if let Some(c) = a.highlight {
        w.u32(c);
    }
    if let Some(f) = &a.family {
        w.str(f);
    }
    if let Some(s) = a.style {
        w.u16(s);
    }
    if let Some(l) = &a.link {
        w.str(l);
    }
    if let Some(o) = &a.object {
        w.var(o.image as u64);
        w.var(o.width as u64);
        w.var(o.height as u64);
    }
    if let Some(v) = a.vert {
        w.u8(match v {
            VertAlign::Baseline => 0,
            VertAlign::Super => 1,
            VertAlign::Sub => 2,
        });
    }
    if let Some(n) = a.note {
        w.var(n as u64);
    }
    if let Some(c) = &a.comments {
        w.var(c.len() as u64);
        for &id in c.iter() {
            w.var(id as u64);
        }
    }
    if let Some(v) = a.rev {
        w.var(v as u64);
    }
    if let Some(v) = a.deleted {
        w.var(v as u64);
    }
    if let Some(f) = a.fmt {
        w.var(f.rev as u64);
        w.var(f.old as u64);
    }
    if let Some(c) = a.conflict {
        w.var(c as u64);
    }
    if let Some(f) = a.field {
        let (k, b) = match f {
            Field::PageRef(b) => (0, b),
            Field::TextRef(b) => (1, b),
        };
        w.u8(k);
        w.var(b);
    }
    if let Some(f) = float {
        w.u8(f.wrap as u8);
        w.u8(f.h_rel as u8);
        w.u8(f.h_align as u8);
        w.ivar(f.x as i64);
        w.u8(f.v_rel as u8);
        w.ivar(f.y as i64);
        w.ivar(f.gap as i64);
    }
    if let Some(l) = &a.lang_ea {
        w.str(l);
    }
    let lm = look_mask(a);
    if lm != 0 {
        w.u8(lm);
        if let Some(c) = a.caps {
            w.u8(c as u8);
        }
        for b in [a.hidden, a.dstrike].into_iter().flatten() {
            w.u8(b as u8);
        }
        if let Some(v) = a.spacing {
            w.ivar(v as i64);
        }
        if let Some(v) = a.scale {
            w.u16(v);
        }
        if let Some(v) = a.effects {
            w.u8(v);
        }
    }
}

/// Which of the letters' look fields are set (ext bit 7's own mask).
fn look_mask(a: &CharAttrs) -> u8 {
    [a.caps.is_some(), a.hidden.is_some(), a.dstrike.is_some(), a.spacing.is_some(), a.scale.is_some(), a.effects.is_some()].into_iter().enumerate().fold(0, |m, (i, on)| m | (on as u8) << i)
}

pub fn decode_attrs(r: &mut Reader) -> Result<CharAttrs, WireError> {
    let m = r.u16()?;
    let ext = if m >> 15 != 0 { r.u16()? } else { 0 };
    if ext >> 8 != 0 || (m >> 15 != 0 && ext == 0) || (ext >> 4 & 3 != 0 && m & (1 << 10) == 0) {
        return Err(WireError::Invalid("attribute mask"));
    }
    let on = |i: u32| m & (1 << i) != 0;
    let b = |i: u32, r: &mut Reader| -> Result<Option<bool>, WireError> {
        if !on(i) {
            return Ok(None);
        }
        match r.u8()? {
            0 => Ok(Some(false)),
            1 => Ok(Some(true)),
            _ => Err(WireError::Invalid("flag")),
        }
    };
    let mut a = CharAttrs { bold: b(0, r)?, italic: b(1, r)?, underline: b(2, r)?, strike: b(3, r)?, ..Default::default() };
    if on(4) {
        let s = r.u16()?;
        if !(2..=3276).contains(&s) {
            return Err(WireError::Invalid("font size"));
        }
        a.size = Some(s);
    }
    if on(5) {
        a.color = Some(r.u32()? & 0xFF_FFFF);
    }
    if on(6) {
        a.highlight = Some(r.u32()? & 0xFF_FFFF);
    }
    if on(7) {
        a.family = Some(Arc::from(r.str("font family", MAX_NAME)?));
    }
    if on(8) {
        a.style = Some(r.u16()?);
    }
    if on(9) {
        a.link = Some(Arc::from(r.str("link", 8192)?));
    }
    if on(10) {
        let image = r.var()?;
        let (w, h) = (r.var()?, r.var()?);
        let side = 1..=MAX_OBJECT_TWIPS as u64;
        if image > u32::MAX as u64 || !side.contains(&w) || !side.contains(&h) {
            return Err(WireError::Invalid("picture frame"));
        }
        a.object = Some(ObjectRef { image: image as u32, width: w as u32, height: h as u32, float: None, shape: false });
    }
    if on(11) {
        a.vert = Some(match r.u8()? {
            0 => VertAlign::Baseline,
            1 => VertAlign::Super,
            2 => VertAlign::Sub,
            _ => return Err(WireError::Invalid("vertical alignment")),
        });
    }
    if on(12) {
        a.note = Some(r.len("note", MAX_NOTES as u64 - 1)? as NoteId);
    }
    if on(13) {
        let n = r.count("comments", 1 << 16, 1)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(r.len("comment", MAX_COMMENTS as u64 - 1)? as CommentId);
        }
        if v.is_empty() || v.windows(2).any(|w| w[0] >= w[1]) {
            return Err(WireError::Invalid("comment list"));
        }
        a.comments = Some(v.into());
    }
    if on(14) {
        a.rev = Some(r.len("revision", MAX_REVISIONS as u64 - 1)? as RevId);
    }
    if ext & 1 != 0 {
        a.deleted = Some(r.len("deletion", MAX_DELETIONS as u64 - 1)? as DelId);
    }
    if ext & 2 != 0 {
        let rev = r.len("revision", MAX_REVISIONS as u64 - 1)? as RevId;
        let old = r.len("attribute", MAX_ATTRS)? as AttrId;
        a.fmt = Some(FmtChange { rev, old });
    }
    if ext & 4 != 0 {
        a.conflict = Some(r.len("conflict", MAX_CONFLICTS as u64 - 1)? as ConflictId);
    }
    if ext & 8 != 0 {
        let k = r.u8()?;
        let b = r.var()?;
        a.field = Some(match k {
            0 => Field::PageRef(b),
            1 => Field::TextRef(b),
            _ => return Err(WireError::Invalid("field")),
        });
    }
    if ext >> 4 & 1 != 0 {
        let wrap = match r.u8()? {
            0 => Wrap::Square,
            1 => Wrap::TopBottom,
            2 => Wrap::Behind,
            3 => Wrap::InFront,
            _ => return Err(WireError::Invalid("wrap")),
        };
        let rel = |v: u8| match v {
            0 => Ok(Rel::Text),
            1 => Ok(Rel::Margin),
            2 => Ok(Rel::Page),
            _ => Err(WireError::Invalid("float position")),
        };
        let h_rel = rel(r.u8()?)?;
        let h_align = match r.u8()? {
            0 => HAlign::Start,
            1 => HAlign::Center,
            2 => HAlign::End,
            3 => HAlign::Offset,
            _ => return Err(WireError::Invalid("float position")),
        };
        let off = |v: i64| if (-(MAX_FLOAT_OFFSET as i64)..=MAX_FLOAT_OFFSET as i64).contains(&v) { Ok(v as i32) } else { Err(WireError::Invalid("float offset")) };
        let x = off(r.ivar()?)?;
        let v_rel = rel(r.u8()?)?;
        let y = off(r.ivar()?)?;
        let gap = r.ivar()?;
        if !(0..=2880).contains(&gap) {
            return Err(WireError::Invalid("float gap"));
        }
        if let Some(o) = a.object.as_mut() {
            o.float = Some(Float { wrap, h_rel, h_align, x, v_rel, y, gap: gap as i32 });
        }
    }
    if ext >> 5 & 1 != 0 {
        if let Some(o) = a.object.as_mut() {
            o.shape = true;
        }
    }
    if ext >> 6 & 1 != 0 {
        let l = r.str("language", 35)?;
        if l.is_empty() || !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(WireError::Invalid("language"));
        }
        a.lang_ea = Some(Arc::from(l));
    }
    if ext >> 7 & 1 != 0 {
        let lm = r.u8()?;
        if lm == 0 || lm >> 6 != 0 {
            return Err(WireError::Invalid("letters mask"));
        }
        if lm & 1 != 0 {
            a.caps = Some(match r.u8()? {
                0 => Caps::Normal,
                1 => Caps::All,
                2 => Caps::Small,
                _ => return Err(WireError::Invalid("capitals")),
            });
        }
        let flag = |r: &mut Reader| match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError::Invalid("flag")),
        };
        if lm & 2 != 0 {
            a.hidden = Some(flag(r)?);
        }
        if lm & 4 != 0 {
            a.dstrike = Some(flag(r)?);
        }
        if lm & 8 != 0 {
            let v = r.ivar()?;
            if v.unsigned_abs() > MAX_SPACING as u64 {
                return Err(WireError::Invalid("letter spacing"));
            }
            a.spacing = Some(v as i16);
        }
        if lm & 16 != 0 {
            let v = r.u16()?;
            if !SCALE_RANGE.contains(&v) {
                return Err(WireError::Invalid("letter width"));
            }
            a.scale = Some(v);
        }
        if lm & 32 != 0 {
            let v = r.u8()?;
            if v & !EFFECTS != 0 {
                return Err(WireError::Invalid("text effects"));
            }
            a.effects = Some(v);
        }
    }
    Ok(a)
}

/// Paragraph properties: a mask of the fields set, then their values.
pub fn encode_props(w: &mut Writer, p: &ParaProps) {
    let mut m = 0u16;
    for (i, on) in [p.align.is_some(), p.indent_start.is_some(), p.indent_end.is_some(), p.first_line.is_some(), p.space_before.is_some(), p.space_after.is_some(), p.line_spacing.is_some(), p.keep_with_next.is_some(), p.keep_together.is_some(), p.page_break_before.is_some(), p.widows.is_some(), p.outline_level.is_some(), p.list.is_some(), p.cell.is_some(), p.rev.is_some()].into_iter().enumerate() {
        if on {
            m |= 1 << i;
        }
    }
    // Bit 15: a second mask follows (tab stops, table of contents entry).
    let more = box_mask(p);
    let ext = p.tabs.is_some() as u16 | (p.toc.is_some() as u16) << 1 | (p.section.is_some() as u16) << 2 | (p.section.is_some_and(|x| x.page.is_some()) as u16) << 3 | (p.rtl.is_some() as u16) << 4 | ((more != 0) as u16) << 5;
    if ext != 0 {
        m |= 1 << 15;
    }
    w.u16(m);
    if ext != 0 {
        w.u16(ext);
    }
    // Bit 5 of the second mask: a third (borders, shading, drop cap, list
    // start, line numbers, hyphenation) follows.
    if more != 0 {
        w.u8(more);
    }
    if let Some(a) = p.align {
        w.u8(a as u8);
    }
    for v in [p.indent_start, p.indent_end, p.first_line, p.space_before, p.space_after].into_iter().flatten() {
        w.ivar(v as i64);
    }
    if let Some(s) = p.line_spacing {
        match s {
            LineSpacing::Multiple(v) => {
                w.u8(0);
                w.var(v as u64);
            }
            LineSpacing::Exact(v) => {
                w.u8(1);
                w.ivar(v as i64);
            }
            LineSpacing::AtLeast(v) => {
                w.u8(2);
                w.ivar(v as i64);
            }
        }
    }
    for b in [p.keep_with_next, p.keep_together, p.page_break_before, p.widows].into_iter().flatten() {
        w.u8(b as u8);
    }
    if let Some(l) = p.outline_level {
        w.u8(l);
    }
    if let Some(l) = p.list {
        match l {
            None => w.u8(0),
            Some(l) => {
                w.u8(match l.kind {
                    ListKind::Bullet => 1,
                    ListKind::Numbered => 2,
                });
                w.var(l.id as u64);
                w.u8(l.level);
            }
        }
    }
    if let Some(c) = p.cell {
        w.var(c.table as u64);
        w.var(c.row as u64);
        w.var(c.col as u64);
        w.var(c.width as u64);
    }
    if let Some(v) = p.rev {
        w.var(v as u64);
    }
    if let Some(t) = &p.tabs {
        w.var(t.len() as u64);
        for s in t {
            w.ivar(s.pos as i64);
            w.u8(match s.align {
                TabAlign::Start => 0,
                TabAlign::Center => 1,
                TabAlign::End => 2,
                TabAlign::Decimal => 3,
            });
            w.u8(match s.leader {
                Leader::None => 0,
                Leader::Dots => 1,
                Leader::Dashes => 2,
                Leader::Line => 3,
            });
        }
    }
    if let Some(l) = p.toc {
        w.u8(l);
    }
    if let Some(x) = p.section {
        w.u8(match x.start {
            SectionStart::NextPage => 0,
            SectionStart::Continuous => 1,
            SectionStart::OddPage => 2,
            SectionStart::EvenPage => 3,
        });
        w.u8(x.columns);
        w.ivar(x.gap as i64);
        w.u8(x.rule as u8);
        if let Some(g) = &x.page {
            encode_page(w, g);
        }
    }
    if let Some(v) = p.rtl {
        w.u8(v as u8);
    }
    if let Some(b) = &p.borders {
        for x in b.sides() {
            match x {
                None => w.u8(0),
                Some(x) => {
                    w.u8(1);
                    encode_border(w, &x);
                }
            }
        }
    }
    if let Some(v) = p.shading {
        match v {
            None => w.u8(0),
            Some(c) => {
                w.u8(1);
                w.var(c as u64 & 0xff_ffff);
            }
        }
    }
    if let Some(d) = p.drop_cap {
        match d {
            None => w.u8(0),
            Some(d) => {
                w.u8(1 + d.margin as u8);
                w.u8(d.lines);
                w.ivar(d.distance as i64);
            }
        }
    }
    if let Some(v) = p.list_start {
        w.var(v as u64);
    }
    for v in [p.line_numbers, p.hyphenate].into_iter().flatten() {
        w.u8(v as u8);
    }
    if more & 64 != 0 {
        if let Some(c) = &p.cell {
            encode_cell_extras(w, c);
        }
    }
    if more & 128 != 0 {
        if let Some(c) = &p.inner {
            w.var(c.table as u64);
            w.var(c.row as u64);
            w.var(c.col as u64);
            w.var(c.width as u64);
            encode_cell_extras(w, c);
        }
    }
}

fn box_mask(p: &ParaProps) -> u8 {
    let cell_extras = p.cell.is_some_and(|c| !c.look.is_default() || c.row_look != RowLook::default() || c.merged_up || c.span != 1);
    p.borders.is_some() as u8 | (p.shading.is_some() as u8) << 1 | (p.drop_cap.is_some() as u8) << 2 | (p.list_start.is_some() as u8) << 3 | (p.line_numbers.is_some() as u8) << 4 | (p.hyphenate.is_some() as u8) << 5 | (cell_extras as u8) << 6 | ((p.cell.is_some() && p.inner.is_some()) as u8) << 7
}

/// `0`, or `1` then a border.
fn encode_opt_border(w: &mut Writer, b: Option<Border>) {
    match b {
        None => w.u8(0),
        Some(x) => {
            w.u8(1);
            encode_border(w, &x);
        }
    }
}

fn decode_opt_border(r: &mut Reader) -> Result<Option<Border>, WireError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(decode_border(r)?)),
        _ => Err(WireError::Invalid("border")),
    }
}

fn encode_opt_color(w: &mut Writer, c: Option<u32>) {
    match c {
        None => w.u8(0),
        Some(c) => {
            w.u8(1);
            w.var(c as u64 & 0xff_ffff);
        }
    }
}

fn encode_pad(w: &mut Writer, p: CellPad) {
    let p = p.bounded();
    for v in [p.top, p.bottom, p.start, p.end] {
        w.var(v as u64);
    }
}

fn decode_pad(r: &mut Reader) -> Result<CellPad, WireError> {
    let mut v = [0u16; 4];
    for x in v.iter_mut() {
        *x = r.len("cell padding", MAX_CELL_PAD as u64)? as u16;
    }
    Ok(CellPad { top: v[0], bottom: v[1], start: v[2], end: v[3] })
}

/// A cell's own look, its row's, merged up and its span.
fn encode_cell_extras(w: &mut Writer, c: &CellRef) {
    let b = c.look.borders;
    for side in [b.top, b.bottom, b.start, b.end] {
        match side {
            None => w.u8(0),
            Some(None) => w.u8(1),
            Some(Some(x)) => {
                w.u8(2);
                encode_border(w, &x);
            }
        }
    }
    match c.look.shading {
        None => w.u8(0),
        Some(None) => w.u8(1),
        Some(Some(x)) => {
            w.u8(2);
            w.var(x as u64 & 0xff_ffff);
        }
    }
    w.u8(match c.look.valign {
        VAlign::Top => 0,
        VAlign::Center => 1,
        VAlign::Bottom => 2,
    });
    match c.look.pad {
        None => w.u8(0),
        Some(p) => {
            w.u8(1);
            encode_pad(w, p);
        }
    }
    let rl = c.row_look;
    w.u8(rl.header as u8 | (rl.cant_split as u8) << 1 | (rl.exact as u8) << 2 | (c.merged_up as u8) << 3);
    w.var(rl.height as u64);
    w.u8(c.span.clamp(1, MAX_CELLS as u8));
}

fn decode_cell_extras(r: &mut Reader, c: &mut CellRef) -> Result<(), WireError> {
    let mut sides = [None; 4];
    for x in sides.iter_mut() {
        *x = match r.u8()? {
            0 => None,
            1 => Some(None),
            2 => Some(Some(decode_border(r)?)),
            _ => return Err(WireError::Invalid("cell border")),
        };
    }
    let [top, bottom, start, end] = sides;
    let shading = match r.u8()? {
        0 => None,
        1 => Some(None),
        2 => Some(Some(r.len("colour", 0xff_ffff)? as u32)),
        _ => return Err(WireError::Invalid("cell shading")),
    };
    let valign = match r.u8()? {
        0 => VAlign::Top,
        1 => VAlign::Center,
        2 => VAlign::Bottom,
        _ => return Err(WireError::Invalid("cell alignment")),
    };
    let pad = match r.u8()? {
        0 => None,
        1 => Some(decode_pad(r)?),
        _ => return Err(WireError::Invalid("cell padding")),
    };
    let bits = r.u8()?;
    if bits > 15 {
        return Err(WireError::Invalid("row flags"));
    }
    let height = r.len("row height", u16::MAX as u64)? as u16;
    let span = r.u8()?;
    if span == 0 || span as usize > MAX_CELLS {
        return Err(WireError::Invalid("cell span"));
    }
    c.look = CellLook { borders: CellBorders { top, bottom, start, end }, shading, valign, pad };
    c.row_look = RowLook { header: bits & 1 != 0, cant_split: bits & 2 != 0, height, exact: bits & 4 != 0 };
    c.merged_up = bits & 8 != 0;
    c.span = span;
    Ok(())
}

/// A table's own look.
pub fn encode_table(w: &mut Writer, t: &TableLook) {
    match &t.style {
        None => w.u8(0),
        Some(n) => {
            w.u8(1);
            w.str(n);
        }
    }
    match &t.borders {
        None => w.u8(0),
        Some(b) => {
            w.u8(1);
            for x in b.sides() {
                encode_opt_border(w, x);
            }
        }
    }
    encode_opt_color(w, t.shading);
    match t.pad {
        None => w.u8(0),
        Some(p) => {
            w.u8(1);
            encode_pad(w, p);
        }
    }
    w.u8(match t.align {
        HAlign::Start | HAlign::Offset => 0,
        HAlign::Center => 1,
        HAlign::End => 2,
    });
    w.ivar(t.indent.clamp(-MAX_TWIPS_TABLE, MAX_TWIPS_TABLE) as i64);
    w.u8(t.look & 63);
    w.u8(t.autofit as u8);
}

pub fn decode_table(r: &mut Reader) -> Result<TableLook, WireError> {
    let style = match r.u8()? {
        0 => None,
        1 => Some(r.str("table style", 256)?.into()),
        _ => return Err(WireError::Invalid("table style")),
    };
    let borders = match r.u8()? {
        0 => None,
        1 => {
            let mut v = [None; 6];
            for x in v.iter_mut() {
                *x = decode_opt_border(r)?;
            }
            let [top, bottom, start, end, inside_h, inside_v] = v;
            Some(TableBorders { top, bottom, start, end, inside_h, inside_v })
        }
        _ => return Err(WireError::Invalid("table borders")),
    };
    let shading = decode_color(r)?;
    let pad = match r.u8()? {
        0 => None,
        1 => Some(decode_pad(r)?),
        _ => return Err(WireError::Invalid("table padding")),
    };
    let align = match r.u8()? {
        0 => HAlign::Start,
        1 => HAlign::Center,
        2 => HAlign::End,
        _ => return Err(WireError::Invalid("table alignment")),
    };
    let indent = r.ivar()?;
    if indent.abs() > MAX_TWIPS_TABLE as i64 {
        return Err(WireError::Invalid("table indent"));
    }
    let look = r.u8()?;
    let autofit = r.u8()?;
    if look > 63 || autofit > 1 {
        return Err(WireError::Invalid("table flags"));
    }
    Ok(TableLook { style, borders, shading, pad, align, indent: indent as i32, look, autofit: autofit == 1 })
}

fn encode_region(w: &mut Writer, x: &RegionLook) {
    encode_opt_color(w, x.shading);
    w.u8(x.bold as u8);
    encode_opt_color(w, x.color);
    encode_opt_border(w, x.rule);
}

fn decode_region(r: &mut Reader) -> Result<RegionLook, WireError> {
    let shading = decode_color(r)?;
    let bold = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(WireError::Invalid("bold")),
    };
    Ok(RegionLook { shading, bold, color: decode_color(r)?, rule: decode_opt_border(r)? })
}

/// A table style.
pub fn encode_table_style(w: &mut Writer, t: &TableStyle) {
    for x in t.borders.sides() {
        encode_opt_border(w, x);
    }
    encode_opt_color(w, t.shading);
    for x in [&t.header, &t.total, &t.first_col, &t.last_col] {
        encode_region(w, x);
    }
    encode_opt_color(w, t.band_row);
    encode_opt_color(w, t.band_col);
}

pub fn decode_table_style(r: &mut Reader) -> Result<TableStyle, WireError> {
    let mut v = [None; 6];
    for x in v.iter_mut() {
        *x = decode_opt_border(r)?;
    }
    let [top, bottom, start, end, inside_h, inside_v] = v;
    let shading = decode_color(r)?;
    let (header, total, first_col, last_col) = (decode_region(r)?, decode_region(r)?, decode_region(r)?, decode_region(r)?);
    Ok(TableStyle { borders: TableBorders { top, bottom, start, end, inside_h, inside_v }, shading, header, total, first_col, last_col, band_row: decode_color(r)?, band_col: decode_color(r)? })
}

fn encode_border(w: &mut Writer, b: &Border) {
    w.u8(match b.style {
        BorderStyle::Single => 0,
        BorderStyle::Double => 1,
        BorderStyle::Dotted => 2,
        BorderStyle::Dashed => 3,
        BorderStyle::Thick => 4,
    });
    w.u8(b.width.min(MAX_BORDER_WIDTH) as u8);
    w.u8(b.space.min(MAX_BORDER_SPACE) as u8);
    match b.color {
        None => w.u8(0),
        Some(c) => {
            w.u8(1);
            w.var(c as u64 & 0xff_ffff);
        }
    }
}

fn decode_border(r: &mut Reader) -> Result<Border, WireError> {
    let style = match r.u8()? {
        0 => BorderStyle::Single,
        1 => BorderStyle::Double,
        2 => BorderStyle::Dotted,
        3 => BorderStyle::Dashed,
        4 => BorderStyle::Thick,
        _ => return Err(WireError::Invalid("border style")),
    };
    let width = r.u8()? as u16;
    let space = r.u8()? as u16;
    if !(1..=MAX_BORDER_WIDTH).contains(&width) || space > MAX_BORDER_SPACE {
        return Err(WireError::Invalid("border"));
    }
    let color = decode_color(r)?;
    Ok(Border { style, width, color, space })
}

/// `0`, or `1` then an sRGB colour.
fn decode_color(r: &mut Reader) -> Result<Option<u32>, WireError> {
    match r.u8()? {
        0 => Ok(None),
        1 => Ok(Some(r.len("colour", 0xff_ffff)? as u32)),
        _ => Err(WireError::Invalid("colour")),
    }
}

/// A list's definition: its levels (each format, label, start, indents).
pub fn encode_list(w: &mut Writer, d: &ListDef) {
    w.var(d.levels.len().min(MAX_LIST_LEVEL as usize + 1) as u64);
    for l in d.levels.iter().take(MAX_LIST_LEVEL as usize + 1) {
        w.u8(match l.format {
            NumFormat::Decimal => 0,
            NumFormat::DecimalZero => 1,
            NumFormat::LowerLetter => 2,
            NumFormat::UpperLetter => 3,
            NumFormat::LowerRoman => 4,
            NumFormat::UpperRoman => 5,
            NumFormat::Bullet => 6,
            NumFormat::None => 7,
        });
        let l = l.clone().bounded();
        w.str(&l.text);
        w.var(l.start as u64);
        w.ivar(l.indent as i64);
        w.ivar(l.hang as i64);
    }
}

pub fn decode_list(r: &mut Reader) -> Result<ListDef, WireError> {
    let n = r.count("list levels", MAX_LIST_LEVEL as u64 + 1, 5)?;
    let mut levels = Vec::with_capacity(n);
    for _ in 0..n {
        let format = match r.u8()? {
            0 => NumFormat::Decimal,
            1 => NumFormat::DecimalZero,
            2 => NumFormat::LowerLetter,
            3 => NumFormat::UpperLetter,
            4 => NumFormat::LowerRoman,
            5 => NumFormat::UpperRoman,
            6 => NumFormat::Bullet,
            7 => NumFormat::None,
            _ => return Err(WireError::Invalid("number format")),
        };
        let text: Arc<str> = r.str("list label", MAX_LEVEL_TEXT as u64)?.into();
        let start = r.len("list start", MAX_LIST_START as u64)? as u32;
        let (indent, hang) = (r.ivar()?, r.ivar()?);
        if indent.abs() > MAX_TWIPS || !(0..=MAX_TWIPS).contains(&hang) {
            return Err(WireError::Invalid("list indent"));
        }
        levels.push(ListLevel { format, text, start, indent: indent as i32, hang: hang as i32 });
    }
    Ok(ListDef { levels })
}

/// Line numbers and hyphenation.
pub fn encode_settings(w: &mut Writer, s: &Settings) {
    w.u8(s.hyphenate as u8);
    match &s.line_numbers {
        None => w.u8(0),
        Some(l) => {
            w.u8(1);
            w.var(l.start as u64);
            w.var(l.count_by as u64);
            w.ivar(l.distance as i64);
            w.u8(match l.restart {
                LineRestart::Page => 0,
                LineRestart::Section => 1,
                LineRestart::Continuous => 2,
            });
        }
    }
    // The page's look; a file from before it ends here.
    w.u8(s.page_color.is_some() as u8 | (s.page_borders.is_some() as u8) << 1 | (s.watermark.is_some() as u8) << 2);
    if let Some(c) = s.page_color {
        w.var(c as u64 & 0xff_ffff);
    }
    if let Some(b) = &s.page_borders {
        w.u8(b.from_page as u8);
        for x in [b.top, b.bottom, b.start, b.end] {
            match x {
                None => w.u8(0),
                Some(x) => {
                    w.u8(1);
                    encode_border(w, &x);
                }
            }
        }
    }
    match &s.watermark {
        None => {}
        Some(Watermark::Text { text, font, color, diagonal }) => {
            w.u8(0);
            w.str(&text[..text.floor_char_boundary(MAX_WATERMARK_TEXT)]);
            w.str(font.as_deref().map_or("", |f| &f[..f.floor_char_boundary(MAX_WATERMARK_TEXT)]));
            w.var(*color as u64 & 0xff_ffff);
            w.u8(*diagonal as u8);
        }
        Some(Watermark::Picture { image, scale, washout }) => {
            w.u8(1);
            w.var(*image as u64);
            w.var((*scale).min(MAX_WATERMARK_SCALE) as u64);
            w.u8(*washout as u8);
        }
    }
}

/// Largest picture watermark, percent of the content box's width.
const MAX_WATERMARK_SCALE: u16 = 500;

pub fn decode_settings(r: &mut Reader) -> Result<Settings, WireError> {
    let hyphenate = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(WireError::Invalid("hyphenation")),
    };
    let line_numbers = match r.u8()? {
        0 => None,
        1 => {
            let start = r.len("line number start", MAX_LIST_START as u64)? as u32;
            let count_by = r.len("line number step", 100)? as u16;
            let distance = r.ivar()?;
            if count_by == 0 || !(0..=MAX_TWIPS).contains(&distance) {
                return Err(WireError::Invalid("line numbers"));
            }
            let restart = match r.u8()? {
                0 => LineRestart::Page,
                1 => LineRestart::Section,
                2 => LineRestart::Continuous,
                _ => return Err(WireError::Invalid("line number restart")),
            };
            Some(LineNumbering { start, count_by, distance: distance as i32, restart })
        }
        _ => return Err(WireError::Invalid("line numbers")),
    };
    let mut s = Settings { line_numbers, hyphenate, ..Settings::default() };
    if r.is_empty() {
        return Ok(s);
    }
    let m = r.u8()?;
    if m > 7 {
        return Err(WireError::Invalid("page look"));
    }
    let flag = |r: &mut Reader| -> Result<bool, WireError> {
        match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError::Invalid("page look flag")),
        }
    };
    if m & 1 != 0 {
        s.page_color = Some(r.len("page colour", 0xff_ffff)? as u32);
    }
    if m & 2 != 0 {
        let from_page = flag(r)?;
        let mut side = || -> Result<Option<Border>, WireError> {
            match r.u8()? {
                0 => Ok(None),
                1 => Ok(Some(decode_border(r)?)),
                _ => Err(WireError::Invalid("page border")),
            }
        };
        let (top, bottom, start, end) = (side()?, side()?, side()?, side()?);
        s.page_borders = Some(PageBorders { top, bottom, start, end, from_page });
    }
    if m & 4 != 0 {
        s.watermark = Some(match r.u8()? {
            0 => {
                let text: Arc<str> = r.str("watermark", MAX_WATERMARK_TEXT as u64)?.into();
                let font = r.str("watermark font", MAX_WATERMARK_TEXT as u64)?;
                let font = (!font.is_empty()).then(|| Arc::<str>::from(font));
                let color = r.len("watermark colour", 0xff_ffff)? as u32;
                Watermark::Text { text, font, color, diagonal: flag(r)? }
            }
            1 => {
                let image = r.len("watermark picture", u32::MAX as u64)? as u32;
                let scale = r.len("watermark scale", MAX_WATERMARK_SCALE as u64)? as u16;
                Watermark::Picture { image, scale, washout: flag(r)? }
            }
            _ => return Err(WireError::Invalid("watermark")),
        });
    }
    Ok(s)
}

/// Twips a length may be (±22 inches).
const MAX_TWIPS: i64 = 31_680;

pub fn decode_props(r: &mut Reader) -> Result<ParaProps, WireError> {
    let m = r.u16()?;
    let ext = if m >> 15 != 0 { r.u16()? } else { 0 };
    if ext >> 6 != 0 || (ext & 8 != 0 && ext & 4 == 0) || (m >> 15 != 0 && ext == 0) {
        return Err(WireError::Invalid("paragraph mask"));
    }
    let more = if ext & 32 != 0 { r.u8()? } else { 0 };
    if ext & 32 != 0 && more == 0 {
        return Err(WireError::Invalid("paragraph mask"));
    }
    let on = |i: u32| m & (1 << i) != 0;
    let mut p = ParaProps::default();
    if on(0) {
        p.align = Some(match r.u8()? {
            0 => Align::Start,
            1 => Align::Center,
            2 => Align::End,
            3 => Align::Justify,
            _ => return Err(WireError::Invalid("alignment")),
        });
    }
    let len = |i: u32, r: &mut Reader| -> Result<Option<i32>, WireError> {
        if !on(i) {
            return Ok(None);
        }
        let v = r.ivar()?;
        if v.abs() > MAX_TWIPS {
            return Err(WireError::Invalid("length"));
        }
        Ok(Some(v as i32))
    };
    p.indent_start = len(1, r)?;
    p.indent_end = len(2, r)?;
    p.first_line = len(3, r)?;
    p.space_before = len(4, r)?;
    p.space_after = len(5, r)?;
    if on(6) {
        p.line_spacing = Some(match r.u8()? {
            0 => LineSpacing::Multiple(r.len("line spacing", 1000)? as u16),
            1 => LineSpacing::Exact(r.ivar()?.clamp(0, MAX_TWIPS) as i32),
            2 => LineSpacing::AtLeast(r.ivar()?.clamp(0, MAX_TWIPS) as i32),
            _ => return Err(WireError::Invalid("line spacing")),
        });
    }
    let b = |i: u32, r: &mut Reader| -> Result<Option<bool>, WireError> {
        if !on(i) {
            return Ok(None);
        }
        match r.u8()? {
            0 => Ok(Some(false)),
            1 => Ok(Some(true)),
            _ => Err(WireError::Invalid("flag")),
        }
    };
    p.keep_with_next = b(7, r)?;
    p.keep_together = b(8, r)?;
    p.page_break_before = b(9, r)?;
    p.widows = b(10, r)?;
    if on(11) {
        p.outline_level = Some(r.u8()?.min(9));
    }
    if on(12) {
        p.list = Some(match r.u8()? {
            0 => None,
            k @ (1 | 2) => {
                let id = r.len("list", u32::MAX as u64)? as u32;
                let level = r.u8()?.min(MAX_LIST_LEVEL);
                Some(ListRef { id, kind: if k == 1 { ListKind::Bullet } else { ListKind::Numbered }, level })
            }
            _ => return Err(WireError::Invalid("list kind")),
        });
    }
    if on(13) {
        let table = r.len("table", u32::MAX as u64)? as u32;
        let row = r.len("row", u32::MAX as u64)? as u32;
        let col = r.len("column", u16::MAX as u64)? as u16;
        let width = r.var()?;
        if !(1..=MAX_OBJECT_TWIPS as u64).contains(&width) {
            return Err(WireError::Invalid("cell width"));
        }
        p.cell = Some(CellRef { table, row, col, width: width as u32, ..CellRef::default() });
    }
    if on(14) {
        p.rev = Some(r.len("revision", MAX_REVISIONS as u64 - 1)? as RevId);
    }
    if ext & 1 != 0 {
        let n = r.count("tab stops", MAX_TABS as u64, 3)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            let pos = r.ivar()?;
            if pos.abs() > MAX_TWIPS {
                return Err(WireError::Invalid("tab stop"));
            }
            let align = match r.u8()? {
                0 => TabAlign::Start,
                1 => TabAlign::Center,
                2 => TabAlign::End,
                3 => TabAlign::Decimal,
                _ => return Err(WireError::Invalid("tab alignment")),
            };
            let leader = match r.u8()? {
                0 => Leader::None,
                1 => Leader::Dots,
                2 => Leader::Dashes,
                3 => Leader::Line,
                _ => return Err(WireError::Invalid("tab leader")),
            };
            v.push(TabStop { pos: pos as i32, align, leader });
        }
        p.tabs = Some(v);
    }
    if ext & 2 != 0 {
        p.toc = Some(r.u8()?.clamp(1, 9));
    }
    if ext & 4 != 0 {
        let start = match r.u8()? {
            0 => SectionStart::NextPage,
            1 => SectionStart::Continuous,
            2 => SectionStart::OddPage,
            3 => SectionStart::EvenPage,
            _ => return Err(WireError::Invalid("section start")),
        };
        let columns = r.u8()?;
        if !(1..=MAX_COLUMNS).contains(&columns) {
            return Err(WireError::Invalid("columns"));
        }
        let gap = r.ivar()?.clamp(0, 2880) as i32;
        let rule = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(WireError::Invalid("flag")),
        };
        // Bit 3: the section's own page setup follows.
        let page = if ext & 8 != 0 { Some(decode_page(r)?) } else { None };
        p.section = Some(Section { start, columns, gap, rule, page });
    }
    // Bit 4: the paragraph's direction.
    if ext & 16 != 0 {
        p.rtl = Some(match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(WireError::Invalid("direction")),
        });
    }
    let flag = |r: &mut Reader| -> Result<bool, WireError> {
        match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError::Invalid("flag")),
        }
    };
    if more & 1 != 0 {
        let mut sides = [None; 5];
        for x in sides.iter_mut() {
            if flag(r)? {
                *x = Some(decode_border(r)?);
            }
        }
        let [top, bottom, start, end, between] = sides;
        p.borders = Some(ParaBorders { top, bottom, start, end, between });
    }
    if more & 2 != 0 {
        p.shading = Some(decode_color(r)?);
    }
    if more & 4 != 0 {
        p.drop_cap = Some(match r.u8()? {
            0 => None,
            k @ (1 | 2) => {
                let lines = r.u8()?;
                let distance = r.ivar()?;
                if !DROP_CAP_LINES.contains(&lines) || !(0..=MAX_TWIPS).contains(&distance) {
                    return Err(WireError::Invalid("drop cap"));
                }
                Some(DropCap { lines, distance: distance as i32, margin: k == 2 })
            }
            _ => return Err(WireError::Invalid("drop cap")),
        });
    }
    if more & 8 != 0 {
        p.list_start = Some(r.len("list start", MAX_LIST_START as u64)? as u32);
    }
    if more & 16 != 0 {
        p.line_numbers = Some(flag(r)?);
    }
    if more & 32 != 0 {
        p.hyphenate = Some(flag(r)?);
    }
    if more & 64 != 0 {
        let Some(c) = p.cell.as_mut() else { return Err(WireError::Invalid("cell look outside a table")) };
        decode_cell_extras(r, c)?;
    }
    if more & 128 != 0 {
        if p.cell.is_none() {
            return Err(WireError::Invalid("inner table outside a table"));
        }
        let table = r.len("table", u32::MAX as u64)? as u32;
        let row = r.len("row", u32::MAX as u64)? as u32;
        let col = r.len("column", u16::MAX as u64)? as u16;
        let width = r.var()?;
        if !(1..=MAX_OBJECT_TWIPS as u64).contains(&width) {
            return Err(WireError::Invalid("cell width"));
        }
        let mut c = CellRef { table, row, col, width: width as u32, ..CellRef::default() };
        decode_cell_extras(r, &mut c)?;
        p.inner = Some(c);
    }
    Ok(p)
}

fn encode_runs(w: &mut Writer, runs: &[(u32, AttrId)]) {
    w.var(runs.len() as u64);
    for &(s, a) in runs {
        w.var(s as u64);
        w.var(a as u64);
    }
}

/// Runs in `text`: starts on character boundaries, in order, attributes in the table.
fn decode_runs(r: &mut Reader, text: &str, attrs: usize) -> Result<Vec<(u32, AttrId)>, WireError> {
    let n = r.count("runs", text.len() as u64 + 1, 2)?;
    let mut v = Vec::with_capacity(n);
    let mut last: Option<u32> = None;
    for _ in 0..n {
        let s = r.len("run start", text.len() as u64)? as u32;
        let a = r.len("attribute", attrs as u64 - 1)? as AttrId;
        if !text.is_char_boundary(s as usize) || last.is_some_and(|l| s <= l) {
            return Err(WireError::Invalid("run start"));
        }
        last = Some(s);
        v.push((s, a));
    }
    Ok(v)
}

fn encode_para(w: &mut Writer, p: &Para) {
    w.u8(1);
    w.var(p.id);
    w.u16(p.style);
    encode_props(w, &p.props);
    w.str(&p.text);
    encode_runs(w, &p.runs);
}

pub fn encode_comment(w: &mut Writer, c: &Comment) {
    w.str(&c.author);
    w.str(&c.initials);
    w.str(&c.date);
    w.str(&c.text);
    w.var(c.parent.map_or(0, |p| p as u64 + 1));
    w.u8(c.done as u8);
}

pub fn decode_comment(r: &mut Reader) -> Result<Comment, WireError> {
    let author = Arc::from(r.str("author", MAX_NAME)?);
    let initials = Arc::from(r.str("initials", MAX_NAME)?);
    let date = Arc::from(r.str("date", 64)?);
    let text = r.str("comment text", MAX_COMMENT_TEXT as u64)?.to_string();
    let parent = match r.len("comment parent", MAX_COMMENTS as u64)? {
        0 => None,
        p => Some((p - 1) as CommentId),
    };
    let done = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(WireError::Invalid("resolved flag")),
    };
    Ok(Comment { author, initials, date, text, parent, done })
}

pub fn encode_revision(w: &mut Writer, v: &Revision) {
    w.str(&v.author);
    w.str(&v.date);
    w.var(v.set as u64);
    w.str(&v.name);
}

pub fn decode_revision(r: &mut Reader) -> Result<Revision, WireError> {
    let author = Arc::from(r.str("author", MAX_NAME)?);
    let date = Arc::from(r.str("date", 64)?);
    let set = r.len("change set", u32::MAX as u64)? as u32;
    let name = Arc::from(r.str("change set name", MAX_NAME)?);
    Ok(Revision { author, date, set, name })
}

pub fn encode_deletion(w: &mut Writer, d: &Deletion) {
    w.var(d.rev as u64);
    w.var(d.pieces.len() as u64);
    for p in &d.pieces {
        if let Some((id, style, props)) = &p.para {
            w.var(*id);
            w.u16(*style);
            encode_props(w, props);
        }
        w.str(&p.text);
        encode_runs(w, &p.runs);
    }
}

pub fn decode_deletion(r: &mut Reader, attrs: usize, styles: usize) -> Result<Deletion, WireError> {
    let rev = r.len("revision", MAX_REVISIONS as u64 - 1)? as RevId;
    let n = r.count("deleted pieces", MAX_BLOCKS, 2)?;
    if n == 0 {
        return Err(WireError::Invalid("empty deletion"));
    }
    let mut pieces = Vec::with_capacity(n);
    for i in 0..n {
        let para = if i > 0 {
            let id = r.var()?;
            let style = r.u16()?;
            if style as usize >= styles {
                return Err(WireError::Invalid("paragraph style"));
            }
            Some((id, style, decode_props(r)?))
        } else {
            None
        };
        let text = r.str("deleted text", MAX_DELETION_TEXT as u64)?.to_string();
        let runs = decode_runs(r, &text, attrs)?;
        pieces.push(DelPiece { para, text, runs });
    }
    Ok(Deletion { rev, pieces })
}

/// A note: its kind, then its paragraphs as blocks.
pub fn encode_note(w: &mut Writer, n: &Note) {
    w.u8(match n.kind {
        NoteKind::Foot => 0,
        NoteKind::End => 1,
    });
    w.var(n.paras.len() as u64);
    for p in &n.paras {
        encode_para(w, p);
    }
}

pub fn decode_note(r: &mut Reader, attrs: usize, styles: usize) -> Result<Note, WireError> {
    let kind = match r.u8()? {
        0 => NoteKind::Foot,
        1 => NoteKind::End,
        _ => return Err(WireError::Invalid("note kind")),
    };
    let k = r.count("note paragraphs", MAX_NOTE_PARAS as u64, 7)?;
    if k == 0 {
        return Err(WireError::Invalid("empty note"));
    }
    let mut paras = Vec::with_capacity(k);
    for _ in 0..k {
        let Block::Para(p) = decode_block(r, attrs, styles)?;
        paras.push(p);
    }
    Ok(Note { kind, paras })
}

fn decode_block(r: &mut Reader, attrs: usize, styles: usize) -> Result<Block, WireError> {
    match r.u8()? {
        1 => {
            let id = r.var()?;
            let style = r.u16()?;
            if style as usize >= styles {
                return Err(WireError::Invalid("paragraph style"));
            }
            let props = decode_props(r)?;
            let text = r.str("paragraph text", MAX_TEXT)?.to_string();
            let runs = decode_runs(r, &text, attrs)?;
            let mut p = Para { id, text, runs, style, props };
            if p.runs.first().is_none_or(|r| r.0 != 0) {
                p.runs.insert(0, (0, 0));
            }
            p.normalize();
            Ok(Block::Para(p))
        }
        _ => Err(WireError::Invalid("block kind")),
    }
}

fn encode_hf(w: &mut Writer, hf: &Option<HeaderFooter>) {
    match hf {
        None => w.u8(0),
        Some(h) => {
            w.u8(1);
            w.str(&h.text);
            w.u8(h.align as u8);
        }
    }
}

/// A header's and a footer's own paragraphs (a count each, then blocks).
/// A drawn shape: outline, fill, outline style, turn, text box and paragraphs.
pub fn encode_shape(w: &mut Writer, s: &crate::model::ShapeData) {
    use crate::model::{Geom, PathCmd};
    match &s.geom {
        Geom::Preset { name, adj } => {
            w.u8(0);
            w.str(name);
            w.var(adj.len() as u64);
            for (n, v) in adj {
                w.str(n);
                w.ivar(*v);
            }
        }
        Geom::Custom(paths) => {
            w.u8(1);
            w.var(paths.len() as u64);
            for p in paths {
                w.ivar(p.w);
                w.ivar(p.h);
                w.u8(p.filled as u8 | (p.stroked as u8) << 1);
                w.var(p.cmds.len() as u64);
                for c in &p.cmds {
                    let (tag, v): (u8, &[i64]) = match c {
                        PathCmd::Move(x, y) => (0, &[*x, *y][..]),
                        PathCmd::Line(x, y) => (1, &[*x, *y][..]),
                        PathCmd::Cubic(a, b, c, d, e, f) => (2, &[*a, *b, *c, *d, *e, *f][..]),
                        PathCmd::Close => (3, &[][..]),
                    };
                    w.u8(tag);
                    for &x in v {
                        w.ivar(x);
                    }
                }
            }
        }
    }
    w.u8(s.fill.is_some() as u8 | (s.line.is_some() as u8) << 1 | (s.flip_h as u8) << 2 | (s.flip_v as u8) << 3);
    if let Some(f) = s.fill {
        w.u32(f);
    }
    if let Some(l) = s.line {
        w.u32(l.color);
        w.ivar(l.width as i64);
        w.u8(l.dash as u8);
        w.u8(l.head as u8 | (l.tail as u8) << 4);
    }
    w.ivar(s.rot as i64);
    for i in s.inset {
        w.ivar(i as i64);
    }
    w.u8(s.anchor as u8);
    w.var(s.paras.len() as u64);
    for p in &s.paras {
        encode_para(w, p);
    }
    w.var(s.kids.len() as u64);
    for k in &s.kids {
        for v in [k.x, k.y, k.w, k.h] {
            w.ivar(v as i64);
        }
        encode_shape(w, &k.shape);
    }
}

/// Most guides' worth of adjust values, paths and steps a shape keeps.
const MAX_SHAPE_ADJ: u64 = 64;
const MAX_SHAPE_PATHS: u64 = 64;
const MAX_PATH_CMDS: u64 = 1 << 16;
const MAX_SHAPE_COORD: i64 = 1 << 40;

pub fn decode_shape(r: &mut Reader, attrs: usize, styles: usize) -> Result<crate::model::ShapeData, WireError> {
    let mut budget = crate::model::MAX_GROUP_SHAPES;
    decode_shape_in(r, attrs, styles, 0, &mut budget)
}

fn decode_shape_in(r: &mut Reader, attrs: usize, styles: usize, depth: usize, budget: &mut usize) -> Result<crate::model::ShapeData, WireError> {
    use crate::model::{Dash, Geom, PathCmd, ShapeData, ShapeLine, ShapePath, TextAnchor, MAX_SHAPE_PARAS};
    let coord = |r: &mut Reader| -> Result<i64, WireError> {
        let v = r.ivar()?;
        if v.abs() > MAX_SHAPE_COORD {
            return Err(WireError::Invalid("shape coordinate"));
        }
        Ok(v)
    };
    let geom = match r.u8()? {
        0 => {
            let name: Arc<str> = Arc::from(r.str("shape name", MAX_NAME)?);
            let n = r.count("adjust values", MAX_SHAPE_ADJ, 2)?;
            let mut adj = Vec::with_capacity(n);
            for _ in 0..n {
                let k: Arc<str> = Arc::from(r.str("adjust name", MAX_NAME)?);
                adj.push((k, coord(r)?));
            }
            Geom::Preset { name, adj }
        }
        1 => {
            let n = r.count("paths", MAX_SHAPE_PATHS, 4)?;
            let mut paths = Vec::with_capacity(n);
            for _ in 0..n {
                let (w, h) = (coord(r)?, coord(r)?);
                let f = r.u8()?;
                if f > 3 {
                    return Err(WireError::Invalid("path flags"));
                }
                let k = r.count("path steps", MAX_PATH_CMDS, 1)?;
                let mut cmds = Vec::with_capacity(k);
                for _ in 0..k {
                    cmds.push(match r.u8()? {
                        0 => PathCmd::Move(coord(r)?, coord(r)?),
                        1 => PathCmd::Line(coord(r)?, coord(r)?),
                        2 => PathCmd::Cubic(coord(r)?, coord(r)?, coord(r)?, coord(r)?, coord(r)?, coord(r)?),
                        3 => PathCmd::Close,
                        _ => return Err(WireError::Invalid("path step")),
                    });
                }
                paths.push(ShapePath { w, h, cmds, filled: f & 1 != 0, stroked: f & 2 != 0 });
            }
            Geom::Custom(paths)
        }
        _ => return Err(WireError::Invalid("shape outline")),
    };
    let flags = r.u8()?;
    if flags > 15 {
        return Err(WireError::Invalid("shape flags"));
    }
    let mut s = ShapeData::new(geom);
    if flags & 1 != 0 {
        s.fill = Some(r.u32()? & 0xFF_FFFF);
    }
    if flags & 2 != 0 {
        let color = r.u32()? & 0xFF_FFFF;
        let width = r.ivar()?;
        if !(0..=2880).contains(&width) {
            return Err(WireError::Invalid("outline width"));
        }
        let dash = match r.u8()? {
            0 => Dash::Solid,
            1 => Dash::Dot,
            2 => Dash::Dash,
            3 => Dash::DashDot,
            4 => Dash::LongDash,
            _ => return Err(WireError::Invalid("dash")),
        };
        let ends = r.u8()?;
        let end = |v: u8| -> Result<crate::model::ArrowEnd, WireError> {
            use crate::model::ArrowEnd::*;
            Ok([None, Triangle, Open, Stealth, Diamond, Oval].get(v as usize).copied().ok_or(WireError::Invalid("line end"))?)
        };
        let (head, tail) = (end(ends & 15)?, end(ends >> 4)?);
        s.line = Some(ShapeLine { color, width: width as i32, dash, head, tail });
    }
    s.flip_h = flags & 4 != 0;
    s.flip_v = flags & 8 != 0;
    s.rot = r.ivar()?.rem_euclid(21_600_000) as i32;
    for i in 0..4 {
        let v = r.ivar()?;
        if !(0..=MAX_TWIPS).contains(&v) {
            return Err(WireError::Invalid("text inset"));
        }
        s.inset[i] = v as i32;
    }
    s.anchor = match r.u8()? {
        0 => TextAnchor::Top,
        1 => TextAnchor::Middle,
        2 => TextAnchor::Bottom,
        _ => return Err(WireError::Invalid("text anchor")),
    };
    let k = r.count("shape paragraphs", MAX_SHAPE_PARAS as u64, 7)?;
    for _ in 0..k {
        let Block::Para(p) = decode_block(r, attrs, styles)?;
        s.paras.push(p);
    }
    let k = r.count("group shapes", crate::model::MAX_GROUP_SHAPES as u64, 8)?;
    if k > 0 && depth >= crate::model::MAX_GROUP_DEPTH {
        return Err(WireError::Invalid("groups too deep"));
    }
    for _ in 0..k {
        if *budget == 0 {
            return Err(WireError::Invalid("too many group shapes"));
        }
        *budget -= 1;
        let mut f = [0i32; 4];
        for v in &mut f {
            let x = r.ivar()?;
            if x.abs() > 100_000_000 {
                return Err(WireError::Invalid("group frame"));
            }
            *v = x as i32;
        }
        let shape = decode_shape_in(r, attrs, styles, depth + 1, budget)?;
        s.kids.push(crate::model::ShapeKid { x: f[0], y: f[1], w: f[2], h: f[3], shape });
    }
    Ok(s)
}

fn encode_margin_paras(w: &mut Writer, header: &Option<HeaderFooter>, footer: &Option<HeaderFooter>) {
    for hf in [header, footer] {
        let paras = hf.as_ref().map_or(&[][..], |h| &h.paras[..]);
        w.var(paras.len() as u64);
        for p in paras {
            encode_para(w, p);
        }
    }
}

fn decode_margin_paras(r: &mut Reader, attrs: usize, styles: usize, header: &mut Option<HeaderFooter>, footer: &mut Option<HeaderFooter>) -> Result<(), WireError> {
    for hf in [header, footer] {
        let k = r.count("margin paragraphs", MAX_MARGIN_PARAS as u64, 7)?;
        let mut paras = Vec::with_capacity(k);
        for _ in 0..k {
            let Block::Para(p) = decode_block(r, attrs, styles)?;
            paras.push(p);
        }
        match hf {
            Some(h) => h.paras = paras,
            None if paras.is_empty() => {}
            None => return Err(WireError::Invalid("margin paragraphs without a header")),
        }
    }
    Ok(())
}

fn decode_hf(r: &mut Reader) -> Result<Option<HeaderFooter>, WireError> {
    Ok(match r.u8()? {
        0 => None,
        1 => {
            let text = r.str("header text", 4096)?.chars().filter(|c| !c.is_control()).collect();
            let align = match r.u8()? {
                0 => Align::Start,
                1 => Align::Center,
                2 => Align::End,
                3 => Align::Justify,
                _ => return Err(WireError::Invalid("alignment")),
            };
            Some(HeaderFooter { text, align, paras: Vec::new() })
        }
        _ => return Err(WireError::Invalid("header")),
    })
}

fn encode_page(w: &mut Writer, g: &PageSetup) {
    for v in [g.width, g.height, g.margin_top, g.margin_bottom, g.margin_start, g.margin_end] {
        w.ivar(v as i64);
    }
}

fn decode_page(r: &mut Reader) -> Result<PageSetup, WireError> {
    let mut g = [0i32; 6];
    for v in g.iter_mut() {
        *v = r.ivar()?.clamp(0, MAX_TWIPS * 2) as i32;
    }
    Ok(PageSetup { width: g[0].max(1440), height: g[1].max(1440), margin_top: g[2], margin_bottom: g[3], margin_start: g[4], margin_end: g[5] })
}

// ---------- ops (the autosave journal) ----------

pub fn encode_op(op: &TextOp) -> Vec<u8> {
    let mut w = Writer::new();
    let frag = |w: &mut Writer, f: &Fragment| {
        w.str(&f.text);
        encode_runs(w, &f.runs);
    };
    match op {
        TextOp::Nop => w.u8(0),
        TextOp::Splice { block, at, del, ins } => {
            w.u8(1);
            w.var(*block as u64);
            w.var(*at as u64);
            w.var(*del as u64);
            frag(&mut w, ins);
        }
        TextOp::Restyle { block, start, end, runs } => {
            w.u8(2);
            w.var(*block as u64);
            w.var(*start as u64);
            w.var(*end as u64);
            encode_runs(&mut w, runs);
        }
        TextOp::Split { block, at, id, style, props } => {
            w.u8(3);
            w.var(*block as u64);
            w.var(*at as u64);
            w.var(*id);
            w.u16(*style);
            encode_props(&mut w, props);
        }
        TextOp::Join { block } => {
            w.u8(4);
            w.var(*block as u64);
        }
        TextOp::SetPara { block, style, props } => {
            w.u8(5);
            w.var(*block as u64);
            w.u16(*style);
            encode_props(&mut w, props);
        }
        TextOp::Insert { at, blocks } => {
            w.u8(6);
            w.var(*at as u64);
            w.var(blocks.len() as u64);
            for b in blocks {
                let Block::Para(p) = b;
                encode_para(&mut w, p);
            }
        }
        TextOp::Remove { at, n } => {
            w.u8(7);
            w.var(*at as u64);
            w.var(*n as u64);
        }
        TextOp::SetPage { page, header, footer } => {
            w.u8(8);
            encode_page(&mut w, page);
            encode_hf(&mut w, header);
            encode_hf(&mut w, footer);
            encode_margin_paras(&mut w, header, footer);
        }
        TextOp::SetComment { id, comment } => {
            w.u8(10);
            w.var(*id as u64);
            match comment {
                None => w.u8(0),
                Some(c) => {
                    w.u8(1);
                    encode_comment(&mut w, c);
                }
            }
        }
        TextOp::SetRevision { id, rev } => {
            w.u8(11);
            w.var(*id as u64);
            match rev {
                None => w.u8(0),
                Some(v) => {
                    w.u8(1);
                    encode_revision(&mut w, v);
                }
            }
        }
        TextOp::SetDeletion { id, del } => {
            w.u8(12);
            w.var(*id as u64);
            match del {
                None => w.u8(0),
                Some(v) => {
                    w.u8(1);
                    encode_deletion(&mut w, v);
                }
            }
        }
        TextOp::SetTracking { on } => {
            w.u8(13);
            w.u8(*on as u8);
        }
        TextOp::SetList { id, def } => {
            w.u8(19);
            w.var(*id as u64);
            match def {
                None => w.u8(0),
                Some(d) => {
                    w.u8(1);
                    encode_list(&mut w, d);
                }
            }
        }
        TextOp::SetSettings { settings } => {
            w.u8(20);
            encode_settings(&mut w, settings);
        }
        TextOp::SetTable { id, look } => {
            w.u8(21);
            w.var(*id as u64);
            match look {
                None => w.u8(0),
                Some(t) => {
                    w.u8(1);
                    encode_table(&mut w, t);
                }
            }
        }
        TextOp::SetTableStyle { name, style } => {
            w.u8(22);
            w.str(name);
            match style {
                None => w.u8(0),
                Some(t) => {
                    w.u8(1);
                    encode_table_style(&mut w, t);
                }
            }
        }
        TextOp::SetConflict { id, alt } => {
            w.u8(14);
            w.var(*id as u64);
            match alt {
                None => w.u8(0),
                Some(v) => {
                    w.u8(1);
                    encode_deletion(&mut w, v);
                }
            }
        }
        TextOp::SetMargins { header, footer, first_page, even_pages } => {
            w.u8(15);
            encode_hf(&mut w, header);
            encode_hf(&mut w, footer);
            encode_margin_paras(&mut w, header, footer);
            for m in [first_page, even_pages] {
                match m {
                    None => w.u8(0),
                    Some(m) => {
                        w.u8(1);
                        encode_hf(&mut w, &m.header);
                        encode_hf(&mut w, &m.footer);
                        encode_margin_paras(&mut w, &m.header, &m.footer);
                    }
                }
            }
        }
        TextOp::SetStyle { id, style } => {
            w.u8(17);
            w.var(*id as u64);
            match style {
                None => w.u8(0),
                Some(s) => {
                    w.u8(1);
                    encode_style(&mut w, s);
                }
            }
        }
        TextOp::PlaceStyle { id, style, stale } => {
            w.u8(18);
            w.var(*id as u64);
            match style {
                None => w.u8(0),
                Some(s) => {
                    w.u8(1);
                    encode_style(&mut w, s);
                }
            }
            w.var(stale.len() as u64);
            for a in stale {
                w.var(*a as u64);
            }
        }
        TextOp::SetShape { id, shape } => {
            w.u8(16);
            w.var(*id as u64);
            match shape {
                None => w.u8(0),
                Some(s) => {
                    w.u8(1);
                    encode_shape(&mut w, s);
                }
            }
        }
        TextOp::SetNote { id, note } => {
            w.u8(9);
            w.var(*id as u64);
            match note {
                None => w.u8(0),
                Some(n) => {
                    w.u8(1);
                    encode_note(&mut w, n);
                }
            }
        }
    }
    w.into_bytes()
}

/// A named style: name, kind, parent and next (0 for none, else index + 1),
/// paragraph and character properties.
pub fn encode_style(w: &mut Writer, s: &Style) {
    w.str(&s.name);
    w.u8(match s.kind {
        StyleKind::Para => 0,
        StyleKind::Char => 1,
    });
    w.var(s.parent.map_or(0, |p| p as u64 + 1));
    w.var(s.next.map_or(0, |p| p as u64 + 1));
    encode_props(w, &s.para);
    encode_attrs(w, &s.chars);
}

/// A style whose references are below `n`.
pub fn decode_style(r: &mut Reader, n: usize) -> Result<Style, WireError> {
    let name: Arc<str> = Arc::from(r.str("style name", MAX_NAME)?);
    let kind = match r.u8()? {
        0 => StyleKind::Para,
        1 => StyleKind::Char,
        _ => return Err(WireError::Invalid("style kind")),
    };
    let idx = |v: u64| -> Result<Option<u16>, WireError> {
        if v == 0 {
            Ok(None)
        } else if v as usize <= n {
            Ok(Some((v - 1) as u16))
        } else {
            Err(WireError::Invalid("style reference"))
        }
    };
    let parent = idx(r.var()?)?;
    let next = idx(r.var()?)?;
    let para = decode_props(r)?;
    let chars = decode_attrs(r)?;
    Ok(Style { name, kind, parent, next, para, chars })
}

/// Decodes an op; `attrs` and `styles` are the table sizes it may refer to.
/// Positions are not checked here: an op that doesn't fit the document becomes
/// a no-op when applied.
pub fn decode_op(b: &[u8], attrs: usize, styles: usize) -> Result<TextOp, WireError> {
    let mut r = Reader::new(b);
    let u32v = |r: &mut Reader, what: &'static str| -> Result<u32, WireError> { Ok(r.len(what, u32::MAX as u64)? as u32) };
    let op = match r.u8()? {
        0 => TextOp::Nop,
        1 => {
            let block = u32v(&mut r, "block")?;
            let at = u32v(&mut r, "offset")?;
            let del = u32v(&mut r, "length")?;
            let text = r.str("text", MAX_TEXT)?.to_string();
            let runs = if text.is_empty() { decode_runs_any(&mut r, attrs)? } else { decode_runs(&mut r, &text, attrs)? };
            TextOp::Splice { block, at, del, ins: Fragment { text, runs } }
        }
        2 => {
            let block = u32v(&mut r, "block")?;
            let start = u32v(&mut r, "start")?;
            let end = u32v(&mut r, "end")?;
            let runs = decode_runs_any(&mut r, attrs)?;
            TextOp::Restyle { block, start, end, runs }
        }
        3 => {
            let block = u32v(&mut r, "block")?;
            let at = u32v(&mut r, "offset")?;
            let id = r.var()?;
            let style = r.u16()?;
            if style as usize >= styles {
                return Err(WireError::Invalid("style"));
            }
            TextOp::Split { block, at, id, style, props: decode_props(&mut r)? }
        }
        4 => TextOp::Join { block: u32v(&mut r, "block")? },
        5 => {
            let block = u32v(&mut r, "block")?;
            let style = r.u16()?;
            if style as usize >= styles {
                return Err(WireError::Invalid("style"));
            }
            TextOp::SetPara { block, style, props: decode_props(&mut r)? }
        }
        6 => {
            let at = u32v(&mut r, "block")?;
            let n = r.count("blocks", MAX_BLOCKS, 5)?;
            let mut blocks = Vec::with_capacity(n);
            for _ in 0..n {
                blocks.push(decode_block(&mut r, attrs, styles)?);
            }
            TextOp::Insert { at, blocks }
        }
        7 => TextOp::Remove { at: u32v(&mut r, "block")?, n: u32v(&mut r, "count")? },
        8 => {
            let page = decode_page(&mut r)?;
            let mut header = decode_hf(&mut r)?;
            let mut footer = decode_hf(&mut r)?;
            // Their own paragraphs (absent in older journals).
            if !r.is_empty() {
                decode_margin_paras(&mut r, attrs, styles, &mut header, &mut footer)?;
            }
            TextOp::SetPage { page, header, footer }
        }
        10 => {
            let id = r.len("comment", MAX_COMMENTS as u64 - 1)? as CommentId;
            let comment = match r.u8()? {
                0 => None,
                1 => Some(decode_comment(&mut r)?),
                _ => return Err(WireError::Invalid("comment")),
            };
            TextOp::SetComment { id, comment }
        }
        17 => {
            // It may add the style one past the table.
            let id = r.len("style", styles.min(MAX_STYLES - 1) as u64)? as u16;
            let style = match r.u8()? {
                0 => None,
                1 => Some(decode_style(&mut r, styles + 1)?),
                _ => return Err(WireError::Invalid("style")),
            };
            TextOp::SetStyle { id, style }
        }
        18 => {
            let id = r.len("style", styles.min(MAX_STYLES - 1) as u64)? as u16;
            let style = match r.u8()? {
                0 => None,
                1 => Some(decode_style(&mut r, styles + 1)?),
                _ => return Err(WireError::Invalid("style")),
            };
            let k = r.count("stale attributes", attrs as u64, 1)?;
            let mut stale = Vec::with_capacity(k);
            for _ in 0..k {
                stale.push(r.len("attribute", attrs.saturating_sub(1) as u64)? as AttrId);
            }
            TextOp::PlaceStyle { id, style, stale }
        }
        16 => {
            let id = r.len("shape", crate::model::MAX_SHAPES as u64 - 1)? as u32;
            let shape = match r.u8()? {
                0 => None,
                1 => Some(decode_shape(&mut r, attrs, styles)?),
                _ => return Err(WireError::Invalid("shape")),
            };
            TextOp::SetShape { id, shape }
        }
        9 => {
            let id = r.len("note", MAX_NOTES as u64 - 1)? as NoteId;
            let note = match r.u8()? {
                0 => None,
                1 => Some(decode_note(&mut r, attrs, styles)?),
                _ => return Err(WireError::Invalid("note")),
            };
            TextOp::SetNote { id, note }
        }
        11 => {
            let id = r.len("revision", MAX_REVISIONS as u64 - 1)? as RevId;
            let rev = match r.u8()? {
                0 => None,
                1 => Some(decode_revision(&mut r)?),
                _ => return Err(WireError::Invalid("revision")),
            };
            TextOp::SetRevision { id, rev }
        }
        12 => {
            let id = r.len("deletion", MAX_DELETIONS as u64 - 1)? as DelId;
            let del = match r.u8()? {
                0 => None,
                1 => Some(decode_deletion(&mut r, attrs, styles)?),
                _ => return Err(WireError::Invalid("deletion")),
            };
            TextOp::SetDeletion { id, del }
        }
        14 => {
            let id = r.len("conflict", MAX_CONFLICTS as u64 - 1)? as ConflictId;
            let alt = match r.u8()? {
                0 => None,
                1 => Some(decode_deletion(&mut r, attrs, styles)?),
                _ => return Err(WireError::Invalid("conflict")),
            };
            TextOp::SetConflict { id, alt }
        }
        15 => {
            let (mut header, mut footer) = (decode_hf(&mut r)?, decode_hf(&mut r)?);
            decode_margin_paras(&mut r, attrs, styles, &mut header, &mut footer)?;
            let mut kinds = [None, None];
            for k in kinds.iter_mut() {
                *k = match r.u8()? {
                    0 => None,
                    1 => {
                        let (mut h, mut f) = (decode_hf(&mut r)?, decode_hf(&mut r)?);
                        decode_margin_paras(&mut r, attrs, styles, &mut h, &mut f)?;
                        Some(crate::model::Margins { header: h, footer: f })
                    }
                    _ => return Err(WireError::Invalid("page margins")),
                };
            }
            let [first_page, even_pages] = kinds;
            TextOp::SetMargins { header, footer, first_page, even_pages }
        }
        13 => TextOp::SetTracking {
            on: match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("tracking flag")),
            },
        },
        19 => {
            let id = r.len("list", u32::MAX as u64)? as u32;
            let def = match r.u8()? {
                0 => None,
                1 => Some(decode_list(&mut r)?),
                _ => return Err(WireError::Invalid("list")),
            };
            TextOp::SetList { id, def }
        }
        20 => TextOp::SetSettings { settings: decode_settings(&mut r)? },
        21 => {
            let id = r.len("table", u32::MAX as u64)? as u32;
            let look = match r.u8()? {
                0 => None,
                1 => Some(decode_table(&mut r)?),
                _ => return Err(WireError::Invalid("table")),
            };
            TextOp::SetTable { id, look }
        }
        22 => {
            let name: Arc<str> = r.str("table style", 256)?.into();
            let style = match r.u8()? {
                0 => None,
                1 => Some(decode_table_style(&mut r)?),
                _ => return Err(WireError::Invalid("table style")),
            };
            TextOp::SetTableStyle { name, style }
        }
        _ => return Err(WireError::Invalid("op")),
    };
    if !r.is_empty() {
        return Err(WireError::Invalid("trailing bytes"));
    }
    Ok(op)
}

/// Runs whose text isn't known here (a restyle): starts in order, attributes in the table.
fn decode_runs_any(r: &mut Reader, attrs: usize) -> Result<Vec<(u32, AttrId)>, WireError> {
    let n = r.count("runs", 1 << 24, 2)?;
    let mut v = Vec::with_capacity(n);
    let mut last: Option<u32> = None;
    for _ in 0..n {
        let s = r.len("run start", u32::MAX as u64)? as u32;
        let a = r.len("attribute", attrs as u64 - 1)? as AttrId;
        if last.is_some_and(|l| s < l) {
            return Err(WireError::Invalid("run start"));
        }
        last = Some(s);
        v.push((s, a));
    }
    Ok(v)
}

// ---------- save ----------

/// Writes `doc`, with the page starts of its last pagination (empty if none).
pub fn save<W: Write>(doc: &TextDoc, checkpoints: &[Checkpoint], out: W) -> io::Result<W> {
    let mut c = ContainerWriter::new(out, DocKind::Write)?;
    let mut w = Writer::new();
    let g = &doc.page;
    for v in [g.width, g.height, g.margin_top, g.margin_bottom, g.margin_start, g.margin_end] {
        w.ivar(v as i64);
    }
    w.var(doc.id_watermark());
    for hf in [&doc.header, &doc.footer] {
        encode_hf(&mut w, hf);
    }
    c.add(DOCUMENT, &w.into_bytes())?;

    let mut w = Writer::new();
    w.var(doc.styles.styles.len() as u64);
    for s in &doc.styles.styles {
        encode_style(&mut w, s);
    }
    c.add(STYLES, &w.into_bytes())?;

    for (i, img) in doc.images.iter().enumerate() {
        let mut w = Writer::new();
        encode_image(&mut w, i as u32, img);
        c.add(IMAGE, &w.into_bytes())?;
    }
    for (i, sh) in doc.shapes.iter() {
        let mut w = Writer::new();
        w.var(i as u64);
        encode_shape(&mut w, sh);
        c.add(SHAPE, &w.into_bytes())?;
    }

    let mut w = Writer::new();
    let n = doc.attrs.len();
    w.var(n as u64 - 1);
    for i in 1..n {
        encode_attrs(&mut w, doc.attrs.get(i as AttrId));
    }
    c.add(ATTRS, &w.into_bytes())?;

    let mut first = 0usize;
    let mut it = doc.blocks.iter().peekable();
    while it.peek().is_some() {
        let mut w = Writer::new();
        w.var(first as u64);
        let chunk: Vec<&Block> = it.by_ref().take(BLOCKS_PER_CHUNK).collect();
        w.var(chunk.len() as u64);
        for b in &chunk {
            let Block::Para(p) = b;
            encode_para(&mut w, p);
        }
        c.add(BLOCKS, &w.into_bytes())?;
        first += chunk.len();
    }

    // The notes that have anchors.
    let numbers = doc.note_numbers();
    if !numbers.is_empty() {
        let mut w = Writer::new();
        let mut ids: Vec<NoteId> = numbers.keys().copied().collect();
        ids.sort_unstable();
        w.var(ids.len() as u64);
        for id in ids {
            w.var(id as u64);
            encode_note(&mut w, doc.notes.get(id).expect("numbered notes exist"));
        }
        c.add(NOTES, &w.into_bytes())?;
    }

    let live = doc.live_comments();
    if !live.is_empty() {
        let mut w = Writer::new();
        w.var(live.len() as u64);
        for id in live {
            w.var(id as u64);
            encode_comment(&mut w, doc.comments.get(id).expect("live comments exist"));
        }
        c.add(COMMENTS, &w.into_bytes())?;
    }

    let revs: Vec<(RevId, &Arc<Revision>)> = doc.revisions.iter().collect();
    if doc.tracking || !revs.is_empty() {
        let mut w = Writer::new();
        w.u8(doc.tracking as u8);
        w.var(revs.len() as u64);
        for (id, v) in revs {
            w.var(id as u64);
            encode_revision(&mut w, v);
        }
        c.add(REVISIONS, &w.into_bytes())?;
    }
    let dels = doc.live_deletions();
    if !dels.is_empty() {
        let mut w = Writer::new();
        w.var(dels.len() as u64);
        for id in dels {
            w.var(id as u64);
            encode_deletion(&mut w, doc.deletions.get(id).expect("live deletions exist"));
        }
        c.add(DELETIONS, &w.into_bytes())?;
    }
    let cons = doc.live_conflicts();
    if !cons.is_empty() {
        let mut w = Writer::new();
        w.var(cons.len() as u64);
        for id in cons {
            w.var(id as u64);
            encode_deletion(&mut w, doc.conflicts.get(id).expect("live conflicts exist"));
        }
        c.add(CONFLICTS, &w.into_bytes())?;
    }

    if [&doc.header, &doc.footer].iter().any(|h| h.as_ref().is_some_and(|h| !h.paras.is_empty())) || doc.first_page.is_some() || doc.even_pages.is_some() {
        let mut w = Writer::new();
        encode_margin_paras(&mut w, &doc.header, &doc.footer);
        for m in [&doc.first_page, &doc.even_pages] {
            match m {
                None => w.u8(0),
                Some(m) => {
                    w.u8(1);
                    encode_hf(&mut w, &m.header);
                    encode_hf(&mut w, &m.footer);
                    encode_margin_paras(&mut w, &m.header, &m.footer);
                }
            }
        }
        c.add(MARGINS, &w.into_bytes())?;
    }

    if !doc.styles.lists.is_empty() {
        let mut w = Writer::new();
        w.var(doc.styles.lists.len().min(MAX_LIST_DEFS) as u64);
        for (id, d) in doc.styles.lists.iter().take(MAX_LIST_DEFS) {
            w.var(*id as u64);
            encode_list(&mut w, d);
        }
        c.add(LISTS, &w.into_bytes())?;
    }
    if doc.settings != Settings::default() {
        let mut w = Writer::new();
        encode_settings(&mut w, &doc.settings);
        c.add(SETTINGS, &w.into_bytes())?;
    }
    if !doc.tables.is_empty() {
        let mut w = Writer::new();
        w.var(doc.tables.len().min(MAX_TABLES) as u64);
        for (id, t) in doc.tables.iter().take(MAX_TABLES) {
            w.var(*id as u64);
            encode_table(&mut w, t);
        }
        c.add(TABLES, &w.into_bytes())?;
    }
    if !doc.styles.tables.is_empty() {
        let mut w = Writer::new();
        w.var(doc.styles.tables.len().min(MAX_TABLE_STYLES) as u64);
        for (name, t) in doc.styles.tables.iter().take(MAX_TABLE_STYLES) {
            w.str(name);
            encode_table_style(&mut w, t);
        }
        c.add(TABLE_STYLES, &w.into_bytes())?;
    }

    if !checkpoints.is_empty() {
        let mut w = Writer::new();
        w.var(checkpoints.len() as u64);
        for cp in checkpoints {
            w.var(cp.block as u64);
            w.var(cp.line as u64);
        }
        c.add(CHECKPOINTS, &w.into_bytes())?;
    }
    c.finish()
}

pub fn save_to_vec(doc: &TextDoc, checkpoints: &[Checkpoint]) -> Vec<u8> {
    save(doc, checkpoints, Vec::new()).expect("writing to a Vec does not fail")
}

/// One picture as the IMAGE chunk and the journal hold it: index, pixel size, bytes.
pub fn encode_image(w: &mut Writer, i: u32, img: &ImageData) {
    w.var(i as u64);
    w.var(img.width as u64);
    w.var(img.height as u64);
    w.bytes(&img.bytes);
}

fn decode_image(data: &[u8]) -> Result<(u64, ImageData), LoadError> {
    let mut r = Reader::new(data);
    let (i, img) = decode_image_record(&mut r)?;
    Ok((i as u64, img))
}

/// A picture from a journal record ([`encode_image`]).
pub fn decode_image_record(r: &mut Reader) -> Result<(u32, ImageData), WireError> {
    let i = r.var()?;
    let (w, h) = (r.var()?, r.var()?);
    let bytes = r.bytes("picture", MAX_IMAGE_BYTES)?;
    if i > u32::MAX as u64 || !(1..=MAX_IMAGE_PX).contains(&w) || !(1..=MAX_IMAGE_PX).contains(&h) || bytes.is_empty() {
        return Err(WireError::Invalid("picture"));
    }
    Ok((i as u32, ImageData { bytes: Arc::from(bytes), width: w as u32, height: h as u32 }))
}

// ---------- load ----------

/// A loaded document and the page starts saved with it.
pub struct Loaded {
    pub doc: TextDoc,
    pub checkpoints: Vec<Checkpoint>,
}

/// Reads a native file. Never panics on any input.
pub fn load(data: &[u8]) -> Result<Loaded, LoadError> {
    let c = ContainerReader::open(data, Limits::default())?;
    if c.kind != DocKind::Write {
        return Err(LoadError::Bad("not a text document"));
    }
    let one = |k: u16, what: &'static str| -> Result<&[u8], LoadError> {
        let mut it = c.entries().iter().enumerate().filter(|(_, e)| e.kind == k);
        let (i, _) = it.next().ok_or(LoadError::Bad(what))?;
        if it.next().is_some() {
            return Err(LoadError::Bad(what));
        }
        Ok(c.chunk(i)?)
    };
    let mut doc = TextDoc::new();
    doc.blocks.remove(0);

    let mut r = Reader::new(one(DOCUMENT, "DOCUMENT chunk")?);
    let mut g = [0i32; 6];
    for v in g.iter_mut() {
        *v = r.ivar()?.clamp(0, MAX_TWIPS * 2) as i32;
    }
    doc.page = PageSetup { width: g[0].max(1440), height: g[1].max(1440), margin_top: g[2], margin_bottom: g[3], margin_start: g[4], margin_end: g[5] };
    let mut watermark = r.var()?;
    if !r.is_empty() {
        doc.header = decode_hf(&mut r)?;
        doc.footer = decode_hf(&mut r)?;
    }

    let mut r = Reader::new(one(STYLES, "STYLES chunk")?);
    let n = r.count("styles", MAX_STYLES as u64, 6)?;
    if n == 0 {
        return Err(LoadError::Bad("no styles"));
    }
    let mut styles = Vec::with_capacity(n);
    for _ in 0..n {
        styles.push(decode_style(&mut r, n)?);
    }
    doc.styles = StyleSheet { styles, ..StyleSheet::default() };
    if c.entries().iter().any(|e| e.kind == LISTS) {
        let mut r = Reader::new(one(LISTS, "LISTS chunk")?);
        let n = r.count("lists", MAX_LIST_DEFS as u64, 2)?;
        for _ in 0..n {
            let id = r.len("list", u32::MAX as u64)? as u32;
            let d = decode_list(&mut r)?;
            if doc.styles.lists.insert(id, d).is_some() {
                return Err(LoadError::Bad("list defined twice"));
            }
        }
        if !r.is_empty() {
            return Err(LoadError::Bad("LISTS chunk"));
        }
    }
    if c.entries().iter().any(|e| e.kind == SETTINGS) {
        let mut r = Reader::new(one(SETTINGS, "SETTINGS chunk")?);
        doc.settings = decode_settings(&mut r)?;
        if !r.is_empty() {
            return Err(LoadError::Bad("SETTINGS chunk"));
        }
    }
    if c.entries().iter().any(|e| e.kind == TABLES) {
        let mut r = Reader::new(one(TABLES, "TABLES chunk")?);
        let n = r.count("tables", MAX_TABLES as u64, 8)?;
        for _ in 0..n {
            let id = r.len("table", u32::MAX as u64)? as u32;
            if doc.tables.insert(id, decode_table(&mut r)?).is_some() {
                return Err(LoadError::Bad("table defined twice"));
            }
        }
        if !r.is_empty() {
            return Err(LoadError::Bad("TABLES chunk"));
        }
    }
    if c.entries().iter().any(|e| e.kind == TABLE_STYLES) {
        let mut r = Reader::new(one(TABLE_STYLES, "TABLE_STYLES chunk")?);
        let n = r.count("table styles", MAX_TABLE_STYLES as u64, 16)?;
        for _ in 0..n {
            let name: Arc<str> = r.str("table style", 256)?.into();
            if doc.styles.tables.insert(name, decode_table_style(&mut r)?).is_some() {
                return Err(LoadError::Bad("table style defined twice"));
            }
        }
        if !r.is_empty() {
            return Err(LoadError::Bad("TABLE_STYLES chunk"));
        }
    }

    let mut images: Vec<(u64, ImageData)> = Vec::new();
    for (i, e) in c.entries().iter().enumerate() {
        if e.kind == IMAGE {
            images.push(decode_image(c.chunk(i)?)?);
        }
    }
    images.sort_by_key(|x| x.0);
    for (k, (i, img)) in images.into_iter().enumerate() {
        if i != k as u64 || doc.images.add(img) != k as u32 {
            return Err(LoadError::Bad("pictures missing, repeated or out of order"));
        }
    }

    let mut r = Reader::new(one(ATTRS, "ATTRS chunk")?);
    let n = r.count("attributes", MAX_ATTRS, 2)?;
    for i in 0..n {
        let a = decode_attrs(&mut r)?;
        if a.object.is_some_and(|o| !o.shape && o.image as usize >= doc.images.len()) {
            return Err(LoadError::Bad("picture reference"));
        }
        // A formatting change names an earlier entry (never itself or a later one).
        if a.fmt.is_some_and(|f| f.old as usize > i) {
            return Err(LoadError::Bad("formatting change"));
        }
        if doc.attrs.intern(a) as usize != i + 1 {
            return Err(LoadError::Bad("attribute listed twice"));
        }
    }
    let attrs = doc.attrs.len();
    let styles = doc.styles.styles.len();

    let mut chunks: Vec<(u64, &[u8])> = Vec::new();
    for (i, e) in c.entries().iter().enumerate() {
        if e.kind == BLOCKS {
            let data = c.chunk(i)?;
            let mut r = Reader::new(data);
            chunks.push((r.var()?, data));
        }
    }
    chunks.sort_by_key(|x| x.0);
    let mut ids = std::collections::HashSet::new();
    for (first, data) in chunks {
        if first != doc.blocks.len() as u64 {
            return Err(LoadError::Bad("block chunks leave a gap or overlap"));
        }
        let mut r = Reader::new(data);
        r.var()?;
        let k = r.count("blocks", BLOCKS_PER_CHUNK as u64, 7)?;
        for _ in 0..k {
            let b = decode_block(&mut r, attrs, styles)?;
            if !ids.insert(b.id()) || b.id() >= watermark {
                return Err(LoadError::Bad("block id"));
            }
            doc.blocks.insert(doc.blocks.len(), b);
        }
        if doc.blocks.len() as u64 > MAX_BLOCKS {
            return Err(LoadError::Bad("too many blocks"));
        }
    }
    if doc.blocks.is_empty() {
        let id = doc.new_id();
        doc.blocks.insert(0, Block::Para(Para::new(id, "", 0)));
    }
    if c.entries().iter().any(|e| e.kind == NOTES) {
        let mut r = Reader::new(one(NOTES, "NOTES chunk")?);
        let n = r.count("notes", MAX_NOTES as u64, 4)?;
        for _ in 0..n {
            let id = r.len("note", MAX_NOTES as u64 - 1)? as NoteId;
            let note = decode_note(&mut r, attrs, styles)?;
            for p in &note.paras {
                if !ids.insert(p.id) || p.id >= watermark {
                    return Err(LoadError::Bad("block id"));
                }
            }
            if doc.notes.get(id).is_some() {
                return Err(LoadError::Bad("note listed twice"));
            }
            doc.notes.set(id, Some(note));
        }
    }
    if c.entries().iter().any(|e| e.kind == COMMENTS) {
        let mut r = Reader::new(one(COMMENTS, "COMMENTS chunk")?);
        let n = r.count("comments", MAX_COMMENTS as u64, 6)?;
        for _ in 0..n {
            let id = r.len("comment", MAX_COMMENTS as u64 - 1)? as CommentId;
            let cm = decode_comment(&mut r)?;
            if doc.comments.get(id).is_some() || cm.parent == Some(id) {
                return Err(LoadError::Bad("comment listed twice"));
            }
            doc.comments.set(id, Some(cm));
        }
    }
    if c.entries().iter().any(|e| e.kind == REVISIONS) {
        let mut r = Reader::new(one(REVISIONS, "REVISIONS chunk")?);
        doc.tracking = match r.u8()? {
            0 => false,
            1 => true,
            _ => return Err(LoadError::Bad("tracking flag")),
        };
        let n = r.count("revisions", MAX_REVISIONS as u64, 4)?;
        for _ in 0..n {
            let id = r.len("revision", MAX_REVISIONS as u64 - 1)? as RevId;
            let v = decode_revision(&mut r)?;
            if doc.revisions.get(id).is_some() {
                return Err(LoadError::Bad("revision listed twice"));
            }
            doc.revisions.set(id, Some(v));
        }
    }
    if c.entries().iter().any(|e| e.kind == DELETIONS) {
        let mut r = Reader::new(one(DELETIONS, "DELETIONS chunk")?);
        let n = r.count("deletions", MAX_DELETIONS as u64, 3)?;
        for _ in 0..n {
            let id = r.len("deletion", MAX_DELETIONS as u64 - 1)? as DelId;
            let d = decode_deletion(&mut r, attrs, styles)?;
            if doc.deletions.get(id).is_some() {
                return Err(LoadError::Bad("deletion listed twice"));
            }
            for p in d.pieces.iter().filter_map(|p| p.para.as_ref()) {
                watermark = watermark.max(p.0 + 1);
            }
            doc.deletions.set(id, Some(d));
        }
    }
    if c.entries().iter().any(|e| e.kind == CONFLICTS) {
        let mut r = Reader::new(one(CONFLICTS, "CONFLICTS chunk")?);
        let n = r.count("conflicts", MAX_CONFLICTS as u64, 3)?;
        for _ in 0..n {
            let id = r.len("conflict", MAX_CONFLICTS as u64 - 1)? as ConflictId;
            let d = decode_deletion(&mut r, attrs, styles)?;
            if doc.conflicts.get(id).is_some() {
                return Err(LoadError::Bad("conflict listed twice"));
            }
            for p in d.pieces.iter().filter_map(|p| p.para.as_ref()) {
                watermark = watermark.max(p.0 + 1);
            }
            doc.conflicts.set(id, Some(d));
        }
    }
    if c.entries().iter().any(|e| e.kind == MARGINS) {
        let mut r = Reader::new(one(MARGINS, "MARGINS chunk")?);
        let mut check = |hf: &Option<HeaderFooter>| -> Result<(), LoadError> {
            for p in hf.iter().flat_map(|h| h.paras.iter()) {
                if !ids.insert(p.id) || p.id >= watermark {
                    return Err(LoadError::Bad("block id"));
                }
            }
            Ok(())
        };
        decode_margin_paras(&mut r, attrs, styles, &mut doc.header, &mut doc.footer)?;
        check(&doc.header)?;
        check(&doc.footer)?;
        // The first page's and even pages' own (absent in older files).
        if !r.is_empty() {
            for first in [true, false] {
                let m = match r.u8()? {
                    0 => None,
                    1 => {
                        let (mut header, mut footer) = (decode_hf(&mut r)?, decode_hf(&mut r)?);
                        decode_margin_paras(&mut r, attrs, styles, &mut header, &mut footer)?;
                        check(&header)?;
                        check(&footer)?;
                        Some(crate::model::Margins { header, footer })
                    }
                    _ => return Err(LoadError::Bad("page margins")),
                };
                if first {
                    doc.first_page = m;
                } else {
                    doc.even_pages = m;
                }
            }
        }
    }
    for (i, e) in c.entries().iter().enumerate() {
        if e.kind != SHAPE {
            continue;
        }
        let mut r = Reader::new(c.chunk(i)?);
        let id = r.len("shape", crate::model::MAX_SHAPES as u64 - 1)? as u32;
        let sh = decode_shape(&mut r, attrs, styles)?;
        for p in sh.all_paras() {
            if !ids.insert(p.id) || p.id >= watermark {
                return Err(LoadError::Bad("block id"));
            }
            // A shape holds no shapes.
            if p.runs.iter().any(|&(_, a)| doc.attrs.get(a).object.is_some_and(|o| o.shape)) {
                return Err(LoadError::Bad("shape in a shape"));
            }
        }
        if doc.shapes.get(id).is_some() {
            return Err(LoadError::Bad("shape listed twice"));
        }
        doc.shapes.set(id, Some(sh));
    }
    for i in 1..doc.attrs.len() {
        if doc.attrs.get(i as AttrId).object.is_some_and(|o| o.shape && doc.shapes.get(o.image).is_none()) {
            return Err(LoadError::Bad("shape reference"));
        }
    }
    doc.raise_watermark(watermark);

    let mut checkpoints = Vec::new();
    if c.entries().iter().any(|e| e.kind == CHECKPOINTS) {
        let mut r = Reader::new(one(CHECKPOINTS, "CHECKPOINTS chunk")?);
        let n = r.count("checkpoints", MAX_BLOCKS, 2)?;
        for _ in 0..n {
            let block = r.len("block", doc.blocks.len() as u64)?;
            let line = r.len("line", 1 << 24)?;
            checkpoints.push(Checkpoint::at(block, line));
        }
    }
    doc.take_changes();
    Ok(Loaded { doc, checkpoints })
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;

    fn sample() -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), color: Some(0x336699), family: Some("IBM Plex Mono".into()), ..Default::default() });
        let link = d.attrs.intern(CharAttrs { link: Some("https://example.org".into()), underline: Some(true), ..Default::default() });
        let h1 = d.styles.by_name("Heading 1").unwrap();
        d.push_para("Title here", h1, vec![]);
        for i in 0..700 {
            d.push_para(&format!("Paragraph {i} with bold and a link; naïve ✓"), 0, vec![(0, 0), (14, bold), (18, 0), (27, link), (31, 0)]);
        }
        d.blocks.update(3, |b| b.as_para_mut().unwrap().props = ParaProps { align: Some(Align::Justify), line_spacing: Some(LineSpacing::Exact(300)), keep_together: Some(true), first_line: Some(-200), list: Some(Some(ListRef { id: 3, kind: ListKind::Numbered, level: 2 })), ..Default::default() });
        d.blocks.update(4, |b| b.as_para_mut().unwrap().props.cell = Some(CellRef { table: 2, row: 7, col: 1, width: 2000, ..Default::default() }));
        d.blocks.update(9, |b| b.as_para_mut().unwrap().props.section = Some(Section { start: SectionStart::Continuous, columns: 3, gap: 400, rule: true, page: None }));
        d.blocks.update(10, |b| b.as_para_mut().unwrap().props.section = Some(Section { page: Some(PageSetup { width: 16838, height: 11906, ..PageSetup::default() }), ..Section::default() }));
        d.page.margin_start = 1000;
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center, paras: Vec::new() });
        let img = d.images.add(ImageData { bytes: Arc::from(&b"\x89PNG fake"[..]), width: 40, height: 20 });
        assert_eq!(d.images.add(ImageData { bytes: Arc::from(&b"\x89PNG fake"[..]), width: 40, height: 20 }), img, "the same bytes are stored once");
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 600, height: 300, float: None, shape: false }), ..Default::default() });
        d.push_para("a \u{FFFC} b", 0, vec![(0, 0), (2, pic), (5, 0)]);
        let sup = d.attrs.intern(CharAttrs { vert: Some(VertAlign::Super), ..Default::default() });
        d.push_para("E = mc2", 0, vec![(0, 0), (6, sup)]);
        // A contents entry (tab stop, field) and a text reference.
        let target = d.para(1).unwrap().id;
        let pr = d.attrs.intern(CharAttrs { field: Some(crate::model::Field::PageRef(target)), ..Default::default() });
        let tr = d.attrs.intern(CharAttrs { field: Some(crate::model::Field::TextRef(target)), bold: Some(true), ..Default::default() });
        let k = d.push_para("Entry\t\u{FFFC} see \u{FFFC}", 0, vec![(0, 0), (6, pr), (9, 0), (14, tr)]);
        d.blocks.update(k, |b| b.as_para_mut().unwrap().props = crate::fields::entry_props(2, 9000));
        for (b, kind, text) in [(10, NoteKind::Foot, "A footnote."), (20, NoteKind::End, "An endnote\nin two paragraphs.")] {
            for op in d.insert_note_ops(b, 2, kind, text, 0) {
                op.apply(&mut d);
            }
        }
        let (b, e) = (30usize, 32usize);
        let len = d.para(e).unwrap().text.len() as u32;
        let c = Comment { author: "Ana Lima".into(), initials: "AL".into(), date: "2026-11-14T20:30:00Z".into(), text: "Check this.\nTwo lines.".into(), parent: None, done: false };
        for op in d.add_comment_ops(b, 3, e, len, c) {
            op.apply(&mut d);
        }
        let reply = Comment { author: "Bo".into(), text: "Checked.".into(), parent: Some(0), done: true, ..Default::default() };
        TextOp::SetComment { id: 1, comment: Some(reply) }.apply(&mut d);
        // A header of its own paragraphs (with a picture).
        let hp = Para { runs: vec![(0, pic), (3, 0)], ..Para::new(d.new_id(), "\u{FFFC} Acme {page}", 0) };
        d.header = Some(HeaderFooter { text: "Acme {page}".into(), align: Align::Start, paras: vec![hp, Para::new(d.new_id(), "second", 0)] });
        // A title page without a header; even pages with their own footer.
        d.first_page = Some(crate::model::Margins { header: None, footer: Some(HeaderFooter::line("Title page", Align::Center)) });
        let ep = Para::new(d.new_id(), "Even {page}", 0);
        d.even_pages = Some(crate::model::Margins { header: d.header.clone().map(|mut h| { h.paras.clear(); h }), footer: Some(HeaderFooter { text: "Even {page}".into(), align: Align::End, paras: vec![ep] }) });
        d.take_changes();
        d
    }

    fn same(a: &TextDoc, b: &TextDoc) {
        assert_eq!(a.blocks.iter().cloned().collect::<Vec<_>>(), b.blocks.iter().cloned().collect::<Vec<_>>());
        assert_eq!(a.page, b.page);
        assert_eq!(a.styles.styles, b.styles.styles);
        assert_eq!(a.attrs.len(), b.attrs.len());
        for i in 0..a.attrs.len() {
            assert_eq!(a.attrs.get(i as AttrId), b.attrs.get(i as AttrId));
        }
        assert_eq!(a.id_watermark(), b.id_watermark());
        assert_eq!((&a.header, &a.footer), (&b.header, &b.footer));
        assert_eq!((&a.first_page, &a.even_pages), (&b.first_page, &b.even_pages));
        assert_eq!(a.images.iter().collect::<Vec<_>>(), b.images.iter().collect::<Vec<_>>());
        assert_eq!(a.notes.iter().collect::<Vec<_>>(), b.notes.iter().collect::<Vec<_>>());
        assert_eq!(a.note_numbers(), b.note_numbers());
        assert_eq!(a.comments.iter().collect::<Vec<_>>(), b.comments.iter().collect::<Vec<_>>());
        assert_eq!(a.comment_ranges(), b.comment_ranges());
    }

    #[test]
    fn boxes_drop_caps_lists_and_settings_round_trip() {
        use crate::model::{Border, BorderStyle, DropCap, LineNumbering, LineRestart, ListDef, ListLevel, NumFormat, ParaBorders, Settings};
        let mut d = TextDoc::new();
        let b = Border { style: BorderStyle::Double, width: 12, color: Some(0x123456), space: 3 };
        d.blocks.update(0, |x| {
            let p = &mut x.as_para_mut().unwrap().props;
            p.borders = Some(ParaBorders { top: Some(b), start: Some(Border::default()), ..Default::default() });
            p.shading = Some(Some(0xffee00));
            p.drop_cap = Some(Some(DropCap { lines: 3, distance: 144, margin: true }));
            p.list_start = Some(5);
            p.line_numbers = Some(false);
            p.hyphenate = Some(false);
        });
        let a = d.push_para("off", 0, vec![]);
        d.blocks.update(a, |x| {
            let p = &mut x.as_para_mut().unwrap().props;
            p.shading = Some(None);
            p.drop_cap = Some(None);
        });
        let mut def = ListDef::builtin(crate::model::ListKind::Numbered);
        def.levels[2] = ListLevel { format: NumFormat::UpperLetter, text: "(%3)".into(), start: 4, indent: 2000, hang: 300 };
        d.styles.lists.insert(9, def.clone());
        d.settings = Settings { hyphenate: true, line_numbers: Some(LineNumbering { start: 2, count_by: 5, distance: 300, restart: LineRestart::Continuous }), ..Default::default() };
        // Settings from before the page's look still read.
        let mut w = Writer::new();
        encode_settings(&mut w, &d.settings);
        let old = w.into_bytes();
        assert_eq!(decode_settings(&mut Reader::new(&old[..old.len() - 1])).unwrap(), d.settings);
        d.settings.page_color = Some(0xFFF2CC);
        d.settings.page_borders = Some(crate::model::PageBorders { top: Some(b), bottom: Some(b), start: None, end: Some(b), from_page: true });
        d.settings.watermark = Some(crate::model::Watermark::Text { text: "DRAFT".into(), font: Some("Calibri".into()), color: 0xC0C0C0, diagonal: true });
        let r = load(&save_to_vec(&d, &[])).unwrap().doc;
        assert_eq!(r.para(0).unwrap().props, d.para(0).unwrap().props);
        assert_eq!(r.para(a).unwrap().props, d.para(a).unwrap().props);
        assert_eq!(r.styles.lists.get(&9), Some(&def));
        assert_eq!(r.settings, d.settings);
        // The ops round-trip too, and a bad border is refused.
        for op in [TextOp::SetList { id: 9, def: Some(def.clone()) }, TextOp::SetList { id: 2, def: None }, TextOp::SetSettings { settings: d.settings.clone() }, TextOp::SetSettings { settings: Settings { watermark: Some(crate::model::Watermark::Picture { image: 0, scale: 120, washout: true }), ..Settings::default() } }] {
            assert_eq!(decode_op(&encode_op(&op), 1, 1).unwrap(), op);
        }
        let mut w = Writer::new();
        encode_border(&mut w, &Border { width: 0, ..b });
        assert!(decode_border(&mut Reader::new(&w.into_bytes())).is_err());
    }

    #[test]
    fn cell_looks_tables_and_table_styles_round_trip() {
        use crate::model::{Border, CellBorders, CellLook, CellPad, CellRef, RegionLook, RowLook, TableBorders, TableLook, TableStyle, VAlign, LOOK_TOTAL};
        let mut d = TextDoc::new();
        let b = Border { color: Some(0x00aa00), width: 8, ..Default::default() };
        let cell = CellRef {
            table: 3,
            row: 1,
            col: 2,
            width: 2400,
            look: CellLook { borders: CellBorders { top: Some(Some(b)), bottom: Some(None), ..Default::default() }, shading: Some(Some(0xabcdef)), valign: VAlign::Bottom, pad: Some(CellPad { top: 10, bottom: 20, start: 30, end: 40 }) },
            row_look: RowLook { header: true, cant_split: true, height: 720, exact: true },
            merged_up: true,
            span: 3,
        };
        d.blocks.update(0, |x| x.as_para_mut().unwrap().props.cell = Some(cell));
        let plain = d.push_para("plain cell", 0, vec![]);
        d.blocks.update(plain, |x| x.as_para_mut().unwrap().props.cell = Some(CellRef { table: 3, row: 2, col: 0, width: 900, ..Default::default() }));
        let look = TableLook { style: Some("Mine".into()), borders: Some(TableBorders { inside_v: Some(b), ..Default::default() }), shading: Some(0xfefefe), pad: Some(CellPad::default()), align: crate::model::HAlign::Center, indent: -144, look: LOOK_TOTAL, autofit: true };
        d.tables.insert(3, look.clone());
        let style = TableStyle { borders: TableBorders::grid(), header: RegionLook { shading: Some(1), bold: true, color: Some(2), rule: Some(b) }, band_row: Some(3), ..Default::default() };
        d.styles.tables.insert("Mine".into(), style.clone());
        let r = load(&save_to_vec(&d, &[])).unwrap().doc;
        assert_eq!(r.para(0).unwrap().props.cell, Some(cell));
        assert_eq!(r.para(plain).unwrap().props, d.para(plain).unwrap().props);
        assert_eq!(r.tables.get(&3), Some(&look));
        assert_eq!(r.styles.tables.get("Mine"), Some(&style));
        for op in [TextOp::SetTable { id: 3, look: Some(look) }, TextOp::SetTable { id: 4, look: None }, TextOp::SetTableStyle { name: "Mine".into(), style: Some(style) }, TextOp::SetTableStyle { name: "X".into(), style: None }] {
            assert_eq!(decode_op(&encode_op(&op), 1, 1).unwrap(), op);
        }
        let mut w = Writer::new();
        let mut p = ParaProps { cell: Some(cell), ..Default::default() };
        encode_props(&mut w, &p);
        let mut bytes = w.into_bytes();
        assert_eq!(decode_props(&mut Reader::new(&bytes)).unwrap(), p);
        bytes.truncate(bytes.len() - 1);
        assert!(decode_props(&mut Reader::new(&bytes)).is_err(), "cut short");
        // A table inside the cell, and one that isn't in a cell is refused.
        p.inner = Some(CellRef { table: 9, row: 0, col: 1, width: 700, span: 2, ..Default::default() });
        let mut w = Writer::new();
        encode_props(&mut w, &p);
        assert_eq!(decode_props(&mut Reader::new(&w.into_bytes())).unwrap(), p);
        p.cell = None;
        let mut w = Writer::new();
        encode_props(&mut w, &p);
        assert_eq!(decode_props(&mut Reader::new(&w.into_bytes())).unwrap().inner, None, "no inner cell without an outer one");
    }

    #[test]
    fn paragraph_direction_round_trips() {
        let mut d = TextDoc::new();
        let a = d.push_para("שלום", 0, vec![]);
        d.blocks.update(a, |x| x.as_para_mut().unwrap().props.rtl = Some(true));
        let r = load(&save_to_vec(&d, &[])).unwrap().doc;
        assert_eq!(r.para(a).unwrap().props.rtl, Some(true));
        assert_eq!(r.para(0).unwrap().props.rtl, None);
    }

    #[test]
    fn east_asian_language_round_trips() {
        let mut d = TextDoc::new();
        let ja = d.attrs.intern(CharAttrs { lang_ea: Some("ja-JP".into()), bold: Some(true), ..Default::default() });
        d.push_para("直骨", 0, vec![(0, ja)]);
        let r = load(&save_to_vec(&d, &[])).unwrap().doc;
        assert_eq!(r.attrs.get(ja).lang_ea.as_deref(), Some("ja-JP"));
        assert_eq!(r.attrs.get(ja).bold, Some(true));
    }

    #[test]
    fn a_picture_reference_past_the_pictures_is_refused() {
        let mut d = TextDoc::new();
        d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: 0, width: 600, height: 300, float: None, shape: false }), ..Default::default() });
        assert!(matches!(load(&save_to_vec(&d, &[])), Err(LoadError::Bad("picture reference"))));
    }

    #[test]
    fn shapes_round_trip_and_bad_ones_are_refused() {
        use crate::model::{Dash, Geom, PathCmd, ShapeData, ShapeLine, ShapePath, TextAnchor};
        let mut d = TextDoc::new();
        let mut sd = ShapeData::new(Geom::Preset { name: "wedgeRectCallout".into(), adj: vec![("adj1".into(), -20_833), ("adj2".into(), 62_500)] });
        sd.fill = Some(0xFFC000);
        sd.line = Some(ShapeLine { color: 0x1F3864, width: 25, dash: Dash::DashDot, head: crate::model::ArrowEnd::Oval, tail: crate::model::ArrowEnd::Stealth });
        sd.rot = 45 * 60_000;
        sd.flip_v = true;
        sd.anchor = TextAnchor::Bottom;
        sd.inset = [0, 10, 20, 30];
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        let mut p = Para::new(d.new_id(), "Note this", 0);
        p.runs = vec![(0, bold)];
        sd.paras.push(p);
        let a = d.shapes.add(sd).unwrap();
        let custom = ShapeData::new(Geom::Custom(vec![ShapePath { w: 100, h: 50, cmds: vec![PathCmd::Move(0, 0), PathCmd::Line(100, 0), PathCmd::Cubic(100, 20, 50, 50, 0, 50), PathCmd::Close], filled: true, stroked: false }]));
        let b = d.shapes.add(custom).unwrap();
        let ra = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: a, width: 2000, height: 1000, float: None, shape: true }), ..Default::default() });
        let rb = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: b, width: 300, height: 300, float: Some(Float::default()), shape: true }), ..Default::default() });
        d.push_para("\u{FFFC}\u{FFFC}", 0, vec![(0, ra), (3, rb)]);
        let back = load(&save_to_vec(&d, &[])).unwrap().doc;
        assert_eq!(back.shapes.iter().map(|(i, s)| (i, (**s).clone())).collect::<Vec<_>>(), d.shapes.iter().map(|(i, s)| (i, (**s).clone())).collect::<Vec<_>>());
        let objs: Vec<_> = back.blocks.iter().filter_map(|b| b.as_para()).flat_map(|p| p.runs.iter().filter_map(|&(_, a)| back.attrs.get(a).object)).collect();
        assert_eq!(objs.len(), 2);
        assert!(objs.iter().all(|o| o.shape));
        // A reference to a shape the file lacks.
        let mut bad = d.clone();
        bad.attrs.intern(CharAttrs { object: Some(ObjectRef { image: 7, width: 300, height: 300, float: None, shape: true }), ..Default::default() });
        assert!(matches!(load(&save_to_vec(&bad, &[])), Err(LoadError::Bad("shape reference"))));
        // A shape inside a shape.
        let mut nest = d.clone();
        let mut inner = (**nest.shapes.get(a).unwrap()).clone();
        let id = nest.new_id();
        inner.paras = vec![Para { id, text: "\u{FFFC}".into(), runs: vec![(0, rb)], style: 0, props: Default::default() }];
        nest.shapes.set(a, Some(inner));
        assert!(matches!(load(&save_to_vec(&nest, &[])), Err(LoadError::Bad("shape in a shape"))));
    }

    #[test]
    fn round_trip() {
        let d = sample();
        let cps = vec![Checkpoint::at(0, 0), Checkpoint::at(40, 2)];
        let bytes = save_to_vec(&d, &cps);
        let l = load(&bytes).unwrap();
        assert_eq!(d.comment_ranges().len(), 1);
        assert_eq!(d.live_comments(), vec![0, 1]);
        same(&d, &l.doc);
        assert_eq!(l.checkpoints, cps);
    }

    #[test]
    fn ops_encode_and_decode() {
        let mut d = sample();
        let id = d.new_id();
        let blocks = vec![Block::Para(Para::new(d.new_id(), "inserted", 0))];
        let ops = vec![
            TextOp::Splice { block: 2, at: 3, del: 2, ins: Fragment { text: "xyz".into(), runs: vec![(0, 1), (1, 0)] } },
            TextOp::Restyle { block: 4, start: 0, end: 5, runs: vec![(0, 2)] },
            TextOp::Split { block: 5, at: 4, id, style: 1, props: ParaProps { keep_with_next: Some(true), ..Default::default() } },
            TextOp::Join { block: 6 },
            TextOp::SetPara { block: 7, style: 2, props: ParaProps::default() },
            TextOp::Insert { at: 9, blocks },
            TextOp::Remove { at: 12, n: 3 },
            TextOp::SetPage { page: PageSetup { width: 12240, height: 15840, ..PageSetup::default() }, header: Some(HeaderFooter { text: "Draft".into(), align: Align::End, paras: Vec::new() }), footer: None },
            TextOp::SetPage { page: PageSetup::default(), header: None, footer: Some(HeaderFooter { text: "x".into(), align: Align::Start, paras: vec![Para::new(d.new_id(), "x {page}", 0)] }) },
            TextOp::SetMargins { header: Some(HeaderFooter::line("H", Align::Center)), footer: None, first_page: Some(crate::model::Margins::default()), even_pages: Some(crate::model::Margins { header: None, footer: Some(HeaderFooter { text: "e".into(), align: Align::Start, paras: vec![Para::new(d.new_id(), "e {page}", 0)] }) }) },
            TextOp::SetNote { id: 5, note: Some(Note { kind: NoteKind::End, paras: vec![Para::new(d.new_id(), "note", 0)] }) },
            TextOp::SetNote { id: 5, note: None },
            TextOp::SetShape { id: 3, shape: Some(crate::model::ShapeData { fill: Some(0xFF0000), paras: vec![Para::new(d.new_id(), "box", 0)], ..crate::model::ShapeData::new(crate::model::Geom::preset("ellipse")) }) },
            TextOp::SetShape { id: 3, shape: None },
            TextOp::SetComment { id: 9, comment: Some(Comment { author: "A".into(), text: "c".into(), parent: Some(0), ..Default::default() }) },
            TextOp::SetComment { id: 9, comment: None },
            TextOp::SetRevision { id: 2, rev: Some(crate::model::Revision { author: "Ann".into(), date: "2026-11-15T00:00:00Z".into(), set: 3, name: "Legal".into() }) },
            TextOp::SetRevision { id: 2, rev: None },
            TextOp::SetDeletion { id: 4, del: Some(Deletion { rev: 2, pieces: vec![DelPiece { para: None, text: "gone".into(), runs: vec![(0, 1)] }, DelPiece { para: Some((77, 1, ParaProps { rev: Some(1), ..Default::default() })), text: "".into(), runs: vec![(0, 0)] }] }) },
            TextOp::SetDeletion { id: 4, del: None },
            TextOp::SetTracking { on: true },
            TextOp::SetStyle { id: 5, style: Some(Style { name: "Quote".into(), kind: StyleKind::Para, parent: Some(0), next: Some(5), para: ParaProps { indent_start: Some(720), ..Default::default() }, chars: CharAttrs { italic: Some(true), ..Default::default() } }) },
            TextOp::SetStyle { id: 5, style: None },
            TextOp::Nop,
        ];
        for op in ops {
            let b = encode_op(&op);
            let back = decode_op(&b, d.attrs.len(), d.styles.styles.len()).unwrap();
            assert_eq!(back, op);
            back.apply(&mut d);
        }
        assert!(decode_op(&encode_op(&TextOp::Restyle { block: 0, start: 0, end: 1, runs: vec![(0, 99)] }), 3, 5).is_err(), "an attribute outside the table");
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let bytes = save_to_vec(&sample(), &[Checkpoint::at(0, 0)]);
        let mut x: u64 = 0x5eed;
        let mut rnd = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        let mut ok = 0;
        for i in 0..3000 {
            let mut b = bytes.clone();
            if i % 3 == 0 {
                b.truncate(rnd(b.len() as u64) as usize);
            } else {
                for _ in 0..1 + rnd(4) {
                    let k = rnd(b.len() as u64) as usize;
                    b[k] = rnd(256) as u8;
                }
            }
            if load(&b).is_ok() {
                ok += 1;
            }
            let _ = decode_op(&b[..b.len().min(64)], 3, 5);
        }
        // Most damage is caught by the checksums; none panics.
        assert!(ok < 3000);
    }
}
