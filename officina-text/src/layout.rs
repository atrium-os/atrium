//! A paragraph laid out at a width: shaped glyphs, lines, line metrics.
//!
//! Shaping is per run of one format (so kerning and ligatures work inside a run),
//! through [`Fonts`]; [`PinnedFonts`] shapes with rustybuzz over the Navigator's
//! pinned font set, falling back face by face for characters a face lacks. Lines
//! break at UAX #14 opportunities, greedily (total-fit justification comes later);
//! spaces at a line's end hang. A word wider than the line breaks between
//! characters.
//!
//! [`LayoutCache`] keeps each paragraph's layout under a hash of what it depends
//! on (text, resolved formats, paragraph format, width), so typing relayouts one
//! paragraph and scrolling relayouts none.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::model::{Align, BlockId, CharFormat, LineSpacing, ObjectRef, Para, ParaFormat, TextDoc, OBJECT_CHAR};
use crate::{twips, LU};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph {
    pub face: u16,
    pub id: u16,
    /// Advance, layout units.
    pub adv: i64,
    pub dx: i64,
    pub dy: i64,
    /// The byte (in the paragraph) of the cluster this glyph draws.
    pub cluster: u32,
    /// Index into [`ParaLayout::formats`].
    pub fmt: u16,
}

/// The face of a glyph that stands for an inline picture (its `id` is 0; the
/// picture is the format's `object`).
pub const OBJECT_FACE: u16 = u16::MAX;

/// A picture's laid-out size in layout units: its frame, scaled down to fit a
/// column `full` wide.
pub fn object_size(o: &ObjectRef, full: i64) -> (i64, i64) {
    let (w, h) = (twips(o.width.max(1) as i64), twips(o.height.max(1) as i64));
    if w > full && full > 0 {
        (full, (h * full / w).max(1))
    } else {
        (w, h)
    }
}

/// A laid-out picture's height from its glyph's advance.
pub fn object_height(o: &ObjectRef, adv: i64) -> i64 {
    (twips(o.height.max(1) as i64) * adv / twips(o.width.max(1) as i64)).max(1)
}

/// Glyphs for `text` in format `f`, whose `object` is `o`: each U+FFFC is the
/// picture, as one glyph as wide as its frame; anything else is shaped.
fn shape_with_objects(fonts: &dyn Fonts, text: &str, f: &CharFormat, o: &ObjectRef, full: i64) -> Vec<Glyph> {
    let mut out = Vec::new();
    let mut from = 0;
    let (w, _) = object_size(o, full);
    for (i, ch) in text.char_indices().chain(std::iter::once((text.len(), '\0'))) {
        if ch != OBJECT_CHAR && i < text.len() {
            continue;
        }
        if i > from {
            out.extend(fonts.shape(&text[from..i], f).into_iter().map(|mut g| {
                g.cluster += from as u32;
                g
            }));
        }
        if i < text.len() {
            out.push(Glyph { face: OBJECT_FACE, id: 0, adv: w, dx: 0, dy: 0, cluster: i as u32, fmt: 0 });
        }
        from = i + ch.len_utf8();
    }
    out
}

/// What shaping and metrics come from.
pub trait Fonts {
    /// Glyphs for `text` in format `f`; clusters relative to `text`, `fmt` 0.
    fn shape(&self, text: &str, f: &CharFormat) -> Vec<Glyph>;
    /// (ascent, descent, natural line height) in layout units, all positive.
    fn metrics(&self, f: &CharFormat) -> (i64, i64, i64);
}

/// The font size of `f` in layout units.
pub fn size_lu(f: &CharFormat) -> i64 {
    f.size as i64 * LU / 2
}

/// Shaping with rustybuzz over the pinned font set (navigator-render's).
pub struct PinnedFonts {
    set: &'static navigator_render::fontset::FontSet,
    hb: Vec<rustybuzz::Face<'static>>,
    tt: Vec<ttf_parser::Face<'static>>,
}

impl PinnedFonts {
    pub fn new() -> Result<PinnedFonts, String> {
        let set = navigator_render::fontset::FontSet::shared()?;
        let hb = set.faces.iter().map(|f| rustybuzz::Face::from_slice(&f.bytes, 0).ok_or("a pinned face doesn't parse")).collect::<Result<_, _>>()?;
        let tt = set.faces.iter().map(|f| ttf_parser::Face::parse(&f.bytes, 0).map_err(|_| "a pinned face doesn't parse")).collect::<Result<_, _>>()?;
        Ok(PinnedFonts { set, hb, tt })
    }

    fn stack(&self, f: &CharFormat) -> Vec<usize> {
        use navigator_render::fontset::Family;
        let fam = f.family.to_ascii_lowercase();
        let mono = ["mono", "courier", "consolas", "menlo", "code"].iter().any(|m| fam.contains(m));
        self.set.stack_of(if mono { Family::Mono } else { Family::Sans }, f.bold, f.italic)
    }

    pub fn face_bytes(&self, face: u16) -> &[u8] {
        &self.set.faces[face as usize].bytes
    }
    pub fn face_address(&self, face: u16) -> &str {
        &self.set.faces[face as usize].address
    }
}

impl Fonts for PinnedFonts {
    fn shape(&self, text: &str, f: &CharFormat) -> Vec<Glyph> {
        let stack = self.stack(f);
        let size = size_lu(f);
        // Split by the first face that has each character.
        let mut segs: Vec<(usize, usize, usize)> = Vec::new(); // face, start, end
        for (i, ch) in text.char_indices() {
            let face = if ch.is_whitespace() || ch.is_control() { stack[0] } else { stack.iter().copied().find(|&k| self.tt[k].glyph_index(ch).is_some()).unwrap_or(stack[0]) };
            match segs.last_mut() {
                Some((fc, _, e)) if *fc == face => *e = i + ch.len_utf8(),
                _ => segs.push((face, i, i + ch.len_utf8())),
            }
        }
        let mut out = Vec::new();
        for (face, s, e) in segs {
            let mut buf = rustybuzz::UnicodeBuffer::new();
            buf.push_str(&text[s..e]);
            buf.guess_segment_properties();
            let shaped = rustybuzz::shape(&self.hb[face], &[], buf);
            let upem = self.set.faces[face].upem.max(1);
            let sc = |v: i32| (v as i64 * size + upem / 2).div_euclid(upem);
            for (gi, gp) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                out.push(Glyph { face: face as u16, id: gi.glyph_id as u16, adv: sc(gp.x_advance), dx: sc(gp.x_offset), dy: -sc(gp.y_offset), cluster: (s as u32) + gi.cluster, fmt: 0 });
            }
        }
        out
    }

    fn metrics(&self, f: &CharFormat) -> (i64, i64, i64) {
        let face = &self.set.faces[self.stack(f)[0]];
        let size = size_lu(f);
        let upem = face.upem.max(1);
        let asc = face.ascent * size / upem;
        let desc = -face.descent * size / upem;
        let gap = self.tt[self.stack(f)[0]].line_gap() as i64 * size / upem;
        (asc, desc, asc + desc + gap)
    }
}

/// One laid-out line.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    /// Bytes of the paragraph it shows.
    pub start: u32,
    pub end: u32,
    /// Its glyphs in [`ParaLayout::glyphs`].
    pub glyphs: std::ops::Range<usize>,
    /// Where its first glyph sits, from the paragraph's start edge (indents and
    /// alignment included).
    pub x: i64,
    /// Width of its content, trailing spaces left out.
    pub width: i64,
    /// Extra advance after each space (justified lines).
    pub space_extra: i64,
    pub ascent: i64,
    pub descent: i64,
    /// Distance from this line's top to the next's.
    pub height: i64,
}

/// A paragraph laid out.
#[derive(Debug, Clone, PartialEq)]
pub struct ParaLayout {
    pub format: ParaFormat,
    pub formats: Vec<CharFormat>,
    pub glyphs: Vec<Glyph>,
    pub lines: Vec<Line>,
    pub space_before: i64,
    pub space_after: i64,
    /// The lines' heights summed (no spacing before or after).
    pub height: i64,
}

impl ParaLayout {
    /// The line holding byte `at` (the end goes to the last line).
    pub fn line_of(&self, at: u32) -> usize {
        self.lines.partition_point(|l| l.end <= at && l.end > l.start).min(self.lines.len().saturating_sub(1))
    }
}

/// Where a line can break: (byte, mandatory).
fn opportunities(text: &str) -> Vec<(u32, bool)> {
    unicode_linebreak::linebreaks(text).map(|(i, o)| (i as u32, o == unicode_linebreak::BreakOpportunity::Mandatory)).collect()
}

/// Lays out `p` (of `doc`) in a column `width` layout units wide.
pub fn layout_para(doc: &TextDoc, p: &Para, width: i64, fonts: &dyn Fonts) -> ParaLayout {
    let (pf, cf) = doc.styles.para_format(p.style, &p.props);
    let full = width - twips(pf.indent_start as i64) - twips(pf.indent_end as i64);
    let mut formats: Vec<CharFormat> = Vec::new();
    let mut glyphs: Vec<Glyph> = Vec::new();
    let mut run_fmt: Vec<(u32, u16)> = Vec::new();
    for (s, e, a) in p.spans() {
        let f = doc.styles.char_format(&cf, doc.attrs.get(a));
        let fi = formats.iter().position(|x| *x == f).unwrap_or_else(|| {
            formats.push(f.clone());
            formats.len() - 1
        }) as u16;
        run_fmt.push((s, fi));
        if s == e {
            continue;
        }
        let text = &p.text[s as usize..e as usize];
        // Tabs: four spaces' width for now (tab stops come with the ruler).
        let mut g = match &f.object {
            Some(o) => shape_with_objects(fonts, text, &f, o, full.max(LU)),
            None => fonts.shape(text, &f),
        };
        let space = if text.contains('\t') { fonts.shape(" ", &f).first().map_or(0, |g| g.adv) } else { 0 };
        for x in g.iter_mut() {
            x.cluster += s;
            x.fmt = fi;
            if p.text.as_bytes().get(x.cluster as usize) == Some(&b'\t') {
                x.adv = space * 4;
            }
        }
        glyphs.extend(g);
    }
    if formats.is_empty() {
        formats.push(cf.clone());
    }
    // Width up to each byte: glyphs are in logical order (no bidi yet).
    let n = p.text.len();
    let mut cum = vec![0i64; n + 1];
    {
        let mut gi = 0;
        let mut acc = 0i64;
        for (b, slot) in cum.iter_mut().enumerate() {
            while gi < glyphs.len() && (glyphs[gi].cluster as usize) < b {
                acc += glyphs[gi].adv;
                gi += 1;
            }
            *slot = acc;
        }
    }
    let is_space = |b: u32| matches!(p.text.as_bytes().get(b as usize), Some(b' ' | b'\t'));
    // Width of start..end with trailing spaces hanging.
    let seg = |start: u32, end: u32| {
        let mut e = end;
        while e > start && is_space(e - 1) {
            e -= 1;
        }
        (cum[e as usize] - cum[start as usize], e)
    };
    // A list item's hang holds its label; its text starts at the indent.
    let first_line = if pf.list.is_some() { pf.first_line.max(0) } else { pf.first_line };
    let avail = |first: bool| (full - if first { twips(first_line as i64) } else { 0 }).max(LU);
    let ops = opportunities(&p.text);
    let mut breaks: Vec<(u32, bool)> = Vec::new(); // (end byte, mandatory)
    let mut start = 0u32;
    let mut last_ok: Option<(u32, bool)> = None;
    let mut k = 0;
    while k < ops.len() {
        let (b, mand) = ops[k];
        if b <= start {
            k += 1;
            continue;
        }
        let first = breaks.is_empty();
        let (w, _) = seg(start, b);
        if w <= avail(first) {
            if mand || b as usize == n {
                breaks.push((b, mand && (b as usize) < n));
                start = b;
                last_ok = None;
            } else {
                last_ok = Some((b, false));
            }
            k += 1;
            continue;
        }
        // Too wide: break at the last opportunity, or inside the word.
        match last_ok.take() {
            Some((lb, _)) => {
                breaks.push((lb, false));
                start = lb;
            }
            None => {
                // Between characters: as many as fit (at least one).
                let mut e = start;
                for (i, ch) in p.text[start as usize..b as usize].char_indices() {
                    let next = start + i as u32 + ch.len_utf8() as u32;
                    if e > start && cum[next as usize] - cum[start as usize] > avail(first) {
                        break;
                    }
                    e = next;
                }
                breaks.push((e, false));
                start = e;
            }
        }
    }
    if breaks.is_empty() || (breaks.last().unwrap().0 as usize) < n {
        breaks.push((n as u32, false));
    }
    // Lines.
    let fmt_at = |b: u32| run_fmt[run_fmt.partition_point(|r| r.0 <= b).saturating_sub(1)].1;
    let mut lines = Vec::with_capacity(breaks.len());
    let mut s = 0u32;
    let mut gi = 0usize;
    let count = breaks.len();
    for (li, &(e, mand)) in breaks.iter().enumerate() {
        let g0 = gi;
        while gi < glyphs.len() && glyphs[gi].cluster < e {
            gi += 1;
        }
        let (w, content_end) = seg(s, e);
        let mut used: Vec<u16> = glyphs[g0..gi].iter().map(|g| g.fmt).collect();
        if used.is_empty() {
            used.push(fmt_at(s));
        }
        used.sort_unstable();
        used.dedup();
        let (mut asc, mut desc, mut nat) = (0, 0, 0);
        for &fi in &used {
            let (a, d, h) = fonts.metrics(&formats[fi as usize]);
            asc = asc.max(a);
            desc = desc.max(d);
            nat = nat.max(h);
        }
        // Pictures sit on the baseline and make the line as tall as they are.
        for g in glyphs[g0..gi].iter().filter(|g| g.face == OBJECT_FACE) {
            if let Some(o) = &formats[g.fmt as usize].object {
                let h = object_height(o, g.adv);
                if h > asc {
                    nat += h - asc;
                    asc = h;
                }
            }
        }
        let height = match pf.line_spacing {
            LineSpacing::Multiple(m) => nat * m.clamp(25, 1000) as i64 / 100,
            LineSpacing::Exact(t) => twips(t.clamp(20, 31680) as i64),
            LineSpacing::AtLeast(t) => nat.max(twips(t.clamp(0, 31680) as i64)),
        };
        let first = li == 0;
        let room = avail(first);
        let base = twips(pf.indent_start as i64) + if first { twips(first_line as i64) } else { 0 };
        let last = li + 1 == count || mand;
        let (x, space_extra) = match pf.align {
            Align::Start => (base, 0),
            Align::Center => (base + (room - w).max(0) / 2, 0),
            Align::End => (base + (room - w).max(0), 0),
            Align::Justify if !last => {
                let spaces = p.text[s as usize..content_end as usize].bytes().filter(|&c| c == b' ').count() as i64;
                (base, if spaces > 0 { (room - w).max(0) / spaces } else { 0 })
            }
            Align::Justify => (base, 0),
        };
        lines.push(Line { start: s, end: e, glyphs: g0..gi, x, width: w, space_extra, ascent: asc, descent: desc, height });
        s = e;
    }
    let height = lines.iter().map(|l| l.height).sum();
    ParaLayout { space_before: twips(pf.space_before.max(0) as i64), space_after: twips(pf.space_after.max(0) as i64), format: pf, formats, glyphs, lines, height }
}

/// Paragraph layouts by block, kept while their inputs hold.
#[derive(Default)]
pub struct LayoutCache {
    map: HashMap<BlockId, (u64, Rc<ParaLayout>)>,
    pub hits: u64,
    pub misses: u64,
}

fn key(doc: &TextDoc, p: &Para, width: i64) -> u64 {
    let mut h = DefaultHasher::new();
    p.text.hash(&mut h);
    p.style.hash(&mut h);
    p.props.hash(&mut h);
    width.hash(&mut h);
    for &(s, a) in &p.runs {
        s.hash(&mut h);
        doc.attrs.get(a).hash(&mut h);
    }
    // Styles change rarely; their identity is part of the key through the
    // resolved paragraph format.
    doc.styles.para_format(p.style, &p.props).hash(&mut h);
    h.finish()
}

impl LayoutCache {
    pub fn get(&mut self, doc: &TextDoc, p: &Para, width: i64, fonts: &dyn Fonts) -> Rc<ParaLayout> {
        let k = key(doc, p, width);
        if let Some((kk, l)) = self.map.get(&p.id) {
            if *kk == k {
                self.hits += 1;
                return l.clone();
            }
        }
        self.misses += 1;
        let l = Rc::new(layout_para(doc, p, width, fonts));
        self.map.insert(p.id, (k, l.clone()));
        l
    }

    /// Drops layouts of blocks no longer in `doc` (call now and then).
    pub fn prune(&mut self, doc: &TextDoc) {
        if self.map.len() <= doc.blocks.len() * 2 {
            return;
        }
        let ids: std::collections::HashSet<BlockId> = doc.blocks.iter().map(|b| b.id()).collect();
        self.map.retain(|k, _| ids.contains(k));
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Fixed metrics for tests: every character `size/2` wide, ascent 0.8, descent 0.2.
pub struct FixedFonts;

impl Fonts for FixedFonts {
    fn shape(&self, text: &str, f: &CharFormat) -> Vec<Glyph> {
        let w = size_lu(f) / 2;
        text.char_indices().map(|(i, c)| Glyph { face: 0, id: c as u16, adv: if c == '\u{200b}' { 0 } else { w }, dx: 0, dy: 0, cluster: i as u32, fmt: 0 }).collect()
    }
    fn metrics(&self, f: &CharFormat) -> (i64, i64, i64) {
        let s = size_lu(f);
        (s * 8 / 10, s * 2 / 10, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CharAttrs, ParaProps};

    fn doc(text: &str) -> TextDoc {
        let mut d = TextDoc::new();
        d.blocks.update(0, |b| b.as_para_mut().unwrap().text = text.to_string());
        d
    }

    #[test]
    fn a_picture_is_one_glyph_on_the_baseline_and_fits_the_column() {
        use crate::model::ObjectRef;
        let mut d = doc("ab\u{FFFC}cd");
        let o = ObjectRef { image: 0, width: 2880, height: 1440 };
        let pic = d.attrs.intern(CharAttrs { object: Some(o), ..Default::default() });
        d.blocks.update(0, |b| b.as_para_mut().unwrap().runs = vec![(0, 0), (2, pic), (5, 0)]);
        // A column 1 inch wide: the 2 x 1 inch frame scales to 1 x 0.5.
        let w = twips(1440);
        let l = layout_para(&d, d.para(0).unwrap(), w, &FixedFonts);
        let g: Vec<&Glyph> = l.glyphs.iter().filter(|g| g.face == OBJECT_FACE).collect();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].adv, w);
        assert_eq!(object_height(&o, g[0].adv), twips(720));
        let line = &l.lines[l.line_of(2)];
        assert_eq!(line.ascent, twips(720), "the picture stands on the baseline");
        assert!(line.height >= twips(720) + line.descent);
        // Only U+FFFC is a picture, even in a run that names one.
        d.blocks.update(0, |b| b.as_para_mut().unwrap().runs = vec![(0, pic)]);
        let l = layout_para(&d, d.para(0).unwrap(), w, &FixedFonts);
        assert_eq!(l.glyphs.iter().filter(|g| g.face == OBJECT_FACE).count(), 1);
    }

    #[test]
    fn greedy_lines_with_hanging_spaces() {
        // 11 pt: each char 5.5 pt = 352 LU. Width 10 chars.
        let d = doc("aaa bbb ccc dddddddddddddd e");
        let w = 10 * 352;
        let l = layout_para(&d, d.para(0).unwrap(), w, &FixedFonts);
        let texts: Vec<&str> = l.lines.iter().map(|x| &d.para(0).unwrap().text[x.start as usize..x.end as usize]).collect();
        assert_eq!(texts, ["aaa bbb ", "ccc ", "dddddddddd", "dddd e"]);
        assert_eq!(l.lines[0].width, 7 * 352, "the trailing space hangs");
        assert!(l.lines.iter().all(|x| x.height == 11 * LU * 115 / 100));
    }

    #[test]
    fn alignment_indents_and_mandatory_breaks() {
        let mut d = doc("ab cd\u{2028}ef");
        d.blocks.update(0, |b| b.as_para_mut().unwrap().props = ParaProps { align: Some(Align::Justify), indent_start: Some(200), first_line: Some(100), ..Default::default() });
        let w = 10 * 352 + twips(200);
        let l = layout_para(&d, d.para(0).unwrap(), w, &FixedFonts);
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[0].x, twips(300));
        assert_eq!(l.lines[1].x, twips(200));
        assert_eq!(l.lines[0].space_extra, 0, "a line ended by a hard break isn't stretched");
        let mut d = doc("ab cd ef gh ij");
        d.blocks.update(0, |b| b.as_para_mut().unwrap().props = ParaProps { align: Some(Align::Justify), ..Default::default() });
        let l = layout_para(&d, d.para(0).unwrap(), 9 * 352, &FixedFonts);
        assert_eq!(l.lines[0].width, 8 * 352);
        assert_eq!(l.lines[0].space_extra, 352 / 2, "one char of slack over two spaces");
        assert_eq!(l.lines.last().unwrap().space_extra, 0);
    }

    #[test]
    fn empty_paragraph_and_bigger_runs() {
        let d = doc("");
        let l = layout_para(&d, d.para(0).unwrap(), 1000, &FixedFonts);
        assert_eq!(l.lines.len(), 1);
        assert!(l.height > 0);
        let mut d = doc("small BIG");
        let big = d.attrs.intern(CharAttrs { size: Some(44), ..Default::default() });
        d.blocks.update(0, |b| b.as_para_mut().unwrap().runs = vec![(0, 0), (6, big)]);
        let l = layout_para(&d, d.para(0).unwrap(), 100_000, &FixedFonts);
        assert_eq!(l.lines[0].ascent, 22 * LU * 8 / 10, "the line is as tall as its biggest run");
        assert_eq!(l.formats.len(), 2);
    }

    #[test]
    fn pinned_fonts_shape_and_fall_back() {
        let f = PinnedFonts::new().unwrap();
        let d = doc("Office ★ fi");
        let l = layout_para(&d, d.para(0).unwrap(), 100_000, &f);
        assert_eq!(l.lines.len(), 1);
        let faces: std::collections::BTreeSet<u16> = l.glyphs.iter().map(|g| g.face).collect();
        assert!(faces.len() >= 2, "★ comes from the fallback face");
        assert!(l.lines[0].width > 0);
    }

    #[test]
    fn cache_hits_until_the_paragraph_changes() {
        let mut d = doc("one two three");
        let mut c = LayoutCache::default();
        let a = c.get(&d, d.para(0).unwrap(), 5000, &FixedFonts);
        let b = c.get(&d, d.para(0).unwrap(), 5000, &FixedFonts);
        assert!(Rc::ptr_eq(&a, &b));
        d.blocks.update(0, |b| b.as_para_mut().unwrap().text.push('!'));
        let e = c.get(&d, d.para(0).unwrap(), 5000, &FixedFonts);
        assert!(!Rc::ptr_eq(&a, &e));
        assert_eq!((c.hits, c.misses), (1, 2));
    }
}
