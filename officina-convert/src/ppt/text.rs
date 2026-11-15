//! Text properties (MS-PPT §2.9): paragraph and character exceptions, as
//! runs over a text (`StyleTextPropAtom`) or by indent level in a master
//! (`TextMasterStyleAtom`).

use super::rec::{i16_at, u16_at, u32_at};

/// A colour as text properties give it: RGB or a slot of the colour scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Rgb(u32),
    Scheme(u8),
}

fn col(b: &[u8], at: usize) -> Option<Col> {
    let (r, g, bl, i) = (*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?);
    match i {
        0xFE => Some(Col::Rgb((r as u32) << 16 | (g as u32) << 8 | bl as u32)),
        0..=7 => Some(Col::Scheme(i)),
        _ => None,
    }
}

/// Paragraph properties; `None` is "as the master says".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pf {
    pub bullet: Option<bool>,
    pub bullet_char: Option<char>,
    pub bullet_font: Option<u16>,
    pub bullet_size: Option<i16>,
    pub bullet_color: Option<Col>,
    /// 0 left, 1 centre, 2 right, 3 justify, 4 distributed.
    pub align: Option<u16>,
    /// Positive: per cent of a line; negative: master units (1/576 in).
    pub line: Option<i16>,
    pub before: Option<i16>,
    pub after: Option<i16>,
    pub left: Option<i16>,
    pub indent: Option<i16>,
}

/// Character properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cf {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub font: Option<u16>,
    /// Points.
    pub size: Option<u16>,
    pub color: Option<Col>,
    /// Per cent up (superscript) or down.
    pub position: Option<i16>,
    /// Which entry of the shape's `StyleTextProp9Atom` applies.
    pub pp9rt: Option<u8>,
}

impl Pf {
    pub fn over(&self, under: &Pf) -> Pf {
        Pf {
            bullet: self.bullet.or(under.bullet),
            bullet_char: self.bullet_char.or(under.bullet_char),
            bullet_font: self.bullet_font.or(under.bullet_font),
            bullet_size: self.bullet_size.or(under.bullet_size),
            bullet_color: self.bullet_color.or(under.bullet_color),
            align: self.align.or(under.align),
            line: self.line.or(under.line),
            before: self.before.or(under.before),
            after: self.after.or(under.after),
            left: self.left.or(under.left),
            indent: self.indent.or(under.indent),
        }
    }
}

impl Cf {
    pub fn over(&self, under: &Cf) -> Cf {
        Cf {
            bold: self.bold.or(under.bold),
            italic: self.italic.or(under.italic),
            underline: self.underline.or(under.underline),
            font: self.font.or(under.font),
            size: self.size.or(under.size),
            color: self.color.or(under.color),
            position: self.position.or(under.position),
            pp9rt: self.pp9rt.or(under.pp9rt),
        }
    }
}

/// A `TextPFException` at `p`: the properties and the bytes it took.
pub fn pf(b: &[u8], mut p: usize) -> Option<(Pf, usize)> {
    let start = p;
    let m = u32_at(b, p);
    if p + 4 > b.len() {
        return None;
    }
    p += 4;
    let mut f = Pf::default();
    let take = |n: usize, p: &mut usize| -> Option<usize> {
        let at = *p;
        if at + n > b.len() {
            return None;
        }
        *p += n;
        Some(at)
    };
    if m & 0xF != 0 {
        let at = take(2, &mut p)?;
        let fl = u16_at(b, at);
        if m & 1 != 0 {
            f.bullet = Some(fl & 1 != 0);
        }
    }
    if m & (1 << 7) != 0 {
        let at = take(2, &mut p)?;
        f.bullet_char = char::from_u32(u16_at(b, at) as u32).filter(|c| !c.is_control());
    }
    if m & (1 << 4) != 0 {
        let at = take(2, &mut p)?;
        f.bullet_font = Some(u16_at(b, at));
    }
    if m & (1 << 6) != 0 {
        let at = take(2, &mut p)?;
        f.bullet_size = Some(i16_at(b, at));
    }
    if m & (1 << 5) != 0 {
        let at = take(4, &mut p)?;
        f.bullet_color = col(b, at);
    }
    if m & (1 << 11) != 0 {
        let at = take(2, &mut p)?;
        f.align = Some(u16_at(b, at));
    }
    if m & (1 << 12) != 0 {
        let at = take(2, &mut p)?;
        f.line = Some(i16_at(b, at));
    }
    if m & (1 << 13) != 0 {
        let at = take(2, &mut p)?;
        f.before = Some(i16_at(b, at));
    }
    if m & (1 << 14) != 0 {
        let at = take(2, &mut p)?;
        f.after = Some(i16_at(b, at));
    }
    if m & (1 << 8) != 0 {
        let at = take(2, &mut p)?;
        f.left = Some(i16_at(b, at));
    }
    if m & (1 << 10) != 0 {
        let at = take(2, &mut p)?;
        f.indent = Some(i16_at(b, at));
    }
    if m & (1 << 15) != 0 {
        take(2, &mut p)?;
    }
    if m & (1 << 20) != 0 {
        let at = take(2, &mut p)?;
        let n = u16_at(b, at) as usize;
        take(n * 4, &mut p)?;
    }
    if m & (1 << 16) != 0 {
        take(2, &mut p)?;
    }
    if m & (7 << 17) != 0 {
        take(2, &mut p)?;
    }
    if m & (1 << 21) != 0 {
        take(2, &mut p)?;
    }
    Some((f, p - start))
}

/// A `TextCFException` at `p`: the properties and the bytes it took.
pub fn cf(b: &[u8], mut p: usize) -> Option<(Cf, usize)> {
    let start = p;
    if p + 4 > b.len() {
        return None;
    }
    let m = u32_at(b, p);
    p += 4;
    let mut f = Cf::default();
    let take = |n: usize, p: &mut usize| -> Option<usize> {
        let at = *p;
        if at + n > b.len() {
            return None;
        }
        *p += n;
        Some(at)
    };
    if m & 0xFFFF != 0 {
        let at = take(2, &mut p)?;
        let s = u16_at(b, at);
        if m & 1 != 0 {
            f.bold = Some(s & 1 != 0);
        }
        if m & 2 != 0 {
            f.italic = Some(s & 2 != 0);
        }
        if m & 4 != 0 {
            f.underline = Some(s & 4 != 0);
        }
        if m & 0x3C00 != 0 {
            f.pp9rt = Some((s >> 10 & 0xF) as u8);
        }
    }
    if m & (1 << 16) != 0 {
        let at = take(2, &mut p)?;
        f.font = Some(u16_at(b, at));
    }
    for bit in [21, 22, 23] {
        if m & (1 << bit) != 0 {
            take(2, &mut p)?;
        }
    }
    if m & (1 << 17) != 0 {
        let at = take(2, &mut p)?;
        f.size = Some(u16_at(b, at)).filter(|&s| (1..=4000).contains(&s));
    }
    if m & (1 << 18) != 0 {
        let at = take(4, &mut p)?;
        f.color = col(b, at);
    }
    if m & (1 << 19) != 0 {
        let at = take(2, &mut p)?;
        f.position = Some(i16_at(b, at));
    }
    if m & (1 << 20) != 0 {
        take(4, &mut p)?;
    }
    for bit in [24, 25] {
        if m & (1 << bit) != 0 {
            take(2, &mut p)?;
        }
    }
    if m & (1 << 26) != 0 {
        take(4, &mut p)?;
    }
    Some((f, p - start))
}

/// Runs of a `StyleTextPropAtom` over a text of `len` characters (the
/// atom counts one more, for the end): paragraph runs (characters, indent
/// level, properties) and character runs (characters, properties).
pub fn style_runs(b: &[u8], len: usize) -> (Vec<(usize, u16, Pf)>, Vec<(usize, Cf)>) {
    let mut paras = Vec::new();
    let mut chars = Vec::new();
    let mut p = 0;
    let mut covered = 0;
    while covered <= len && p + 6 <= b.len() && paras.len() < 1 << 16 {
        let n = u32_at(b, p) as usize;
        let lvl = u16_at(b, p + 4);
        let Some((f, used)) = pf(b, p + 6) else { return (paras, chars) };
        p += 6 + used;
        paras.push((n, lvl.min(8), f));
        covered = covered.saturating_add(n.max(1));
    }
    covered = 0;
    while covered <= len && p + 4 <= b.len() && chars.len() < 1 << 16 {
        let n = u32_at(b, p) as usize;
        let Some((f, used)) = cf(b, p + 4) else { break };
        p += 4 + used;
        chars.push((n, f));
        covered = covered.saturating_add(n.max(1));
    }
    (paras, chars)
}

/// The automatic numbering of each entry of a `StyleTextProp9Atom`
/// (PowerPoint 2000's paragraph extras): scheme and first number.
pub fn style9(b: &[u8]) -> Vec<Option<(u16, i16)>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 4 <= b.len() && out.len() < 1 << 12 {
        // TextPFException9.
        let m = u32_at(b, p);
        p += 4;
        if m & 1 << 23 != 0 {
            p += 2;
        }
        let mut has = false;
        if m & 1 << 24 != 0 {
            has = u16_at(b, p) != 0;
            p += 2;
        }
        let mut num = None;
        if m & 1 << 25 != 0 {
            if p + 4 > b.len() {
                break;
            }
            num = Some((u16_at(b, p), i16_at(b, p + 2)));
            p += 4;
        }
        // TextCFException9: a mask, and the PowerPoint 2002 run id.
        if p + 4 > b.len() {
            break;
        }
        let m = u32_at(b, p);
        p += 4;
        if m & 1 << 26 != 0 {
            p += 4;
        }
        // TextSIException.
        if p + 4 > b.len() {
            break;
        }
        let m = u32_at(b, p);
        p += 4;
        for (bit, n) in [(0, 2), (1, 2), (2, 2), (5, 4), (6, 2)] {
            if m & 1 << bit != 0 {
                p += n;
            }
        }
        if m & 1 << 9 != 0 {
            let n = u32_at(b, p) as usize;
            p = p.saturating_add(4).saturating_add(n.min(1 << 16) * 4);
        }
        out.push(if has || num.is_some() { Some(num.unwrap_or((3, 1))) } else { None });
    }
    out
}

/// A `TextMasterStyleAtom` of text type `inst`: each indent level's
/// paragraph and character properties.
pub fn master_style(b: &[u8], inst: u16) -> Vec<(Pf, Cf)> {
    let mut out = Vec::new();
    let n = u16_at(b, 0) as usize;
    let mut p = 2;
    for _ in 0..n.min(9) {
        if inst >= 5 {
            p += 2;
        }
        let Some((f, used)) = pf(b, p) else { break };
        p += used;
        let Some((c, used)) = cf(b, p) else { break };
        p += used;
        out.push((f, c));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exceptions() {
        // Paragraph: bullet on, centred.
        let mut b = Vec::new();
        b.extend_from_slice(&(1u32 | 1 << 11).to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        let (f, n) = pf(&b, 0).unwrap();
        assert_eq!((f.bullet, f.align, n), (Some(true), Some(1), 8));
        // Character: bold, 28 pt, red.
        let mut b = Vec::new();
        b.extend_from_slice(&(1u32 | 1 << 17 | 1 << 18).to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&28u16.to_le_bytes());
        b.extend_from_slice(&[0xFF, 0, 0, 0xFE]);
        let (c, n) = cf(&b, 0).unwrap();
        assert_eq!((c.bold, c.size, c.color, n), (Some(true), Some(28), Some(Col::Rgb(0xFF0000)), 12));
        // Cut short: nothing, no panic.
        assert!(cf(&b[..7], 0).is_none());
        // Numbering extras: a plain entry, then 1. 2. 3. from 1.
        let mut b = vec![0u8; 12];
        b.extend_from_slice(&(3u32 << 24).to_le_bytes());
        b.extend_from_slice(&[1, 0, 3, 0, 1, 0]);
        b.extend_from_slice(&[0; 8]);
        assert_eq!(style9(&b), vec![None, Some((3, 1))]);
        assert!(style9(&b[..14]).len() == 1);
    }
}
