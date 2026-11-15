//! `.doc` in (Word 97–2003, MS-DOC), for the converter process.
//!
//! The binary document is translated into the WordprocessingML Word itself
//! would write for it (document, styles, numbering, footnotes, endnotes,
//! header, footer and pictures, in a package kept in memory), and that goes
//! through the `.docx` reader: one mapping of formatting to the text model,
//! one set of fidelity notes.
//!
//! Read: the text through the piece table (8- and 16-bit pieces), paragraph
//! and character properties from their formatted disk pages and from fast
//! saves' property lists, styles with inheritance and the default font,
//! lists (`PlfLst`/`PlfLfo`), tables (rows and cells, widths, merges),
//! inline pictures (PNG and JPEG from the Data stream), footnotes and
//! endnotes, the first section's header and footer, sections (page size,
//! margins, columns, breaks), fields (as fields, so page numbers, tables of
//! contents and cross-references work), bookmarks, tracked insertions and
//! deletions with their authors. A document protected with a password (RC4
//! or RC4 CryptoAPI) is decrypted here, inside the sandbox.
//!
//! Reported, not carried: comments, text boxes and floating shapes,
//! embedded objects, pictures in other formats, nested tables (their text
//! kept). Refused: Word 95 and older, XOR obfuscation.

pub mod bin;
pub mod props;
#[doc(hidden)]
pub mod sample;

use std::collections::HashMap;

use officina_text::model::TextDoc;

use crate::cfb::{self, Cfb};
use crate::crypt::CryptError;
use crate::docx::Loaded;
use crate::report::{Kind, Report};
use bin::{fc, Clx, Fib, FkpRun};
use props::{esc, esc_s, Chp, Pap, Sep, Tap};

#[derive(Debug)]
pub enum DocError {
    NeedsPassword,
    WrongPassword,
    Bad(String),
}

impl std::fmt::Display for DocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocError::NeedsPassword => f.write_str(crate::launch::NEEDS_PASSWORD),
            DocError::WrongPassword => f.write_str(crate::launch::WRONG_PASSWORD),
            DocError::Bad(e) => write!(f, "not a document this can read: {e}"),
        }
    }
}

impl std::error::Error for DocError {}

fn bad(s: impl Into<String>) -> DocError {
    DocError::Bad(s.into())
}

impl From<bin::Bad> for DocError {
    fn from(b: bin::Bad) -> Self {
        DocError::Bad(b.0)
    }
}

/// Characters a document may have.
const MAX_TEXT: u32 = 1 << 28;
/// Pictures a document may carry.
const MAX_PICTURES: usize = 1 << 16;

/// Whether `data` is a Word 97–2003 document (a compound file with a
/// `WordDocument` stream).
pub fn is_doc(data: &[u8]) -> bool {
    cfb::is_cfb(data) && Cfb::open(data).is_ok_and(|c| c.has("WordDocument"))
}

/// A list's levels: start, number format, text, paragraph properties.
struct Level {
    start: i32,
    nfc: u8,
    text: String,
    papx: Vec<u8>,
}

struct Style {
    name: String,
    stk: u8,
    base: Option<u16>,
    next: Option<u16>,
    papx: Vec<u8>,
    chpx: Vec<u8>,
}

/// The translation's state.
struct Doc<'a> {
    data: &'a [u8],
    clx: Clx,
    text: Vec<u16>,
    papx: Vec<FkpRun>,
    chpx: Vec<FkpRun>,
    fonts: Vec<String>,
    styles: Vec<Option<Style>>,
    /// Per style: its character properties with its bases', for toggles.
    style_chp: Vec<Chp>,
    authors: Vec<String>,
    /// Footnote and endnote references: character position → note id.
    ftn_refs: HashMap<u32, usize>,
    edn_refs: HashMap<u32, usize>,
    /// Bookmark starts and ends in the main text: (cp, xml), sorted.
    marks: Vec<(u32, String)>,
    /// Section ends (the character position after each section's mark),
    /// and each section's properties.
    sed_ends: Vec<u32>,
    seps: Vec<Sep>,
    /// Pictures: (part name, bytes), and their relationship ids.
    media: Vec<(String, Vec<u8>)>,
    rep: Report,
    next_rev: u32,
    /// Comment references in the main text: position → comment id.
    atn_refs: HashMap<u32, usize>,
    /// Text boxes in the main text: anchor position → their story's range.
    boxes: HashMap<u32, (u32, u32)>,
    /// Header and footer relationship ids, once made.
    hdr_refs: String,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
}
fn i16_at(b: &[u8], o: usize) -> i16 {
    u16_at(b, o) as i16
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

/// The stylesheet (MS-DOC §2.9.271).
fn stylesheet(b: &[u8]) -> (Vec<Option<Style>>, u16) {
    let mut out = Vec::new();
    let cb_stshi = u16_at(b, 0) as usize;
    let cstd = u16_at(b, 2) as usize;
    let cb_base = u16_at(b, 4) as usize;
    let ftc = u16_at(b, 14);
    let mut p = 2 + cb_stshi;
    for _ in 0..cstd.min(4096) {
        let cb = u16_at(b, p) as usize;
        p += 2;
        let Some(std) = b.get(p..p + cb) else { break };
        p += cb;
        if cb == 0 || cb < cb_base || cb_base < 10 {
            out.push(None);
            continue;
        }
        let stk = (u16_at(std, 2) & 0xF) as u8;
        let base = u16_at(std, 2) >> 4;
        let cupx = (u16_at(std, 4) & 0xF) as usize;
        let next = u16_at(std, 4) >> 4;
        let mut q = cb_base;
        let cch = u16_at(std, q) as usize;
        q += 2;
        let name: Vec<u16> = (0..cch).map(|i| u16_at(std, q + 2 * i)).collect();
        q += 2 * cch + 2;
        let mut upx = Vec::new();
        for _ in 0..cupx.min(3) {
            if q % 2 == 1 {
                q += 1;
            }
            let n = u16_at(std, q) as usize;
            q += 2;
            upx.push(std.get(q..q + n).unwrap_or(&[]).to_vec());
            q += n;
        }
        let (papx, chpx) = match stk {
            1 => (upx.first().map(|u| u.get(2..).unwrap_or(&[]).to_vec()).unwrap_or_default(), upx.get(1).cloned().unwrap_or_default()),
            2 => (Vec::new(), upx.first().cloned().unwrap_or_default()),
            _ => (Vec::new(), Vec::new()),
        };
        out.push(Some(Style { name: String::from_utf16_lossy(&name), stk, base: (base != 0xFFF).then_some(base), next: (next != 0xFFF).then_some(next), papx, chpx }));
    }
    (out, ftc)
}

/// Lists: per `LSTF`, its id and levels; then the `LFO`s' list ids.
fn lists(table: &[u8], fib: &Fib) -> (Vec<(i32, Vec<Level>)>, Vec<i32>) {
    let mut lsts = Vec::new();
    let mut lfos = Vec::new();
    if let Some(&(at, lcb)) = fib.fclcb.get(fc::PLF_LST) {
        if lcb > 0 {
            let b = table.get(at as usize..).unwrap_or(&[]);
            let n = (i16_at(b, 0).max(0) as usize).min(4096);
            let mut heads = Vec::new();
            for i in 0..n {
                let o = 2 + i * 28;
                if o + 28 > b.len() {
                    break;
                }
                heads.push((u32_at(b, o) as i32, b[o + 26] & 1 != 0));
            }
            let mut p = 2 + n * 28;
            'lists: for (lsid, simple) in heads {
                let mut levels = Vec::new();
                for _ in 0..if simple { 1 } else { 9 } {
                    let Some(l) = b.get(p..p + 28) else { break 'lists };
                    let start = u32_at(l, 0) as i32;
                    let nfc = l[4];
                    let (cb_chpx, cb_papx) = (l[24] as usize, l[25] as usize);
                    p += 28;
                    let papx = b.get(p..p + cb_papx).unwrap_or(&[]).to_vec();
                    p += cb_papx + cb_chpx;
                    let cch = u16_at(b, p) as usize;
                    p += 2;
                    let mut text = String::new();
                    for k in 0..cch.min(64) {
                        let u = u16_at(b, p + 2 * k);
                        if u < 9 {
                            text.push_str(&format!("%{}", u + 1));
                        } else {
                            text.extend(char::decode_utf16([u]).map(|c| c.unwrap_or('\u{FFFD}')));
                        }
                    }
                    p += 2 * cch;
                    levels.push(Level { start, nfc, text, papx });
                }
                lsts.push((lsid, levels));
            }
        }
    }
    let b = fib.part(fc::PLF_LFO, table);
    let n = (u32_at(b, 0) as usize).min(8192);
    for i in 0..n {
        let o = 4 + i * 16;
        if o + 16 > b.len() {
            break;
        }
        lfos.push(u32_at(b, o) as i32);
    }
    (lsts, lfos)
}

fn numbering_xml(lsts: &[(i32, Vec<Level>)], lfos: &[i32]) -> String {
    let mut x = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:numbering xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#);
    for (k, (_, levels)) in lsts.iter().enumerate() {
        x.push_str(&format!(r#"<w:abstractNum w:abstractNumId="{k}">"#));
        for (i, l) in levels.iter().enumerate() {
            let fmt = match l.nfc {
                0 => "decimal",
                1 => "upperRoman",
                2 => "lowerRoman",
                3 => "upperLetter",
                4 => "lowerLetter",
                5 => "ordinal",
                22 => "decimalZero",
                23 => "bullet",
                255 => "none",
                _ => "decimal",
            };
            let mut pp = String::new();
            let mut pap = Pap::default();
            pap.apply(&l.papx);
            pap.xml(&mut pp);
            x.push_str(&format!(r#"<w:lvl w:ilvl="{i}"><w:start w:val="{}"/><w:numFmt w:val="{fmt}"/><w:lvlText w:val="{}"/><w:pPr>{pp}</w:pPr></w:lvl>"#, l.start, esc_s(&l.text)));
        }
        x.push_str("</w:abstractNum>");
    }
    for (j, lsid) in lfos.iter().enumerate() {
        if let Some(k) = lsts.iter().position(|l| l.0 == *lsid) {
            x.push_str(&format!(r#"<w:num w:numId="{}"><w:abstractNumId w:val="{k}"/></w:num>"#, j + 1));
        }
    }
    x.push_str("</w:numbering>");
    x
}

/// The first PNG or JPEG in OfficeArt records (a picture's `BLIP`).
pub(crate) fn find_blip(b: &[u8], depth: usize) -> Result<(&'static str, &[u8]), bool> {
    let mut p = 0;
    let mut other = false;
    while p + 8 <= b.len() && depth < 8 {
        let vi = u16_at(b, p);
        let ty = u16_at(b, p + 2);
        let len = (u32_at(b, p + 4) as usize).min(b.len() - p - 8);
        let body = &b[p + 8..p + 8 + len];
        let inst = vi >> 4;
        if vi & 0xF == 0xF {
            match find_blip(body, depth + 1) {
                Ok(x) => return Ok(x),
                Err(o) => other |= o,
            }
        } else if ty == 0xF007 && body.len() >= 36 {
            let name = body[33] as usize;
            match find_blip(body.get(36 + name..).unwrap_or(&[]), depth + 1) {
                Ok(x) => return Ok(x),
                Err(o) => other |= o,
            }
        } else {
            let (ext, two) = match ty {
                0xF01E => ("png", inst == 0x6E1),
                0xF01D | 0xF02A => ("jpeg", inst == 0x46B || inst == 0x6E3),
                0xF01A | 0xF01B | 0xF01C | 0xF01F | 0xF029 => {
                    other = true;
                    ("", false)
                }
                _ => ("", false),
            };
            if !ext.is_empty() {
                if let Some(d) = body.get(if two { 33 } else { 17 }..) {
                    return Ok((ext, d));
                }
            }
        }
        p += 8 + len;
    }
    Err(other)
}

/// One cell's or row's content being collected into a table.
#[derive(Default)]
struct Table {
    rows: Vec<(Vec<String>, Tap)>,
    cells: Vec<String>,
    cell: String,
    nested: bool,
}

impl Table {
    fn xml(self, out: &mut String) {
        let mut rows = self.rows;
        if !self.cells.is_empty() || !self.cell.is_empty() {
            let mut cells = self.cells;
            if !self.cell.is_empty() {
                cells.push(self.cell);
            }
            rows.push((cells, Tap::default()));
        }
        if rows.is_empty() {
            return;
        }
        out.push_str("<w:tbl><w:tblPr/>");
        for (cells, tap) in rows {
            out.push_str("<w:tr>");
            let mut i = 0;
            while i < cells.len() {
                let mut span = 1;
                while tap.merged.get(i + span).copied().unwrap_or(false) && i + span < cells.len() {
                    span += 1;
                }
                let w = match (tap.centers.get(i), tap.centers.get(i + span)) {
                    (Some(a), Some(b)) if b > a => (*b as i32 - *a as i32) as u32,
                    _ => 1440,
                };
                out.push_str(&format!(r#"<w:tc><w:tcPr><w:tcW w:w="{w}" w:type="dxa"/>"#));
                if span > 1 {
                    out.push_str(&format!(r#"<w:gridSpan w:val="{span}"/>"#));
                }
                match tap.vmerge.get(i) {
                    Some(3) => out.push_str(r#"<w:vMerge w:val="restart"/>"#),
                    Some(1) => out.push_str("<w:vMerge/>"),
                    _ => {}
                }
                out.push_str("</w:tcPr>");
                let mut content = String::new();
                for c in &cells[i..i + span] {
                    content.push_str(c);
                }
                if content.is_empty() {
                    content.push_str("<w:p/>");
                }
                out.push_str(&content);
                out.push_str("</w:tc>");
                i += span;
            }
            out.push_str("</w:tr>");
        }
        out.push_str("</w:tbl>");
    }
}

/// Which text a story is, for its special characters.
#[derive(Clone, Copy, PartialEq)]
enum Story {
    Main,
    Note,
    Header,
}

impl<'a> Doc<'a> {
    fn ch(&self, cp: u32) -> u16 {
        self.text.get(cp as usize).copied().unwrap_or(0x0D)
    }

    /// A paragraph's properties, from the run holding its mark.
    fn pap_at(&self, mark: u32) -> (u16, Pap) {
        let mut pap = Pap::default();
        let mut istd = 0;
        if let Some(f) = self.clx.fc(mark) {
            if let Some(i) = bin::run_at(&self.papx, f) {
                istd = self.papx[i].istd;
                pap.apply(&self.papx[i].grpprl);
            }
        }
        self.piece_sprms(mark, |g| pap.apply(g));
        (istd, pap)
    }

    /// A fast save's property list for the piece holding `cp`.
    fn piece_sprms(&self, cp: u32, mut f: impl FnMut(&[u8])) {
        if let Some(p) = self.clx.piece(cp) {
            let prm = self.clx.pieces[p].prm;
            if prm & 1 == 1 {
                if let Some(g) = self.clx.prcs.get((prm >> 1) as usize) {
                    f(g);
                }
            }
        }
    }

    /// Runs of one character formatting in `cp0..cp1`: (from, to, sprms).
    fn char_runs(&self, cp0: u32, cp1: u32) -> Vec<(u32, u32, Option<usize>)> {
        let mut out = Vec::new();
        let mut cp = cp0;
        while cp < cp1 {
            let Some(pi) = self.clx.piece(cp) else {
                out.push((cp, cp1, None));
                break;
            };
            let p = self.clx.pieces[pi];
            let per = if p.compressed { 1 } else { 2 };
            let f = p.fc + (cp - p.cp0) * per;
            let mut end = p.cp1.min(cp1);
            let run = bin::run_at(&self.chpx, f);
            if let Some(r) = run {
                let to = p.cp0 + (self.chpx[r].fc1 - p.fc).div_ceil(per);
                end = end.min(to.max(cp + 1));
            } else if let Some(n) = self.chpx.get(self.chpx.partition_point(|r| r.fc0 <= f)) {
                if n.fc0 > f {
                    let to = p.cp0 + (n.fc0 - p.fc).div_ceil(per);
                    end = end.min(to.max(cp + 1));
                }
            }
            out.push((cp, end, run));
            cp = end;
        }
        out
    }

    fn rev_attrs(&mut self, chp: &Chp, del: bool) -> String {
        let (who, when) = if del { (chp.del_author, chp.del_dttm) } else { (chp.author, chp.dttm) };
        self.rev_of(who, when)
    }

    /// A revision's attributes: id, author and (when known) date.
    fn rev_of(&mut self, who: u16, when: u32) -> String {
        self.next_rev += 1;
        let author = self.authors.get(who as usize).cloned().unwrap_or_else(|| "Unknown".into());
        let date = bin::dttm(when).map(|d| format!(r#" w:date="{d}""#)).unwrap_or_default();
        format!(r#" w:id="{}" w:author="{}"{date}"#, self.next_rev, esc_s(&author))
    }

    /// The character properties at `cp`, over paragraph style `istd`'s.
    fn chp_at(&self, cp: u32, istd: u16) -> Chp {
        let base = self.style_chp.get(istd as usize).cloned().unwrap_or_default();
        let mut chp = Chp::default();
        if let Some((_, _, Some(r))) = self.char_runs(cp, cp + 1).first() {
            chp.apply(&self.chpx[*r].grpprl.clone(), &base);
        }
        self.piece_sprms(cp, |g| chp.apply(g, &base));
        chp
    }

    /// A picture at a `sprmCPicLocation`, as `w:drawing`.
    fn picture(&mut self, at: u32) -> Option<String> {
        let b = self.data.get(at as usize..)?;
        let lcb = (u32_at(b, 0) as usize).min(b.len());
        let cb_header = u16_at(b, 4) as usize;
        if lcb < cb_header || cb_header < 0x44 {
            return None;
        }
        let mm = i16_at(b, 6);
        let (dxa, dya, mx, my) = (i16_at(b, 28).max(0) as u64, i16_at(b, 30).max(0) as u64, u16_at(b, 32) as u64, u16_at(b, 34) as u64);
        let mut p = cb_header;
        if mm == 0x66 {
            p += 1 + *b.get(p).unwrap_or(&0) as usize;
        }
        let (ext, bytes) = match find_blip(b.get(p..lcb)?, 0) {
            Ok(x) => x,
            Err(other) => {
                if other {
                    self.rep.note(Kind::Dropped, "pictures in formats not shown (EMF, WMF, PICT, BMP, TIFF)");
                }
                return None;
            }
        };
        if self.media.len() >= MAX_PICTURES {
            return None;
        }
        let n = self.media.len() + 1;
        self.media.push((format!("word/media/image{n}.{ext}"), bytes.to_vec()));
        let scale = |g: u64, m: u64| g * if m == 0 { 1000 } else { m } / 1000 * 635;
        let (cx, cy) = (scale(dxa, mx), scale(dya, my));
        Some(format!(r#"<w:drawing><wp:inline><wp:extent cx="{cx}" cy="{cy}"/><a:graphic><a:graphicData><pic:pic><pic:blipFill><a:blip r:embed="rIdImg{n}"/></pic:blipFill></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing>"#))
    }

    /// A story's paragraphs and tables, `cp0..cp1`, as body XML.
    fn story(&mut self, cp0: u32, cp1: u32, kind: Story) -> String {
        let mut out = String::new();
        let mut table: Option<Table> = None;
        let mut fields: Vec<bool> = Vec::new();
        let mut cp = cp0;
        let mut mark_at = self.marks.partition_point(|m| m.0 < cp0);
        let mut nested_note = false;
        while cp < cp1 {
            // The paragraph: up to its mark.
            let mut e = cp;
            while e < cp1 {
                let c = self.ch(e);
                if c == 0x0D || c == 0x07 || (c == 0x0C && kind == Story::Main && self.sed_ends.contains(&(e + 1))) {
                    break;
                }
                e += 1;
            }
            let end = e.min(cp1 - 1);
            let (istd, pap) = self.pap_at(end);
            let in_table = pap.in_table || pap.itap >= 1;
            let ttp = pap.ttp || (pap.itap == 1 && pap.inner_ttp) || (in_table && self.ch(end) == 0x07 && pap.ttp);
            if in_table && kind == Story::Main || in_table && kind != Story::Main {
                if ttp {
                    let t = table.get_or_insert_with(Table::default);
                    let g = self.clx.fc(end).and_then(|f| bin::run_at(&self.papx, f)).map(|i| self.papx[i].grpprl.clone()).unwrap_or_default();
                    let cells = std::mem::take(&mut t.cells);
                    t.rows.push((cells, Tap::read(&g)));
                    cp = e + 1;
                    continue;
                }
                if pap.itap >= 2 && pap.inner_ttp {
                    cp = e + 1;
                    continue;
                }
                if pap.itap >= 2 && !nested_note {
                    nested_note = true;
                    self.rep.note(Kind::Approximate, "nested tables (their text kept in the cell)");
                }
            } else if let Some(t) = table.take() {
                t.xml(&mut out);
            }
            // The paragraph's XML.
            let mut p = String::from("<w:p><w:pPr>");
            if istd != 0 {
                p.push_str(&format!(r#"<w:pStyle w:val="S{istd}"/>"#));
            }
            if let Some(ilfo) = pap.ilfo {
                p.push_str(&format!(r#"<w:numPr><w:ilvl w:val="{}"/><w:numId w:val="{}"/></w:numPr>"#, pap.ilvl.unwrap_or(0).min(8), if ilfo >= 0x7FF { 0 } else { ilfo }));
            }
            pap.xml(&mut p);
            // A tracked paragraph mark: inserted (a split) or deleted (a join).
            if e < cp1 && self.ch(e) == 0x0D && kind == Story::Main {
                let m = self.chp_at(e, istd);
                if m.del {
                    let r = self.rev_attrs(&m, true);
                    p.push_str(&format!("<w:rPr><w:del{r}/></w:rPr>"));
                } else if m.ins {
                    let r = self.rev_attrs(&m, false);
                    p.push_str(&format!("<w:rPr><w:ins{r}/></w:rPr>"));
                }
            }
            if kind == Story::Main && self.ch(e) == 0x0C {
                if let Some(k) = self.sed_ends.iter().position(|&x| x == e + 1) {
                    if k + 1 < self.seps.len() {
                        p.push_str("<w:sectPr>");
                        self.seps[k].clone().xml(&mut p);
                        p.push_str(&self.hdr_refs);
                        p.push_str("</w:sectPr>");
                    }
                }
            }
            p.push_str("</w:pPr>");
            let base = self.style_chp.get(istd as usize).cloned().unwrap_or_default();
            for (a, b, run) in self.char_runs(cp, e.min(cp1)) {
                let mut chp = Chp::default();
                if let Some(r) = run {
                    chp.apply(&self.chpx[r].grpprl.clone(), &base);
                }
                self.piece_sprms(a, |g| chp.apply(g, &base));
                let mut rpr = String::new();
                chp.xml(&self.fonts, &mut rpr);
                if let Some((who, when)) = chp.prop_rmark {
                    // The format before isn't kept in the file: the style's.
                    let ch = format!("<w:rPrChange{}><w:rPr/></w:rPrChange>", self.rev_of(who, when));
                    match rpr.strip_suffix("</w:rPr>") {
                        Some(r) => rpr = format!("{r}{ch}</w:rPr>"),
                        None => rpr = format!("<w:rPr>{ch}</w:rPr>"),
                    }
                }
                let (open, close, t) = if chp.del {
                    (format!("<w:del{}>", self.rev_attrs(&chp, true)), "</w:del>", "w:delText")
                } else if chp.ins {
                    (format!("<w:ins{}>", self.rev_attrs(&chp, false)), "</w:ins>", "w:t")
                } else {
                    (String::new(), "", "w:t")
                };
                let mut units: Vec<u16> = Vec::new();
                let mut items = String::new();
                let flush = |units: &mut Vec<u16>, items: &mut String, instr: bool| {
                    if units.is_empty() {
                        return;
                    }
                    let s = String::from_utf16_lossy(units);
                    units.clear();
                    let tag = if instr { "w:instrText" } else { t };
                    items.push_str(&format!(r#"<{tag} xml:space="preserve">"#));
                    esc(&s, items);
                    items.push_str(&format!("</{tag}>"));
                };
                let emit = |items: &mut String, p: &mut String, rpr: &str| {
                    if !items.is_empty() {
                        p.push_str(&open);
                        p.push_str("<w:r>");
                        p.push_str(rpr);
                        p.push_str(items);
                        p.push_str("</w:r>");
                        p.push_str(close);
                        items.clear();
                    }
                };
                for k in a..b {
                    if kind == Story::Main {
                        while mark_at < self.marks.len() && self.marks[mark_at].0 <= k {
                            let instr = fields.last() == Some(&true);
                            flush(&mut units, &mut items, instr);
                            emit(&mut items, &mut p, &rpr);
                            p.push_str(&self.marks[mark_at].1);
                            mark_at += 1;
                        }
                    }
                    let c = self.ch(k);
                    let instr = fields.last() == Some(&true);
                    match c {
                        0x13 | 0x14 | 0x15 => {
                            flush(&mut units, &mut items, instr);
                            emit(&mut items, &mut p, &rpr);
                            let ty = match c {
                                0x13 => {
                                    fields.push(true);
                                    "begin"
                                }
                                0x14 => {
                                    if let Some(f) = fields.last_mut() {
                                        *f = false;
                                    }
                                    "separate"
                                }
                                _ => {
                                    fields.pop();
                                    "end"
                                }
                            };
                            p.push_str(&format!(r#"<w:r><w:fldChar w:fldCharType="{ty}"/></w:r>"#));
                        }
                        0x09 => {
                            flush(&mut units, &mut items, instr);
                            items.push_str("<w:tab/>");
                        }
                        0x0B => {
                            flush(&mut units, &mut items, instr);
                            items.push_str("<w:br/>");
                        }
                        0x0C => {
                            flush(&mut units, &mut items, instr);
                            items.push_str(r#"<w:br w:type="page"/>"#);
                        }
                        0x0E => {
                            flush(&mut units, &mut items, instr);
                            items.push_str(r#"<w:br w:type="column"/>"#);
                        }
                        0x1E => units.push(0x2011),
                        0x1F => units.push(0x00AD),
                        0x02 if chp.spec => {
                            flush(&mut units, &mut items, instr);
                            if kind == Story::Main {
                                if let Some(id) = self.ftn_refs.get(&k) {
                                    items.push_str(&format!(r#"<w:footnoteReference w:id="{id}"/>"#));
                                } else if let Some(id) = self.edn_refs.get(&k) {
                                    items.push_str(&format!(r#"<w:endnoteReference w:id="{id}"/>"#));
                                }
                            }
                        }
                        0x01 if chp.spec => {
                            flush(&mut units, &mut items, instr);
                            if chp.ole {
                                self.rep.note(Kind::Dropped, "embedded objects (their picture kept when there is one)");
                            }
                            if !chp.data && !instr {
                                if let Some(x) = chp.pic.and_then(|at| self.picture(at)) {
                                    items.push_str(&x);
                                }
                            }
                        }
                        0x08 if chp.spec => match self.boxes.remove(&k).filter(|_| kind == Story::Main) {
                            Some((ba, mut bb)) => {
                                // Each box's text ends with a paragraph mark of its own.
                                if bb > ba + 1 && self.ch(bb - 1) == 0x0D && self.ch(bb - 2) == 0x0D {
                                    bb -= 1;
                                }
                                let body = self.story(ba, bb, Story::Note);
                                flush(&mut units, &mut items, instr);
                                items.push_str(r#"<w:pict><v:shape><v:textbox><w:txbxContent>"#);
                                items.push_str(&body);
                                items.push_str("</w:txbxContent></v:textbox></v:shape></w:pict>");
                            }
                            None => self.rep.note(Kind::Dropped, "floating pictures, shapes and text boxes"),
                        },
                        0x05 if chp.spec => {
                            if kind == Story::Main {
                                if let Some(id) = self.atn_refs.get(&k) {
                                    flush(&mut units, &mut items, instr);
                                    items.push_str(&format!(r#"<w:commentReference w:id="{id}"/>"#));
                                }
                            }
                        }
                        0x28 if chp.spec && chp.symbol.is_some() => {
                            let (_, ch) = chp.symbol.unwrap();
                            units.push(ch);
                        }
                        c if c < 0x20 => {}
                        c => units.push(c),
                    }
                }
                let instr = fields.last() == Some(&true);
                flush(&mut units, &mut items, instr);
                emit(&mut items, &mut p, &rpr);
            }
            p.push_str("</w:p>");
            if in_table {
                let t = table.get_or_insert_with(Table::default);
                t.cell.push_str(&p);
                if self.ch(e) == 0x07 && pap.itap <= 1 {
                    let c = std::mem::take(&mut t.cell);
                    t.cells.push(c);
                }
                t.nested |= pap.itap >= 2;
            } else {
                out.push_str(&p);
            }
            cp = e + 1;
        }
        if let Some(t) = table.take() {
            t.xml(&mut out);
        }
        // Fields still open at the end: closed, so their text stays.
        out
    }
}

const NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" xmlns:v="urn:schemas-microsoft-com:vml" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml""#;

/// The WordprocessingML package for a `.doc`: (part name, bytes).
fn translate(data: &[u8], password: Option<&str>, rep: &mut Report) -> Result<Vec<(String, Vec<u8>)>, DocError> {
    if !cfb::is_cfb(data) {
        let head = String::from_utf8_lossy(&data[..data.len().min(512)]).to_ascii_lowercase();
        return Err(bad(if head.starts_with("{\\rtf") {
            "this .doc is really an RTF document, which isn't supported yet"
        } else if head.contains("<html") || head.contains("<?xml") {
            "this .doc is really an HTML or XML document, which isn't supported"
        } else if data.len() >= 2 && u16::from_le_bytes([data[0], data[1]]) == 0xA5DB {
            "Word 2 documents aren't supported"
        } else {
            "not a Word 97–2003 document"
        }));
    }
    let c = Cfb::open(data).map_err(|e| bad(e.0))?;
    if !c.has("WordDocument") {
        return Err(bad("no WordDocument stream"));
    }
    let mut word = c.stream("WordDocument").map_err(|e| bad(e.0))?;
    // The FIB's start is in the clear; the rest is read once decrypted.
    let (encrypted, obfuscated, table1, l_key) = bin::fib_base(&word).ok_or_else(|| bad("the FIB is cut short"))?;
    // Word 95 and older keep no table stream; say which version it is.
    Fib::read(&word[..word.len().min(68)]).or_else(|e| if e.0.contains("cut short") { Ok(Fib::default()) } else { Err(e) })?;
    let tname = if table1 { "1Table" } else { "0Table" };
    let mut table = c.stream(tname).map_err(|_| bad(format!("no {tname} stream")))?;
    let mut data_s = if c.has("Data") { c.stream("Data").map_err(|e| bad(e.0))? } else { Vec::new() };
    if encrypted {
        let Some(pw) = password else { return Err(DocError::NeedsPassword) };
        let k = match bin::cipher(&table, obfuscated, l_key, pw) {
            Ok(k) => k,
            Err(CryptError::WrongPassword) => return Err(DocError::WrongPassword),
            Err(e) => return Err(bad(e.to_string())),
        };
        bin::rc4_stream(&mut word, &k, 68);
        bin::rc4_stream(&mut table, &k, l_key as usize);
        bin::rc4_stream(&mut data_s, &k, 0);
    }
    let fib = Fib::read(&word)?;
    let clx = Clx::read(fib.part(fc::CLX, &table))?;
    let total = [fib.ccp_text, fib.ccp_ftn, fib.ccp_hdd, fib.ccp_atn, fib.ccp_edn, fib.ccp_txbx, fib.ccp_hdr_txbx].iter().fold(0u64, |a, &b| a + b as u64);
    let last = clx.pieces.last().map_or(0, |p| p.cp1);
    // No text lies past the piece table, whatever the FIB's lengths say.
    let total = (total.min(MAX_TEXT as u64) as u32).min(last.min(MAX_TEXT));
    if fib.ccp_text > total {
        return Err(bad("the text is shorter than the document says"));
    }
    let text = bin::text(&word, &clx, total);
    let papx = bin::fkp_runs(&word, fib.part(fc::PLCF_BTE_PAPX, &table), true);
    let chpx = bin::fkp_runs(&word, fib.part(fc::PLCF_BTE_CHPX, &table), false);
    let fonts = bin::fonts(fib.part(fc::STTBF_FFN, &table));
    let (styles, default_ftc) = stylesheet(fib.part(fc::STSHF, &table));
    // Each style's character properties over its bases', for toggles.
    let mut style_chp = vec![Chp::default(); styles.len()];
    for (i, cell) in style_chp.iter_mut().enumerate() {
        let mut chain = Vec::new();
        let mut k = Some(i as u16);
        while let Some(j) = k {
            if chain.len() > 16 || chain.contains(&j) {
                break;
            }
            chain.push(j);
            k = styles.get(j as usize).and_then(|s| s.as_ref()).and_then(|s| s.base);
        }
        let mut c = Chp::default();
        for j in chain.iter().rev() {
            if let Some(Some(s)) = styles.get(*j as usize) {
                let mut own = Chp::default();
                own.apply(&s.chpx, &c);
                c = own.over(&c);
            }
        }
        *cell = c;
    }
    let authors = bin::sttb(fib.part(fc::STTBF_RMARK, &table));
    let mut d = Doc { data: &data_s, clx, text, papx, chpx, fonts, styles, style_chp, authors, ftn_refs: HashMap::new(), edn_refs: HashMap::new(), marks: Vec::new(), sed_ends: Vec::new(), seps: Vec::new(), media: Vec::new(), rep: Report::default(), next_rev: 0, hdr_refs: String::new(), boxes: HashMap::new(), atn_refs: HashMap::new() };
    // Notes: their references and text ranges.
    let ftn_base = fib.ccp_text;
    let hdd_base = ftn_base + fib.ccp_ftn;
    let atn_base = hdd_base + fib.ccp_hdd;
    let edn_base = atn_base + fib.ccp_atn;
    let note_ranges = |refs: usize, txts: usize, base: u32, map: &mut HashMap<u32, usize>| -> Vec<(u32, u32)> {
        let (rcps, _) = bin::plc(fib.part(refs, &table), 2);
        let (tcps, _) = bin::plc(fib.part(txts, &table), 0);
        let mut out = Vec::new();
        for (i, &r) in rcps.iter().take(rcps.len().saturating_sub(1)).enumerate().take(1 << 16) {
            let (Some(&a), Some(&b)) = (tcps.get(i), tcps.get(i + 1)) else { break };
            map.insert(r, i + 1);
            out.push((base.saturating_add(a).min(total), base.saturating_add(b).min(total)));
        }
        out
    };
    let mut ftn_refs = HashMap::new();
    let mut edn_refs = HashMap::new();
    let ftns = note_ranges(fc::PLCFFND_REF, fc::PLCFFND_TXT, ftn_base, &mut ftn_refs);
    let edns = note_ranges(fc::PLCFEND_REF, fc::PLCFEND_TXT, edn_base, &mut edn_refs);
    d.ftn_refs = ftn_refs;
    d.edn_refs = edn_refs;
    // Comments: each reference's author, initials and range; their text,
    // and (Word 2002 on) their dates and threads.
    struct Atn {
        author: String,
        initials: String,
        date: Option<String>,
        parent: Option<usize>,
        text: (u32, u32),
    }
    let mut atns: Vec<Atn> = Vec::new();
    {
        let owners = bin::xsts(fib.part(fc::GRP_XST_ATN_OWNERS, &table));
        let (rcps, atrds) = bin::plc(fib.part(fc::PLCFAND_REF, &table), 30);
        let (tcps, _) = bin::plc(fib.part(fc::PLCFAND_TXT, &table), 0);
        let extra = fib.part(fc::ATRD_EXTRA, &table);
        // Ranges: bookmarks tagged as each reference says.
        let tags = bin::sttb_extra(fib.part(fc::STTBF_ATN_BKMK, &table));
        let (bstarts, bkfs) = bin::plc(fib.part(fc::PLCF_ATN_BKF, &table), 4);
        let (bends, _) = bin::plc(fib.part(fc::PLCF_ATN_BKL, &table), 0);
        let mut range_of: HashMap<u32, (u32, u32)> = HashMap::new();
        for (i, (_, x)) in tags.iter().enumerate().take(1 << 16) {
            let (Some(&a), Some(f)) = (bstarts.get(i), bkfs.get(i)) else { break };
            let Some(&b) = bends.get(u16_at(f, 0) as usize) else { continue };
            if x.len() >= 6 && b >= a && b <= fib.ccp_text {
                range_of.insert(u32_at(x, 2), (a, b));
            }
        }
        for (i, r) in atrds.iter().enumerate().take(1 << 14) {
            let (Some(&cp), Some(&a), Some(&b)) = (rcps.get(i), tcps.get(i), tcps.get(i + 1)) else { break };
            if cp >= fib.ccp_text {
                continue;
            }
            let initials: String = {
                let n = (u16_at(r, 0) as usize).min(9);
                String::from_utf16_lossy(&(0..n).map(|k| u16_at(r, 2 + 2 * k)).collect::<Vec<_>>())
            };
            let author = owners.get(u16_at(r, 20) as usize).cloned().unwrap_or_else(|| "Unknown".into());
            let tag = u32_at(r, 26);
            let (s0, s1) = range_of.get(&tag).copied().filter(|_| tag != u32::MAX).unwrap_or((cp, cp));
            let x = extra.get(i * 18..i * 18 + 18);
            let date = x.and_then(|x| bin::dttm(u32_at(x, 0)));
            let parent = x.filter(|x| u32_at(x, 6) > 0).and_then(|x| {
                let d = u32_at(x, 10) as i32;
                let p = i as i64 + d as i64;
                (d != 0 && p >= 0 && (p as usize) < i).then_some(p as usize)
            });
            let id = atns.len();
            d.atn_refs.insert(cp, id);
            d.marks.push((s0, format!(r#"<w:commentRangeStart w:id="{id}"/>"#)));
            d.marks.push((s1, format!(r#"<w:commentRangeEnd w:id="{id}"/>"#)));
            atns.push(Atn { author, initials, date, parent, text: (atn_base.saturating_add(a).min(total), atn_base.saturating_add(b).min(total)) });
        }
    }
    // Text boxes: each anchor's shape (PlcSpaMom) to its story (PlcftxbxTxt) by shape id.
    let txbx_base = edn_base + fib.ccp_edn;
    let (bcps, bxs) = bin::plc(fib.part(fc::PLCFTXBX_TXT, &table), 22);
    let mut by_spid: HashMap<u32, (u32, u32)> = HashMap::new();
    for (i, f) in bxs.iter().enumerate().take(1 << 14) {
        let (Some(&a), Some(&b)) = (bcps.get(i), bcps.get(i + 1)) else { break };
        let lid = u32_at(f, 14);
        if b > a && b <= fib.ccp_txbx {
            by_spid.insert(lid, (txbx_base + a, (txbx_base + b).min(total)));
        }
    }
    let (acps, spas) = bin::plc(fib.part(fc::PLC_SPA_MOM, &table), 26);
    for (i, s) in spas.iter().enumerate().take(1 << 14) {
        if let (Some(&cp), Some(&r)) = (acps.get(i), by_spid.get(&u32_at(s, 0))) {
            d.boxes.insert(cp, r);
        }
    }
    if fib.ccp_hdr_txbx > 0 {
        d.rep.note(Kind::Dropped, "text boxes in headers and footers");
    }
    // Bookmarks.
    let names = bin::sttb(fib.part(fc::STTBF_BKMK, &table));
    let (starts, fbkf) = bin::plc(fib.part(fc::PLCF_BKF, &table), 4);
    let (ends, _) = bin::plc(fib.part(fc::PLCF_BKL, &table), 0);
    for (i, f) in fbkf.iter().enumerate().take(names.len()).take(1 << 16) {
        let ibkl = u16_at(f, 0) as usize;
        let (Some(&s), Some(&e)) = (starts.get(i), ends.get(ibkl)) else { continue };
        if s > fib.ccp_text || e > fib.ccp_text || e < s {
            continue;
        }
        d.marks.push((s, format!(r#"<w:bookmarkStart w:id="{i}" w:name="{}"/>"#, esc_s(&names[i]))));
        d.marks.push((e, format!(r#"<w:bookmarkEnd w:id="{i}"/>"#)));
    }
    d.marks.sort_by_key(|m| m.0);
    // Sections.
    let (scps, seds) = bin::plc(fib.part(fc::PLCF_SED, &table), 12);
    for (i, s) in seds.iter().enumerate().take(4096) {
        let at = u32_at(s, 2);
        let sep = if at == u32::MAX {
            Sep::default()
        } else {
            let cb = u16_at(&word, at as usize) as usize;
            Sep::read(word.get(at as usize + 2..at as usize + 2 + cb).unwrap_or(&[]))
        };
        d.seps.push(sep);
        d.sed_ends.push(scps.get(i + 1).copied().unwrap_or(fib.ccp_text));
    }
    if d.seps.is_empty() {
        d.seps.push(Sep::default());
    }
    // The first section's header and footer.
    let (hdd, _) = bin::plc(fib.part(fc::PLCF_HDD, &table), 0);
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rels = String::new();
    let story_range = |k: usize| -> Option<(u32, u32)> {
        let (a, b) = (*hdd.get(k)?, *hdd.get(k + 1)?);
        (b > a).then(|| (hdd_base.saturating_add(a).min(total), hdd_base.saturating_add(b).min(total)))
    };
    for (k, tag, rel, file) in [(7usize, "hdr", "header", "header1"), (9, "ftr", "footer", "footer1"), (10, "hdr", "header", "header2"), (11, "ftr", "footer", "footer2")] {
        if k >= 10 && !d.seps[0].title_page {
            continue;
        }
        let Some((a, mut b)) = story_range(k) else { continue };
        // Each story ends with an extra paragraph mark.
        if b > a + 1 && d.ch(b - 1) == 0x0D && d.ch(b - 2) == 0x0D {
            b -= 1;
        }
        let body = d.story(a, b, Story::Header);
        if body.is_empty() {
            continue;
        }
        parts.push((format!("word/{file}.xml"), format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:{tag} {NS}>{body}</w:{tag}>"#).into_bytes()));
        rels.push_str(&format!(r#"<Relationship Id="rId{file}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{rel}" Target="{file}.xml"/>"#));
        let ty = if k >= 10 { "first" } else { "default" };
        d.hdr_refs.push_str(&format!(r#"<w:{rel}Reference w:type="{ty}" r:id="rId{file}"/>"#));
    }
    // Notes.
    for (name, tag, ranges) in [("footnotes", "footnote", &ftns), ("endnotes", "endnote", &edns)] {
        if ranges.is_empty() {
            continue;
        }
        let mut x = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:{name} {NS}>"#);
        for (i, &(a, b)) in ranges.iter().enumerate() {
            let body = d.story(a, b, Story::Note);
            x.push_str(&format!(r#"<w:{tag} w:id="{}">{body}</w:{tag}>"#, i + 1));
        }
        x.push_str(&format!("</w:{name}>"));
        parts.push((format!("word/{name}.xml"), x.into_bytes()));
        rels.push_str(&format!(r#"<Relationship Id="rId{name}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/{name}" Target="{name}.xml"/>"#));
    }
    // Comments, each with an id on its last paragraph for its thread.
    if !atns.is_empty() {
        let mut x = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:comments {NS}>"#);
        let mut ext = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w15:commentsEx {NS}>"#);
        for (i, a) in atns.iter().enumerate() {
            let mut body = d.story(a.text.0, a.text.1, Story::Note);
            if body.is_empty() {
                body.push_str("<w:p></w:p>");
            }
            let pid = format!("{:08X}", i + 1);
            if let Some(at) = body.rfind("<w:p>") {
                body.replace_range(at..at + 5, &format!(r#"<w:p w14:paraId="{pid}">"#));
            }
            let date = a.date.as_ref().map(|d| format!(r#" w:date="{d}""#)).unwrap_or_default();
            x.push_str(&format!(r#"<w:comment w:id="{i}" w:author="{}" w:initials="{}"{date}>{body}</w:comment>"#, esc_s(&a.author), esc_s(&a.initials)));
            let parent = a.parent.map(|p| format!(r#" w15:paraIdParent="{:08X}""#, p + 1)).unwrap_or_default();
            ext.push_str(&format!(r#"<w15:commentEx w15:paraId="{pid}"{parent} w15:done="0"/>"#));
        }
        x.push_str("</w:comments>");
        ext.push_str("</w15:commentsEx>");
        parts.push(("word/comments.xml".into(), x.into_bytes()));
        parts.push(("word/commentsExtended.xml".into(), ext.into_bytes()));
        rels.push_str(r#"<Relationship Id="rIdcomments" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="comments.xml"/>"#);
        rels.push_str(r#"<Relationship Id="rIdcommentsEx" Type="http://schemas.microsoft.com/office/2011/relationships/commentsExtended" Target="commentsExtended.xml"/>"#);
    }
    // The main text, and the last section's properties.
    let body = d.story(0, fib.ccp_text, Story::Main);
    let mut doc = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {NS}><w:body>{body}<w:sectPr>"#);
    d.seps.last().cloned().unwrap_or_default().xml(&mut doc);
    doc.push_str(&d.hdr_refs);
    doc.push_str("</w:sectPr></w:body></w:document>");
    // Styles.
    let mut st = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:docDefaults><w:rPrDefault><w:rPr>"#);
    if let Some(f) = d.fonts.get(default_ftc as usize) {
        let f = esc_s(f);
        st.push_str(&format!(r#"<w:rFonts w:ascii="{f}" w:hAnsi="{f}"/>"#));
    }
    st.push_str(r#"<w:sz w:val="20"/></w:rPr></w:rPrDefault></w:docDefaults>"#);
    for (i, s) in d.styles.iter().enumerate() {
        let Some(s) = s else { continue };
        let ty = match s.stk {
            1 => "paragraph",
            2 => "character",
            _ => continue,
        };
        st.push_str(&format!(r#"<w:style w:type="{ty}" w:styleId="S{i}"><w:name w:val="{}"/>"#, esc_s(&s.name)));
        if let Some(b) = s.base {
            st.push_str(&format!(r#"<w:basedOn w:val="S{b}"/>"#));
        }
        if let Some(n) = s.next.filter(|_| s.stk == 1) {
            st.push_str(&format!(r#"<w:next w:val="S{n}"/>"#));
        }
        if s.stk == 1 {
            let mut pap = Pap::default();
            pap.apply(&s.papx);
            let mut pp = String::new();
            if let Some(ilfo) = pap.ilfo.filter(|&x| x > 0 && x < 0x7FF) {
                pp.push_str(&format!(r#"<w:numPr><w:ilvl w:val="{}"/><w:numId w:val="{ilfo}"/></w:numPr>"#, pap.ilvl.unwrap_or(0).min(8)));
            }
            pap.xml(&mut pp);
            st.push_str(&format!("<w:pPr>{pp}</w:pPr>"));
        }
        let mut chp = Chp::default();
        chp.apply(&s.chpx, &Chp::default());
        chp.istd = None;
        chp.xml(&d.fonts, &mut st);
        st.push_str("</w:style>");
    }
    st.push_str("</w:styles>");
    // Lists.
    let (lsts, lfos) = lists(&table, &fib);
    rep.merge(std::mem::take(&mut d.rep));
    for (i, (name, _)) in d.media.iter().enumerate() {
        rels.push_str(&format!(r#"<Relationship Id="rIdImg{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="{}"/>"#, i + 1, name.trim_start_matches("word/")));
    }
    rels.push_str(r#"<Relationship Id="rIdStyles" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#);
    if !lsts.is_empty() {
        parts.push(("word/numbering.xml".into(), numbering_xml(&lsts, &lfos).into_bytes()));
        rels.push_str(r#"<Relationship Id="rIdNumbering" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering" Target="numbering.xml"/>"#);
    }
    // Settings: whether changes are tracked (the DOP's fRevMarking).
    if fib.part(fc::DOP, &table).get(5).is_some_and(|b| b & 0x80 != 0) {
        parts.push(("word/settings.xml".into(), format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:settings {NS}><w:trackRevisions/></w:settings>"#).into_bytes()));
        rels.push_str(r#"<Relationship Id="rIdSettings" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings" Target="settings.xml"/>"#);
    }
    parts.extend(std::mem::take(&mut d.media));
    parts.push(("word/document.xml".into(), doc.into_bytes()));
    parts.push(("word/styles.xml".into(), st.into_bytes()));
    parts.push(("word/_rels/document.xml.rels".into(), format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels}</Relationships>"#).into_bytes()));
    parts.push(("_rels/.rels".into(), br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_vec()));
    Ok(parts)
}

/// The WordprocessingML package a `.doc` reads as (for tests and tools).
pub fn to_docx(data: &[u8], password: Option<&str>) -> Result<Vec<u8>, DocError> {
    let mut rep = Report::default();
    let parts = translate(data, password, &mut rep)?;
    let mut z = crate::zip::ZipWriter::new(Vec::new());
    for (n, b) in &parts {
        z.add_stored(n, b).map_err(|e| bad(e.to_string()))?;
    }
    z.finish().map_err(|e| bad(e.to_string()))
}

/// Reads a `.doc` document, decrypting it with `password` when it is
/// protected.
pub fn read(data: &[u8], password: Option<&str>) -> Result<(TextDoc, Loaded), DocError> {
    let mut rep = Report::default();
    let parts = translate(data, password, &mut rep)?;
    let mut z = crate::zip::ZipWriter::new(Vec::new());
    for (n, b) in &parts {
        z.add_stored(n, b).map_err(|e| bad(e.to_string()))?;
    }
    drop(parts);
    let pkg = z.finish().map_err(|e| bad(e.to_string()))?;
    let (doc, mut loaded) = crate::docx::read(&pkg).map_err(|e| bad(e.to_string()))?;
    rep.merge(std::mem::take(&mut loaded.report));
    loaded.report = rep;
    Ok((doc, loaded))
}
