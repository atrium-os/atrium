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

use crate::model::{Note, NoteId, NoteKind, VertAlign, MAX_NOTES, MAX_NOTE_PARAS, Align, AttrId, CellRef, HeaderFooter, ImageData, ObjectRef, MAX_OBJECT_TWIPS, ListKind, ListRef, MAX_LIST_LEVEL, Block, CharAttrs, LineSpacing, PageSetup, Para, ParaProps, Style, StyleKind, StyleSheet, TextDoc, MAX_STYLES};
use crate::ops::{Fragment, TextOp};
use crate::paginate::Checkpoint;

pub const DOCUMENT: u16 = 1;
pub const STYLES: u16 = 2;
pub const ATTRS: u16 = 3;
pub const BLOCKS: u16 = 4;
pub const CHECKPOINTS: u16 = 5;
pub const IMAGE: u16 = 6;
pub const NOTES: u16 = 7;

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
    for (i, on) in [a.bold.is_some(), a.italic.is_some(), a.underline.is_some(), a.strike.is_some(), a.size.is_some(), a.color.is_some(), a.highlight.is_some(), a.family.is_some(), a.style.is_some(), a.link.is_some(), a.object.is_some(), a.vert.is_some(), a.note.is_some()].into_iter().enumerate() {
        if on {
            m |= 1 << i;
        }
    }
    w.u16(m);
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
}

pub fn decode_attrs(r: &mut Reader) -> Result<CharAttrs, WireError> {
    let m = r.u16()?;
    if m >> 13 != 0 {
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
        a.object = Some(ObjectRef { image: image as u32, width: w as u32, height: h as u32 });
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
    Ok(a)
}

/// Paragraph properties: a mask of the fields set, then their values.
pub fn encode_props(w: &mut Writer, p: &ParaProps) {
    let mut m = 0u16;
    for (i, on) in [p.align.is_some(), p.indent_start.is_some(), p.indent_end.is_some(), p.first_line.is_some(), p.space_before.is_some(), p.space_after.is_some(), p.line_spacing.is_some(), p.keep_with_next.is_some(), p.keep_together.is_some(), p.page_break_before.is_some(), p.widows.is_some(), p.outline_level.is_some(), p.list.is_some(), p.cell.is_some()].into_iter().enumerate() {
        if on {
            m |= 1 << i;
        }
    }
    w.u16(m);
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
}

/// Twips a length may be (±22 inches).
const MAX_TWIPS: i64 = 31_680;

pub fn decode_props(r: &mut Reader) -> Result<ParaProps, WireError> {
    let m = r.u16()?;
    if m >> 14 != 0 {
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
        p.cell = Some(CellRef { table, row, col, width: width as u32 });
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
            Some(HeaderFooter { text, align })
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
            let header = decode_hf(&mut r)?;
            TextOp::SetPage { page, header, footer: decode_hf(&mut r)? }
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
        w.str(&s.name);
        w.u8(match s.kind {
            StyleKind::Para => 0,
            StyleKind::Char => 1,
        });
        w.var(s.parent.map_or(0, |p| p as u64 + 1));
        w.var(s.next.map_or(0, |p| p as u64 + 1));
        encode_props(&mut w, &s.para);
        encode_attrs(&mut w, &s.chars);
    }
    c.add(STYLES, &w.into_bytes())?;

    for (i, img) in doc.images.iter().enumerate() {
        let mut w = Writer::new();
        encode_image(&mut w, i as u32, img);
        c.add(IMAGE, &w.into_bytes())?;
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
    let watermark = r.var()?;
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
        let name: Arc<str> = Arc::from(r.str("style name", MAX_NAME)?);
        let kind = match r.u8()? {
            0 => StyleKind::Para,
            1 => StyleKind::Char,
            _ => return Err(LoadError::Bad("style kind")),
        };
        let idx = |v: u64| -> Result<Option<u16>, LoadError> {
            if v == 0 {
                Ok(None)
            } else if v as usize <= n {
                Ok(Some((v - 1) as u16))
            } else {
                Err(LoadError::Bad("style reference"))
            }
        };
        let parent = idx(r.var()?)?;
        let next = idx(r.var()?)?;
        let para = decode_props(&mut r)?;
        let chars = decode_attrs(&mut r)?;
        styles.push(Style { name, kind, parent, next, para, chars });
    }
    doc.styles = StyleSheet { styles, ..StyleSheet::default() };

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
        if a.object.is_some_and(|o| o.image as usize >= doc.images.len()) {
            return Err(LoadError::Bad("picture reference"));
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
    doc.raise_watermark(watermark);

    let mut checkpoints = Vec::new();
    if c.entries().iter().any(|e| e.kind == CHECKPOINTS) {
        let mut r = Reader::new(one(CHECKPOINTS, "CHECKPOINTS chunk")?);
        let n = r.count("checkpoints", MAX_BLOCKS, 2)?;
        for _ in 0..n {
            let block = r.len("block", doc.blocks.len() as u64)?;
            let line = r.len("line", 1 << 24)?;
            checkpoints.push(Checkpoint { block, line });
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
        d.blocks.update(4, |b| b.as_para_mut().unwrap().props.cell = Some(CellRef { table: 2, row: 7, col: 1, width: 2000 }));
        d.page.margin_start = 1000;
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center });
        let img = d.images.add(ImageData { bytes: Arc::from(&b"\x89PNG fake"[..]), width: 40, height: 20 });
        assert_eq!(d.images.add(ImageData { bytes: Arc::from(&b"\x89PNG fake"[..]), width: 40, height: 20 }), img, "the same bytes are stored once");
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 600, height: 300 }), ..Default::default() });
        d.push_para("a \u{FFFC} b", 0, vec![(0, 0), (2, pic), (5, 0)]);
        let sup = d.attrs.intern(CharAttrs { vert: Some(VertAlign::Super), ..Default::default() });
        d.push_para("E = mc2", 0, vec![(0, 0), (6, sup)]);
        for (b, kind, text) in [(10, NoteKind::Foot, "A footnote."), (20, NoteKind::End, "An endnote\nin two paragraphs.")] {
            for op in d.insert_note_ops(b, 2, kind, text, 0) {
                op.apply(&mut d);
            }
        }
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
        assert_eq!(a.images.iter().collect::<Vec<_>>(), b.images.iter().collect::<Vec<_>>());
        assert_eq!(a.notes.iter().collect::<Vec<_>>(), b.notes.iter().collect::<Vec<_>>());
        assert_eq!(a.note_numbers(), b.note_numbers());
    }

    #[test]
    fn a_picture_reference_past_the_pictures_is_refused() {
        let mut d = TextDoc::new();
        d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: 0, width: 600, height: 300 }), ..Default::default() });
        assert!(matches!(load(&save_to_vec(&d, &[])), Err(LoadError::Bad("picture reference"))));
    }

    #[test]
    fn round_trip() {
        let d = sample();
        let cps = vec![Checkpoint { block: 0, line: 0 }, Checkpoint { block: 40, line: 2 }];
        let bytes = save_to_vec(&d, &cps);
        let l = load(&bytes).unwrap();
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
            TextOp::SetPage { page: PageSetup { width: 12240, height: 15840, ..PageSetup::default() }, header: Some(HeaderFooter { text: "Draft".into(), align: Align::End }), footer: None },
            TextOp::SetNote { id: 5, note: Some(Note { kind: NoteKind::End, paras: vec![Para::new(d.new_id(), "note", 0)] }) },
            TextOp::SetNote { id: 5, note: None },
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
        let bytes = save_to_vec(&sample(), &[Checkpoint { block: 0, line: 0 }]);
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
