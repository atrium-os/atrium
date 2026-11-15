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
    pub effects: bool,
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
                0x0838 | 0x0839 | 0x0858 | 0x0854 => self.effects = s.u8() == 1 || self.effects,
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
        flag(&mut s, "vanish", self.vanish.filter(|&v| v));
        if self.effects {
            s.push_str("<w:outline/>");
        }
        if let Some(c) = self.color {
            s.push_str(&format!(r#"<w:color w:val="{c:06X}"/>"#));
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
    pub borders: bool,
    pub shading: bool,
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
                0x8418 | 0x8419 | 0x261B | 0x2423 | 0x442B | 0x842F | 0x8430 => self.frame = true,
                0x6424 | 0x6425 | 0x6426 | 0x6427 | 0xC64E | 0xC64F | 0xC650 | 0xC651 => self.borders = true,
                0x442D | 0xC64D => self.shading = true,
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
        if self.borders {
            out.push_str("<w:pBdr/>");
        }
        if self.shading {
            out.push_str("<w:shd/>");
        }
        if self.frame {
            out.push_str("<w:framePr/>");
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
                    for i in 0..n {
                        let Some(tc) = tcs.get(i * 20..i * 20 + 2) else { break };
                        let f = u16::from_le_bytes([tc[0], tc[1]]);
                        t.merged[i] = f & 0x0002 != 0;
                        t.vmerge[i] = match (f & 0x0020 != 0, f & 0x0040 != 0) {
                            (true, true) => 3,
                            (true, false) => 1,
                            _ => 0,
                        };
                    }
                }
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
}

impl Default for Sep {
    fn default() -> Self {
        Sep { bkc: 2, width: 12240, height: 15840, left: 1800, right: 1800, top: 1440, bottom: 1440, columns: 1, gap: 720, rule: false, title_page: false }
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
}
