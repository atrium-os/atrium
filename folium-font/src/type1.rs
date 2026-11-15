//! Type 1 fonts (Adobe Type 1 Font Format): the cleartext part's encoding
//! and matrix, the eexec-encrypted private part's subroutines and
//! charstrings, and a charstring interpreter with flex, hint replacement
//! and seac. Bounded: charstring count and size, subroutine depth, and
//! operations per glyph.

use std::collections::HashMap;
use std::sync::Arc;

use crate::geom::{Matrix, Path};
use crate::tables::STANDARD;

const CHARSTRINGS_MAX: usize = 65536;
const OPS_MAX: usize = 1 << 16;
const SUBR_DEPTH_MAX: usize = 10;
const STACK_MAX: usize = 48;

#[derive(Debug, Clone)]
pub struct Type1 {
    pub matrix: Matrix,
    /// The font's built-in encoding, code to glyph name.
    pub encoding: Vec<Option<Arc<str>>>,
    charstrings: HashMap<Arc<str>, Vec<u8>>,
    subrs: Vec<Vec<u8>>,
}

fn decrypt(data: &[u8], mut r: u16, skip: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &c in data {
        out.push(c ^ (r >> 8) as u8);
        r = (c as u16).wrapping_add(r).wrapping_mul(52845).wrapping_add(22719);
    }
    out.drain(..skip.min(out.len()));
    out
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> { h.windows(n.len()).position(|w| w == n) }

/// Strip PFB segment headers if the program has them.
fn unpfb(data: &[u8]) -> (Vec<u8>, usize) {
    if data.first() != Some(&0x80) {
        return (data.to_vec(), 0);
    }
    let mut out = Vec::new();
    let mut clear = 0;
    let mut i = 0;
    while i + 6 <= data.len() && data[i] == 0x80 && data[i + 1] != 3 {
        let len = u32::from_le_bytes([data[i + 2], data[i + 3], data[i + 4], data[i + 5]]) as usize;
        let seg = &data[i + 6..(i + 6 + len).min(data.len())];
        if data[i + 1] == 1 && clear == 0 {
            clear = seg.len();
        }
        out.extend_from_slice(seg);
        i += 6 + len;
    }
    (out, clear)
}

impl Type1 {
    /// `len1` is the cleartext part's length from `/Length1` (0 if unknown).
    pub fn parse(data: &[u8], len1: usize) -> Option<Type1> {
        let (data, pfb_clear) = unpfb(data);
        let mut len1 = if pfb_clear > 0 { pfb_clear } else { len1 };
        // Trust `/Length1` only if `eexec` ends right before it.
        let ee = find(&data, b"eexec")?;
        if len1 == 0 || len1 > data.len() || len1 < ee + 5 || len1 > ee + 5 + 4 {
            len1 = ee + 5;
            while len1 < data.len() && matches!(data[len1], b'\r' | b'\n' | b' ' | b'\t') {
                len1 += 1;
            }
        }
        let clear = &data[..len1.min(data.len())];
        let mut enc = &data[len1.min(data.len())..];
        // Hex-encoded private part.
        let hexed;
        if enc.len() >= 4 && enc[..4].iter().all(|c| c.is_ascii_hexdigit()) {
            hexed = enc
                .chunks(2)
                .filter_map(|p| std::str::from_utf8(p).ok().and_then(|s| u8::from_str_radix(s, 16).ok()))
                .collect::<Vec<_>>();
            enc = &hexed;
        }
        let private = decrypt(enc, 55665, 4);
        let matrix = parse_matrix(clear).unwrap_or(Matrix::scale(0.001, 0.001));
        let encoding = parse_encoding(clear);
        let len_iv = find(&private, b"/lenIV")
            .and_then(|p| std::str::from_utf8(&private[p + 6..(p + 12).min(private.len())]).ok()?.split_whitespace().next()?.parse::<i32>().ok())
            .unwrap_or(4);
        let cs_decrypt = |b: &[u8]| if len_iv < 0 { b.to_vec() } else { decrypt(b, 4330, len_iv as usize) };
        // Subrs: `dup i n RD <bytes> NP`.
        let mut subrs = Vec::new();
        if let Some(p) = find(&private, b"/Subrs") {
            let mut s = Scanner { b: &private, i: p + 6 };
            let n = s.int().unwrap_or(0).clamp(0, 65536) as usize;
            subrs = vec![Vec::new(); n];
            while let Some(w) = s.word() {
                if w == b"dup" {
                    let (Some(i), Some(len)) = (s.int(), s.int()) else { break };
                    s.word(); // RD or -|
                    s.i += 1;
                    let Some(bytes) = s.take(len.max(0) as usize) else { break };
                    if (0..n as i64).contains(&i) {
                        subrs[i as usize] = cs_decrypt(bytes);
                    }
                } else if w == b"/CharStrings" || w == b"end" && subrs.iter().all(|s| !s.is_empty()) {
                    break;
                } else if w.starts_with(b"/CharStrings") {
                    break;
                }
            }
        }
        // CharStrings: `/name n RD <bytes> ND`.
        let mut charstrings = HashMap::new();
        let p = find(&private, b"/CharStrings")?;
        let mut s = Scanner { b: &private, i: p + 12 };
        while let Some(w) = s.word() {
            if let Some(name) = w.strip_prefix(b"/") {
                let Some(len) = s.int() else { continue };
                s.word();
                s.i += 1;
                let Some(bytes) = s.take(len.max(0) as usize) else { break };
                charstrings.insert(Arc::from(String::from_utf8_lossy(name).as_ref()), cs_decrypt(bytes));
                if charstrings.len() >= CHARSTRINGS_MAX {
                    break;
                }
            } else if w == b"end" && !charstrings.is_empty() {
                break;
            }
        }
        if charstrings.is_empty() {
            return None;
        }
        Some(Type1 { matrix, encoding, charstrings, subrs })
    }

    pub fn has_glyph(&self, name: &str) -> bool { self.charstrings.contains_key(name) }

    /// A glyph's outline in glyph space (before the font matrix), and its
    /// advance width from `hsbw`/`sbw`.
    pub fn outline(&self, name: &str) -> Option<(Path, f64)> {
        let cs = self.charstrings.get(name)?;
        let mut st = Interp { path: Path::new(), x: 0.0, y: 0.0, stack: Vec::new(), ps: Vec::new(), flex: None, ops: 0, width: 0.0, sbx: 0.0, open: false };
        st.run(self, cs, 0, 0)?;
        st.finish();
        Some((st.path, st.width))
    }
}

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Scanner<'a> {
    fn word(&mut self) -> Option<&'a [u8]> {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
        let s = self.i;
        while self.i < self.b.len() && !self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
        (self.i > s).then(|| &self.b[s..self.i])
    }
    fn int(&mut self) -> Option<i64> { std::str::from_utf8(self.word()?).ok()?.parse().ok() }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let r = self.b.get(self.i..self.i + n)?;
        self.i += n;
        Some(r)
    }
}

fn parse_matrix(clear: &[u8]) -> Option<Matrix> {
    let p = find(clear, b"/FontMatrix")?;
    let rest = &clear[p + 11..(p + 200).min(clear.len())];
    let a = rest.iter().position(|&c| c == b'[' || c == b'{')?;
    let b = rest.iter().position(|&c| c == b']' || c == b'}')?;
    let v: Vec<f64> = std::str::from_utf8(rest.get(a + 1..b)?).ok()?.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    Matrix::from_array(&v).filter(|m| m.det().abs() > 1e-12)
}

fn parse_encoding(clear: &[u8]) -> Vec<Option<Arc<str>>> {
    let mut out: Vec<Option<Arc<str>>> = vec![None; 256];
    let Some(p) = find(clear, b"/Encoding") else { return out };
    let rest = &clear[p..];
    if find(&rest[..rest.len().min(64)], b"StandardEncoding").is_some() {
        for (i, n) in STANDARD.iter().enumerate() {
            out[i] = n.map(Arc::from);
        }
        return out;
    }
    let end = find(rest, b"readonly def").or_else(|| find(rest, b" def")).unwrap_or(rest.len()).min(64 << 10);
    let mut s = Scanner { b: &rest[..end.max(9).min(rest.len())], i: 9 };
    while let Some(w) = s.word() {
        if w == b"dup" {
            let Some(code) = s.int() else { continue };
            let Some(name) = s.word() else { break };
            if let (Some(n), true) = (name.strip_prefix(b"/"), (0..256).contains(&code)) {
                out[code as usize] = Some(Arc::from(String::from_utf8_lossy(n).as_ref()));
            }
        }
    }
    out
}

struct Interp {
    path: Path,
    x: f64,
    y: f64,
    stack: Vec<f64>,
    /// The PostScript stack `callothersubr` leaves for `pop`.
    ps: Vec<f64>,
    /// Points collected during a flex.
    flex: Option<Vec<(f64, f64)>>,
    ops: usize,
    width: f64,
    sbx: f64,
    open: bool,
}

impl Interp {
    fn finish(&mut self) {
        if self.open {
            self.path.close();
            self.open = false;
        }
    }
    fn move_to(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        if let Some(f) = &mut self.flex {
            f.push((self.x, self.y));
            return;
        }
        self.finish();
        self.path.move_to(self.x, self.y);
    }
    fn line_to(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
        if !self.open && self.path.els.is_empty() {
            self.path.move_to(self.x - dx, self.y - dy);
        }
        self.open = true;
        self.path.line_to(self.x, self.y);
    }
    fn curve(&mut self, a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) {
        let (x1, y1) = (self.x + a, self.y + b);
        let (x2, y2) = (x1 + c, y1 + d);
        self.x = x2 + e;
        self.y = y2 + f;
        self.open = true;
        self.path.cubic_to(x1, y1, x2, y2, self.x, self.y);
    }

    fn run(&mut self, font: &Type1, cs: &[u8], depth: usize, seac_depth: usize) -> Option<bool> {
        if depth > SUBR_DEPTH_MAX {
            return None;
        }
        let mut i = 0;
        while i < cs.len() {
            self.ops += 1;
            if self.ops > OPS_MAX {
                return None;
            }
            let v = cs[i] as i32;
            i += 1;
            if v >= 32 {
                let n = match v {
                    32..=246 => (v - 139) as f64,
                    247..=250 => ((v - 247) * 256 + *cs.get(i)? as i32 + 108) as f64,
                    251..=254 => (-(v - 251) * 256 - *cs.get(i)? as i32 - 108) as f64,
                    _ => {
                        let b = cs.get(i..i + 4)?;
                        i += 4;
                        i32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64
                    }
                };
                if (247..=254).contains(&v) {
                    i += 1;
                }
                if self.stack.len() >= STACK_MAX {
                    return None;
                }
                self.stack.push(n);
                continue;
            }
            let s = std::mem::take(&mut self.stack);
            let g = |k: usize| s.get(k).copied().unwrap_or(0.0);
            match v {
                1 | 3 => {}                                   // hstem, vstem
                4 => self.move_to(0.0, g(0)),                 // vmoveto
                5 => self.line_to(g(0), g(1)),                // rlineto
                6 => self.line_to(g(0), 0.0),                 // hlineto
                7 => self.line_to(0.0, g(0)),                 // vlineto
                8 => self.curve(g(0), g(1), g(2), g(3), g(4), g(5)),
                9 => self.finish(),                           // closepath
                10 => {
                    // callsubr
                    let n = *s.last()? as i64;
                    self.stack = s[..s.len() - 1].to_vec();
                    let sub = font.subrs.get(usize::try_from(n).ok()?)?;
                    if self.run(font, sub, depth + 1, seac_depth)? {
                        return Some(true);
                    }
                    continue;
                }
                11 => {
                    self.stack = s;
                    return Some(false); // return
                }
                12 => {
                    let e = *cs.get(i)?;
                    i += 1;
                    match e {
                        0 | 1 | 2 => {} // dotsection, vstem3, hstem3
                        6 => {
                            // seac: asb adx ady bchar achar
                            if seac_depth > 0 {
                                return None;
                            }
                            let (asb, adx, ady) = (g(0), g(1), g(2));
                            let base = STANDARD.get(g(3) as usize).copied().flatten()?;
                            let acc = STANDARD.get(g(4) as usize).copied().flatten()?;
                            let width = self.width;
                            let mut b = Interp { path: Path::new(), x: 0.0, y: 0.0, stack: Vec::new(), ps: Vec::new(), flex: None, ops: self.ops, width: 0.0, sbx: 0.0, open: false };
                            b.run(font, font.charstrings.get(base)?, 0, 1)?;
                            b.finish();
                            let mut a = Interp { path: Path::new(), x: 0.0, y: 0.0, stack: Vec::new(), ps: Vec::new(), flex: None, ops: b.ops, width: 0.0, sbx: 0.0, open: false };
                            a.run(font, font.charstrings.get(acc)?, 0, 1)?;
                            a.finish();
                            // The accent's origin is (adx - asb, ady) from the
                            // base's; each draws its own side bearing.
                            self.path = b.path;
                            self.path.extend(&a.path.transform(&Matrix::translate(adx - asb, ady)));
                            self.width = width;
                            self.ops = a.ops;
                            return Some(true);
                        }
                        7 => {
                            // sbw
                            self.sbx = g(0);
                            self.x = g(0);
                            self.y = g(1);
                            self.width = g(2);
                        }
                        12 => {
                            // div
                            let (a, b) = (g(s.len().saturating_sub(2)), g(s.len().saturating_sub(1)));
                            self.stack = s[..s.len().saturating_sub(2)].to_vec();
                            self.stack.push(if b != 0.0 { a / b } else { 0.0 });
                            continue;
                        }
                        16 => {
                            // callothersubr: args… n othersubr#
                            let k = s.len();
                            if k < 2 {
                                return None;
                            }
                            let num = s[k - 1] as i64;
                            let n = (s[k - 2] as usize).min(k - 2);
                            let args: Vec<f64> = s[k - 2 - n..k - 2].to_vec();
                            self.stack = s[..k - 2 - n].to_vec();
                            match num {
                                1 => self.flex = Some(Vec::new()),
                                0 => {
                                    // End of flex: 7 points, the first a reference.
                                    let pts = self.flex.take().unwrap_or_default();
                                    if pts.len() >= 7 {
                                        let p = &pts[1..7];
                                        self.open = true;
                                        self.path.cubic_to(p[0].0, p[0].1, p[1].0, p[1].1, p[2].0, p[2].1);
                                        self.path.cubic_to(p[3].0, p[3].1, p[4].0, p[4].1, p[5].0, p[5].1);
                                        self.x = p[5].0;
                                        self.y = p[5].1;
                                    }
                                    // pop pop leaves the end point.
                                    self.ps = vec![self.y, self.x];
                                }
                                2 => {}
                                3 => self.ps = vec![3.0], // hint replacement: callsubr 3
                                _ => {
                                    self.ps = args.into_iter().rev().collect();
                                }
                            }
                            continue;
                        }
                        17 => {
                            // pop
                            self.stack = s;
                            self.stack.push(self.ps.pop().unwrap_or(0.0));
                            continue;
                        }
                        33 => {
                            // setcurrentpoint
                            self.x = g(0);
                            self.y = g(1);
                        }
                        _ => {}
                    }
                }
                13 => {
                    // hsbw
                    self.sbx = g(0);
                    self.x = g(0);
                    self.y = 0.0;
                    self.width = g(1);
                }
                14 => {
                    self.finish();
                    return Some(true); // endchar
                }
                21 => self.move_to(g(0), g(1)),
                22 => self.move_to(g(0), 0.0),
                30 => self.curve(0.0, g(0), g(1), g(2), g(3), 0.0), // vhcurveto
                31 => self.curve(g(0), 0.0, g(1), g(2), 0.0, g(3)), // hvcurveto
                _ => {}
            }
        }
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(plain: &[u8], r: u16) -> Vec<u8> {
        let mut out = Vec::new();
        let mut r = r;
        for &p in plain {
            let c = p ^ (r >> 8) as u8;
            out.push(c);
            r = (c as u16).wrapping_add(r).wrapping_mul(52845).wrapping_add(22719);
        }
        out
    }

    fn num(v: i32) -> Vec<u8> {
        match v {
            -107..=107 => vec![(v + 139) as u8],
            108..=1131 => {
                let w = v - 108;
                vec![(w / 256 + 247) as u8, (w % 256) as u8]
            }
            -1131..=-108 => {
                let w = -v - 108;
                vec![(w / 256 + 251) as u8, (w % 256) as u8]
            }
            _ => {
                let mut b = vec![255];
                b.extend_from_slice(&v.to_be_bytes());
                b
            }
        }
    }

    /// A tiny font: "square" is a 500-unit box drawn through a subr.
    #[test]
    fn parse_and_draw() {
        let mut sq = Vec::new();
        for v in [0, 600] {
            sq.extend(num(v));
        }
        sq.push(13); // hsbw 0 600
        sq.extend(num(100));
        sq.extend(num(0));
        sq.push(21); // rmoveto 100 0
        sq.extend(num(0));
        sq.push(10); // callsubr 0
        sq.extend(num(-500));
        sq.push(6); // hlineto -500
        sq.push(9);
        sq.push(14);
        let mut sub = Vec::new();
        sub.extend(num(500));
        sub.push(6);
        sub.extend(num(500));
        sub.push(7);
        sub.push(11);
        let cs = |b: &[u8]| {
            let mut p = vec![0u8; 4];
            p.extend_from_slice(b);
            enc(&p, 4330)
        };
        let mut private = b"dup /Private 8 dict dup begin /lenIV 4 def /Subrs 1 array\ndup 0 ".to_vec();
        let s0 = cs(&sub);
        private.extend(format!("{} RD ", s0.len()).as_bytes());
        private.extend(&s0);
        private.extend(b" NP\nND\n2 index /CharStrings 1 dict dup begin\n/square ");
        let c0 = cs(&sq);
        private.extend(format!("{} RD ", c0.len()).as_bytes());
        private.extend(&c0);
        private.extend(b" ND\nend end\n");
        let mut p4 = vec![1u8, 2, 3, 4];
        p4.extend(private);
        let mut font = b"%!PS-AdobeFont-1.0: Test\n/FontMatrix [0.001 0 0 0.001 0 0] readonly def\n/Encoding 256 array\n0 1 255 {1 index exch /.notdef put} for\ndup 65 /square put\nreadonly def\ncurrentfile eexec\n".to_vec();
        let l1 = font.len();
        font.extend(enc(&p4, 55665));
        let f = Type1::parse(&font, l1).unwrap();
        assert_eq!(f.encoding[65].as_deref(), Some("square"));
        let (p, w) = f.outline("square").unwrap();
        assert_eq!(w, 600.0);
        let b = p.bounds();
        assert_eq!((b.x0, b.y0, b.x1, b.y1), (100.0, 0.0, 600.0, 500.0));
        assert!((f.matrix.a - 0.001).abs() < 1e-12);
    }
}
