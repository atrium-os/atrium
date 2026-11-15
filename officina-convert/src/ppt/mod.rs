//! `.ppt` (PowerPoint 97–2003) in (docs/spec/officina.md §16.40, O7d).
//!
//! The binary presentation is read record by record (`rec.rs`), through its
//! edits and persist directory, decrypted when protected (`file.rs`), and
//! translated into PresentationML parts (presentation, theme, one master
//! and layout per main master, slides, notes, pictures) that the `.pptx`
//! reader then reads, so one reader builds the deck and reports what's
//! missing for both formats. Text properties are in `text.rs`.
//!
//! Carried: slide size, masters with their text styles and colour scheme,
//! slides and their shapes (preset geometry, fill and line colour, line
//! width, rotation and flips, groups), placeholders, text (paragraph
//! alignment, indents, spacing, bullets; bold, italic, underline, font,
//! size, colour, super/subscript; numbering; hyperlinks; fields), gradient
//! fills, dashes and arrowheads, freeforms, pictures (PNG and JPEG),
//! backgrounds in a solid colour, speaker notes, hidden slides, and
//! transitions and builds (`anim.rs`).

pub mod anim;
pub mod file;
pub mod rec;
#[doc(hidden)]
pub mod sample;
pub mod text;

use std::collections::HashMap;
use std::fmt::Write as _;

use officina_slides::model::Deck;

use crate::doc::props::esc_s;
use crate::pptx::Loaded;
use crate::report::{Kind, Report};
use file::{Open, PptFile};
use rec::{i16_at, i32_at, ty, u16_at, u32_at, Rec};
use text::{Cf, Col, Pf};

#[derive(Debug, PartialEq, Eq)]
pub enum PptError {
    NeedsPassword,
    WrongPassword,
    Bad(String),
}

impl std::fmt::Display for PptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PptError::NeedsPassword => f.write_str(crate::launch::NEEDS_PASSWORD),
            PptError::WrongPassword => f.write_str(crate::launch::WRONG_PASSWORD),
            PptError::Bad(e) => write!(f, "not a presentation this can read: {e}"),
        }
    }
}

impl std::error::Error for PptError {}

impl From<Open> for PptError {
    fn from(o: Open) -> Self {
        match o {
            Open::NeedsPassword => PptError::NeedsPassword,
            Open::WrongPassword => PptError::WrongPassword,
            Open::Bad(e) => PptError::Bad(e),
        }
    }
}

/// Whether `data` looks like a PowerPoint 97–2003 file.
pub fn is_ppt(data: &[u8]) -> bool {
    crate::cfb::is_cfb(data) && crate::cfb::Cfb::open(data).is_ok_and(|c| c.has("PowerPoint Document"))
}

/// Most slides, shapes in a slide, and group depth.
const MAX_SLIDES: usize = 10_000;
const MAX_SHAPES: usize = 20_000;
const MAX_DEPTH: usize = 16;
/// Most characters in one text.
const MAX_TEXT: usize = 1 << 20;

/// Master units (1/576 in) to EMU.
fn emu(v: i64) -> i64 {
    v * 3175 / 2
}

/// One text as its records give it.
#[derive(Clone, Default)]
struct Tx<'a> {
    text: Vec<u16>,
    style: Option<&'a [u8]>,
    /// Hyperlinks: (from, to, link id).
    links: Vec<(usize, usize, u32)>,
    /// Fields at their marker character: slide number (true) or date.
    fields: Vec<(usize, bool)>,
}

/// The texts of a record list: each `TextHeaderAtom` and the atoms after it.
fn texts<'a>(recs: impl Iterator<Item = Rec<'a>>) -> Vec<Tx<'a>> {
    let mut out: Vec<Tx> = Vec::new();
    let mut link: Option<u32> = None;
    for r in recs {
        match r.ty {
            ty::TEXT_HEADER => {
                if out.len() >= 1 << 14 {
                    break;
                }
                out.push(Tx::default());
            }
            ty::TEXT_CHARS => {
                if let Some(t) = out.last_mut() {
                    t.text = r.body.chunks_exact(2).take(MAX_TEXT).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                }
            }
            ty::TEXT_BYTES => {
                if let Some(t) = out.last_mut() {
                    t.text = r.body.iter().take(MAX_TEXT).map(|&b| b as u16).collect();
                }
            }
            ty::STYLE_TEXT_PROP => {
                if let Some(t) = out.last_mut() {
                    t.style = Some(r.body);
                }
            }
            ty::INTERACTIVE_INFO => {
                link = r.child(ty::INTERACTIVE_INFO_ATOM).filter(|a| a.body.get(8) == Some(&4)).map(|a| u32_at(a.body, 4));
            }
            ty::SLIDE_NUMBER_MC | ty::DATE_TIME_MC | ty::GENERIC_DATE_MC | ty::RTF_DATE_TIME_MC => {
                if let Some(t) = out.last_mut() {
                    if t.fields.len() < 4096 {
                        t.fields.push((u32_at(r.body, 0) as usize, r.ty == ty::SLIDE_NUMBER_MC));
                    }
                }
            }
            ty::TEXT_INTERACTIVE_INFO => {
                if let (Some(t), Some(id)) = (out.last_mut(), link.take()) {
                    let (a, b) = (u32_at(r.body, 0) as usize, u32_at(r.body, 4) as usize);
                    if a < b && t.links.len() < 4096 {
                        t.links.push((a, b, id));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Shape properties (`OfficeArtFOPT`): property id → value and complex data.
#[derive(Clone, Default)]
struct Opts<'a>(HashMap<u16, (u32, &'a [u8])>);

impl<'a> Opts<'a> {
    fn read(r: &Rec<'a>, into: &mut Opts<'a>) {
        let n = r.inst as usize;
        let b = r.body;
        let mut complex = n * 6;
        for i in 0..n.min(b.len() / 6) {
            let id = u16_at(b, i * 6);
            let v = u32_at(b, i * 6 + 2);
            let pid = id & 0x3FFF;
            let data = if id & 0x8000 != 0 {
                let d = b.get(complex..complex.saturating_add(v as usize)).unwrap_or(&[]);
                complex = complex.saturating_add(v as usize);
                d
            } else {
                &[][..]
            };
            into.0.insert(pid, (v, data));
        }
    }

    fn get(&self, pid: u16) -> Option<u32> {
        self.0.get(&pid).map(|x| x.0)
    }

    /// A boolean of a property set (`fUse` bit `16 + bit`, value bit `bit`).
    fn flag(&self, set: u16, bit: u32) -> Option<bool> {
        let v = self.get(set)?;
        (v & (1 << (16 + bit)) != 0).then_some(v & (1 << bit) != 0)
    }

    /// A UTF-16 string property.
    fn string(&self, pid: u16) -> Option<String> {
        let d = self.0.get(&pid)?.1;
        let u: Vec<u16> = d.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).take(1024).collect();
        (!u.is_empty()).then(|| String::from_utf16_lossy(&u))
    }
}

/// What a slide's shapes are read against.
struct Ctx<'a> {
    scheme: [u32; 8],
    /// Outline texts (from the slide list), by `OutlineTextRefAtom` index.
    outline: Vec<Tx<'a>>,
    rels: Vec<String>,
    notes: bool,
    /// Body text sizes by level, from the master.
    sizes: Vec<Option<u16>>,
}

struct Tr<'a> {
    f: &'a PptFile,
    fonts: Vec<String>,
    /// Pictures by BSE index (1-based): the BSE body.
    bse: Vec<&'a [u8]>,
    defaults: Opts<'a>,
    links: HashMap<u32, String>,
    rep: Report,
    media: Vec<(String, Vec<u8>)>,
    /// Picture BSE index → media part, once made.
    media_of: HashMap<usize, Option<String>>,
    shapes: usize,
    next_id: u32,
    loaded_pictures: u64,
    /// This slide's shapes: file shape id → written id.
    spids: HashMap<u32, u32>,
    /// This slide's PowerPoint 97 builds: order, atom, file shape id.
    anims: Vec<(u16, &'a [u8], u32)>,
}

/// The data of a programmable tag named `name` among `r`'s `ProgTags`.
pub(crate) fn tag_data<'a>(r: Rec<'a>, name: &str) -> Option<Rec<'a>> {
    let want: Vec<u16> = name.encode_utf16().collect();
    r.child(0x1388)?.kids().filter(|t| t.ty == 0x138A).find(|t| {
        t.child(ty::CSTRING).is_some_and(|c| c.body.chunks_exact(2).map(|x| u16::from_le_bytes([x[0], x[1]])).eq(want.iter().copied()))
    })?.child(0x138B)
}

/// DrawingML's name for an automatic numbering scheme.
fn auto_num(s: u16) -> &'static str {
    const NAMES: [&str; 19] = ["alphaLcPeriod", "alphaUcPeriod", "arabicParenR", "arabicPeriod", "romanLcParenBoth", "romanLcParenR", "romanLcPeriod", "romanUcPeriod", "alphaLcParenBoth", "alphaLcParenR", "alphaUcParenBoth", "alphaUcParenR", "arabicParenBoth", "arabicPlain", "romanUcParenBoth", "romanUcParenR", "ea1ChsPlain", "ea1ChsPeriod", "circleNumDbPlain"];
    NAMES.get(s as usize).copied().unwrap_or("arabicPeriod")
}

/// Most points in a freeform.
const MAX_POINTS: usize = 1 << 16;

/// The elements of an `IMsoArray` property: (element size, bytes).
fn array<'b>(d: &'b [u8]) -> Option<(usize, usize, &'b [u8])> {
    let n = u16_at(d, 0) as usize;
    let size = match u16_at(d, 4) {
        0xFFF0 => 4,
        s => s as usize,
    };
    let body = d.get(6..)?;
    (size > 0).then(|| (n.min(body.len() / size), size, body))
}

/// A freeform's outline (vertices 0x0145, segments 0x0146, inside the
/// geometry box 0x0140–0x0143) as DrawingML custom geometry.
fn freeform(o: &Opts) -> Option<String> {
    let (n, size, v) = array(o.0.get(&0x0145)?.1)?;
    if n < 2 || !matches!(size, 4 | 8) {
        return None;
    }
    // Points worked out from the shape's formulas (guides) aren't drawn.
    let guided = (0..n).any(|i| if size == 4 { u16_at(v, i * 4) & 0xFF00 == 0x8000 || u16_at(v, i * 4 + 2) & 0xFF00 == 0x8000 } else { u32_at(v, i * 8) & 0xFFFF_FF00 == 0x8000_0000 || u32_at(v, i * 8 + 4) & 0xFFFF_FF00 == 0x8000_0000 });
    if guided {
        return None;
    }
    let pts: Vec<(i64, i64)> = (0..n.min(MAX_POINTS))
        .map(|i| if size == 4 { (i16_at(v, i * 4) as i64, i16_at(v, i * 4 + 2) as i64) } else { (i32_at(v, i * 8) as i64, i32_at(v, i * 8 + 4) as i64) })
        .collect();
    let (l, t) = (o.get(0x0140).unwrap_or(0) as i32 as i64, o.get(0x0141).unwrap_or(0) as i32 as i64);
    let (r, b) = (o.get(0x0142).map_or(21600, |v| v as i32 as i64), o.get(0x0143).map_or(21600, |v| v as i32 as i64));
    let (w, h) = ((r - l).max(1), (b - t).max(1));
    let mut segs: Vec<u16> = Vec::new();
    if let Some((m, 2, sb)) = o.0.get(&0x0146).and_then(|d| array(d.1)) {
        segs = (0..m.min(MAX_POINTS)).map(|i| u16_at(sb, i * 2)).collect();
    }
    if segs.is_empty() {
        segs = vec![0x4000, (pts.len() as u16 - 1).min(0x1FFF), 0x8000];
    }
    let mut out = format!(r#"<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l="0" t="0" r="r" b="b"/><a:pathLst><a:path w="{w}" h="{h}">"#);
    let mut p = 0;
    let pt = |out: &mut String, (x, y): (i64, i64)| {
        let _ = write!(out, r#"<a:pt x="{}" y="{}"/>"#, (x - l).clamp(-w * 4, w * 5), (y - t).clamp(-h * 4, h * 5));
    };
    let mut moved = false;
    for seg in segs {
        let count = (seg & 0x1FFF) as usize;
        match seg >> 13 {
            0 => {
                for _ in 0..count.max(1) {
                    let Some(&q) = pts.get(p) else { break };
                    out.push_str(if moved { "<a:lnTo>" } else { "<a:moveTo>" });
                    pt(&mut out, q);
                    out.push_str(if moved { "</a:lnTo>" } else { "</a:moveTo>" });
                    moved = true;
                    p += 1;
                }
            }
            1 => {
                for _ in 0..count.max(1) {
                    let Some(q) = pts.get(p..p + 3) else { break };
                    if !moved {
                        break;
                    }
                    out.push_str("<a:cubicBezTo>");
                    for &c in q {
                        pt(&mut out, c);
                    }
                    out.push_str("</a:cubicBezTo>");
                    p += 3;
                }
            }
            2 => {
                let Some(&q) = pts.get(p) else { break };
                out.push_str("<a:moveTo>");
                pt(&mut out, q);
                out.push_str("</a:moveTo>");
                moved = true;
                p += 1;
            }
            3 => out.push_str("<a:close/>"),
            4 => break,
            // Escapes: their points are skipped.
            5 | 6 => p += (seg & 0xFF) as usize,
            _ => {}
        }
    }
    out.push_str("</a:path></a:pathLst></a:custGeom>");
    moved.then_some(out)
}

fn rgb_of(v: u32) -> u32 {
    (v & 0xFF) << 16 | (v & 0xFF00) | (v >> 16 & 0xFF)
}

fn hex(c: u32) -> String {
    format!("{:06X}", c & 0xFF_FFFF)
}

/// `msosptXxx` shape types to DrawingML presets.
fn preset(spt: u16) -> Option<&'static str> {
    Some(match spt {
        1 | 75 | 202 => "rect",
        2 => "roundRect",
        3 => "ellipse",
        4 => "diamond",
        5 => "triangle",
        6 => "rtTriangle",
        7 => "parallelogram",
        8 => "trapezoid",
        9 => "hexagon",
        10 => "octagon",
        11 => "plus",
        12 => "star5",
        13 => "rightArrow",
        15 => "homePlate",
        16 => "cube",
        20 => "line",
        21 => "plaque",
        22 => "can",
        23 => "donut",
        32 => "straightConnector1",
        33 => "bentConnector2",
        34 => "bentConnector3",
        37 => "curvedConnector3",
        55 => "chevron",
        56 => "pentagon",
        61 => "wedgeRectCallout",
        62 => "wedgeRoundRectCallout",
        63 => "wedgeEllipseCallout",
        64 => "wave",
        65 => "foldedCorner",
        66 => "leftArrow",
        67 => "downArrow",
        68 => "upArrow",
        69 => "leftRightArrow",
        70 => "upDownArrow",
        71 => "irregularSeal1",
        72 => "irregularSeal2",
        73 => "lightningBolt",
        74 => "heart",
        84 => "bevel",
        92 => "star24",
        96 => "smileyFace",
        183 => "sun",
        184 => "moon",
        187 => "star4",
        188 => "doubleWave",
        _ => return None,
    })
}

/// A placeholder's `p:ph` from `PlaceholderEnum`.
fn ph_type(id: u8) -> Option<&'static str> {
    Some(match id {
        1 | 13 | 17 => "title",
        3 | 15 => "ctrTitle",
        4 | 16 => "subTitle",
        2 | 14 | 18 => "body",
        6 | 12 => "body",
        5 | 11 => "sldImg",
        7 => "dt",
        8 => "sldNum",
        9 => "ftr",
        10 => "hdr",
        19 | 25 => "obj",
        20 => "chart",
        21 => "tbl",
        22 | 26 => "pic",
        23 => "dgm",
        24 => "media",
        _ => return None,
    })
}

impl<'a> Tr<'a> {
    fn col(&self, c: Col, ctx: &Ctx) -> u32 {
        match c {
            Col::Rgb(v) => v,
            Col::Scheme(i) => ctx.scheme[i as usize & 7],
        }
    }

    /// An OfficeArt colour (`OfficeArtCOLORREF`).
    fn art_col(&self, v: u32, ctx: &Ctx) -> Option<u32> {
        let flags = v >> 24;
        if flags & 0x08 != 0 {
            ctx.scheme.get((v & 0xFF) as usize).copied()
        } else if flags & 0x10 != 0 {
            None
        } else {
            Some(rgb_of(v))
        }
    }

    fn font(&self, i: u16) -> Option<&str> {
        self.fonts.get(i as usize).map(|s| s.as_str()).filter(|s| !s.is_empty())
    }

    /// `size`: the text's size in points, when known, to put spacing given
    /// as a share of the line in points.
    fn ppr(&self, pf: &Pf, ctx: &Ctx, tag: &str, lvl: Option<u16>, size: Option<u16>, num: Option<(u16, i16)>, out: &mut String) {
        let mut a = String::new();
        if let Some(l) = lvl.filter(|&l| l > 0) {
            let _ = write!(a, r#" lvl="{}""#, l.min(8));
        }
        if let Some(al) = pf.align {
            let v = match al {
                1 => "ctr",
                2 => "r",
                3 => "just",
                4 => "dist",
                _ => "l",
            };
            let _ = write!(a, r#" algn="{v}""#);
        }
        if let Some(l) = pf.left {
            let _ = write!(a, r#" marL="{}""#, emu(l.max(0) as i64));
        }
        if let (Some(i), Some(l)) = (pf.indent, pf.left) {
            let _ = write!(a, r#" indent="{}""#, emu(i as i64 - l as i64));
        } else if let Some(i) = pf.indent {
            let _ = write!(a, r#" indent="{}""#, emu(i as i64));
        }
        let _ = write!(out, "<{tag}{a}>");
        let spacing = |v: i16| if v >= 0 { format!(r#"<a:spcPct val="{}"/>"#, v as i64 * 1000) } else { format!(r#"<a:spcPts val="{}"/>"#, (-(v as i64)) * 25 / 2) };
        if let Some(v) = pf.line {
            let _ = write!(out, "<a:lnSpc>{}</a:lnSpc>", spacing(v));
        }
        // Space before and after: a share of the line (1.2 × the size) in points.
        let gap = |v: i16| match size {
            Some(sz) if v >= 0 => format!(r#"<a:spcPts val="{}"/>"#, (v as i64 * sz as i64 * 6 / 5).min(158_400)),
            _ => spacing(v),
        };
        if let Some(v) = pf.before {
            let _ = write!(out, "<a:spcBef>{}</a:spcBef>", gap(v));
        }
        if let Some(v) = pf.after {
            let _ = write!(out, "<a:spcAft>{}</a:spcAft>", gap(v));
        }
        match pf.bullet {
            Some(false) => out.push_str("<a:buNone/>"),
            Some(true) => {
                if let Some(c) = pf.bullet_color {
                    let _ = write!(out, r#"<a:buClr><a:srgbClr val="{}"/></a:buClr>"#, hex(self.col(c, ctx)));
                }
                if let Some(s) = pf.bullet_size.filter(|&s| s > 0) {
                    let _ = write!(out, r#"<a:buSzPct val="{}"/>"#, (s as i64).clamp(25, 400) * 1000);
                }
                if let Some(f) = pf.bullet_font.and_then(|f| self.font(f)) {
                    let _ = write!(out, r#"<a:buFont typeface="{}"/>"#, esc_s(f));
                }
                if let Some((scheme, at)) = num {
                    let _ = write!(out, r#"<a:buAutoNum type="{}" startAt="{}"/>"#, auto_num(scheme), at.clamp(1, 32767));
                } else {
                    let ch = pf.bullet_char.unwrap_or('•');
                    let _ = write!(out, r#"<a:buChar char="{}"/>"#, esc_s(&ch.to_string()));
                }
            }
            None => {}
        }
    }

    fn rpr(&self, cf: &Cf, ctx: &Ctx, tag: &str, link: Option<&str>, rels: &mut Vec<String>, out: &mut String) {
        let mut a = String::new();
        if let Some(s) = cf.size {
            let _ = write!(a, r#" sz="{}""#, (s as u32 * 100).clamp(100, 400_000));
        }
        if let Some(b) = cf.bold {
            let _ = write!(a, r#" b="{}""#, b as u8);
        }
        if let Some(i) = cf.italic {
            let _ = write!(a, r#" i="{}""#, i as u8);
        }
        if let Some(u) = cf.underline {
            let _ = write!(a, r#" u="{}""#, if u { "sng" } else { "none" });
        }
        if let Some(p) = cf.position.filter(|&p| p != 0) {
            let _ = write!(a, r#" baseline="{}""#, (p as i64).clamp(-100, 100) * 1000);
        }
        let _ = write!(out, "<{tag}{a}>");
        if let Some(c) = cf.color {
            let _ = write!(out, r#"<a:solidFill><a:srgbClr val="{}"/></a:solidFill>"#, hex(self.col(c, ctx)));
        }
        if let Some(f) = cf.font.and_then(|f| self.font(f)) {
            let _ = write!(out, r#"<a:latin typeface="{}"/>"#, esc_s(f));
        }
        if let Some(l) = link {
            rels.push(format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="{}" TargetMode="External"/>"#, rels.len() + 1, esc_s(l)));
            let _ = write!(out, r#"<a:hlinkClick r:id="rId{}"/>"#, rels.len());
        }
        let _ = write!(out, "</{tag}>");
    }

    /// A text's paragraphs as `a:p`s.
    fn paras(&self, t: &Tx, ctx: &mut Ctx, nums: &[Option<(u16, i16)>], out: &mut String) {
        let n = t.text.len();
        let (pruns, cruns) = t.style.map(|s| text::style_runs(s, n)).unwrap_or_default();
        // Per character: its paragraph run and character run.
        let lookup = |runs: &[usize], at: usize| -> usize {
            let mut acc = 0;
            for (i, &r) in runs.iter().enumerate() {
                acc += r;
                if at < acc {
                    return i;
                }
            }
            runs.len().saturating_sub(1)
        };
        let plens: Vec<usize> = pruns.iter().map(|r| r.0).collect();
        let clens: Vec<usize> = cruns.iter().map(|r| r.0).collect();
        let mut start = 0;
        loop {
            let end = (start..n).find(|&i| t.text[i] == 0x0D).unwrap_or(n);
            let (lvl, pf) = pruns.get(lookup(&plens, start)).map(|r| (r.1, r.2.clone())).unwrap_or_default();
            out.push_str("<a:p>");
            let size = cruns.get(lookup(&clens, start)).and_then(|r| r.1.size).or_else(|| ctx.sizes.get(lvl as usize).copied().flatten());
            let num = cruns.get(lookup(&clens, start)).and_then(|r| r.1.pp9rt).and_then(|i| nums.get(i as usize).copied().flatten());
            self.ppr(&pf, ctx, "a:pPr", Some(lvl), size, num, out);
            out.push_str("</a:pPr>");
            let mut i = start;
            let mut last_cf = cruns.get(lookup(&clens, start)).map(|r| r.1.clone()).unwrap_or_default();
            while i < end {
                let ci = lookup(&clens, i);
                let link = t.links.iter().find(|l| l.0 <= i && i < l.1).map(|l| l.2);
                // A run: same character properties and link.
                let mut j = i + 1;
                while j < end && lookup(&clens, j) == ci && t.links.iter().find(|l| l.0 <= j && j < l.1).map(|l| l.2) == link && t.text[j] != 0x0B {
                    j += 1;
                }
                if t.text[i] == 0x0B {
                    out.push_str("<a:br/>");
                    i += 1;
                    continue;
                }
                if let Some(&(_, num)) = t.fields.iter().find(|f| f.0 == i) {
                    // A field at its marker: the slide number, or the date
                    // (which the file doesn't keep: today's).
                    let cf = cruns.get(ci).map(|r| r.1.clone()).unwrap_or_default();
                    let (kind, shown) = if num { ("slidenum", "‹#›".to_string()) } else { ("datetime1", today()) };
                    let _ = write!(out, r#"<a:fld id="{{B6F15528-21DE-4FAA-801E-634DDDAF4B2B}}" type="{kind}">"#);
                    let mut none = Vec::new();
                    self.rpr(&cf, ctx, "a:rPr", None, &mut none, out);
                    let _ = write!(out, "<a:t>{}</a:t></a:fld>", esc_s(&shown));
                    i += 1;
                    continue;
                }
                if t.fields.iter().any(|f| f.0 > i && f.0 < j) {
                    j = t.fields.iter().map(|f| f.0).filter(|&f| f > i && f < j).min().unwrap_or(j);
                }
                let cf = cruns.get(ci).map(|r| r.1.clone()).unwrap_or_default();
                let s: String = String::from_utf16_lossy(&t.text[i..j]).chars().filter(|c| !c.is_control() || *c == '\t').collect();
                if !s.is_empty() {
                    out.push_str("<a:r>");
                    let url = link.and_then(|id| self.links.get(&id)).map(|s| s.as_str());
                    let mut rels = std::mem::take(&mut ctx.rels);
                    self.rpr(&cf, ctx, "a:rPr", url, &mut rels, out);
                    ctx.rels = rels;
                    let _ = write!(out, "<a:t>{}</a:t></a:r>", esc_s(&s));
                }
                last_cf = cf;
                i = j;
            }
            let mut rels = Vec::new();
            self.rpr(&last_cf, ctx, "a:endParaRPr", None, &mut rels, out);
            out.push_str("</a:p>");
            if end >= n {
                break;
            }
            start = end + 1;
            if start >= n {
                break;
            }
        }
    }

    /// The picture of BSE `pib` as a media part, made once.
    fn picture(&mut self, pib: usize) -> Option<String> {
        if let Some(m) = self.media_of.get(&pib) {
            return m.clone();
        }
        let got = (|| {
            let b = *self.bse.get(pib.checked_sub(1)?)?;
            let name_len = *b.get(33)? as usize;
            let embedded = b.get(36 + name_len..).filter(|e| e.len() >= 8);
            let bytes = match embedded {
                Some(e) => e.to_vec(),
                None => self.f.picture(u32_at(b, 28) as usize)?,
            };
            match crate::doc::find_blip(&bytes, 0) {
                Ok((ext, data)) => Some((ext, data.to_vec())),
                Err(other) => {
                    if other {
                        self.rep.note(Kind::Dropped, "pictures in formats not shown (EMF, WMF, PICT, BMP, TIFF)");
                    }
                    None
                }
            }
        })();
        let m = got.map(|(ext, data)| {
            let name = format!("ppt/media/image{}.{}", self.media.len() + 1, if ext == "jpeg" { "jpeg" } else { ext });
            self.media.push((name.clone(), data));
            name
        });
        self.media_of.insert(pib, m.clone());
        m
    }

    fn id(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    /// The shapes of a group (`SpgrContainer`), its first being the group's own.
    fn group(&mut self, g: Rec<'a>, ctx: &mut Ctx<'a>, depth: usize, out: &mut String) {
        for (i, k) in g.kids().enumerate() {
            if self.shapes >= MAX_SHAPES {
                self.rep.note(Kind::Dropped, "shapes past the slide's limit");
                return;
            }
            match k.ty {
                ty::SP_CONTAINER if i > 0 => self.shape(k, ctx, out),
                ty::SPGR_CONTAINER if depth < MAX_DEPTH => {
                    // A nested group: its first shape holds the group's frame.
                    let Some(first) = k.kids().next().filter(|f| f.ty == ty::SP_CONTAINER) else { continue };
                    let anchor = anchor_of(first);
                    let child = first.child(ty::FSPGR).map(|f| [i32_at(f.body, 0), i32_at(f.body, 4), i32_at(f.body, 8), i32_at(f.body, 12)]);
                    let (Some(a), Some(c)) = (anchor, child) else {
                        self.group(k, ctx, depth + 1, out);
                        continue;
                    };
                    self.shapes += 1;
                    let id = self.id();
                    let _ = write!(
                        out,
                        r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="{id}" name="Group {id}"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/><a:chOff x="{}" y="{}"/><a:chExt cx="{}" cy="{}"/></a:xfrm></p:grpSpPr>"#,
                        emu(a[0] as i64),
                        emu(a[1] as i64),
                        emu((a[2] as i64 - a[0] as i64).max(0)),
                        emu((a[3] as i64 - a[1] as i64).max(0)),
                        emu(c[0] as i64),
                        emu(c[1] as i64),
                        emu((c[2] as i64 - c[0] as i64).max(0)),
                        emu((c[3] as i64 - c[1] as i64).max(0))
                    );
                    self.group(k, ctx, depth + 1, out);
                    out.push_str("</p:grpSp>");
                }
                ty::SPGR_CONTAINER => self.rep.note(Kind::Dropped, "groups nested too deep"),
                _ => {}
            }
        }
    }

    /// One shape (`SpContainer`) as `p:sp` or `p:pic`.
    fn shape(&mut self, sp: Rec<'a>, ctx: &mut Ctx<'a>, out: &mut String) {
        let Some(fsp) = sp.child(ty::FSP) else { return };
        let spt = fsp.inst;
        let flags = u32_at(fsp.body, 4);
        if flags & 0x0408 != 0 {
            // Deleted, or the background.
            return;
        }
        self.shapes += 1;
        let mut o = self.defaults.clone();
        for k in sp.kids() {
            if matches!(k.ty, ty::FOPT | ty::SECONDARY_FOPT | ty::TERTIARY_FOPT) {
                Opts::read(&k, &mut o);
            }
        }
        let Some(a) = anchor_of(sp) else { return };
        let (mut x, mut y, mut w, mut h) = (a[0] as i64, a[1] as i64, (a[2] as i64 - a[0] as i64).max(0), (a[3] as i64 - a[1] as i64).max(0));
        let rot = o.get(0x0004).map(|r| (r as i32) as f64 / 65536.0).unwrap_or(0.0).rem_euclid(360.0);
        if (45.0..135.0).contains(&rot) || (225.0..315.0).contains(&rot) {
            // The anchor of a shape turned near 90° is stored turned.
            let (cx, cy) = (x + w / 2, y + h / 2);
            std::mem::swap(&mut w, &mut h);
            x = cx - w / 2;
            y = cy - h / 2;
        }
        let ph = sp.child(ty::CLIENT_DATA).and_then(|d| d.child(ty::PLACEHOLDER)).map(|p| (u32_at(p.body, 0), p.body.get(4).copied().unwrap_or(0)));
        let tb = sp.child(ty::CLIENT_TEXTBOX);
        let id = self.id();
        let spid = u32_at(fsp.body, 0);
        self.spids.insert(spid, id);
        if let Some(a) = sp.child(ty::CLIENT_DATA).and_then(|d| d.child(anim::ANIMATION_INFO)).and_then(|c| c.child(anim::ANIMATION_INFO_ATOM)) {
            if a.body.len() >= 24 && self.anims.len() < 1024 {
                self.anims.push((u16_at(a.body, 16), a.body, spid));
            }
        }
        let name = o.string(0x0380).unwrap_or_else(|| format!("Shape {id}"));
        let descr = o.string(0x0381).map(|d| format!(r#" descr="{}""#, esc_s(&d))).unwrap_or_default();
        let hidden = if o.flag(0x03BF, 1) == Some(true) { r#" hidden="1""# } else { "" };
        let mut xfrm = String::from("<a:xfrm");
        if rot != 0.0 {
            let _ = write!(xfrm, r#" rot="{}""#, (rot * 60000.0).round() as i64);
        }
        if flags & 0x40 != 0 {
            xfrm.push_str(r#" flipH="1""#);
        }
        if flags & 0x80 != 0 {
            xfrm.push_str(r#" flipV="1""#);
        }
        let _ = write!(xfrm, r#"><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></a:xfrm>"#, emu(x), emu(y), emu(w), emu(h));
        let ph_xml = match ph.and_then(|(idx, t)| ph_type(t).map(|ty| (idx, ty))) {
            Some((idx, "obj")) => format!(r#"<p:ph idx="{idx}"/>"#),
            Some((idx, t)) => format!(r#"<p:ph type="{t}" idx="{idx}"/>"#),
            None => String::new(),
        };
        // A picture.
        let pib = o.get(0x0104).filter(|&p| p > 0 && (spt == 75 || o.get(0x0180) == Some(3) || tb.is_none()));
        if let Some(pib) = pib.filter(|_| spt == 75 || spt == 1) {
            let Some(m) = self.picture(pib as usize) else { return };
            self.loaded_pictures += 1;
            ctx.rels.push(format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/{}"/>"#, ctx.rels.len() + 1, m.trim_start_matches("ppt/media/")));
            let rid = ctx.rels.len();
            // Cropping: 16.16 fractions of each side.
            let crop = [0x0102u16, 0x0100, 0x0103, 0x0101].map(|p| o.get(p).map(|v| ((v as i32) as i64 * 100_000 / 65536).clamp(-100_000, 100_000)).unwrap_or(0));
            let src = if crop.iter().any(|&c| c != 0) { format!(r#"<a:srcRect l="{}" t="{}" r="{}" b="{}"/>"#, crop[0], crop[1], crop[2], crop[3]) } else { String::new() };
            let _ = write!(
                out,
                r#"<p:pic><p:nvPicPr><p:cNvPr id="{id}" name="{}"{descr}{hidden}/><p:cNvPicPr/><p:nvPr>{ph_xml}</p:nvPr></p:nvPicPr><p:blipFill><a:blip r:embed="rId{rid}"/>{src}<a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>{xfrm}<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic>"#,
                esc_s(&name)
            );
            return;
        }
        let mut cust = None;
        let geom = match preset(spt) {
            Some(p) => p,
            None if (136..=175).contains(&spt) => "rect",
            None => {
                cust = freeform(&o);
                if cust.is_none() {
                    if o.get(0x0156).is_some() {
                        self.rep.note(Kind::Approximate, "formula-driven custom shapes drawn as rectangles");
                    } else if spt == 0 || o.get(0x0145).is_some() {
                        self.rep.note(Kind::Approximate, "freeform shapes drawn as rectangles");
                    } else {
                        self.rep.note(Kind::Approximate, "less common shapes drawn as rectangles");
                    }
                }
                "rect"
            }
        };
        let is_line = matches!(geom, "line" | "straightConnector1" | "bentConnector2" | "bentConnector3" | "curvedConnector3");
        let geom_xml = cust.unwrap_or_else(|| format!(r#"<a:prstGeom prst="{geom}"><a:avLst/></a:prstGeom>"#));
        let _ = write!(out, r#"<p:sp><p:nvSpPr><p:cNvPr id="{id}" name="{}"{descr}{hidden}/><p:cNvSpPr/><p:nvPr>{ph_xml}</p:nvPr></p:nvSpPr><p:spPr>{xfrm}{geom_xml}"#, esc_s(&name));
        // Fill and line (placeholders and text boxes usually have neither).
        let filled = o.flag(0x01BF, 4).unwrap_or(ph.is_none() && spt != 202 && !is_line);
        if filled && !is_line {
            match o.get(0x0180).unwrap_or(0) {
                0 => match o.get(0x0181).and_then(|c| self.art_col(c, ctx)) {
                    Some(c) => {
                        let _ = write!(out, r#"<a:solidFill><a:srgbClr val="{}"/></a:solidFill>"#, hex(c));
                    }
                    None => out.push_str(r#"<a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill>"#),
                },
                t @ 4..=8 => {
                    let a = o.get(0x0181).and_then(|c| self.art_col(c, ctx)).unwrap_or(0xFFFFFF);
                    let b = o.get(0x0183).and_then(|c| self.art_col(c, ctx)).unwrap_or(0xFFFFFF);
                    // The focus says where the fill colour sits: at the start
                    // (0), the end (100), or the middle (50).
                    let focus = o.get(0x018C).map_or(0, |f| f as i32);
                    let stops = match focus {
                        f if f >= 75 || f <= -75 => vec![(0, b), (100_000, a)],
                        f if f >= 25 => vec![(0, b), (50_000, a), (100_000, b)],
                        f if f <= -25 => vec![(0, a), (50_000, b), (100_000, a)],
                        _ => vec![(0, a), (100_000, b)],
                    };
                    out.push_str("<a:gradFill><a:gsLst>");
                    for (pos, c) in stops {
                        let _ = write!(out, r#"<a:gs pos="{pos}"><a:srgbClr val="{}"/></a:gs>"#, hex(c));
                    }
                    out.push_str("</a:gsLst>");
                    if matches!(t, 5 | 6) {
                        out.push_str(r#"<a:path path="circle"><a:fillToRect l="50000" t="50000" r="50000" b="50000"/></a:path>"#);
                    } else {
                        // The file's angle (16.16 degrees) is on the page, a
                        // quarter turn from DrawingML's; the shape's own
                        // turn is taken off as the fill turns with it.
                        let deg = o.get(0x018B).map_or(0.0, |v| v as i32 as f64 / 65536.0);
                        let ang = (90.0 - deg - rot).rem_euclid(360.0);
                        let _ = write!(out, r#"<a:lin ang="{}" scaled="0"/>"#, (ang * 60_000.0).round() as i64 % 21_600_000);
                    }
                    out.push_str("</a:gradFill>");
                }
                _ => {
                    self.rep.note(Kind::Approximate, "pattern and texture fills shown in their first colour");
                    let c = o.get(0x0181).and_then(|c| self.art_col(c, ctx)).unwrap_or(0xFFFFFF);
                    let _ = write!(out, r#"<a:solidFill><a:srgbClr val="{}"/></a:solidFill>"#, hex(c));
                }
            }
        } else {
            out.push_str("<a:noFill/>");
        }
        let lined = o.flag(0x01FF, 3).unwrap_or(ph.is_none() && spt != 202);
        if lined {
            let w = o.get(0x01CB).unwrap_or(9525).min(20 * 12700);
            let c = o.get(0x01C0).and_then(|c| self.art_col(c, ctx)).unwrap_or(0);
            let _ = write!(out, r#"<a:ln w="{w}"><a:solidFill><a:srgbClr val="{}"/></a:solidFill>"#, hex(c));
            let dash = ["solid", "sysDash", "sysDot", "sysDashDot", "sysDashDotDot", "dot", "dash", "lgDash", "dashDot", "lgDashDot", "lgDashDotDot"];
            if let Some(d) = o.get(0x01CE).filter(|&d| d > 0) {
                let _ = write!(out, r#"<a:prstDash val="{}"/>"#, dash.get(d as usize).unwrap_or(&"dash"));
            }
            let size = ["sm", "med", "lg"];
            for (tag, kind, wd, len) in [("headEnd", 0x01D0, 0x01D2, 0x01D3), ("tailEnd", 0x01D1, 0x01D4, 0x01D5)] {
                let Some(k) = o.get(kind).filter(|&k| (1..=5).contains(&k)) else { continue };
                let name = ["", "triangle", "stealth", "diamond", "oval", "arrow"][k as usize];
                let sz = |p: u16| size[o.get(p).unwrap_or(1).min(2) as usize];
                let _ = write!(out, r#"<a:{tag} type="{name}" w="{}" len="{}"/>"#, sz(wd), sz(len));
            }
            out.push_str("</a:ln>");
        } else {
            out.push_str("<a:ln><a:noFill/></a:ln>");
        }
        out.push_str("</p:spPr>");
        // Text: in the shape, or in the slide list for a placeholder.
        let wordart = (136..=175).contains(&spt).then(|| o.string(0x00C0)).flatten();
        if wordart.is_some() {
            self.rep.note(Kind::Approximate, "WordArt shown as plain text");
        }
        let tx = wordart.map(|w| Tx { text: w.encode_utf16().collect(), ..Default::default() }).or_else(|| tb.and_then(|t| {
            if let Some(r) = t.child(ty::OUTLINE_TEXT_REF) {
                ctx.outline.get(u32_at(r.body, 0) as usize).cloned()
            } else {
                texts(t.kids()).into_iter().next()
            }
        }));
        if let Some(t) = tx {
            let ins = |p: u16, d: i64| o.get(p).map(|v| v as i64).unwrap_or(d);
            let anchor = match o.get(0x0087).unwrap_or(0) {
                1 | 6 => r#" anchor="ctr""#,
                2 | 7 => r#" anchor="b""#,
                _ => "",
            };
            let wrap = if o.get(0x0085) == Some(2) { r#" wrap="none""# } else { "" };
            let _ = write!(out, r#"<p:txBody><a:bodyPr lIns="{}" tIns="{}" rIns="{}" bIns="{}"{anchor}{wrap}/><a:lstStyle/>"#, ins(0x0081, 91440), ins(0x0082, 45720), ins(0x0083, 91440), ins(0x0084, 45720));
            let mut body = String::new();
            // Numbering (PowerPoint 2000 on) is in the shape's `___PPT9` tag.
            let nums = sp.child(ty::CLIENT_DATA).and_then(|d| tag_data(d, "___PPT9")).and_then(|b| rec::recs(b.body).find(|r| r.ty == 0x0FAC)).map(|a| text::style9(a.body)).unwrap_or_default();
            self.paras(&t, ctx, &nums, &mut body);
            out.push_str(&body);
            out.push_str("</p:txBody>");
        }
        out.push_str("</p:sp>");
    }
}

/// Today's date as M/D/YYYY (a `.ppt` date field keeps only its format).
fn today() -> String {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() / 86400).unwrap_or(0) as i64;
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{m}/{d}/{y}")
}

/// A shape's frame: the client anchor (8 or 16 bytes) or child anchor, as
/// left, top, right, bottom.
fn anchor_of(sp: Rec) -> Option<[i32; 4]> {
    if let Some(a) = sp.child(ty::CHILD_ANCHOR).filter(|a| a.body.len() >= 16) {
        return Some([i32_at(a.body, 0), i32_at(a.body, 4), i32_at(a.body, 8), i32_at(a.body, 12)]);
    }
    let a = sp.child(ty::CLIENT_ANCHOR)?;
    if a.body.len() >= 16 {
        // top, left, right, bottom
        Some([i32_at(a.body, 4), i32_at(a.body, 0), i32_at(a.body, 8), i32_at(a.body, 12)])
    } else if a.body.len() >= 8 {
        Some([i16_at(a.body, 2) as i32, i16_at(a.body, 0) as i32, i16_at(a.body, 4) as i32, i16_at(a.body, 6) as i32])
    } else {
        None
    }
}

/// A slide's or master's colour scheme, else `fallback`.
fn scheme_of(c: &Rec, fallback: [u32; 8]) -> [u32; 8] {
    match c.kids().find(|k| k.ty == ty::COLOR_SCHEME && k.inst == 1).filter(|k| k.body.len() >= 32) {
        Some(s) => std::array::from_fn(|i| rgb_of(u32_at(s.body, i * 4))),
        None => fallback,
    }
}

const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn rels_xml(rels: &[String]) -> Vec<u8> {
    format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}</Relationships>"#, rels.concat()).into_bytes()
}

/// The background colour a slide's drawing gives (its background shape).
fn background(tr: &Tr, drawing: Option<Rec>, ctx: &Ctx) -> Option<u32> {
    let dg = drawing?.find(ty::DG_CONTAINER, 1)?;
    for sp in dg.kids().filter(|k| k.ty == ty::SP_CONTAINER) {
        let fsp = sp.child(ty::FSP)?;
        if u32_at(fsp.body, 4) & 0x400 == 0 {
            continue;
        }
        let mut o = Opts::default();
        if let Some(f) = sp.child(ty::FOPT) {
            Opts::read(&f, &mut o);
        }
        if o.get(0x0180).unwrap_or(0) == 0 {
            return o.get(0x0181).and_then(|c| tr.art_col(c, ctx));
        }
        return o.get(0x0181).and_then(|c| tr.art_col(c, ctx));
    }
    None
}

fn bg_xml(c: Option<u32>) -> String {
    c.map(|c| format!(r#"<p:bg><p:bgPr><a:solidFill><a:srgbClr val="{}"/></a:solidFill><a:effectLst/></p:bgPr></p:bg>"#, hex(c))).unwrap_or_default()
}

fn tree_start() -> &'static str {
    r#"<p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>"#
}

/// The PresentationML package for a `.ppt`: (part name, bytes).
fn translate(data: &[u8], password: Option<&str>, rep: &mut Report) -> Result<Vec<(String, Vec<u8>)>, PptError> {
    if !crate::cfb::is_cfb(data) {
        let head = String::from_utf8_lossy(&data[..data.len().min(512)]).to_ascii_lowercase();
        return Err(PptError::Bad(if head.contains("<html") || head.contains("<?xml") { "this .ppt is really an HTML or XML file, which isn't supported".into() } else { "not a PowerPoint 97–2003 presentation".into() }));
    }
    let f = PptFile::open(data, password)?;
    let doc = f.obj(f.doc_ref).filter(|d| d.ty == ty::DOCUMENT).ok_or_else(|| PptError::Bad("no document record".into()))?;
    let mut tr = Tr { f: &f, fonts: Vec::new(), bse: Vec::new(), defaults: Opts::default(), links: HashMap::new(), rep: Report::default(), media: Vec::new(), media_of: HashMap::new(), shapes: 0, next_id: 1, loaded_pictures: 0, spids: HashMap::new(), anims: Vec::new() };
    // Slide size (master units).
    let (sw, sh) = doc.child(ty::DOCUMENT_ATOM).map(|a| (i32_at(a.body, 0), i32_at(a.body, 4))).unwrap_or((5760, 4320));
    let (sw, sh) = (emu(sw.clamp(576, 576 * 56) as i64), emu(sh.clamp(576, 576 * 56) as i64));
    // Fonts.
    if let Some(fc) = doc.child(ty::ENVIRONMENT).and_then(|e| e.child(ty::FONT_COLLECTION)) {
        let mut fonts: Vec<(u16, String)> = fc.kids().filter(|k| k.ty == ty::FONT_ENTITY).take(4096).map(|k| (k.inst, String::from_utf16_lossy(&k.body.chunks_exact(2).take(32).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect::<Vec<_>>()))).collect();
        fonts.sort_by_key(|x| x.0);
        for (i, name) in fonts {
            let i = i as usize;
            if i < 4096 {
                if tr.fonts.len() <= i {
                    tr.fonts.resize(i + 1, String::new());
                }
                tr.fonts[i] = name;
            }
        }
    }
    // Pictures and drawing defaults.
    if let Some(dgg) = doc.child(ty::PP_DRAWING_GROUP).and_then(|g| g.child(ty::DGG_CONTAINER)) {
        if let Some(bs) = dgg.child(ty::BSTORE) {
            tr.bse = bs.kids().take(1 << 16).map(|b| if b.ty == ty::BSE { b.body } else { &[][..] }).collect();
        }
        if let Some(o) = dgg.child(ty::FOPT) {
            let mut d = Opts::default();
            Opts::read(&o, &mut d);
            tr.defaults = d;
        }
    }
    // Hyperlinks.
    if let Some(l) = doc.child(ty::EX_OBJ_LIST) {
        for h in l.kids().filter(|k| k.ty == ty::EX_HYPERLINK).take(1 << 14) {
            let Some(id) = h.child(ty::EX_HYPERLINK_ATOM).map(|a| u32_at(a.body, 0)) else { continue };
            let target = h.kids().find(|k| k.ty == ty::CSTRING && k.inst == 1).map(|k| String::from_utf16_lossy(&k.body.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()));
            if let Some(t) = target.filter(|t| !t.is_empty() && t.len() < 8192) {
                tr.links.insert(id, t);
            }
        }
    }
    // The slide lists: masters (1), slides (0), notes (2), each entry with
    // the texts that follow it.
    let mut lists: [Vec<(Rec, Vec<Tx>)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for l in doc.kids().filter(|k| k.ty == ty::SLIDE_LIST_WITH_TEXT) {
        let i = (l.inst as usize).min(2);
        let mut cur: Option<(Rec, Vec<Rec>)> = None;
        for k in l.kids() {
            if k.ty == ty::SLIDE_PERSIST_ATOM {
                if let Some((a, rs)) = cur.take() {
                    lists[i].push((a, texts(rs.into_iter())));
                }
                if lists[i].len() >= MAX_SLIDES {
                    break;
                }
                cur = Some((k, Vec::new()));
            } else if let Some((_, rs)) = cur.as_mut() {
                if rs.len() < 1 << 16 {
                    rs.push(k);
                }
            }
        }
        if let Some((a, rs)) = cur.take() {
            lists[i].push((a, texts(rs.into_iter())));
        }
    }
    let default_scheme = [0xFFFFFF, 0x000000, 0x808080, 0x000000, 0xBBE0E3, 0x333399, 0x009999, 0x99CC00];
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    // Masters: main masters become a master and a layout each; title
    // masters point at theirs.
    let mut master_of: HashMap<u32, usize> = HashMap::new();
    let mut title_masters: Vec<(u32, u32)> = Vec::new();
    let mut masters = 0usize;
    let mut master_schemes: Vec<[u32; 8]> = Vec::new();
    let mut master_sizes: Vec<Vec<Option<u16>>> = Vec::new();
    for (a, _) in &lists[1] {
        let id = u32_at(a.body, 12);
        let Some(m) = f.obj(u32_at(a.body, 0)) else { continue };
        if m.ty == ty::SLIDE {
            let to = m.child(ty::SLIDE_ATOM).map(|s| u32_at(s.body, 12)).unwrap_or(0);
            title_masters.push((id, to));
            continue;
        }
        if m.ty != ty::MAIN_MASTER || masters >= 64 {
            continue;
        }
        masters += 1;
        let n = masters;
        master_of.insert(id, n);
        let scheme = scheme_of(&m, default_scheme);
        master_schemes.push(scheme);
        let mut ctx = Ctx { scheme, outline: Vec::new(), rels: Vec::new(), notes: false, sizes: Vec::new() };
        ctx.rels.push(format!(r#"<Relationship Id="rId1" Type="{REL}/slideLayout" Target="../slideLayouts/slideLayout{n}.xml"/>"#));
        // One theme, the first master's: colours are written out where they're used.
        if n == 1 {
            ctx.rels.push(format!(r#"<Relationship Id="rId2" Type="{REL}/theme" Target="../theme/theme1.xml"/>"#));
        }
        let mut tree = String::new();
        if let Some(sg) = m.child(ty::PP_DRAWING).and_then(|d| d.find(ty::SPGR_CONTAINER, 1)) {
            tr.group(sg, &mut ctx, 0, &mut tree);
        }
        let bg = background(&tr, m.child(ty::PP_DRAWING), &ctx).or(Some(scheme[0]));
        // Text styles by level: title (0), body (1), other (4).
        let mut styles = String::new();
        let mut body_sizes: Vec<Option<u16>> = Vec::new();
        for (tag, inst) in [("p:titleStyle", 0u16), ("p:bodyStyle", 1), ("p:otherStyle", 4)] {
            let levels = m.kids().find(|k| k.ty == ty::TEXT_MASTER_STYLE && k.inst == inst).map(|k| text::master_style(k.body, inst)).unwrap_or_default();
            let _ = write!(styles, "<{tag}>");
            let mut prev: (Pf, Cf) = Default::default();
            let mut sizes = Vec::new();
            for (l, (pf, cf)) in levels.iter().enumerate() {
                let pf = pf.over(&prev.0);
                let cf = cf.over(&prev.1);
                tr.ppr(&pf, &ctx, &format!("a:lvl{}pPr", l + 1), None, cf.size, None, &mut styles);
                let mut none = Vec::new();
                tr.rpr(&cf, &ctx, "a:defRPr", None, &mut none, &mut styles);
                let _ = write!(styles, "</a:lvl{}pPr>", l + 1);
                sizes.push(cf.size);
                prev = (pf, cf);
            }
            if inst == 1 {
                body_sizes = sizes;
            }
            let _ = write!(styles, "</{tag}>");
        }
        master_sizes.push(body_sizes);
        let x = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldMaster {NS}><p:cSld>{}{}{tree}</p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="{}" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles>{styles}</p:txStyles></p:sldMaster>"#,
            bg_xml(bg),
            tree_start(),
            2147483648u64 + n as u64
        );
        parts.push((format!("ppt/slideMasters/slideMaster{n}.xml"), x.into_bytes()));
        parts.push((format!("ppt/slideMasters/_rels/slideMaster{n}.xml.rels"), rels_xml(&ctx.rels)));
        parts.push((format!("ppt/slideLayouts/slideLayout{n}.xml"), format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sldLayout {NS} type="blank"><p:cSld name="Layout {n}">{}</p:spTree></p:cSld></p:sldLayout>"#, tree_start()).into_bytes()));
        parts.push((format!("ppt/slideLayouts/_rels/slideLayout{n}.xml.rels"), rels_xml(&[format!(r#"<Relationship Id="rId1" Type="{REL}/slideMaster" Target="../slideMasters/slideMaster{n}.xml"/>"#)])));
        // The theme: the first master's scheme.
        if n > 1 {
            continue;
        }
        let s = scheme;
        let fonts = tr.fonts.first().cloned().filter(|f| !f.is_empty()).unwrap_or_else(|| "Arial".into());
        let theme = format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Theme {n}"><a:themeElements><a:clrScheme name="Scheme {n}"><a:dk1><a:srgbClr val="{}"/></a:dk1><a:lt1><a:srgbClr val="{}"/></a:lt1><a:dk2><a:srgbClr val="{}"/></a:dk2><a:lt2><a:srgbClr val="{}"/></a:lt2><a:accent1><a:srgbClr val="{}"/></a:accent1><a:accent2><a:srgbClr val="{}"/></a:accent2><a:accent3><a:srgbClr val="{}"/></a:accent3><a:accent4><a:srgbClr val="{}"/></a:accent4><a:accent5><a:srgbClr val="{}"/></a:accent5><a:accent6><a:srgbClr val="{}"/></a:accent6><a:hlink><a:srgbClr val="{}"/></a:hlink><a:folHlink><a:srgbClr val="{}"/></a:folHlink></a:clrScheme><a:fontScheme name="Fonts"><a:majorFont><a:latin typeface="{f}"/></a:majorFont><a:minorFont><a:latin typeface="{f}"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#,
            hex(s[1]),
            hex(s[0]),
            hex(s[3]),
            hex(s[2]),
            hex(s[4]),
            hex(s[5]),
            hex(s[6]),
            hex(s[7]),
            hex(s[4]),
            hex(s[5]),
            hex(s[6]),
            hex(s[7]),
            f = esc_s(&fonts)
        );
        parts.push((format!("ppt/theme/theme{n}.xml"), theme.into_bytes()));
    }
    if masters == 0 {
        rep.note(Kind::Approximate, "a deck without masters or layouts given the default ones");
    }
    for (id, to) in title_masters {
        if let Some(&m) = master_of.get(&to) {
            master_of.insert(id, m);
        }
    }
    // Notes by their slide's notes id.
    let notes: HashMap<u32, Rec> = lists[2].iter().filter_map(|(a, _)| Some((u32_at(a.body, 12), f.obj(u32_at(a.body, 0)).filter(|n| n.ty == ty::NOTES)?))).collect();
    // Slides.
    let mut slide_ids = String::new();
    let mut pres_rels: Vec<String> = Vec::new();
    for m in 1..=masters {
        pres_rels.push(format!(r#"<Relationship Id="rIdM{m}" Type="{REL}/slideMaster" Target="slideMasters/slideMaster{m}.xml"/>"#));
    }
    let mut n = 0usize;
    for (a, outline) in &lists[0] {
        let Some(s) = f.obj(u32_at(a.body, 0)).filter(|s| s.ty == ty::SLIDE) else {
            tr.rep.note(Kind::Dropped, "slides the file names but doesn't hold");
            continue;
        };
        n += 1;
        tr.shapes = 0;
        let atom = s.child(ty::SLIDE_ATOM);
        let master = atom.and_then(|x| master_of.get(&u32_at(x.body, 12))).copied().unwrap_or(1);
        let mscheme = master_schemes.get(master.saturating_sub(1)).copied().unwrap_or(default_scheme);
        let scheme = scheme_of(&s, mscheme);
        let mut ctx = Ctx { scheme, outline: outline.clone(), rels: Vec::new(), notes: false, sizes: master_sizes.get(master.saturating_sub(1)).cloned().unwrap_or_default() };
        if masters > 0 {
            ctx.rels.push(format!(r#"<Relationship Id="rId1" Type="{REL}/slideLayout" Target="../slideLayouts/slideLayout{master}.xml"/>"#));
        }
        let mut tree = String::new();
        tr.spids.clear();
        tr.anims.clear();
        if let Some(sg) = s.child(ty::PP_DRAWING).and_then(|d| d.find(ty::SPGR_CONTAINER, 1)) {
            tr.group(sg, &mut ctx, 0, &mut tree);
        }
        if s.find(0x2EE0, 4).is_some() {
            tr.rep.note(Kind::Dropped, "slide comments");
        }
        let follow_bg = atom.is_some_and(|x| u16_at(x.body, 20) & 0x4 != 0);
        let bg = if follow_bg { None } else { background(&tr, s.child(ty::PP_DRAWING), &ctx) };
        let info = s.child(ty::SLIDE_SHOW_SLIDE_INFO).map(|i| i.body);
        let hidden = info.is_some_and(|b| u16_at(b, 10) & 0x4 != 0);
        let show = if hidden { r#" show="0""# } else { "" };
        let trans = info.and_then(|b| anim::transition(b, &mut tr.rep)).map(|t| crate::pptx::write::transition(&t)).unwrap_or_default();
        let mut anims = std::mem::take(&mut tr.anims);
        let timing = anim::timing(s, &tr.spids, &mut anims, &mut tr.rep);
        let x = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sld {NS}{show}><p:cSld>{}{}{tree}</p:spTree></p:cSld>{trans}{timing}</p:sld>"#, bg_xml(bg), tree_start());
        // Notes.
        if let Some(nt) = atom.and_then(|x| notes.get(&u32_at(x.body, 16))) {
            let mut nctx = Ctx { scheme, outline: Vec::new(), rels: Vec::new(), notes: true, sizes: Vec::new() };
            let mut ntree = String::new();
            if let Some(sg) = nt.child(ty::PP_DRAWING).and_then(|d| d.find(ty::SPGR_CONTAINER, 1)) {
                tr.group(sg, &mut nctx, 0, &mut ntree);
            }
            debug_assert!(nctx.notes);
            parts.push((format!("ppt/notesSlides/notesSlide{n}.xml"), format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:notes {NS}><p:cSld>{}{ntree}</p:spTree></p:cSld></p:notes>"#, tree_start()).into_bytes()));
            parts.push((format!("ppt/notesSlides/_rels/notesSlide{n}.xml.rels"), rels_xml(&nctx.rels)));
            ctx.rels.push(format!(r#"<Relationship Id="rId{}" Type="{REL}/notesSlide" Target="../notesSlides/notesSlide{n}.xml"/>"#, ctx.rels.len() + 1));
        }
        parts.push((format!("ppt/slides/slide{n}.xml"), x.into_bytes()));
        parts.push((format!("ppt/slides/_rels/slide{n}.xml.rels"), rels_xml(&ctx.rels)));
        pres_rels.push(format!(r#"<Relationship Id="rIdS{n}" Type="{REL}/slide" Target="slides/slide{n}.xml"/>"#));
        let _ = write!(slide_ids, r#"<p:sldId id="{}" r:id="rIdS{n}"/>"#, 255 + n);
    }
    let masters_xml: String = (1..=masters).map(|m| format!(r#"<p:sldMasterId id="{}" r:id="rIdM{m}"/>"#, 2147483647u64 + 1000 + m as u64)).collect();
    let pres = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:presentation {NS}><p:sldMasterIdLst>{masters_xml}</p:sldMasterIdLst><p:sldIdLst>{slide_ids}</p:sldIdLst><p:sldSz cx="{sw}" cy="{sh}"/><p:notesSz cx="{sh}" cy="{sw}"/></p:presentation>"#);
    parts.push(("ppt/presentation.xml".into(), pres.into_bytes()));
    parts.push(("ppt/_rels/presentation.xml.rels".into(), rels_xml(&pres_rels)));
    parts.push(("_rels/.rels".into(), rels_xml(&[format!(r#"<Relationship Id="rId1" Type="{REL}/officeDocument" Target="ppt/presentation.xml"/>"#)])));
    parts.extend(std::mem::take(&mut tr.media));
    rep.merge(std::mem::take(&mut tr.rep));
    Ok(parts)
}

/// Reads a `.ppt` (with `password` when it is protected) into a deck.
pub fn read(data: &[u8], password: Option<&str>) -> Result<(Deck, Loaded), PptError> {
    let mut rep = Report::default();
    let parts = translate(data, password, &mut rep)?;
    let mut z = crate::zip::ZipWriter::new(Vec::new());
    for (n, b) in &parts {
        z.add_stored(n, b).map_err(|e| PptError::Bad(e.to_string()))?;
    }
    drop(parts);
    let pkg = z.finish().map_err(|e| PptError::Bad(e.to_string()))?;
    let (deck, mut loaded) = crate::pptx::read(&pkg).map_err(|e| PptError::Bad(e.to_string()))?;
    rep.merge(std::mem::take(&mut loaded.report));
    loaded.report = rep;
    Ok((deck, loaded))
}

/// The PresentationML parts a `.ppt` turns into (for tools and tests).
#[doc(hidden)]
pub fn to_pptx_parts(data: &[u8], password: Option<&str>) -> Result<Vec<(String, Vec<u8>)>, PptError> {
    translate(data, password, &mut Report::default())
}
