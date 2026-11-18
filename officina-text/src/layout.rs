//! A paragraph laid out at a width: shaped glyphs, lines, line metrics.
//!
//! Shaping is per run of one format (so kerning and ligatures work inside a run),
//! through [`Fonts`]; [`PinnedFonts`] shapes with rustybuzz over the pinned set and
//! the system font library, falling back face by face for characters a face
//! lacks. Lines
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

use crate::model::{Align, BlockId, CharFormat, Leader, LineSpacing, TabAlign, DEFAULT_TAB, ObjectRef, Para, ParaFormat, TextDoc, VertAlign, OBJECT_CHAR};
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
            // A floating picture takes no room in the line; the page places it.
            let adv = if o.float.is_some() { 0 } else { w };
            out.push(Glyph { face: OBJECT_FACE, id: 0, adv, dx: 0, dy: 0, cluster: i as u32, fmt: 0 });
        }
        from = i + ch.len_utf8();
    }
    out
}

/// Glyphs for a run in a note anchor's format: each U+FFFC shows the note's
/// label (all its glyphs on the anchor's byte); anything else is shaped.
fn shape_with_label(fonts: &dyn Fonts, text: &str, f: &CharFormat, label: &str) -> Vec<Glyph> {
    let mut out = Vec::new();
    let mut from = 0;
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
            out.extend(fonts.shape(label, f).into_iter().map(|mut g| {
                g.cluster = i as u32;
                g
            }));
        }
        from = i + ch.len_utf8();
    }
    out
}

/// Superscript and subscript (and note anchors, raised): the size they are
/// drawn at, and how far their baseline moves down (negative is up).
pub fn vert_shift(f: &mut CharFormat) -> i64 {
    let v = if f.note.is_some() { VertAlign::Super } else { f.vert };
    if v == VertAlign::Baseline {
        return 0;
    }
    let full = size_lu(f);
    f.size = ((f.size as u32 * 2 / 3) as u16).max(2);
    f.vert = v;
    match v {
        VertAlign::Super => -full / 3,
        _ => full / 7,
    }
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

/// Shaping with rustybuzz over the system's fonts: the pinned set
/// (navigator-render's IBM Plex and DejaVu, faces `0..base`) and the system
/// font library (`atrium-fonts`, face `base + id`), mapped as they're first
/// used.
///
/// A run's font name picks its family: one we ship, a metric-compatible
/// stand-in (Calibri is drawn with Carlito, Times New Roman with Liberation
/// Serif), or the nearest class; IBM Plex otherwise. A character the family
/// lacks goes to its script's Noto face, then the pinned fallbacks, then the
/// broad Noto faces.
pub struct PinnedFonts {
    set: &'static navigator_fontset::FontSet,
    lib: &'static atrium_fonts::Library,
    faces: Vec<std::sync::OnceLock<Option<Loaded>>>,
}

struct Loaded {
    hb: rustybuzz::Face<'static>,
    tt: ttf_parser::Face<'static>,
    upem: i64,
    ascent: i64,
    descent: i64,
}

impl Loaded {
    fn new(bytes: &'static [u8]) -> Option<Loaded> {
        let tt = ttf_parser::Face::parse(bytes, 0).ok()?;
        let hb = rustybuzz::Face::from_slice(bytes, 0)?;
        let (upem, ascent, descent) = (tt.units_per_em() as i64, tt.ascender() as i64, tt.descender() as i64);
        Some(Loaded { hb, tt, upem, ascent, descent })
    }
}

impl PinnedFonts {
    pub fn new() -> Result<PinnedFonts, String> {
        let set = navigator_fontset::FontSet::shared()?;
        let fonts = PinnedFonts { set, lib: atrium_fonts::Library::system(), faces: (0..set.faces.len() + atrium_fonts::faces().len()).map(|_| Default::default()).collect() };
        for i in 0..set.faces.len() {
            fonts.get(i).ok_or("a pinned face doesn't parse")?;
        }
        Ok(fonts)
    }

    /// The first library face: faces below it are the pinned set's.
    pub fn base(&self) -> usize {
        self.set.faces.len()
    }

    fn get(&self, k: usize) -> Option<&Loaded> {
        self.faces.get(k)?
            .get_or_init(|| {
                let bytes: &'static [u8] = match k.checked_sub(self.base()) {
                    None => &self.set.faces[k].bytes,
                    Some(id) => self.lib.bytes(id).ok()?,
                };
                Loaded::new(bytes)
            })
            .as_ref()
    }

    fn lib_face(&self, family: &str, bold: bool, italic: bool) -> Option<usize> {
        let id = atrium_fonts::Library::face(family, bold, italic)?;
        let k = self.base() + id;
        self.get(k).map(|_| k)
    }

    /// `family`'s face for `ch`, its colour twin first (emoji: the screen
    /// and the PDF writer both draw colour glyphs), when it has `ch`.
    fn lib_face_for(&self, family: &str, f: &CharFormat, ch: char) -> Option<usize> {
        atrium_fonts::Library::color_twin(family)
            .into_iter()
            .chain([family])
            .find_map(|fam| self.lib_face(fam, f.bold, f.italic).filter(|&k| self.has(k, ch)))
    }

    /// The faces for a format, in order: the family's face (or the pinned
    /// stack's), then the pinned fallbacks.
    fn stack(&self, f: &CharFormat) -> Vec<usize> {
        use navigator_fontset::Family;
        let class = atrium_fonts::Library::class(&f.family);
        let pinned = self.set.stack_of(if class == Some(atrium_fonts::Class::Mono) { Family::Mono } else { Family::Sans }, f.bold, f.italic);
        match atrium_fonts::Library::resolve(&f.family).and_then(|r| self.lib_face(r.family, f.bold, f.italic)) {
            Some(k) => std::iter::once(k).chain(pinned.into_iter().filter(|&i| self.set.faces[i].name.starts_with("DejaVu"))).collect(),
            None => pinned,
        }
    }

    fn has(&self, k: usize, ch: char) -> bool {
        self.get(k).is_some_and(|l| l.tt.glyph_index(ch).is_some())
    }

    /// The face that draws `ch` in `stack`.
    fn face_for(&self, stack: &[usize], ch: char, f: &CharFormat, han: Option<atrium_fonts::Han>) -> usize {
        if ch.is_whitespace() || ch.is_control() || self.has(stack[0], ch) {
            return stack[0];
        }
        let lib = |fam: &str| self.lib_face_for(fam, f, ch);
        if let Some(k) = atrium_fonts::Library::script_family(ch).and_then(lib) {
            return k;
        }
        if let Some(&k) = stack[1..].iter().find(|&&k| self.has(k, ch)) {
            return k;
        }
        atrium_fonts::Library::fallback_in(ch, han).find_map(lib).unwrap_or(stack[0])
    }

    pub fn face_bytes(&self, face: u16) -> &[u8] {
        let k = face as usize;
        match k.checked_sub(self.base()) {
            None => &self.set.faces[k].bytes,
            Some(id) => self.lib.bytes(id).unwrap_or(&[]),
        }
    }
    pub fn face_address(&self, face: u16) -> &str {
        let k = face as usize;
        match k.checked_sub(self.base()) {
            None => &self.set.faces[k].address,
            Some(id) => atrium_fonts::faces().get(id).map_or("", |f| f.address),
        }
    }
    /// The family a face belongs to.
    pub fn face_family(&self, face: u16) -> &str {
        let k = face as usize;
        match k.checked_sub(self.base()) {
            None => self.set.faces[k].name,
            Some(id) => atrium_fonts::faces().get(id).map_or("", |f| f.family),
        }
    }
}

impl Fonts for PinnedFonts {
    fn shape(&self, text: &str, f: &CharFormat) -> Vec<Glyph> {
        let stack = self.stack(f);
        let size = size_lu(f);
        // Split by the first face that has each character.
        let mut segs: Vec<(usize, usize, usize)> = Vec::new(); // face, start, end
        // Which CJK forms a Han character takes (kana or Hangul in the run,
        // else the locale).
        // The run's kana or Hangul, else the document's East Asian
        // language, else the locale.
        let han = atrium_fonts::Han::of_text(text).or_else(|| f.lang_ea.as_deref().and_then(atrium_fonts::Han::of_locale)).or_else(atrium_fonts::Han::of_system);
        for (i, ch) in text.char_indices() {
            // Joiners, variation selectors, skin tones and tag characters
            // stay with what they follow, so an emoji sequence is shaped
            // whole by one face.
            let joins = matches!(ch as u32, 0x200C | 0x200D | 0xFE00..=0xFE0F | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F | 0x20E3);
            // VS16 asks for the emoji form (❤️, ☺️, keycaps).
            let emoji_vs = text[i + ch.len_utf8()..].starts_with('\u{FE0F}');
            let face = match segs.last() {
                // A variation selector always: a face may leave it out of
                // its cmap (Noto Color Emoji does), and shaping drops it.
                Some(&(fc, _, _)) if joins && (self.has(fc, ch) || matches!(ch as u32, 0xFE00..=0xFE0F)) => fc,
                _ => match emoji_vs.then(|| self.lib_face_for("Noto Emoji", f, ch)).flatten() {
                    Some(k) => k,
                    None => self.face_for(&stack, ch, f, han),
                },
            };
            match segs.last_mut() {
                Some((fc, _, e)) if *fc == face => *e = i + ch.len_utf8(),
                _ => segs.push((face, i, i + ch.len_utf8())),
            }
        }
        let mut out = Vec::new();
        for (face, s, e) in segs {
            let Some(l) = self.get(face) else { continue };
            let mut buf = rustybuzz::UnicodeBuffer::new();
            buf.push_str(&text[s..e]);
            buf.guess_segment_properties();
            let rtl = buf.direction() == rustybuzz::Direction::RightToLeft;
            let shaped = rustybuzz::shape(&l.hb, &[], buf);
            let upem = l.upem.max(1);
            let sc = |v: i32| (v as i64 * size + upem / 2).div_euclid(upem);
            let at = out.len();
            for (gi, gp) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                out.push(Glyph { face: face as u16, id: gi.glyph_id as u16, adv: sc(gp.x_advance), dx: sc(gp.x_offset), dy: -sc(gp.y_offset), cluster: (s as u32) + gi.cluster, fmt: 0 });
            }
            // Right-to-left output comes in visual order; layout keeps
            // glyphs logical (`crate::bidi` puts lines back in visual order).
            if rtl {
                out[at..].reverse();
            }
        }
        out
    }

    fn metrics(&self, f: &CharFormat) -> (i64, i64, i64) {
        let Some(face) = self.get(self.stack(f)[0]) else { return (0, 0, 0) };
        let size = size_lu(f);
        let upem = face.upem.max(1);
        let asc = face.ascent * size / upem;
        let desc = -face.descent * size / upem;
        let gap = face.tt.line_gap() as i64 * size / upem;
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
    /// Distance from this line's top to the next's (`skip` included).
    pub height: i64,
    /// Room left above the line for floating pictures it goes below.
    pub skip: i64,
    /// Space tabs took that a leader fills: from, to (from the paragraph's
    /// start edge, like `x`), and the leader.
    pub leaders: Vec<(i64, i64, Leader)>,
    /// Its glyphs (indices into [`ParaLayout::glyphs`]) in the order they
    /// are drawn from the left, when some are right to left; empty when that
    /// is their logical order.
    pub visual: Vec<u32>,
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
    /// Embedding level of each byte (`crate::bidi`), empty when the
    /// paragraph is all left to right.
    pub levels: Vec<u8>,
    /// Its base direction is right to left: its start edge is the right.
    pub rtl: bool,
}

impl ParaLayout {
    /// Line `li`'s glyph indices in drawing order, left to right.
    pub fn visual_order(&self, li: usize) -> Vec<usize> {
        let line = &self.lines[li];
        if line.visual.is_empty() { line.glyphs.clone().collect() } else { line.visual.iter().map(|&i| i as usize).collect() }
    }

    /// The line's clusters left to right: (byte, left x, right x, right to
    /// left), x from the column's left like `Line::x`.
    fn visual_clusters(&self, li: usize, text: &str) -> Vec<(u32, i64, i64, bool)> {
        let line = &self.lines[li];
        let bytes = text.as_bytes();
        let mut out: Vec<(u32, i64, i64, bool)> = Vec::new();
        let mut x = line.x;
        for k in self.visual_order(li) {
            let g = self.glyphs[k];
            let mut w = g.adv;
            if line.space_extra != 0 && bytes.get(g.cluster as usize) == Some(&b' ') {
                w += line.space_extra;
            }
            let rtl = self.levels.get(g.cluster as usize).is_some_and(|l| l % 2 == 1);
            match out.last_mut() {
                Some(c) if c.0 == g.cluster => c.2 += w,
                _ => out.push((g.cluster, x, x + w, rtl)),
            }
            x += w;
        }
        out
    }

    /// x (from the column's left) of the caret before byte `at` in line
    /// `li`: a right-to-left character's start is its right side.
    pub fn caret_x(&self, li: usize, at: u32, text: &str) -> i64 {
        let line = &self.lines[li];
        let cs = self.visual_clusters(li, text);
        if cs.is_empty() {
            return line.x;
        }
        // The cluster holding `at`, or the last one before it.
        match cs.iter().filter(|c| c.0 <= at).max_by_key(|c| c.0) {
            Some(&(c, l, r, rtl)) if c == at => if rtl { r } else { l },
            Some(&(_, l, r, rtl)) => if rtl { l } else { r },
            None => {
                // Before the line's first character: its start edge.
                let first = cs.iter().min_by_key(|c| c.0).unwrap();
                if first.3 { first.2 } else { first.1 }
            }
        }
    }

    /// Where bytes `from..to` of line `li` are drawn, as spans of x (from
    /// the column's left), left to right: one per stretch that is together
    /// on screen (a selection across directions is several).
    pub fn range_spans(&self, li: usize, from: u32, to: u32, text: &str) -> Vec<(i64, i64)> {
        let mut out: Vec<(i64, i64)> = Vec::new();
        for (c, l, r, _) in self.visual_clusters(li, text) {
            if c < from || c >= to {
                continue;
            }
            match out.last_mut() {
                Some(s) if s.1 == l => s.1 = r,
                _ => out.push((l, r)),
            }
        }
        out
    }

    /// The byte nearest x in line `li` (x from the column's left), when the
    /// line has right-to-left text; `None` for a left-to-right line, where
    /// the caller's own walk applies.
    pub fn hit_x(&self, li: usize, x: i64, text: &str) -> Option<u32> {
        let line = &self.lines[li];
        if line.visual.is_empty() {
            return None;
        }
        let cs = self.visual_clusters(li, text);
        let after = |c: u32| cs.iter().map(|d| d.0).filter(|&d| d > c).min().unwrap_or(line.end);
        let k = cs.iter().position(|c| x < c.2).unwrap_or(cs.len() - 1);
        let (c, l, r, rtl) = cs[k];
        let left_half = x < (l + r) / 2;
        Some(if left_half != rtl { c } else { after(c) })
    }

    /// The line holding byte `at` (the end goes to the last line).
    pub fn line_of(&self, at: u32) -> usize {
        self.lines.partition_point(|l| l.end <= at && l.end > l.start).min(self.lines.len().saturating_sub(1))
    }
}

/// A stretch of a paragraph's height where floating pictures take room: from
/// `top` to `bottom` (layout units down from its first line's top), `left` and
/// `right` cut from its column's start and end edges. Where too little is left
/// between them a line goes below the band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Band {
    pub top: i64,
    pub bottom: i64,
    pub left: i64,
    pub right: i64,
}

/// Narrowest room beside a floating picture a line goes in (an inch).
pub const MIN_WRAP: i64 = 1440 * crate::LU / 20;

/// Where a line `h` tall whose top would be at `y` goes among `bands` in a
/// column `width` wide: the room it skips first and what's cut from each side.
fn place_line(bands: &[Band], width: i64, y: i64, h: i64) -> (i64, i64, i64) {
    let mut at = y;
    for _ in 0..=bands.len() {
        let over = || bands.iter().filter(move |b| b.top < at + h.max(1) && b.bottom > at);
        let (l, r) = over().fold((0, 0), |(l, r), b| (l.max(b.left), r.max(b.right)));
        if width - l - r >= MIN_WRAP {
            return (at - y, l, r);
        }
        let Some(below) = over().map(|b| b.bottom).max() else { break };
        at = below;
    }
    (at - y, 0, 0)
}

/// Where a line can break: (byte, mandatory).
fn opportunities(text: &str) -> Vec<(u32, bool)> {
    unicode_linebreak::linebreaks(text).map(|(i, o)| (i as u32, o == unicode_linebreak::BreakOpportunity::Mandatory)).collect()
}

/// Lays out `p` (of `doc`) in a column `width` layout units wide.
pub fn layout_para(doc: &TextDoc, p: &Para, width: i64, fonts: &dyn Fonts) -> ParaLayout {
    layout_wrapped(doc, p, width, fonts, &[])
}

/// Lays out `p` in a column `width` wide around floating pictures' `bands`.
pub fn layout_wrapped(doc: &TextDoc, p: &Para, width: i64, fonts: &dyn Fonts, bands: &[Band]) -> ParaLayout {
    let (pf, cf) = doc.styles.para_format(p.style, &p.props);
    // Right-to-left text: levels for the lines' visual order, and the text
    // shaped with brackets mirrored where they sit right to left.
    let bidi = crate::bidi::levels_in(&p.text, pf.rtl);
    let rtl = bidi.as_ref().map_or(pf.rtl == Some(true), |b| b.1);
    let shape_src: std::borrow::Cow<str> = match &bidi {
        Some((lv, _)) => crate::bidi::mirrored(&p.text, lv).into(),
        None => p.text.as_str().into(),
    };
    let full = width - twips(pf.indent_start as i64) - twips(pf.indent_end as i64);
    let mut formats: Vec<CharFormat> = Vec::new();
    let mut glyphs: Vec<Glyph> = Vec::new();
    let mut run_fmt: Vec<(u32, u16)> = Vec::new();
    for (s, e, a) in p.spans() {
        let mut f = doc.styles.char_format(&cf, doc.attrs.get(a));
        let shift = vert_shift(&mut f);
        let fi = formats.iter().position(|x| *x == f).unwrap_or_else(|| {
            formats.push(f.clone());
            formats.len() - 1
        }) as u16;
        run_fmt.push((s, fi));
        if s == e {
            continue;
        }
        let text = &shape_src[s as usize..e as usize];
        // Tabs: four spaces' width for now (tab stops come with the ruler).
        let mut g = match (&f.object, f.note) {
            // A tombstone (tracked deletion) takes no room.
            _ if f.deleted.is_some() || f.conflict.is_some() => text.char_indices().map(|(i, _)| Glyph { face: OBJECT_FACE, id: 0, adv: 0, dx: 0, dy: 0, cluster: i as u32, fmt: 0 }).collect(),
            (Some(o), _) => shape_with_objects(fonts, text, &f, o, full.max(LU)),
            (None, Some(n)) => shape_with_label(fonts, text, &f, &doc.note_label(n)),
            (None, None) => match f.field {
                Some(x) => shape_with_label(fonts, text, &f, &doc.field_text(x)),
                None => fonts.shape(text, &f),
            },
        };
        let space = if text.contains('\t') { fonts.shape(" ", &f).first().map_or(0, |g| g.adv) } else { 0 };
        for x in g.iter_mut() {
            x.cluster += s;
            x.fmt = fi;
            x.dy += shift;
            if p.text.as_bytes().get(x.cluster as usize) == Some(&b'\t') {
                x.adv = space * 4;
            }
        }
        glyphs.extend(g);
    }
    if formats.is_empty() {
        formats.push(cf.clone());
    }
    // Width up to each byte: glyphs are in logical order (lines get their
    // visual order below).
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
    let (ind_s, ind_e) = (twips(pf.indent_start as i64), twips(pf.indent_end as i64));
    // Around floats each line's room depends on where it falls: lines are
    // broken with guessed heights, then again with the heights they got,
    // until the room each was given holds (at most three times).
    let nominal = {
        let (_, _, h) = fonts.metrics(&formats[0]);
        match pf.line_spacing {
            LineSpacing::Multiple(m) => h * m.clamp(25, 1000) as i64 / 100,
            LineSpacing::Exact(t) => twips(t.clamp(20, 31680) as i64),
            LineSpacing::AtLeast(t) => h.max(twips(t.clamp(0, 31680) as i64)),
        }
    };
    let ops = opportunities(&p.text);
    let mut heights: Vec<i64> = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    for pass in 0..3 {
        // Room per line: (skip above, start edge from the column's, width).
        let mut geo: Vec<(i64, i64, i64)> = Vec::new();
        let mut top = 0i64;
        let mut room_of = |li: usize, geo: &mut Vec<(i64, i64, i64)>| -> i64 {
            while geo.len() <= li {
                let k = geo.len();
                let h = heights.get(k).copied().unwrap_or(nominal);
                let (skip, l, r) = if bands.is_empty() { (0, 0, 0) } else { place_line(bands, width, top, h) };
                // A right-to-left paragraph starts at the right.
                let (l, r) = if rtl { (r, l) } else { (l, r) };
                let (st, en) = (ind_s.max(l), ind_e.max(r));
                let fl = if k == 0 { twips(first_line as i64) } else { 0 };
                geo.push((skip, st + fl, (width - st - en - fl).max(LU)));
                top += skip + h;
            }
            geo[li].2
        };
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
            let room = room_of(breaks.len(), &mut geo);
            let (w, _) = seg(start, b);
            if w <= room {
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
                        if e > start && cum[next as usize] - cum[start as usize] > room {
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
        for li in 0..breaks.len() {
            room_of(li, &mut geo);
        }
        // Lines.
        let fmt_at = |b: u32| run_fmt[run_fmt.partition_point(|r| r.0 <= b).saturating_sub(1)].1;
        lines = Vec::with_capacity(breaks.len());
        let mut s = 0u32;
        let mut gi = 0usize;
        let count = breaks.len();
        for (li, &(e, mand)) in breaks.iter().enumerate() {
            let g0 = gi;
            while gi < glyphs.len() && glyphs[gi].cluster < e {
                gi += 1;
            }
            let (mut w, content_end) = seg(s, e);
            let (skip, base, room) = geo[li];
            // Tabs: each takes the text after it to the next stop, measured from
            // the column's start edge.
            let mut leaders = Vec::new();
            let tab = |g: &Glyph| p.text.as_bytes().get(g.cluster as usize) == Some(&b'\t');
            if glyphs[g0..gi].iter().any(tab) {
                let mut pen = base;
                for k in g0..gi {
                    if tab(&glyphs[k]) {
                        // The text up to the next tab (spaces at the line's end left out).
                        let (mut after, mut point) = (0i64, None);
                        for g in glyphs[k + 1..gi].iter().take_while(|g| !tab(g)) {
                            if point.is_none() && p.text.as_bytes().get(g.cluster as usize) == Some(&b'.') {
                                point = Some(after);
                            }
                            if g.cluster < content_end {
                                after += g.adv;
                            }
                        }
                        let custom = pf.tabs.iter().find(|t| twips(t.pos as i64) > pen);
                        let (stop, align, leader) = match custom {
                            Some(t) => (twips(t.pos as i64), t.align, t.leader),
                            None => {
                                let step = twips(DEFAULT_TAB as i64);
                                let from = pf.tabs.last().map_or(0, |t| twips(t.pos as i64)).max(pen);
                                ((from / step + 1) * step, TabAlign::Start, Leader::None)
                            }
                        };
                        let to = match align {
                            TabAlign::Start => stop,
                            TabAlign::End => stop - after,
                            TabAlign::Center => stop - after / 2,
                            TabAlign::Decimal => stop - point.unwrap_or(after),
                        };
                        let adv = (to - pen).max(0);
                        glyphs[k].adv = adv;
                        if leader != Leader::None && adv > 0 {
                            leaders.push((pen, pen + adv, leader));
                        }
                    }
                    pen += glyphs[k].adv;
                }
                w = glyphs[g0..gi].iter().filter(|g| g.cluster < content_end).map(|g| g.adv).sum();
                // Leaders are drawn from the line's start, like its glyphs.
                for l in leaders.iter_mut() {
                    l.0 -= base;
                    l.1 -= base;
                }
            }
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
                if let Some(o) = formats[g.fmt as usize].object.as_ref().filter(|o| o.float.is_none()) {
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
            for l in leaders.iter_mut() {
                l.0 += x;
                l.1 += x;
            }
            // Right to left: the same distances from the right edge.
            let x = if rtl {
                let spaces = if space_extra != 0 { p.text[s as usize..content_end as usize].bytes().filter(|&c| c == b' ').count() as i64 } else { 0 };
                for l in leaders.iter_mut() {
                    *l = (width - l.1, width - l.0, l.2);
                }
                width - x - (w + space_extra * spaces)
            } else {
                x
            };
            let visual = match &bidi {
                Some((lv, r)) => crate::bidi::visual(&glyphs[g0..gi], &p.text, lv, *r, g0),
                None => Vec::new(),
            };
            lines.push(Line { start: s, end: e, glyphs: g0..gi, x, width: w, space_extra, ascent: asc, descent: desc, height: height + skip, skip, leaders, visual });
            s = e;
        }
        if bands.is_empty() || pass == 2 {
            break;
        }
        // Did each line get the room it was broken for?
        let got: Vec<i64> = lines.iter().map(|l| l.height - l.skip).collect();
        let mut check = Vec::new();
        let mut y = 0;
        for (k, &h) in got.iter().enumerate() {
            let (skip, l, r) = place_line(bands, width, y, h);
            let (l, r) = if rtl { (r, l) } else { (l, r) };
            let (st, en) = (ind_s.max(l), ind_e.max(r));
            let fl = if k == 0 { twips(first_line as i64) } else { 0 };
            check.push((skip, st + fl, (width - st - en - fl).max(LU)));
            y += skip + h;
        }
        if check == geo[..lines.len()] {
            break;
        }
        heights = got;
    }
    let height = lines.iter().map(|l| l.height).sum();
    ParaLayout { space_before: twips(pf.space_before.max(0) as i64), space_after: twips(pf.space_after.max(0) as i64), format: pf, formats, glyphs, lines, height, levels: bidi.map(|b| b.0).unwrap_or_default(), rtl }
}

/// Paragraph layouts by block, kept while their inputs hold.
#[derive(Default)]
pub struct LayoutCache {
    map: HashMap<BlockId, (u64, Rc<ParaLayout>)>,
    pub hits: u64,
    pub misses: u64,
}

fn key(doc: &TextDoc, p: &Para, width: i64, bands: &[Band]) -> u64 {
    let mut h = DefaultHasher::new();
    bands.hash(&mut h);
    p.text.hash(&mut h);
    p.style.hash(&mut h);
    p.props.hash(&mut h);
    width.hash(&mut h);
    for &(s, a) in &p.runs {
        s.hash(&mut h);
        let at = doc.attrs.get(a);
        at.hash(&mut h);
        // A note anchor shows its number, which other anchors move.
        if let Some(n) = at.note {
            doc.note_label(n).hash(&mut h);
        }
        if let Some(x) = at.field {
            doc.field_text(x).hash(&mut h);
        }
    }
    // Styles change rarely; their identity is part of the key through the
    // resolved paragraph format.
    doc.styles.para_format(p.style, &p.props).hash(&mut h);
    h.finish()
}

impl LayoutCache {
    pub fn get(&mut self, doc: &TextDoc, p: &Para, width: i64, fonts: &dyn Fonts) -> Rc<ParaLayout> {
        self.get_wrapped(doc, p, width, fonts, &[])
    }

    /// `p`'s layout around floating pictures' `bands` (see [`layout_wrapped`]).
    pub fn get_wrapped(&mut self, doc: &TextDoc, p: &Para, width: i64, fonts: &dyn Fonts, bands: &[Band]) -> Rc<ParaLayout> {
        let k = key(doc, p, width, bands);
        if let Some((kk, l)) = self.map.get(&p.id) {
            if *kk == k {
                self.hits += 1;
                return l.clone();
            }
        }
        self.misses += 1;
        let l = Rc::new(layout_wrapped(doc, p, width, fonts, bands));
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
    fn tabs_go_to_their_stops() {
        use crate::model::{Leader, TabAlign, TabStop};
        let mut d = doc("Intro\t12\tx\ty");
        let tabs = vec![TabStop { pos: 7200, align: TabAlign::End, leader: Leader::Dots }, TabStop { pos: 8000, align: TabAlign::Start, leader: Leader::None }];
        d.blocks.update(0, |b| b.as_para_mut().unwrap().props.tabs = Some(tabs));
        let l = layout_para(&d, d.para(0).unwrap(), twips(9360), &FixedFonts);
        assert_eq!(l.lines.len(), 1);
        let pen = |at: u32| l.glyphs.iter().take_while(|g| g.cluster < at).map(|g| g.adv).sum::<i64>();
        // "12" ends at the right stop, with dots before it; "x" starts at 8000.
        assert_eq!(pen(8), twips(7200));
        assert_eq!(l.lines[0].leaders.len(), 1);
        assert_eq!(l.lines[0].leaders[0].1, pen(6));
        assert_eq!(pen(9), twips(8000));
        // Past the stops, every half inch.
        assert_eq!(pen(11) % twips(720), 0);
        assert!(pen(11) > twips(8000));
    }

    #[test]
    fn a_picture_is_one_glyph_on_the_baseline_and_fits_the_column() {
        use crate::model::ObjectRef;
        let mut d = doc("ab\u{FFFC}cd");
        let o = ObjectRef { image: 0, width: 2880, height: 1440, float: None, shape: false };
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
    fn office_fonts_get_their_stand_ins_and_scripts_their_noto_faces() {
        let f = PinnedFonts::new().unwrap();
        let family = |name: &str, text: &str| {
            let mut d = doc(text);
            let a = d.attrs.intern(CharAttrs { family: Some(name.into()), ..Default::default() });
            d.blocks.update(0, |b| b.as_para_mut().unwrap().runs = vec![(0, a)]);
            let l = layout_para(&d, d.para(0).unwrap(), 1_000_000, &f);
            l.glyphs.iter().map(|g| f.face_family(g.face).to_string()).collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(family("Calibri", "Office"), ["Carlito".to_string()].into());
        assert_eq!(family("Times New Roman", "Office"), ["Liberation Serif".to_string()].into());
        assert_eq!(family("Courier New", "x"), ["Liberation Mono".to_string()].into());
        assert_eq!(family("Cambria", "x"), ["Caladea".to_string()].into());
        assert_eq!(family("", "Office"), ["IBM Plex Sans".to_string()].into());
        assert_eq!(family("Arial", "नमस्ते"), ["Noto Sans Devanagari".to_string()].into());
        assert_eq!(family("", "வணக்கம் abc"), ["IBM Plex Sans".to_string(), "Noto Sans Tamil".to_string()].into());
        assert_eq!(family("", "שלום"), ["Noto Sans Hebrew".to_string()].into());
        // Emoji take the colour face, and a ZWJ family or a skin tone is one glyph.
        assert_eq!(family("", "😀"), ["Noto Color Emoji".to_string()].into());
        // VS16 takes the emoji face for a character a text face also has.
        assert_eq!(family("", "\u{2764}\u{FE0F}"), ["Noto Color Emoji".to_string()].into());
        // A keycap (digit, VS16, U+20E3) is one colour glyph.
        assert_eq!(family("", "1\u{FE0F}\u{20E3}"), ["Noto Color Emoji".to_string()].into());
        let l = layout_para(&doc("1\u{FE0F}\u{20E3}"), doc("1\u{FE0F}\u{20E3}").para(0).unwrap(), 1_000_000, &f);
        assert_eq!(l.glyphs.len(), 1, "{:?}", l.glyphs);
        for seq in ["👨\u{200D}👩\u{200D}👧", "👍\u{1F3FD}", "🇮🇳"] {
            let l = layout_para(&doc(seq), doc(seq).para(0).unwrap(), 1_000_000, &f);
            assert_eq!(l.glyphs.len(), 1, "{seq}: {:?}", l.glyphs);
        }
        // Devanagari shapes: the conjunct क्ष is fewer glyphs than characters.
        let mut d = doc("क्ष");
        let a = d.attrs.intern(CharAttrs { family: Some("Noto Sans Devanagari".into()), ..Default::default() });
        d.blocks.update(0, |b| b.as_para_mut().unwrap().runs = vec![(0, a)]);
        let l = layout_para(&d, d.para(0).unwrap(), 1_000_000, &f);
        assert!(l.glyphs.len() < 3, "{:?}", l.glyphs);
        // The PDF writer gets the library face's bytes and address.
        let g = l.glyphs[0];
        assert!(f.face_bytes(g.face).len() > 1000);
        assert!(f.face_address(g.face).starts_with("blake3:"));
    }

    #[test]
    fn a_paragraph_set_right_to_left_starts_at_the_right_even_in_latin() {
        let f = PinnedFonts::new().unwrap();
        let mut d = doc("Hello.");
        d.blocks.update(0, |b| b.as_para_mut().unwrap().props.rtl = Some(true));
        let l = layout_para(&d, d.para(0).unwrap(), 1_000_000, &f);
        assert!(l.rtl);
        let line = &l.lines[0];
        assert_eq!(line.x + line.width, 1_000_000);
        // The full stop is drawn first (leftmost): it sits at the paragraph's level.
        assert_eq!(l.glyphs[line.visual[0] as usize].cluster, 5);
        // Set left to right, Hebrew text starts at the left.
        let mut d = doc("שלום");
        d.blocks.update(0, |b| b.as_para_mut().unwrap().props.rtl = Some(false));
        let l = layout_para(&d, d.para(0).unwrap(), 1_000_000, &f);
        assert!(!l.rtl);
        assert_eq!(l.lines[0].x, 0);
    }

    #[test]
    fn right_to_left_paragraphs_start_at_the_right_in_visual_order() {
        let f = PinnedFonts::new().unwrap();
        let text = "שלום עולם";
        let d = doc(text);
        let l = layout_para(&d, d.para(0).unwrap(), 1_000_000, &f);
        assert!(l.rtl);
        let line = &l.lines[0];
        assert!(line.x > 500_000, "starts at the right: {}", line.x);
        assert_eq!(line.x + line.width, 1_000_000);
        // Glyphs stay logical; the line draws them last letter first.
        assert!(l.glyphs.windows(2).all(|w| w[0].cluster <= w[1].cluster));
        let first_drawn = l.glyphs[line.visual[0] as usize].cluster;
        assert_eq!(first_drawn as usize, text.char_indices().last().unwrap().0);
        // The caret before the first letter is at the right edge; after the
        // last, at the left.
        assert_eq!(l.caret_x(0, 0, text), line.x + line.width);
        assert_eq!(l.caret_x(0, text.len() as u32, text), line.x);
        // A click right of the middle of the first letter lands before it.
        let w0 = l.glyphs[0].adv;
        assert_eq!(l.hit_x(0, line.x + line.width - w0 / 4, text), Some(0));
        assert_eq!(l.hit_x(0, line.x + line.width - w0 * 3 / 4, text), Some(2));

        // Left to right with a Hebrew word: Latin first, the word reversed.
        let text = "ab שלום cd";
        let d = doc(text);
        let l = layout_para(&d, d.para(0).unwrap(), 1_000_000, &f);
        assert!(!l.rtl);
        let line = &l.lines[0];
        assert_eq!(line.x, 0);
        let order: Vec<u32> = line.visual.iter().map(|&k| l.glyphs[k as usize].cluster).collect();
        let hebrew = text.find('ש').unwrap() as u32;
        let pos_shin = order.iter().position(|&c| c == hebrew).unwrap();
        let pos_mem = order.iter().position(|&c| c == text.find('ם').unwrap() as u32).unwrap();
        assert!(pos_mem < pos_shin, "{order:?}");
        assert_eq!(order[0], 0);
        assert_eq!(*order.last().unwrap(), (text.len() - 1) as u32);
        // Selecting "ab " and the Hebrew word's first letter: two stretches,
        // the letter being the word's rightmost.
        let lamed = text.find('ל').unwrap() as u32;
        let spans = l.range_spans(0, 0, lamed, text);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(spans[0].0, 0);
        assert_eq!(spans[1].1, l.caret_x(0, hebrew, text));
        // Every byte the caret can sit at maps to an x and back.
        for (i, _) in text.char_indices() {
            let x = l.caret_x(0, i as u32, text);
            assert!(x >= 0 && x <= line.width, "{i} {x}");
        }
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
