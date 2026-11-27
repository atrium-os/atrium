//! Character, paragraph, table and section properties from their sprms
//! (MS-DOC §2.6), and the WordprocessingML that says the same.

use super::bin::{sprms, Sprm};

/// XML-escaped text: `&`, `<`, `>`, `"`, and characters XML can't hold
/// dropped.
pub fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
}

pub fn esc_s(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    esc(s, &mut o);
    o
}

/// Word's 16 colours by index (`Ico`).
pub fn ico(i: u8) -> Option<u32> {
    Some(match i {
        1 => 0x000000,
        2 => 0x0000FF,
        3 => 0x00FFFF,
        4 => 0x00FF00,
        5 => 0xFF00FF,
        6 => 0xFF0000,
        7 => 0xFFFF00,
        8 => 0xFFFFFF,
        9 => 0x000080,
        10 => 0x008080,
        11 => 0x008000,
        12 => 0x800080,
        13 => 0x800000,
        14 => 0x808000,
        15 => 0x808080,
        16 => 0xC0C0C0,
        _ => return None,
    })
}

fn highlight_name(i: u8) -> Option<&'static str> {
    Some(match i {
        1 => "black",
        2 => "blue",
        3 => "cyan",
        4 => "green",
        5 => "magenta",
        6 => "red",
        7 => "yellow",
        8 => "white",
        9 => "darkBlue",
        10 => "darkCyan",
        11 => "darkGreen",
        12 => "darkMagenta",
        13 => "darkRed",
        14 => "darkYellow",
        15 => "darkGray",
        16 => "lightGray",
        _ => return None,
    })
}

/// Character properties a run sets (`None`: as its style).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Chp {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub strike: Option<bool>,
    pub dstrike: Option<bool>,
    pub caps: Option<bool>,
    pub small_caps: Option<bool>,
    pub vanish: Option<bool>,
    pub underline: Option<bool>,
    pub hps: Option<u16>,
    pub color: Option<u32>,
    pub highlight: Option<u8>,
    pub font: Option<u16>,
    /// 1 superscript, 2 subscript, 0 baseline.
    pub iss: Option<u8>,
    pub istd: Option<u16>,
    /// The run's characters are special (pictures, note marks, fields' marks).
    pub spec: bool,
    pub pic: Option<u32>,
    pub data: bool,
    pub ole: bool,
    pub ins: bool,
    pub del: bool,
    pub author: u16,
    /// The deletion's author (sprmCIbstRMarkDel).
    pub del_author: u16,
    /// When inserted and when deleted (DTTMs).
    pub dttm: u32,
    pub del_dttm: u32,
    /// A tracked format change: its author and time (sprmCPropRMark).
    pub prop_rmark: Option<(u16, u32)>,
    pub symbol: Option<(u16, u16)>,
    /// Outline, shadow, emboss, imprint (bits 0 to 3), as set.
    pub effects: Option<u8>,
    /// Letter spacing, twips (sprmCDxaSpace).
    pub spacing: Option<i16>,
    /// Letter width, percent (sprmCCharScale).
    pub scale: Option<u16>,
    pub shading: bool,
}

fn toggle(s: &Sprm, base: Option<bool>) -> Option<bool> {
    match s.u8() {
        0 => Some(false),
        1 => Some(true),
        0x81 => Some(!base.unwrap_or(false)),
        _ => None,
    }
}

impl Chp {
    /// Applies `grpprl` over `self`; `base` is the style's own, for the
    /// sprms that toggle against it.
    pub fn apply(&mut self, grpprl: &[u8], base: &Chp) {
        for s in sprms(grpprl) {
            match s.op {
                0x0800 => self.del = s.u8() != 0,
                0x0801 => self.ins = s.u8() != 0,
                0x6A03 => self.pic = Some(s.u32()),
                0x4804 => self.author = s.u16(),
                0x4863 => self.del_author = s.u16(),
                0x6805 => self.dttm = s.u32(),
                0x6864 => self.del_dttm = s.u32(),
                0xCA89 | 0xCA57 => {
                    let a = s.arg;
                    self.prop_rmark = (a.len() >= 7 && a[0] != 0).then(|| (u16::from_le_bytes([a[1], a[2]]), u32::from_le_bytes([a[3], a[4], a[5], a[6]])));
                }
                0x0806 => self.data = s.u8() != 0,
                0x080A => self.ole = s.u8() != 0,
                0x2A0C => self.highlight = Some(s.u8()),
                0x0835 => self.bold = toggle(&s, base.bold),
                0x0836 => self.italic = toggle(&s, base.italic),
                0x0837 => self.strike = toggle(&s, base.strike),
                0x2A53 => self.dstrike = toggle(&s, base.dstrike),
                0x083A => self.small_caps = toggle(&s, base.small_caps),
                0x083B => self.caps = toggle(&s, base.caps),
                0x083C => self.vanish = toggle(&s, base.vanish),
                0x0838 | 0x0839 | 0x0858 | 0x0854 => {
                    let bit = match s.op {
                        0x0838 => 1,
                        0x0839 => 2,
                        0x0858 => 4,
                        _ => 8,
                    };
                    let was = self.effects.or(base.effects).unwrap_or(0);
                    let on = match s.u8() {
                        0 => false,
                        1 => true,
                        0x81 => base.effects.unwrap_or(0) & bit == 0,
                        _ => continue,
                    };
                    self.effects = Some(if on { was | bit } else { was & !bit });
                }
                0x8840 => self.spacing = Some((s.u16() as i16).clamp(-1584, 1584)),
                0x4852 => self.scale = Some(s.u16().clamp(1, 600)),
                0x2A3E => self.underline = Some(s.u8() != 0),
                0x2A42 => {
                    self.color = ico(s.u8());
                }
                0x6870 => {
                    let v = s.u32();
                    self.color = if v & 0xFF00_0000 != 0 { None } else { Some((v & 0xFF) << 16 | (v & 0xFF00) | (v >> 16 & 0xFF)) };
                }
                0x4A43 => self.hps = Some(s.u16().clamp(2, 3276)),
                0x2A48 => self.iss = Some(s.u8()),
                0x4A4F => self.font = Some(s.u16()),
                0x4A30 => self.istd = Some(s.u16()),
                0x0855 => self.spec = s.u8() != 0,
                0x6A09 => self.symbol = Some((s.u16(), u16::from_le_bytes([*s.arg.get(2).unwrap_or(&0), *s.arg.get(3).unwrap_or(&0)]))),
                0x4866 | 0xCA71 => self.shading = true,
                0x2A33 => {
                    // sprmCPlain: back to the style's.
                    let keep = (self.spec, self.pic, self.data, self.ole, self.ins, self.del, self.author, self.del_author, self.dttm, self.del_dttm, self.prop_rmark);
                    *self = Chp::default();
                    (self.spec, self.pic, self.data, self.ole, self.ins, self.del, self.author, self.del_author, self.dttm, self.del_dttm, self.prop_rmark) = keep;
                }
                _ => {}
            }
        }
    }

    /// `under` with what `self` sets laid over it.
    pub fn over(&self, under: &Chp) -> Chp {
        Chp {
            bold: self.bold.or(under.bold),
            italic: self.italic.or(under.italic),
            strike: self.strike.or(under.strike),
            dstrike: self.dstrike.or(under.dstrike),
            caps: self.caps.or(under.caps),
            small_caps: self.small_caps.or(under.small_caps),
            vanish: self.vanish.or(under.vanish),
            underline: self.underline.or(under.underline),
            hps: self.hps.or(under.hps),
            color: self.color.or(under.color),
            highlight: self.highlight.or(under.highlight),
            font: self.font.or(under.font),
            iss: self.iss.or(under.iss),
            istd: self.istd.or(under.istd),
            effects: self.effects.or(under.effects),
            spacing: self.spacing.or(under.spacing),
            scale: self.scale.or(under.scale),
            ..self.clone()
        }
    }

    /// The `w:rPr` (without the element when empty).
    pub fn xml(&self, fonts: &[String], out: &mut String) {
        let mut s = String::new();
        if let Some(i) = self.istd.filter(|&i| i != 10) {
            s.push_str(&format!(r#"<w:rStyle w:val="S{i}"/>"#));
        }
        if let Some(f) = self.font.and_then(|f| fonts.get(f as usize)).filter(|f| !f.is_empty()) {
            let f = esc_s(f);
            s.push_str(&format!(r#"<w:rFonts w:ascii="{f}" w:hAnsi="{f}"/>"#));
        }
        let flag = |s: &mut String, name: &str, v: Option<bool>| {
            if let Some(v) = v {
                s.push_str(&format!(r#"<w:{name} w:val="{}"/>"#, if v { 1 } else { 0 }));
            }
        };
        flag(&mut s, "b", self.bold);
        flag(&mut s, "i", self.italic);
        flag(&mut s, "caps", self.caps.filter(|&v| v));
        flag(&mut s, "smallCaps", self.small_caps.filter(|&v| v));
        flag(&mut s, "strike", self.strike);
        flag(&mut s, "dstrike", self.dstrike.filter(|&v| v));
        if let Some(fx) = self.effects {
            for (bit, name) in [(1, "outline"), (2, "shadow"), (4, "emboss"), (8, "imprint")] {
                flag(&mut s, name, Some(fx & bit != 0));
            }
        }
        flag(&mut s, "vanish", self.vanish.filter(|&v| v));
        if let Some(c) = self.color {
            s.push_str(&format!(r#"<w:color w:val="{c:06X}"/>"#));
        }
        if let Some(v) = self.spacing {
            s.push_str(&format!(r#"<w:spacing w:val="{v}"/>"#));
        }
        if let Some(v) = self.scale {
            s.push_str(&format!(r#"<w:w w:val="{v}"/>"#));
        }
        if let Some(h) = self.hps {
            s.push_str(&format!(r#"<w:sz w:val="{h}"/>"#));
        }
        if let Some(n) = self.highlight.and_then(highlight_name) {
            s.push_str(&format!(r#"<w:highlight w:val="{n}"/>"#));
        }
        if let Some(u) = self.underline {
            s.push_str(if u { r#"<w:u w:val="single"/>"# } else { r#"<w:u w:val="none"/>"# });
        }
        if let Some(v) = self.iss {
            s.push_str(match v {
                1 => r#"<w:vertAlign w:val="superscript"/>"#,
                2 => r#"<w:vertAlign w:val="subscript"/>"#,
                _ => r#"<w:vertAlign w:val="baseline"/>"#,
            });
        }
        if !s.is_empty() {
            out.push_str("<w:rPr>");
            out.push_str(&s);
            out.push_str("</w:rPr>");
        }
    }
}

/// A tab stop: position (twips) and `TBD` (alignment, leader).
pub type Tab = (i16, u8);

/// Paragraph properties a paragraph or style sets.
#[derive(Debug, Clone, Default)]
pub struct Pap {
    pub jc: Option<u8>,
    pub left: Option<i16>,
    pub right: Option<i16>,
    pub first: Option<i16>,
    pub before: Option<u16>,
    pub after: Option<u16>,
    pub line: Option<(i16, bool)>,
    pub keep: Option<bool>,
    pub keep_next: Option<bool>,
    pub page_break: Option<bool>,
    pub widow: Option<bool>,
    pub outline: Option<u8>,
    pub ilfo: Option<u16>,
    pub ilvl: Option<u8>,
    pub in_table: bool,
    pub ttp: bool,
    pub itap: u32,
    pub inner_cell: bool,
    pub inner_ttp: bool,
    pub tabs_add: Vec<Tab>,
    pub tabs_del: Vec<i16>,
    pub frame: bool,
    /// Border sides as `w:pBdr` children: top, left, bottom, right, between.
    pub borders: [Option<String>; 5],
    /// The shading's colour (`w:shd`'s fill), or none set to clear it.
    pub shading: Option<Option<u32>>,
    /// A dropped capital: 1 in the text, 2 in the margin; its lines.
    pub drop_cap: Option<(u8, u8)>,
    /// A frame's distance from the text beside it (twips).
    pub from_text: Option<i16>,
    pub no_line_numbers: Option<bool>,
    pub no_hyphenation: Option<bool>,
}

/// A border (`BRC80`, 4 bytes, or `BRC`, 8) as a `w:pBdr` side named `name`;
/// none when it is no border.
fn brc_xml(name: &str, arg: &[u8]) -> Option<String> {
    let (dpt, ty, color, space) = match arg.len() {
        4.. if arg.len() < 8 => {
            if arg[..4] == [0xFF; 4] {
                return None;
            }
            (arg[0], arg[1], ico(arg[2]), arg[3] & 0x1F)
        }
        8.. => {
            let auto = arg[3] == 0xFF;
            (arg[4], arg[5], (!auto).then(|| (arg[0] as u32) << 16 | (arg[1] as u32) << 8 | arg[2] as u32), arg[6] & 0x1F)
        }
        _ => return None,
    };
    let val = match ty {
        0 | 255 => return None,
        2 => "thick",
        3 => "double",
        6 => "dotted",
        7 => "dashed",
        8 => "dotDash",
        9 => "dotDotDash",
        10 => "triple",
        11..=19 => "thinThickSmallGap",
        21 => "doubleWave",
        22 => "dashSmallGap",
        _ => "single",
    };
    let color = color.map_or("auto".to_string(), |c| format!("{c:06X}"));
    Some(format!(r#"<w:{name} w:val="{val}" w:sz="{}" w:space="{space}" w:color="{color}"/>"#, dpt.max(1)))
}

/// A COLORREF's colour (none when automatic).
fn colorref(b: &[u8]) -> Option<u32> {
    match b {
        [r, g, bl, a, ..] if *a != 0xFF => Some((*r as u32) << 16 | (*g as u32) << 8 | *bl as u32),
        _ => None,
    }
}

fn tabs(arg: &[u8], add: &mut Vec<Tab>, del: &mut Vec<i16>, close: bool) {
    let mut p = 0;
    let n = *arg.first().unwrap_or(&0) as usize;
    p += 1;
    for k in 0..n {
        if let Some(b) = arg.get(p + 2 * k..p + 2 * k + 2) {
            del.push(i16::from_le_bytes([b[0], b[1]]));
        }
    }
    p += n * 2 * if close { 2 } else { 1 };
    let m = *arg.get(p).unwrap_or(&0) as usize;
    p += 1;
    for k in 0..m {
        let (Some(b), Some(&t)) = (arg.get(p + 2 * k..p + 2 * k + 2), arg.get(p + 2 * m + k)) else { break };
        add.push((i16::from_le_bytes([b[0], b[1]]), t));
    }
}

impl Pap {
    pub fn apply(&mut self, grpprl: &[u8]) {
        for s in sprms(grpprl) {
            match s.op {
                0x2403 | 0x2461 => self.jc = Some(s.u8()),
                0x840F | 0x845E => self.left = Some(s.i16()),
                0x840E | 0x845D => self.right = Some(s.i16()),
                0x8411 | 0x8460 => self.first = Some(s.i16()),
                0x6412 => self.line = Some((s.i16(), i16::from_le_bytes([*s.arg.get(2).unwrap_or(&0), *s.arg.get(3).unwrap_or(&0)]) != 0)),
                0xA413 => self.before = Some(s.u16()),
                0xA414 => self.after = Some(s.u16()),
                0x2405 => self.keep = Some(s.u8() != 0),
                0x2406 => self.keep_next = Some(s.u8() != 0),
                0x2407 => self.page_break = Some(s.u8() != 0),
                0x2431 => self.widow = Some(s.u8() != 0),
                0x2640 => self.outline = Some(s.u8()),
                0x460B => self.ilfo = Some(s.u16()),
                0x260A => self.ilvl = Some(s.u8()),
                0x2416 => self.in_table = s.u8() != 0,
                0x2417 => self.ttp = s.u8() != 0,
                0x6649 => self.itap = s.u32(),
                0x244B => self.inner_cell = s.u8() != 0,
                0x244C => self.inner_ttp = s.u8() != 0,
                0xC60D => tabs(s.arg, &mut self.tabs_add, &mut self.tabs_del, false),
                0xC615 => {
                    let arg = if s.arg.first() == Some(&255) { &s.arg[1..] } else { s.arg };
                    tabs(arg, &mut self.tabs_add, &mut self.tabs_del, s.arg.first() == Some(&255));
                }
                0x842F => {
                    self.from_text = Some(s.i16());
                    self.frame = true;
                }
                0x8418 | 0x8419 | 0x261B | 0x2423 | 0x442B | 0x8430 => self.frame = true,
                0x6424 | 0xC64E => self.borders[0] = brc_xml("top", s.arg),
                0x6425 | 0xC64F => self.borders[1] = brc_xml("left", s.arg),
                0x6426 | 0xC650 => self.borders[2] = brc_xml("bottom", s.arg),
                0x6427 | 0xC651 => self.borders[3] = brc_xml("right", s.arg),
                0x6428 | 0xC652 => self.borders[4] = brc_xml("between", s.arg),
                // SHD80: foreground and background colour indexes, pattern.
                0x442D => {
                    let v = s.u16();
                    let (fore, back, pat) = ((v & 0x1F) as u8, ((v >> 5) & 0x1F) as u8, (v >> 10) as u8);
                    self.shading = Some(if pat == 1 { ico(fore).or(Some(0)) } else { ico(back) });
                }
                // SHD: foreground and background COLORREFs, pattern.
                0xC64D if s.arg.len() >= 10 => {
                    let pat = u16::from_le_bytes([s.arg[8], s.arg[9]]);
                    self.shading = Some(if pat == 1 { colorref(&s.arg[0..4]).or(Some(0)) } else { colorref(&s.arg[4..8]) });
                }
                0x442C => {
                    let v = s.u16();
                    let (kind, lines) = ((v & 7) as u8, ((v >> 3) & 0x1F) as u8);
                    self.drop_cap = (kind == 1 || kind == 2).then_some((kind, lines));
                }
                0x240C => self.no_line_numbers = Some(s.u8() != 0),
                0x242A => self.no_hyphenation = Some(s.u8() != 0),
                _ => {}
            }
        }
    }

    /// The `w:pPr` children (the style and anything beyond these added by
    /// the caller).
    pub fn xml(&self, out: &mut String) {
        if self.keep_next == Some(true) {
            out.push_str("<w:keepNext/>");
        }
        if self.keep == Some(true) {
            out.push_str("<w:keepLines/>");
        }
        match self.drop_cap {
            Some((kind, lines)) => out.push_str(&format!(r#"<w:framePr w:dropCap="{}" w:lines="{}" w:hSpace="{}"/>"#, if kind == 2 { "margin" } else { "drop" }, lines.max(1), self.from_text.unwrap_or(0).max(0))),
            None if self.frame => out.push_str("<w:framePr/>"),
            None => {}
        }
        if let Some(v) = self.no_line_numbers {
            out.push_str(&format!(r#"<w:suppressLineNumbers w:val="{}"/>"#, v as u8));
        }
        if self.borders.iter().any(Option::is_some) {
            out.push_str("<w:pBdr>");
            for b in self.borders.iter().flatten() {
                out.push_str(b);
            }
            out.push_str("</w:pBdr>");
        }
        match self.shading {
            Some(Some(c)) => out.push_str(&format!(r#"<w:shd w:val="clear" w:color="auto" w:fill="{c:06X}"/>"#)),
            Some(None) => out.push_str(r#"<w:shd w:val="clear" w:color="auto" w:fill="auto"/>"#),
            None => {}
        }
        if let Some(v) = self.no_hyphenation {
            out.push_str(&format!(r#"<w:suppressAutoHyphens w:val="{}"/>"#, v as u8));
        }
        if let Some(v) = self.page_break {
            out.push_str(&format!(r#"<w:pageBreakBefore w:val="{}"/>"#, v as u8));
        }
        if let Some(v) = self.widow {
            out.push_str(&format!(r#"<w:widowControl w:val="{}"/>"#, v as u8));
        }
        if !self.tabs_add.is_empty() || !self.tabs_del.is_empty() {
            out.push_str("<w:tabs>");
            for d in &self.tabs_del {
                out.push_str(&format!(r#"<w:tab w:val="clear" w:pos="{d}"/>"#));
            }
            for &(pos, tbd) in &self.tabs_add {
                let jc = match tbd & 7 {
                    1 => "center",
                    2 => "right",
                    3 => "decimal",
                    4 => "bar",
                    _ => "left",
                };
                let leader = match (tbd >> 3) & 7 {
                    1 => "dot",
                    2 => "hyphen",
                    3 => "underscore",
                    4 => "heavy",
                    5 => "middleDot",
                    _ => "none",
                };
                out.push_str(&format!(r#"<w:tab w:val="{jc}" w:leader="{leader}" w:pos="{pos}"/>"#));
            }
            out.push_str("</w:tabs>");
        }
        if self.before.is_some() || self.after.is_some() || self.line.is_some() {
            out.push_str("<w:spacing");
            if let Some(b) = self.before {
                out.push_str(&format!(r#" w:before="{b}""#));
            }
            if let Some(a) = self.after {
                out.push_str(&format!(r#" w:after="{a}""#));
            }
            if let Some((l, mult)) = self.line {
                if mult {
                    out.push_str(&format!(r#" w:line="{}" w:lineRule="auto""#, l.max(1)));
                } else if l < 0 {
                    out.push_str(&format!(r#" w:line="{}" w:lineRule="exact""#, -(l as i32)));
                } else {
                    out.push_str(&format!(r#" w:line="{l}" w:lineRule="atLeast""#));
                }
            }
            out.push_str("/>");
        }
        if self.left.is_some() || self.right.is_some() || self.first.is_some() {
            out.push_str("<w:ind");
            if let Some(l) = self.left {
                out.push_str(&format!(r#" w:left="{l}""#));
            }
            if let Some(r) = self.right {
                out.push_str(&format!(r#" w:right="{r}""#));
            }
            if let Some(f) = self.first {
                if f < 0 {
                    out.push_str(&format!(r#" w:hanging="{}""#, -(f as i32)));
                } else {
                    out.push_str(&format!(r#" w:firstLine="{f}""#));
                }
            }
            out.push_str("/>");
        }
        if let Some(j) = self.jc {
            out.push_str(match j {
                1 => r#"<w:jc w:val="center"/>"#,
                2 => r#"<w:jc w:val="right"/>"#,
                3 | 4 => r#"<w:jc w:val="both"/>"#,
                _ => r#"<w:jc w:val="left"/>"#,
            });
        }
        if let Some(o) = self.outline.filter(|&o| o < 9) {
            out.push_str(&format!(r#"<w:outlineLvl w:val="{o}"/>"#));
        }
    }
}

/// A table row's properties (from its row-end paragraph).
#[derive(Debug, Clone, Default)]
pub struct Tap {
    /// Cell boundaries, twips from the left.
    pub centers: Vec<i16>,
    /// Per cell: merged into the one before; merged down (continues).
    pub merged: Vec<bool>,
    pub vmerge: Vec<u8>,
    /// Per cell: its borders as `w:tcBorders` children, background, and
    /// vertical alignment (0 top, 1 centre, 2 bottom).
    pub borders: Vec<String>,
    pub shading: Vec<Option<u32>>,
    pub valign: Vec<u8>,
    /// The table's borders as `w:tblBorders` children.
    pub table_borders: Option<String>,
    pub header: bool,
    pub cant_split: bool,
    /// Twips; negative is exact.
    pub height: i16,
    /// 0 left, 1 centre, 2 right.
    pub jc: u8,
}

/// Borders from a run of Brc (`size` bytes each) named in order.
/// Without `nil`, a side with none is left out (the table's shows).
fn brcs_xml(arg: &[u8], size: usize, names: &[&str], nil: bool) -> String {
    let mut s = String::new();
    for (i, name) in names.iter().enumerate() {
        match arg.get(i * size..i * size + size).and_then(|b| brc_xml(name, b)) {
            Some(x) => s += &x,
            None if nil && arg.len() >= (i + 1) * size => s += &format!(r#"<w:{name} w:val="nil"/>"#),
            None => {}
        }
    }
    s
}

impl Tap {
    pub fn read(grpprl: &[u8]) -> Tap {
        let mut t = Tap::default();
        for s in sprms(grpprl) {
            match s.op {
                0xD608 | 0xD606 => {
                    let a = s.arg;
                    let n = (*a.first().unwrap_or(&0) as usize).min(64);
                    if a.len() < 1 + 2 * (n + 1) {
                        continue;
                    }
                    t.centers = (0..=n).map(|i| i16::from_le_bytes([a[1 + 2 * i], a[2 + 2 * i]])).collect();
                    let tcs = &a[1 + 2 * (n + 1)..];
                    t.merged = vec![false; n];
                    t.vmerge = vec![0; n];
                    t.borders = vec![String::new(); n];
                    t.shading = vec![None; n];
                    t.valign = vec![0; n];
                    for i in 0..n {
                        let Some(tc) = tcs.get(i * 20..i * 20 + 2) else { break };
                        let f = u16::from_le_bytes([tc[0], tc[1]]);
                        t.merged[i] = f & 0x0002 != 0;
                        t.valign[i] = ((f >> 7) & 3).min(2) as u8;
                        if let Some(brc) = tcs.get(i * 20 + 4..i * 20 + 20) {
                            t.borders[i] = brcs_xml(brc, 4, &["top", "left", "bottom", "right"], false);
                        }
                        t.vmerge[i] = match (f & 0x0020 != 0, f & 0x0040 != 0) {
                            (true, true) => 3,
                            (true, false) => 1,
                            _ => 0,
                        };
                    }
                }
                // Table borders (Brc80 then Brc): top, left, bottom, right,
                // inside rules.
                0xD605 => t.table_borders = Some(brcs_xml(s.arg, 4, &["top", "left", "bottom", "right", "insideH", "insideV"], true)),
                0xD613 => t.table_borders = Some(brcs_xml(s.arg, 8, &["top", "left", "bottom", "right", "insideH", "insideV"], true)),
                // Cells' backgrounds: a Shd each (foreground, background, pattern).
                0xD612 => {
                    for (i, shd) in s.arg.chunks_exact(10).enumerate().take(64) {
                        if t.shading.len() <= i {
                            t.shading.resize(i + 1, None);
                        }
                        t.shading[i] = colorref(&shd[4..8]);
                    }
                }
                0xD62C => {
                    let (a, b, v) = (s.u8() as usize, *s.arg.get(1).unwrap_or(&0) as usize, *s.arg.get(2).unwrap_or(&0));
                    for i in a..b.min(t.valign.len()) {
                        t.valign[i] = v.min(2);
                    }
                }
                0x3404 => t.header = s.u8() != 0,
                0x3403 | 0x3644 => t.cant_split = s.u8() != 0,
                0x9407 => t.height = i16::from_le_bytes([*s.arg.first().unwrap_or(&0), *s.arg.get(1).unwrap_or(&0)]),
                0x5400 | 0x548A => t.jc = (s.u8()).min(2),
                0xD62B => {
                    let (i, v) = (s.u8() as usize, *s.arg.get(1).unwrap_or(&0));
                    if let Some(m) = t.vmerge.get_mut(i) {
                        *m = v;
                    }
                }
                0x5624 => {
                    let (a, b) = (s.u8() as usize, *s.arg.get(1).unwrap_or(&0) as usize);
                    for i in a + 1..b.min(t.merged.len()) {
                        t.merged[i] = true;
                    }
                }
                _ => {}
            }
        }
        t
    }
}

/// A section's page and columns.
#[derive(Debug, Clone)]
pub struct Sep {
    pub bkc: u8,
    pub width: u16,
    pub height: u16,
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
    pub columns: u16,
    pub gap: u16,
    pub rule: bool,
    pub title_page: bool,
    /// Line numbers: every how many (0: none), distance, restart, first less one.
    pub lnn_mod: u16,
    pub lnn_dxa: i16,
    pub lnc: u8,
    pub lnn_min: u16,
}

impl Default for Sep {
    fn default() -> Self {
        Sep { bkc: 2, width: 12240, height: 15840, left: 1800, right: 1800, top: 1440, bottom: 1440, columns: 1, gap: 720, rule: false, title_page: false, lnn_mod: 0, lnn_dxa: 0, lnc: 0, lnn_min: 0 }
    }
}

impl Sep {
    pub fn read(grpprl: &[u8]) -> Sep {
        let mut p = Sep::default();
        for s in sprms(grpprl) {
            match s.op {
                0x3009 => p.bkc = s.u8(),
                0xB01F => p.width = s.u16(),
                0xB020 => p.height = s.u16(),
                0xB021 => p.left = s.u16() as i32,
                0xB022 => p.right = s.u16() as i32,
                0x9023 => p.top = (s.i16() as i32).abs(),
                0x9024 => p.bottom = (s.i16() as i32).abs(),
                0x500B => p.columns = s.u16().saturating_add(1).min(16),
                0x900C => p.gap = s.u16(),
                0x3019 => p.rule = s.u8() != 0,
                0x300A => p.title_page = s.u8() != 0,
                0x5015 => p.lnn_mod = s.u16(),
                0x9016 => p.lnn_dxa = s.i16(),
                0x3013 => p.lnc = s.u8(),
                0x501B => p.lnn_min = s.u16(),
                _ => {}
            }
        }
        p
    }

    /// The `w:sectPr` children.
    pub fn xml(&self, out: &mut String) {
        let ty = match self.bkc {
            0 => "continuous",
            1 => "nextColumn",
            3 => "evenPage",
            4 => "oddPage",
            _ => "nextPage",
        };
        out.push_str(&format!(r#"<w:type w:val="{ty}"/><w:pgSz w:w="{}" w:h="{}"/><w:pgMar w:top="{}" w:right="{}" w:bottom="{}" w:left="{}"/>"#, self.width.max(1440), self.height.max(1440), self.top, self.right, self.bottom, self.left));
        if self.lnn_mod > 0 {
            let restart = match self.lnc {
                1 => "newSection",
                2 => "continuous",
                _ => "newPage",
            };
            out.push_str(&format!(r#"<w:lnNumType w:countBy="{}" w:start="{}" w:distance="{}" w:restart="{restart}"/>"#, self.lnn_mod, self.lnn_min, self.lnn_dxa.max(0)));
        }
        if self.columns > 1 {
            out.push_str(&format!(r#"<w:cols w:num="{}" w:space="{}" w:sep="{}"/>"#, self.columns, self.gap, self.rule as u8));
        }
        if self.title_page {
            out.push_str("<w:titlePg/>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_sprms_and_toggles() {
        let mut c = Chp::default();
        // Bold on, size 24 half-points, red by COLORREF, superscript.
        c.apply(&[0x35, 0x08, 1, 0x43, 0x4A, 24, 0, 0x70, 0x68, 0xFF, 0, 0, 0, 0x48, 0x2A, 1], &Chp::default());
        assert_eq!((c.bold, c.hps, c.color, c.iss), (Some(true), Some(24), Some(0xFF0000), Some(1)));
        // Italic "opposite of the style" over an italic style.
        let base = Chp { italic: Some(true), ..Chp::default() };
        let mut d = Chp::default();
        d.apply(&[0x36, 0x08, 0x81], &base);
        assert_eq!(d.italic, Some(false));
        let mut x = String::new();
        c.xml(&[], &mut x);
        assert!(x.contains(r#"<w:b w:val="1"/>"#) && x.contains(r#"<w:color w:val="FF0000"/>"#) && x.contains("superscript"), "{x}");
    }

    #[test]
    fn paragraph_sprms() {
        let mut p = Pap::default();
        // Centred, left indent 720, hanging 360, 12 pt after, exact 14 pt lines.
        p.apply(&[0x61, 0x24, 1, 0x5E, 0x84, 0xD0, 2, 0x60, 0x84, 0x98, 0xFE, 0x14, 0xA4, 240, 0, 0x12, 0x64, 0xE8, 0xFE, 0, 0]);
        let mut x = String::new();
        p.xml(&mut x);
        assert!(x.contains(r#"w:left="720""#) && x.contains(r#"w:hanging="360""#) && x.contains(r#"w:after="240""#) && x.contains(r#"w:line="280" w:lineRule="exact""#) && x.contains("center"), "{x}");
    }

    #[test]
    fn border_shading_and_drop_cap_sprms() {
        let mut p = Pap::default();
        // A top BRC80 (1 pt double, red, 4 pt away), a bottom BRC (blue
        // COLORREF, single), SHD80 back colour yellow, a 3-line drop cap,
        // no line numbers.
        p.apply(&[0x24, 0x64, 8, 3, 6, 4, 0x50, 0xC6, 8, 0, 0, 0xFF, 0, 4, 1, 2, 0, 0x2D, 0x44, 7 << 5, 0, 0x2C, 0x44, 1 | 3 << 3, 0, 0x0C, 0x24, 1]);
        let mut x = String::new();
        p.xml(&mut x);
        assert!(x.contains(r#"<w:top w:val="double" w:sz="8" w:space="4" w:color="FF0000"/>"#), "{x}");
        assert!(x.contains(r#"<w:bottom w:val="single" w:sz="4" w:space="2" w:color="0000FF"/>"#), "{x}");
        assert!(x.contains(r#"w:fill="FFFF00""#) && x.contains(r#"w:dropCap="drop" w:lines="3""#) && x.contains("suppressLineNumbers"), "{x}");
    }
}
