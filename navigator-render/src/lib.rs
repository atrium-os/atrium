//! Markdown + viewport + pinned font set → NSG (navigator-backend §4.1a, M0).
//!
//! ★ A PURE FUNCTION, AND HERMETIC BY CONSTRUCTION. Every length is an integer
//! in 1/64 px (`U`); shaping yields integer font units and scaling is integer
//! arithmetic with one rounding rule (`scale`). There is no clock, no network,
//! no randomness, no hash-map iteration, and no float in the output — so there
//! is nothing that could differ between two machines running the same bytes.
//!
//! M0 scope, stated so a missing feature reads as a limit and not a bug:
//! greedy line breaking at spaces; no italic face (emphasis is marked on the
//! run, drawn upright); images render their alt text; raw HTML is skipped and
//! counted; code blocks do not wrap (overflow is counted).

pub mod conformance;
pub mod fontset;
pub mod html;
pub mod nsg;

use fontset::{Family, FontSet};
use pulldown_cmark::{Event, HeadingLevel, Options as MdOptions, Parser, Tag, TagEnd};

/// 1/64 px.
pub type U = i64;
pub const PX: U = 64;

pub struct Options {
    pub width_px: i64,
}
impl Default for Options {
    fn default() -> Self { Options { width_px: 800 } }
}

/// What M0 could not render faithfully, counted rather than hidden.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    /// Glyphs no face in the stack had — drawn as .notdef.
    pub notdef: usize,
    pub html_skipped: usize,
    pub images_as_alt: usize,
    /// Lines wider than the viewport (unbreakable words, code).
    pub overflow_lines: usize,
    /// Runs marked emphasis but drawn with the upright face.
    pub em_upright: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rect { pub x: U, pub y: U, pub w: U, pub h: U, pub rgba: u32,
                  /// Corner radii (top-left, top-right, bottom-right, bottom-left);
                  /// all zero = square. Written only when non-zero, so NSG without
                  /// rounded boxes is byte-identical to before they existed.
                  pub radii: [U; 4],
                  /// Ring width: 0 fills the rect; n > 0 paints only the n-wide band
                  /// inside its edge — one node for a rounded border.
                  pub ring: U }

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub face: usize,
    pub size: U,
    pub rgba: u32,
    pub x: U,
    /// Baseline.
    pub y: U,
    pub em: bool,
    /// (glyph id, dx, dy) relative to (x, y).
    pub glyphs: Vec<(u16, U, U)>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link { pub x: U, pub y: U, pub w: U, pub h: U, pub href: String }

/// What a node is clipped by and which opacity group it belongs to, as
/// indices into the scene's `clips` and `groups`.
///
/// ★ These are FLAT attributes of a node, not begin/end markers, because
/// `order` is re-sorted for painting (stacking contexts): a push/pop pair
/// could not survive that, and an index on the node does.
pub type Attrs = (Option<u32>, Option<u32>, Option<u32>);

/// A 2D affine transform, already composed with its ancestors'.
/// `a b c d` are scalars in **1/65536**, `e f` are a translation in 1/64 px:
/// `x' = a·x + c·y + e`, `y' = b·x + d·y + f`.
pub const XF_ONE: i64 = 65536;
pub type Xform = [i64; 6];

#[derive(Debug, Default)]
pub struct Scene {
    pub width: U,
    pub height: U,
    pub rects: Vec<Rect>,
    pub runs: Vec<Run>,
    pub links: Vec<Link>,
    /// Draw order across the three kinds: (kind, index). 0 rect, 1 run, 2 link.
    pub order: Vec<(u8, usize)>,
    /// Clip rectangles, each already intersected with its ancestors', so a
    /// node needs only one index and a reader needs no stack.
    pub clips: Vec<(U, U, U, U)>,
    /// Paint-level transforms (profile §3.8: they never affect layout).
    /// During layout these are LOCAL matrices; `resolve_xforms` composes each
    /// with its ancestors' once every box's size is known.
    pub xforms: Vec<Xform>,
    pub xform_parents: Vec<Option<u32>>,
    /// Opacity groups: (alpha 0-255, the group this one composites into).
    /// Nested groups keep a parent, because compositing them is not the same
    /// as multiplying the alphas.
    pub groups: Vec<(u32, Option<u32>)>,
    /// Parallel to `rects` / `runs` / `links`: pushed by the same method that
    /// pushes the node, so they cannot come apart.
    pub rect_attrs: Vec<Attrs>,
    pub run_attrs: Vec<Attrs>,
    pub link_attrs: Vec<Attrs>,
    /// The clip and group a node created now belongs to.
    pub(crate) cur: Attrs,
}

impl Scene {
    pub(crate) fn rect(&mut self, r: Rect) { self.order.push((0, self.rects.len())); self.rects.push(r); self.rect_attrs.push(self.cur) }
    pub(crate) fn run(&mut self, r: Run) { self.order.push((1, self.runs.len())); self.runs.push(r); self.run_attrs.push(self.cur) }
    pub(crate) fn link(&mut self, l: Link) { self.order.push((2, self.links.len())); self.links.push(l); self.link_attrs.push(self.cur) }
    /// A rect painted into a slot reserved earlier (a box's own background,
    /// which is decided only after its children are laid out).
    pub(crate) fn insert_rect(&mut self, slot: usize, r: Rect) {
        self.order.insert(slot, (0, self.rects.len()));
        self.rects.push(r);
        self.rect_attrs.push(self.cur);
    }
    /// A clip rectangle, intersected with whatever is already in force.
    pub(crate) fn clip(&mut self, x: U, y: U, w: U, h: U) -> u32 {
        let (x, y, w, h) = match self.cur.0.and_then(|i| self.clips.get(i as usize).copied()) {
            Some((px, py, pw, ph)) => {
                let (x0, y0) = (x.max(px), y.max(py));
                (x0, y0, (x + w).min(px + pw) - x0, (y + h).min(py + ph) - y0)
            }
            None => (x, y, w, h),
        };
        self.clips.push((x, y, w.max(0), h.max(0)));
        self.clips.len() as u32 - 1
    }
    pub(crate) fn group(&mut self, alpha: u32) -> u32 {
        self.groups.push((alpha, self.cur.1));
        self.groups.len() as u32 - 1
    }
    /// A transform. ★ It cannot be composed with its ancestor's HERE: a
    /// box's matrix needs its own height, which is known only after its
    /// children have been laid out — so an ancestor's matrix is still a
    /// placeholder while its descendants are declaring theirs. The local
    /// matrix and the parent are recorded, and `resolve_xforms` composes
    /// them at the end.
    pub(crate) fn xform(&mut self, m: Xform) -> u32 {
        self.xforms.push(m);
        self.xform_parents.push(self.cur.2);
        self.xforms.len() as u32 - 1
    }
    /// Compose every transform with its ancestors', outermost first, so each
    /// one is absolute and a reader needs no stack (as with clips).
    pub(crate) fn resolve_xforms(&mut self) {
        for i in 0..self.xforms.len() {
            let mut chain = vec![];
            let mut p = self.xform_parents[i];
            while let Some(j) = p { chain.push(j as usize); p = self.xform_parents[j as usize] }
            for j in chain { self.xforms[i] = compose(self.xforms[j], self.xforms[i]) }
        }
    }
}

/// Sine and cosine of an angle in DEGREES, without libm.
///
/// ★ `f64::sin` is NOT specified to be correctly rounded, and two platforms'
/// libm can differ in the last bits — which would make NSG machine-dependent,
/// the one thing it must never be. IEEE-754 `+`, `*` and `/` ARE specified,
/// so a fixed polynomial evaluated in f64 gives the same bits everywhere.
///
/// Exact quadrant reduction in degrees first (so 90, 180, 270 are exact),
/// then a Taylor series to x^13 on |x| <= pi/4, where its error is ~3e-14 —
/// far below
/// the 1/65536 the matrix is rounded to.
pub fn sin_cos_deg(deg: f64) -> (f64, f64) {
    // Reduce to [0, 360) exactly, in degrees.
    let d = deg - 360.0 * (deg / 360.0).floor();
    let (q, r) = ((d / 90.0).floor() as i64 % 4, d - 90.0 * (d / 90.0).floor());
    // r in [0, 90); fold to [0, 45] so the series is only ever used there.
    let (s45, c45) = {
        let x = (if r <= 45.0 { r } else { 90.0 - r }) * (std::f64::consts::PI / 180.0);
        let x2 = x * x;
        // sin x = x - x^3/3! + x^5/5! - ... + x^13/13!
        let sin = x * (1.0 - x2 * (1.0 / 6.0 - x2 * (1.0 / 120.0 - x2 * (1.0 / 5040.0 - x2 * (1.0 / 362880.0
                     - x2 * (1.0 / 39916800.0 - x2 * (1.0 / 6227020800.0)))))));
        // cos x = 1 - x^2/2! + x^4/4! - ... + x^12/12!
        let cos = 1.0 - x2 * (0.5 - x2 * (1.0 / 24.0 - x2 * (1.0 / 720.0 - x2 * (1.0 / 40320.0
                     - x2 * (1.0 / 3628800.0 - x2 * (1.0 / 479001600.0))))));
        if r <= 45.0 { (sin, cos) } else { (cos, sin) }
    };
    match q {
        0 => (s45, c45),
        1 => (c45, -s45),
        2 => (-s45, -c45),
        _ => (-c45, s45),
    }
}

/// Round-half-away-from-zero of `v * num / den`, in integers.
pub fn scale(v: i64, num: i64, den: i64) -> i64 {
    let p = v * num;
    if p >= 0 { (p + den / 2) / den } else { -((-p + den / 2) / den) }
}

const TEXT: u32 = 0x1f2328ff;
const MUTED: u32 = 0x59636eff;
const LINK: u32 = 0x0969daff;
const RULE: u32 = 0xd0d7deff;
const CODE_BG: u32 = 0xf6f8faff;
const QUOTE_BAR: u32 = 0xd0d7deff;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Style { pub(crate) family: Family, pub(crate) bold: bool, pub(crate) em: bool, pub(crate) size: U, pub(crate) rgba: u32, pub(crate) link: Option<usize>,
    /// letter-spacing / word-spacing, added after each glyph / each space.
    pub(crate) ls: U, pub(crate) ws: U,
    /// font-variant-numeric: tabular-nums (the OpenType `tnum` feature).
    pub(crate) tnum: bool }

enum Atom { Word(String, Style), Space(Style), Break }

const TNUM: [rustybuzz::Feature; 1] = [rustybuzz::Feature { tag: rustybuzz::ttf_parser::Tag::from_bytes(b"tnum"), value: 1, start: 0, end: u32::MAX }];

pub(crate) struct Shaper<'a> {
    fonts: &'a FontSet,
    tt: Vec<ttf_parser::Face<'a>>,
    hb: Vec<rustybuzz::Face<'a>>,
}

/// A shaped piece of one face: glyphs positioned from its own origin.
pub(crate) struct Piece { pub(crate) face: usize, pub(crate) glyphs: Vec<(u16, U, U)>, pub(crate) width: U, pub(crate) text: String }

impl<'a> Shaper<'a> {
    pub(crate) fn new(fonts: &'a FontSet) -> Self {
        let tt = fonts.faces.iter().map(|f| ttf_parser::Face::parse(&f.bytes, 0).expect("pinned")).collect();
        let hb = fonts.faces.iter().map(|f| rustybuzz::Face::from_slice(&f.bytes, 0).expect("pinned")).collect();
        Shaper { fonts, tt, hb }
    }

    /// Split `text` by the first face in the stack that has each character,
    /// then shape each piece. Characters no face has go to the primary face
    /// and are counted as .notdef.
    pub(crate) fn shape(&self, text: &str, st: &Style, report: &mut Report) -> Vec<Piece> {
        let stack = self.fonts.stack(st.family, st.bold);
        let mut segs: Vec<(usize, String)> = vec![];
        for ch in text.chars() {
            let face = if ch.is_whitespace() || ch.is_control() { stack[0] } else {
                stack.iter().copied().find(|&i| self.tt[i].glyph_index(ch).is_some()).unwrap_or_else(|| {
                    report.notdef += 1;
                    stack[0]
                })
            };
            match segs.last_mut() {
                Some((f, s)) if *f == face => s.push(ch),
                _ => segs.push((face, ch.to_string())),
            }
        }
        segs.into_iter().map(|(face, s)| {
            let mut buf = rustybuzz::UnicodeBuffer::new();
            buf.push_str(&s);
            buf.guess_segment_properties();
            let feats: &[rustybuzz::Feature] = if st.tnum { &TNUM } else { &[] };
            let out = rustybuzz::shape(&self.hb[face], feats, buf);
            let upem = self.fonts.faces[face].upem;
            let (mut pen, mut glyphs, mut extra) = (0i64, vec![], 0 as U);
            for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
                glyphs.push((info.glyph_id as u16,
                             scale(pen + pos.x_offset as i64, st.size, upem) + extra,
                             -scale(pos.y_offset as i64, st.size, upem)));
                pen += pos.x_advance as i64;
                // Spacing is added AFTER each glyph (and after each space for
                // word-spacing), in 1/64 px, outside the font-unit pen so the
                // integer scaling of the shaped advances is untouched.
                extra += st.ls;
                if s.as_bytes().get(info.cluster as usize) == Some(&b' ') { extra += st.ws }
            }
            Piece { face, glyphs, width: scale(pen, st.size, upem) + extra, text: s }
        }).collect()
    }

    /// Ascent and line height for a style, from its primary face.
    pub(crate) fn metrics(&self, st: &Style) -> (U, U) {
        let f = &self.fonts.faces[self.fonts.stack(st.family, st.bold)[0]];
        let asc = scale(f.ascent, st.size, f.upem);
        let desc = scale(-f.descent, st.size, f.upem);
        let lh = st.size * 3 / 2;
        (asc + (lh - asc - desc) / 2, lh)
    }

    /// Ascent and descent (both positive) of a style's primary face.
    pub(crate) fn asc_desc(&self, st: &Style) -> (U, U) {
        let f = &self.fonts.faces[self.fonts.stack(st.family, st.bold)[0]];
        (scale(f.ascent, st.size, f.upem), scale(-f.descent, st.size, f.upem))
    }
}

struct Layout<'a> {
    sh: Shaper<'a>,
    scene: Scene,
    report: Report,
    links: Vec<String>,
    width: U,
    margin: U,
    y: U,
    indent: U,
    styles: Vec<Style>,
    atoms: Vec<Atom>,
    /// List marker waiting for the first line of its item.
    marker: Option<String>,
    lists: Vec<Option<u64>>,
    quotes: Vec<U>,
    code: Option<String>,
    in_image: bool,
    table: Option<Table>,
}

#[derive(Default)]
struct Table { rows: Vec<Vec<Vec<Atom>>>, head_rows: usize, in_head: bool }

pub fn render(md: &str, fonts: &FontSet, opts: &Options) -> (Scene, Report) {
    let margin = 32 * PX;
    let base = Style { family: Family::Sans, bold: false, em: false, size: 16 * PX, rgba: TEXT, link: None, ls: 0, ws: 0, tnum: false };
    let mut l = Layout {
        sh: Shaper::new(fonts), scene: Scene { width: opts.width_px * PX, ..Default::default() },
        report: Report::default(), links: vec![], width: opts.width_px * PX, margin, y: margin,
        indent: 0, styles: vec![base], atoms: vec![], marker: None, lists: vec![], quotes: vec![],
        code: None, in_image: false, table: None,
    };
    let mut opt = MdOptions::empty();
    opt.insert(MdOptions::ENABLE_TABLES);
    opt.insert(MdOptions::ENABLE_STRIKETHROUGH);
    for ev in Parser::new_ext(md, opt) { l.event(ev) }
    l.flush();
    l.scene.height = l.y + margin;
    (l.scene, l.report)
}

impl<'a> Layout<'a> {
    fn st(&self) -> Style { self.styles.last().cloned().expect("base style") }
    fn push(&mut self, f: impl FnOnce(&mut Style)) { let mut s = self.st(); f(&mut s); self.styles.push(s) }
    fn pop(&mut self) { if self.styles.len() > 1 { self.styles.pop(); } }
    fn left(&self) -> U { self.margin + self.indent }
    fn avail(&self) -> U { self.width - self.margin - self.left() }

    fn text(&mut self, t: &str) {
        if let Some(c) = self.code.as_mut() { c.push_str(t); return }
        let st = self.st();
        let target: &mut Vec<Atom> = match self.table.as_mut() {
            Some(tb) => tb.rows.last_mut().and_then(|r| r.last_mut()).expect("cell open"),
            None => &mut self.atoms,
        };
        let mut word = String::new();
        for ch in t.chars() {
            if ch.is_whitespace() {
                if !word.is_empty() { target.push(Atom::Word(std::mem::take(&mut word), st.clone())) }
                if !matches!(target.last(), Some(Atom::Space(_)) | None) { target.push(Atom::Space(st.clone())) }
            } else { word.push(ch) }
        }
        if !word.is_empty() { target.push(Atom::Word(word, st)) }
    }

    fn gap(&mut self, st: &Style) { self.y += st.size * 3 / 4 }

    fn event(&mut self, ev: Event) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if self.in_image { self.text(&format!("[{t}]")) } else { self.text(&t) }
            }
            Event::Code(t) => { self.push(|s| s.family = Family::Mono); self.text(&t); self.pop() }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.atoms.push(Atom::Break),
            Event::Rule => {
                self.flush();
                let y = self.y + 8 * PX;
                self.scene.rect(Rect { x: self.left(), y, w: self.avail(), h: 2 * PX, rgba: RULE, radii: [0; 4], ring: 0 });
                self.y = y + 2 * PX + 16 * PX;
            }
            Event::Html(_) | Event::InlineHtml(_) => self.report.html_skipped += 1,
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
            Event::FootnoteReference(r) => self.text(&format!("[{r}]")),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                let size = match level {
                    HeadingLevel::H1 => 32, HeadingLevel::H2 => 24, HeadingLevel::H3 => 20,
                    HeadingLevel::H4 => 16, HeadingLevel::H5 => 14, HeadingLevel::H6 => 13,
                };
                self.y += size * PX / 2;
                self.push(|s| { s.size = size * PX; s.bold = true });
            }
            Tag::BlockQuote(_) => { self.flush(); self.quotes.push(self.y); self.indent += 16 * PX; self.push(|s| s.rgba = MUTED) }
            Tag::CodeBlock(_) => { self.flush(); self.code = Some(String::new()) }
            Tag::List(first) => { self.flush(); self.lists.push(first); self.indent += 24 * PX }
            Tag::Item => {
                self.flush();
                let m = match self.lists.last_mut() {
                    Some(Some(n)) => { let s = format!("{n}."); *n += 1; s }
                    _ => "•".to_string(),
                };
                self.marker = Some(m);
            }
            Tag::Emphasis => { self.report.em_upright += 1; self.push(|s| s.em = true) }
            Tag::Strong => self.push(|s| s.bold = true),
            Tag::Strikethrough => self.push(|_| {}),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.to_string());
                let i = self.links.len() - 1;
                self.push(|s| { s.rgba = LINK; s.link = Some(i) })
            }
            Tag::Image { .. } => { self.report.images_as_alt += 1; self.in_image = true }
            Tag::Table(_) => { self.flush(); self.table = Some(Table::default()) }
            Tag::TableHead => { if let Some(t) = self.table.as_mut() { t.in_head = true; t.rows.push(vec![]) } }
            Tag::TableRow => { if let Some(t) = self.table.as_mut() { t.rows.push(vec![]) } }
            Tag::TableCell => {
                let head = self.table.as_ref().is_some_and(|t| t.in_head);
                if head { self.push(|s| s.bold = true) } else { self.push(|_| {}) }
                if let Some(t) = self.table.as_mut() { t.rows.last_mut().expect("row").push(vec![]) }
            }
            Tag::HtmlBlock => {}
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => { self.flush(); let st = self.st(); self.gap(&st) }
            TagEnd::Heading(_) => { self.flush(); let st = self.st(); self.pop(); self.y += st.size / 3 }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.indent -= 16 * PX;
                self.pop();
                if let Some(top) = self.quotes.pop() {
                    self.scene.rect(Rect { x: self.left(), y: top, w: 4 * PX, h: self.y - top, rgba: QUOTE_BAR, radii: [0; 4], ring: 0 });
                }
            }
            TagEnd::CodeBlock => self.code_block(),
            TagEnd::List(_) => { self.flush(); self.lists.pop(); self.indent -= 24 * PX; let st = self.st(); self.gap(&st) }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => self.pop(),
            TagEnd::Image => self.in_image = false,
            TagEnd::TableHead => { if let Some(t) = self.table.as_mut() { t.in_head = false; t.head_rows = t.rows.len() } }
            TagEnd::TableCell => self.pop(),
            TagEnd::Table => self.table_end(),
            _ => {}
        }
    }

    fn code_block(&mut self) {
        let Some(src) = self.code.take() else { return };
        let st = Style { family: Family::Mono, bold: false, em: false, size: 14 * PX, rgba: TEXT, link: None, ls: 0, ws: 0, tnum: false };
        let (asc, lh) = self.sh.metrics(&st);
        let lines: Vec<&str> = src.strip_suffix('\n').unwrap_or(&src).split('\n').collect();
        let pad = 8 * PX;
        let top = self.y;
        self.scene.rect(Rect { x: self.left(), y: top, w: self.avail(), h: lines.len() as U * lh + 2 * pad, rgba: CODE_BG, radii: [0; 4], ring: 0 });
        let mut y = top + pad;
        for line in lines {
            let mut x = self.left() + pad;
            for p in self.sh.shape(line, &st, &mut self.report) {
                let w = p.width;
                self.scene.run(Run { face: p.face, size: st.size, rgba: st.rgba, x, y: y + asc, em: false, glyphs: p.glyphs, text: p.text });
                x += w;
            }
            if x > self.left() + self.avail() { self.report.overflow_lines += 1 }
            y += lh;
        }
        self.y = y + pad + 12 * PX;
    }

    /// Lay out the pending inline atoms as a paragraph at the current indent.
    fn flush(&mut self) {
        if self.atoms.is_empty() {
            if let Some(m) = self.marker.take() { self.atoms.push(Atom::Word(m, self.st())); self.atoms.push(Atom::Space(self.st())); }
            else { return }
        }
        let atoms = std::mem::take(&mut self.atoms);
        let (left, avail) = (self.left(), self.avail());
        let marker = self.marker.take();
        let y = flow(&self.sh, &atoms, left, avail, self.y, marker, &mut self.scene, &mut self.report, &self.links);
        self.y = y;
    }

    fn table_end(&mut self) {
        let Some(t) = self.table.take() else { return };
        let ncols = t.rows.iter().map(|r| r.len()).max().unwrap_or(0).max(1) as U;
        let (left, avail) = (self.left(), self.avail());
        let colw = avail / ncols;
        let pad = 6 * PX;
        for row in &t.rows {
            let top = self.y;
            let mut bottom = top;
            for (c, cell) in row.iter().enumerate() {
                let x = left + c as U * colw;
                let y = flow(&self.sh, cell, x + pad, colw - 2 * pad, top + pad, None, &mut self.scene, &mut self.report, &self.links);
                bottom = bottom.max(y + pad);
            }
            for c in 0..=ncols {
                self.scene.rect(Rect { x: left + c * colw, y: top, w: PX, h: bottom - top, rgba: RULE, radii: [0; 4], ring: 0 });
            }
            self.scene.rect(Rect { x: left, y: top, w: ncols * colw, h: PX, rgba: RULE, radii: [0; 4], ring: 0 });
            self.y = bottom;
        }
        self.scene.rect(Rect { x: left, y: self.y, w: ncols * colw, h: PX, rgba: RULE, radii: [0; 4], ring: 0 });
        self.y += 16 * PX;
    }
}

/// Greedy line breaking. Returns the y below the last line.
#[allow(clippy::too_many_arguments)]
fn flow(sh: &Shaper, atoms: &[Atom], left: U, avail: U, mut y: U, marker: Option<String>,
        scene: &mut Scene, report: &mut Report, links: &[String]) -> U {
    struct Placed { x: U, st: Style, pieces: Vec<Piece> }
    // Shape once: words and the space that follows each style.
    let mut lines: Vec<Vec<Placed>> = vec![vec![]];
    let mut x = 0;
    let mut pending_space: Option<Style> = None;
    for a in atoms {
        match a {
            Atom::Break => { lines.push(vec![]); x = 0; pending_space = None }
            Atom::Space(st) => pending_space = Some(st.clone()),
            Atom::Word(w, st) => {
                let pieces = sh.shape(w, st, report);
                let ww: U = pieces.iter().map(|p| p.width).sum();
                let sw = match (&pending_space, lines.last().map(|l| l.is_empty())) {
                    (Some(s), Some(false)) => sh.shape(" ", s, report).iter().map(|p| p.width).sum(),
                    _ => 0,
                };
                if x + sw + ww > avail && !lines.last().expect("line").is_empty() {
                    lines.push(vec![]); x = 0;
                } else { x += sw }
                if ww > avail { report.overflow_lines += 1 }
                lines.last_mut().expect("line").push(Placed { x, st: st.clone(), pieces });
                x += ww;
                pending_space = None;
            }
        }
    }
    let mut first = true;
    for line in lines.into_iter().filter(|l| !l.is_empty()) {
        let (asc, lh) = line.iter().map(|p| sh.metrics(&p.st)).max_by_key(|m| m.1).expect("non-empty");
        let base = y + asc;
        if first {
            if let Some(m) = &marker {
                let st = line[0].st.clone();
                let ps = sh.shape(m, &Style { link: None, rgba: TEXT, ..st.clone() }, report);
                let mw: U = ps.iter().map(|p| p.width).sum();
                let mut mx = left - mw - 8 * PX;
                for p in ps { let w = p.width; scene.run(Run { face: p.face, size: st.size, rgba: TEXT, x: mx, y: base, em: false, glyphs: p.glyphs, text: p.text }); mx += w }
            }
            first = false;
        }
        // Coalesce: consecutive pieces of the same face and style on this
        // line become one run, with the gap (a space) folded into positions.
        let mut cur: Option<Run> = None;
        let mut link_span: Option<(usize, U, U)> = None;
        for p in &line {
            let mut px = left + p.x;
            for piece in &p.pieces {
                let same = cur.as_ref().is_some_and(|r| r.face == piece.face && r.size == p.st.size && r.rgba == p.st.rgba && r.em == p.st.em);
                if !same {
                    if let Some(r) = cur.take() { scene.run(r) }
                    cur = Some(Run { face: piece.face, size: p.st.size, rgba: p.st.rgba, x: px, y: base, em: p.st.em, glyphs: vec![], text: String::new() });
                } else if let Some(r) = cur.as_mut() {
                    // A gap between words inside one run is a space in its text.
                    if !r.text.is_empty() && !r.text.ends_with(' ') && px > run_end(r) { r.text.push(' ') }
                }
                let r = cur.as_mut().expect("run");
                for &(g, dx, dy) in &piece.glyphs { r.glyphs.push((g, px - r.x + dx, dy)) }
                r.text.push_str(&piece.text);
                px += piece.width;
            }
            if let Some(i) = p.st.link {
                match link_span.as_mut() {
                    Some((j, _, x1)) if *j == i => *x1 = px,
                    _ => {
                        if let Some((j, x0, x1)) = link_span.take() { scene.link(Link { x: x0, y, w: x1 - x0, h: lh, href: links[j].clone() }) }
                        link_span = Some((i, left + p.x, px));
                    }
                }
            } else if let Some((j, x0, x1)) = link_span.take() {
                scene.link(Link { x: x0, y, w: x1 - x0, h: lh, href: links[j].clone() });
            }
        }
        if let Some(r) = cur.take() { scene.run(r) }
        if let Some((j, x0, x1)) = link_span.take() { scene.link(Link { x: x0, y, w: x1 - x0, h: lh, href: links[j].clone() }) }
        y += lh;
    }
    y
}

fn run_end(r: &Run) -> U { r.x + r.glyphs.last().map(|g| g.1).unwrap_or(0) }

#[cfg(test)]
mod trig_tests {
    use super::sin_cos_deg;

    /// The values a rotation must get exactly right, and the quadrants.
    #[test]
    fn sin_cos_matches_the_known_angles() {
        let near = |a: f64, b: f64, what: &str| assert!((a - b).abs() < 1e-12, "{what}: {a} vs {b}");
        for (deg, s, c) in [(0.0, 0.0, 1.0), (30.0, 0.5, 0.75f64.sqrt()), (45.0, 0.5f64.sqrt(), 0.5f64.sqrt()),
                            (60.0, 0.75f64.sqrt(), 0.5), (90.0, 1.0, 0.0), (120.0, 0.75f64.sqrt(), -0.5),
                            (180.0, 0.0, -1.0), (270.0, -1.0, 0.0), (360.0, 0.0, 1.0), (-90.0, -1.0, 0.0)] {
            let (gs, gc) = sin_cos_deg(deg);
            near(gs, s, &format!("sin {deg}"));
            near(gc, c, &format!("cos {deg}"));
        }
        // The identity every angle must satisfy, across the whole circle.
        for i in -720..720 {
            let (s, c) = sin_cos_deg(i as f64 / 2.0);
            near(s * s + c * c, 1.0, "s^2 + c^2");
        }
    }
}

/// `outer` applied AFTER `inner`, in the same fixed point.
pub fn compose(outer: Xform, inner: Xform) -> Xform {
    let m = |a: i64, b: i64| scale(a, b, XF_ONE);
    [m(outer[0], inner[0]) + m(outer[2], inner[1]),
     m(outer[1], inner[0]) + m(outer[3], inner[1]),
     m(outer[0], inner[2]) + m(outer[2], inner[3]),
     m(outer[1], inner[2]) + m(outer[3], inner[3]),
     m(outer[0], inner[4]) + m(outer[2], inner[5]) + outer[4],
     m(outer[1], inner[4]) + m(outer[3], inner[5]) + outer[5]]
}
