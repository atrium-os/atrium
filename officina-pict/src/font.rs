//! The faces metafile text is drawn with: Liberation Sans, Serif and Mono,
//! regular and bold (italic is slanted). Chosen by the face name the file
//! asks for, as folium-font chooses PDF substitutes.

use std::sync::OnceLock;

use tiny_skia::PathBuilder;
use ttf_parser::{Face, GlyphId, OutlineBuilder};

static BYTES: [&[u8]; 6] = [
    include_bytes!("../../fonts/liberation/LiberationSans-Regular.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSans-Bold.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSerif-Regular.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSerif-Bold.ttf"),
    include_bytes!("../../fonts/liberation/LiberationMono-Regular.ttf"),
    include_bytes!("../../fonts/liberation/LiberationMono-Bold.ttf"),
];

pub fn face(i: usize) -> Option<&'static Face<'static>> {
    static FACES: OnceLock<Vec<Option<Face<'static>>>> = OnceLock::new();
    FACES.get_or_init(|| BYTES.iter().map(|b| Face::parse(b, 0).ok()).collect()).get(i)?.as_ref()
}

/// Which face for a name, a weight and the pitch-and-family byte.
pub fn pick(name: &str, weight: i32, pitch_family: u8) -> usize {
    let n = name.to_ascii_lowercase();
    let family = pitch_family >> 4;
    let base = if n.contains("courier") || n.contains("mono") || n.contains("consol") || n.contains("lucida console") || n.contains("fixedsys") || (n.is_empty() && pitch_family & 3 == 1) {
        4
    } else if n.contains("times") || (n.contains("serif") && !n.contains("sans")) || n.contains("georgia") || n.contains("garamond") || n.contains("cambria") || n.contains("book")
        || n.contains("roman") || n.contains("palatino") || n.contains("century") || n.contains("mincho") || (n.is_empty() && family == 1)
    {
        2
    } else {
        0
    };
    base + (weight >= 600) as usize
}

/// What the face can't draw: symbol and dingbat fonts.
pub enum Symbols {
    No,
    /// Symbol: Greek letters and maths at Latin code points.
    Symbol,
    /// Wingdings and the like: nothing drawn.
    Dingbats,
}

pub fn symbols(name: &str, charset: u8) -> Symbols {
    let n = name.to_ascii_lowercase();
    if n.contains("wingding") || n.contains("webding") || n.contains("dingbat") || n.contains("marlett") || n.contains("monotype sorts") {
        Symbols::Dingbats
    } else if n == "symbol" || n.starts_with("symbol ") || (charset == 2 && !n.contains("mt extra")) {
        Symbols::Symbol
    } else {
        Symbols::No
    }
}

/// The Symbol font's code points 0x20–0xFF as Unicode.
pub fn symbol_char(c: u32) -> Option<char> {
    let c = if (0xF020..=0xF0FF).contains(&c) { c - 0xF000 } else { c };
    const GREEK: &str = "ΑΒΧΔΕΦΓΗΙϑΚΛΜΝΟΠΘΡΣΤΥςΩΞΨΖ";
    const GREEK_LOWER: &str = "αβχδεφγηιϕκλμνοπθρστυϖωξψζ";
    let ch = match c {
        0x41..=0x5A => GREEK.chars().nth((c - 0x41) as usize)?,
        0x61..=0x7A => GREEK_LOWER.chars().nth((c - 0x61) as usize)?,
        0x22 => '∀',
        0x24 => '∃',
        0x27 => '∋',
        0x2A => '∗',
        0x2D => '−',
        0x40 => '≅',
        0x5C => '∴',
        0x5E => '⊥',
        0x60 => '‾',
        0xA2 => '′',
        0xA3 => '≤',
        0xA5 => '∞',
        0xA7 => '♣',
        0xA8 => '♦',
        0xA9 => '♥',
        0xAA => '♠',
        0xAB => '↔',
        0xAC => '←',
        0xAD => '↑',
        0xAE => '→',
        0xAF => '↓',
        0xB0 => '°',
        0xB1 => '±',
        0xB2 => '″',
        0xB3 => '≥',
        0xB4 => '×',
        0xB5 => '∝',
        0xB6 => '∂',
        0xB7 => '•',
        0xB8 => '÷',
        0xB9 => '≠',
        0xBA => '≡',
        0xBB => '≈',
        0xBC => '…',
        0xC4 => '⊗',
        0xC5 => '⊕',
        0xC6 => '∅',
        0xC7 => '∩',
        0xC8 => '∪',
        0xC9 => '⊃',
        0xCA => '⊇',
        0xCB => '⊄',
        0xCC => '⊂',
        0xCD => '⊆',
        0xCE => '∈',
        0xCF => '∉',
        0xD0 => '∠',
        0xD1 => '∇',
        0xD5 => '∏',
        0xD6 => '√',
        0xD7 => '⋅',
        0xD8 => '¬',
        0xD9 => '∧',
        0xDA => '∨',
        0xDB => '⇔',
        0xDC => '⇐',
        0xDD => '⇑',
        0xDE => '⇒',
        0xDF => '⇓',
        0xE0 => '◊',
        0xE1 => '〈',
        0xE5 => '∑',
        0xF1 => '〉',
        0xF2 => '∫',
        0x20..=0x7E => char::from_u32(c)?,
        _ => return None,
    };
    Some(ch)
}

/// Collects a glyph outline into canvas space through an affine map.
pub struct Outline<'a> {
    pub pb: &'a mut PathBuilder,
    /// a b c d e f: x' = a x + c y + e, y' = b x + d y + f.
    pub m: [f32; 6],
}

impl Outline<'_> {
    fn p(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.m;
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
    }
}

impl OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.pb.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.pb.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (a, b) = self.p(x1, y1);
        let (x, y) = self.p(x, y);
        self.pb.quad_to(a, b, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (a, b) = self.p(x1, y1);
        let (c, d) = self.p(x2, y2);
        let (x, y) = self.p(x, y);
        self.pb.cubic_to(a, b, c, d, x, y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

pub fn glyph(f: &Face, c: char) -> Option<GlyphId> {
    f.glyph_index(c).filter(|g| g.0 != 0)
}
