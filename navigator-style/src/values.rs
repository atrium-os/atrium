//! The property set — Atrium Document Profile v1, §3.3–§3.9 — as data.
//!
//! ★ THE 64 ROWS ARE THE CONFORMANCE DENOMINATOR (profile §3.9, §5). Each row
//! lists its longhands, the value atoms it admits, its initial value WRITTEN
//! AS CSS (and parsed by the same grammar, so an initial value outside its
//! own grammar is a test failure, not a latent inconsistency), and whether it
//! inherits. Anything not in this table is a diagnostic (§5.1).

use crate::token::{Tok, Token};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit { Px, Em, Rem, Ch, Vw, Vh }

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Length { pub v: f64, pub unit: Unit }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }

#[derive(Debug, Clone, PartialEq)]
pub enum Calc { Num(f64), Len(Length), Pct(f64), Add(Box<Calc>, Box<Calc>), Sub(Box<Calc>, Box<Calc>),
                Mul(Box<Calc>, Box<Calc>), Div(Box<Calc>, Box<Calc>),
                /// `min()`, `max()`, `clamp(min, val, max)` — admitted since
                /// they are as bounded and deterministic as the rest of
                /// `calc()`: a fixed argument list, no content dependence,
                /// and the same nesting ceiling. Modern CSS cannot be read
                /// without them (§3.11).
                Min(Vec<Calc>), Max(Vec<Calc>), Clamp(Box<Calc>, Box<Calc>, Box<Calc>) }

#[derive(Debug, Clone, PartialEq)]
pub enum Track { Len(Length), Pct(f64), Fr(f64), MinContent, MaxContent, Auto, MinMax(Box<Track>, Box<Track>) }

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridLine { Auto, Line(i32), Span(i32) }

#[derive(Debug, Clone, PartialEq)]
pub enum Tf { Translate(Box<V>, Box<V>), Scale(f64, f64), Rotate(f64) }

#[derive(Debug, Clone, PartialEq)]
pub struct Stop { pub color: Color, pub at: Option<f64> }

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color { Rgba(Rgba), Current, Transparent }

/// A specified value, typed by the row's grammar.
#[derive(Debug, Clone, PartialEq)]
pub enum V {
    Kw(&'static str),
    Len(Length),
    Pct(f64),
    Num(f64),
    Int(i64),
    Color(Color),
    Calc(Box<Calc>),
    Url(String),
    Gradient { angle_deg: f64, stops: Vec<Stop> },
    Shadow { x: Length, y: Length, blur: Length, spread: Length, color: Color },
    Transform(Vec<Tf>),
    Tracks(Vec<Track>),
    Track(Track),
    Line(GridLine),
    /// Declared web-font families, then exactly one profile stack.
    Family { web: Vec<String>, stack: &'static str },
    Pair(Box<V>, Box<V>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Specified {
    Inherit,
    Initial,
    Value(V),
    /// Contains `var()`: substituted at cascade time, then parsed by this
    /// row's grammar (custom properties, profile §3.6).
    Unresolved(Vec<Token>),
}

/// One value atom a row may admit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum A {
    Kw(&'static [&'static str]),
    Len, LenNonNeg, Pct, PctNonNeg, Num, NumNonNeg, Opacity, Int,
    Color, Url, Gradient, Shadow, Transform, TrackList, TrackSize, GridLine, Family, FontWeight,
    /// Two of: length or percentage (transform-origin) / two lengths (border-spacing).
    PairLenPct, PairLen,
    /// `<number>fr`-free ratio for aspect-ratio.
    Ratio,
}

pub struct Row {
    pub id: u8,
    pub section: &'static str,
    pub props: &'static [&'static str],
    pub atoms: &'static [A],
    pub initial: &'static str,
    pub inherited: bool,
}

const LENPCT_AUTO: &[A] = &[A::Len, A::Pct, A::Kw(&["auto"])];

pub const ROWS: &[Row] = &[
    // §3.3 Box and layout (16)
    Row { id: 1, section: "3.3", props: &["display"], atoms: &[A::Kw(&["block", "inline", "inline-block", "flex", "grid", "table", "table-row", "table-cell", "none"])], initial: "inline", inherited: false },
    Row { id: 2, section: "3.3", props: &["position"], atoms: &[A::Kw(&["static", "relative", "absolute", "fixed"])], initial: "static", inherited: false },
    Row { id: 3, section: "3.3", props: &["top", "right", "bottom", "left"], atoms: LENPCT_AUTO, initial: "auto", inherited: false },
    Row { id: 4, section: "3.3", props: &["width", "height"], atoms: &[A::Len, A::Pct, A::Kw(&["auto", "min-content", "max-content"])], initial: "auto", inherited: false },
    Row { id: 5, section: "3.3", props: &["min-width", "min-height"], atoms: LENPCT_AUTO, initial: "auto", inherited: false },
    Row { id: 6, section: "3.3", props: &["max-width", "max-height"], atoms: &[A::Len, A::Pct, A::Kw(&["none"])], initial: "none", inherited: false },
    Row { id: 7, section: "3.3", props: &["aspect-ratio"], atoms: &[A::Ratio, A::Kw(&["auto"])], initial: "auto", inherited: false },
    Row { id: 8, section: "3.3", props: &["margin-top", "margin-right", "margin-bottom", "margin-left"], atoms: LENPCT_AUTO, initial: "0px", inherited: false },
    Row { id: 9, section: "3.3", props: &["padding-top", "padding-right", "padding-bottom", "padding-left"], atoms: &[A::LenNonNeg, A::PctNonNeg], initial: "0px", inherited: false },
    Row { id: 10, section: "3.3", props: &["border-top-width", "border-right-width", "border-bottom-width", "border-left-width"], atoms: &[A::LenNonNeg], initial: "0px", inherited: false },
    Row { id: 11, section: "3.3", props: &["border-top-style", "border-right-style", "border-bottom-style", "border-left-style"], atoms: &[A::Kw(&["none", "solid", "dashed", "dotted"])], initial: "none", inherited: false },
    Row { id: 12, section: "3.3", props: &["border-top-color", "border-right-color", "border-bottom-color", "border-left-color"], atoms: &[A::Color], initial: "currentColor", inherited: false },
    Row { id: 13, section: "3.3", props: &["border-top-left-radius", "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius"], atoms: &[A::LenNonNeg, A::PctNonNeg], initial: "0px", inherited: false },
    Row { id: 14, section: "3.3", props: &["overflow-x", "overflow-y"], atoms: &[A::Kw(&["visible", "hidden", "auto", "scroll"])], initial: "visible", inherited: false },
    Row { id: 15, section: "3.3", props: &["isolation"], atoms: &[A::Kw(&["isolate", "auto"])], initial: "auto", inherited: false },
    Row { id: 16, section: "3.3", props: &["z-index"], atoms: &[A::Int, A::Kw(&["auto"])], initial: "auto", inherited: false },
    // §3.4 Flex (9)
    Row { id: 17, section: "3.4", props: &["flex-direction"], atoms: &[A::Kw(&["row", "column"])], initial: "row", inherited: false },
    Row { id: 18, section: "3.4", props: &["flex-wrap"], atoms: &[A::Kw(&["nowrap", "wrap"])], initial: "nowrap", inherited: false },
    Row { id: 19, section: "3.4", props: &["justify-content"], atoms: &[A::Kw(&["flex-start", "flex-end", "center", "space-between", "space-around", "space-evenly"])], initial: "flex-start", inherited: false },
    // align-items/align-self serve flex (§3.4) and grid (§3.5); one property,
    // the union of both value sets, interpreted by the container's layout.
    Row { id: 20, section: "3.4", props: &["align-items"], atoms: &[A::Kw(&["stretch", "flex-start", "flex-end", "center", "baseline", "start", "end"])], initial: "stretch", inherited: false },
    Row { id: 21, section: "3.4", props: &["align-self"], atoms: &[A::Kw(&["auto", "stretch", "flex-start", "flex-end", "center", "baseline", "start", "end"])], initial: "auto", inherited: false },
    Row { id: 22, section: "3.4", props: &["align-content"], atoms: &[A::Kw(&["stretch", "flex-start", "flex-end", "center", "baseline"])], initial: "stretch", inherited: false },
    Row { id: 23, section: "3.4", props: &["row-gap", "column-gap"], atoms: &[A::LenNonNeg, A::PctNonNeg], initial: "0px", inherited: false },
    Row { id: 24, section: "3.4", props: &["flex-grow", "flex-shrink"], atoms: &[A::NumNonNeg], initial: "0", inherited: false },
    Row { id: 25, section: "3.4", props: &["flex-basis"], atoms: &[A::Len, A::Pct, A::Kw(&["auto", "content"])], initial: "auto", inherited: false },
    // §3.5 Grid (6)
    Row { id: 26, section: "3.5", props: &["grid-template-columns", "grid-template-rows"], atoms: &[A::Kw(&["none"]), A::TrackList], initial: "none", inherited: false },
    Row { id: 27, section: "3.5", props: &["grid-auto-columns", "grid-auto-rows"], atoms: &[A::TrackSize], initial: "auto", inherited: false },
    Row { id: 28, section: "3.5", props: &["grid-auto-flow"], atoms: &[A::Kw(&["row", "column"])], initial: "row", inherited: false },
    Row { id: 29, section: "3.5", props: &["grid-row-start", "grid-row-end", "grid-column-start", "grid-column-end"], atoms: &[A::GridLine], initial: "auto", inherited: false },
    Row { id: 30, section: "3.5", props: &["justify-items"], atoms: &[A::Kw(&["stretch", "start", "end", "center"])], initial: "stretch", inherited: false },
    Row { id: 31, section: "3.5", props: &["justify-self"], atoms: &[A::Kw(&["auto", "stretch", "start", "end", "center"])], initial: "auto", inherited: false },
    // §3.7 Typography (17)
    Row { id: 32, section: "3.7", props: &["color"], atoms: &[A::Color], initial: "#1f2328", inherited: true },
    Row { id: 33, section: "3.7", props: &["font-family"], atoms: &[A::Family], initial: "sans", inherited: true },
    Row { id: 34, section: "3.7", props: &["font-size"], atoms: &[A::LenNonNeg, A::PctNonNeg], initial: "16px", inherited: true },
    Row { id: 35, section: "3.7", props: &["font-weight"], atoms: &[A::FontWeight], initial: "400", inherited: true },
    Row { id: 36, section: "3.7", props: &["font-style"], atoms: &[A::Kw(&["normal", "italic"])], initial: "normal", inherited: true },
    Row { id: 37, section: "3.7", props: &["line-height"], atoms: &[A::NumNonNeg, A::LenNonNeg, A::PctNonNeg], initial: "1.5", inherited: true },
    Row { id: 38, section: "3.7", props: &["letter-spacing", "word-spacing"], atoms: &[A::Len], initial: "0px", inherited: true },
    Row { id: 39, section: "3.7", props: &["text-align"], atoms: &[A::Kw(&["start", "end", "center", "justify"])], initial: "start", inherited: true },
    Row { id: 40, section: "3.7", props: &["text-indent"], atoms: &[A::Len, A::Pct], initial: "0px", inherited: true },
    Row { id: 41, section: "3.7", props: &["text-decoration-line"], atoms: &[A::Kw(&["none", "underline", "line-through"])], initial: "none", inherited: false },
    Row { id: 42, section: "3.7", props: &["text-decoration-color"], atoms: &[A::Color], initial: "currentColor", inherited: false },
    Row { id: 43, section: "3.7", props: &["text-transform"], atoms: &[A::Kw(&["none", "uppercase", "lowercase", "capitalize"])], initial: "none", inherited: true },
    Row { id: 44, section: "3.7", props: &["white-space"], atoms: &[A::Kw(&["normal", "pre", "pre-wrap", "nowrap"])], initial: "normal", inherited: true },
    Row { id: 45, section: "3.7", props: &["overflow-wrap"], atoms: &[A::Kw(&["normal", "break-word"])], initial: "normal", inherited: true },
    Row { id: 46, section: "3.7", props: &["tab-size"], atoms: &[A::Int], initial: "8", inherited: true },
    Row { id: 47, section: "3.7", props: &["font-variant-numeric"], atoms: &[A::Kw(&["normal", "tabular-nums"])], initial: "normal", inherited: true },
    Row { id: 48, section: "3.7", props: &["direction"], atoms: &[A::Kw(&["ltr", "rtl"])], initial: "ltr", inherited: true },
    // §3.8 Paint (14)
    Row { id: 49, section: "3.8", props: &["background-color"], atoms: &[A::Color], initial: "transparent", inherited: false },
    Row { id: 50, section: "3.8", props: &["background-image"], atoms: &[A::Kw(&["none"]), A::Url, A::Gradient], initial: "none", inherited: false },
    Row { id: 51, section: "3.8", props: &["background-position-x", "background-position-y"], atoms: &[A::Len, A::Pct, A::Kw(&["left", "center", "right", "top", "bottom"])], initial: "0%", inherited: false },
    Row { id: 52, section: "3.8", props: &["background-size"], atoms: &[A::Kw(&["auto", "cover", "contain"]), A::LenNonNeg, A::PctNonNeg], initial: "auto", inherited: false },
    Row { id: 53, section: "3.8", props: &["background-repeat"], atoms: &[A::Kw(&["repeat", "repeat-x", "repeat-y", "no-repeat"])], initial: "repeat", inherited: false },
    Row { id: 54, section: "3.8", props: &["opacity"], atoms: &[A::Opacity], initial: "1", inherited: false },
    Row { id: 55, section: "3.8", props: &["visibility"], atoms: &[A::Kw(&["visible", "hidden"])], initial: "visible", inherited: true },
    Row { id: 56, section: "3.8", props: &["box-shadow"], atoms: &[A::Kw(&["none"]), A::Shadow], initial: "none", inherited: false },
    Row { id: 57, section: "3.8", props: &["outline-width", "outline-style", "outline-color"], atoms: &[A::LenNonNeg, A::Kw(&["none", "solid", "dashed", "dotted"]), A::Color], initial: "", inherited: false },
    Row { id: 58, section: "3.8", props: &["object-fit"], atoms: &[A::Kw(&["fill", "contain", "cover", "none", "scale-down"])], initial: "fill", inherited: false },
    Row { id: 59, section: "3.8", props: &["list-style-type"], atoms: &[A::Kw(&["disc", "circle", "square", "decimal", "none"])], initial: "disc", inherited: true },
    Row { id: 60, section: "3.8", props: &["list-style-position"], atoms: &[A::Kw(&["inside", "outside"])], initial: "outside", inherited: true },
    Row { id: 61, section: "3.8", props: &["transform"], atoms: &[A::Kw(&["none"]), A::Transform], initial: "none", inherited: false },
    Row { id: 62, section: "3.8", props: &["transform-origin"], atoms: &[A::PairLenPct], initial: "50% 50%", inherited: false },
    // §3.9 Tables (2)
    Row { id: 63, section: "3.9", props: &["border-spacing"], atoms: &[A::PairLen], initial: "0px 0px", inherited: true },
    Row { id: 64, section: "3.9", props: &["vertical-align"], atoms: &[A::Kw(&["top", "middle", "bottom", "baseline"])], initial: "baseline", inherited: false },
];

/// Outline's three longhands share a row but not a grammar.
pub fn outline_atoms(prop: &str) -> Option<&'static [A]> {
    match prop {
        "outline-width" => Some(&[A::LenNonNeg]),
        "outline-style" => Some(&[A::Kw(&["none", "solid", "dashed", "dotted"])]),
        "outline-color" => Some(&[A::Color]),
        _ => None,
    }
}
pub fn outline_initial(prop: &str) -> &'static str {
    match prop { "outline-width" => "0px", "outline-style" => "none", _ => "currentColor" }
}

pub fn row_of(prop: &str) -> Option<&'static Row> { ROWS.iter().find(|r| r.props.contains(&prop)) }

/// The atoms and initial value for one longhand.
pub fn grammar(prop: &str) -> Option<(&'static [A], &'static str, bool)> {
    let r = row_of(prop)?;
    if let Some(a) = outline_atoms(prop) { return Some((a, outline_initial(prop), r.inherited)) }
    // flex-shrink shares a row with flex-grow but starts at 1 (§3.4).
    let initial = if prop == "flex-shrink" { "1" } else { r.initial };
    Some((r.atoms, initial, r.inherited))
}

/// Shorthands the profile excludes (§3.2 convention 1): named so the
/// diagnostic can say "use the longhands" instead of "unknown property".
pub const SHORTHANDS: &[&str] = &[
    "margin", "padding", "border", "border-top", "border-right", "border-bottom", "border-left",
    "border-width", "border-style", "border-color", "border-radius", "background", "font", "flex",
    "flex-flow", "grid", "grid-area", "grid-row", "grid-column", "grid-template", "gap", "inset",
    "outline", "overflow", "list-style", "text-decoration", "place-items", "place-content", "place-self",
    "background-position", "columns",
];

pub const STACKS: &[&str] = &["sans", "mono"];

/// The CSS named-colour table (CSS Color 4 §6.1): a fixed, spec-defined
/// table — not a host setting, so it leaks nothing (G6).
pub fn named_color(name: &str) -> Option<Rgba> {
    const T: &[(&str, u32)] = &[
        ("aliceblue",0xf0f8ff),("antiquewhite",0xfaebd7),("aqua",0x00ffff),("aquamarine",0x7fffd4),("azure",0xf0ffff),
        ("beige",0xf5f5dc),("bisque",0xffe4c4),("black",0x000000),("blanchedalmond",0xffebcd),("blue",0x0000ff),
        ("blueviolet",0x8a2be2),("brown",0xa52a2a),("burlywood",0xdeb887),("cadetblue",0x5f9ea0),("chartreuse",0x7fff00),
        ("chocolate",0xd2691e),("coral",0xff7f50),("cornflowerblue",0x6495ed),("cornsilk",0xfff8dc),("crimson",0xdc143c),
        ("cyan",0x00ffff),("darkblue",0x00008b),("darkcyan",0x008b8b),("darkgoldenrod",0xb8860b),("darkgray",0xa9a9a9),
        ("darkgreen",0x006400),("darkgrey",0xa9a9a9),("darkkhaki",0xbdb76b),("darkmagenta",0x8b008b),("darkolivegreen",0x556b2f),
        ("darkorange",0xff8c00),("darkorchid",0x9932cc),("darkred",0x8b0000),("darksalmon",0xe9967a),("darkseagreen",0x8fbc8f),
        ("darkslateblue",0x483d8b),("darkslategray",0x2f4f4f),("darkslategrey",0x2f4f4f),("darkturquoise",0x00ced1),("darkviolet",0x9400d3),
        ("deeppink",0xff1493),("deepskyblue",0x00bfff),("dimgray",0x696969),("dimgrey",0x696969),("dodgerblue",0x1e90ff),
        ("firebrick",0xb22222),("floralwhite",0xfffaf0),("forestgreen",0x228b22),("fuchsia",0xff00ff),("gainsboro",0xdcdcdc),
        ("ghostwhite",0xf8f8ff),("gold",0xffd700),("goldenrod",0xdaa520),("gray",0x808080),("green",0x008000),
        ("greenyellow",0xadff2f),("grey",0x808080),("honeydew",0xf0fff0),("hotpink",0xff69b4),("indianred",0xcd5c5c),
        ("indigo",0x4b0082),("ivory",0xfffff0),("khaki",0xf0e68c),("lavender",0xe6e6fa),("lavenderblush",0xfff0f5),
        ("lawngreen",0x7cfc00),("lemonchiffon",0xfffacd),("lightblue",0xadd8e6),("lightcoral",0xf08080),("lightcyan",0xe0ffff),
        ("lightgoldenrodyellow",0xfafad2),("lightgray",0xd3d3d3),("lightgreen",0x90ee90),("lightgrey",0xd3d3d3),("lightpink",0xffb6c1),
        ("lightsalmon",0xffa07a),("lightseagreen",0x20b2aa),("lightskyblue",0x87cefa),("lightslategray",0x778899),("lightslategrey",0x778899),
        ("lightsteelblue",0xb0c4de),("lightyellow",0xffffe0),("lime",0x00ff00),("limegreen",0x32cd32),("linen",0xfaf0e6),
        ("magenta",0xff00ff),("maroon",0x800000),("mediumaquamarine",0x66cdaa),("mediumblue",0x0000cd),("mediumorchid",0xba55d3),
        ("mediumpurple",0x9370db),("mediumseagreen",0x3cb371),("mediumslateblue",0x7b68ee),("mediumspringgreen",0x00fa9a),("mediumturquoise",0x48d1cc),
        ("mediumvioletred",0xc71585),("midnightblue",0x191970),("mintcream",0xf5fffa),("mistyrose",0xffe4e1),("moccasin",0xffe4b5),
        ("navajowhite",0xffdead),("navy",0x000080),("oldlace",0xfdf5e6),("olive",0x808000),("olivedrab",0x6b8e23),
        ("orange",0xffa500),("orangered",0xff4500),("orchid",0xda70d6),("palegoldenrod",0xeee8aa),("palegreen",0x98fb98),
        ("paleturquoise",0xafeeee),("palevioletred",0xdb7093),("papayawhip",0xffefd5),("peachpuff",0xffdab9),("peru",0xcd853f),
        ("pink",0xffc0cb),("plum",0xdda0dd),("powderblue",0xb0e0e6),("purple",0x800080),("rebeccapurple",0x663399),
        ("red",0xff0000),("rosybrown",0xbc8f8f),("royalblue",0x4169e1),("saddlebrown",0x8b4513),("salmon",0xfa8072),
        ("sandybrown",0xf4a460),("seagreen",0x2e8b57),("seashell",0xfff5ee),("sienna",0xa0522d),("silver",0xc0c0c0),
        ("skyblue",0x87ceeb),("slateblue",0x6a5acd),("slategray",0x708090),("slategrey",0x708090),("snow",0xfffafa),
        ("springgreen",0x00ff7f),("steelblue",0x4682b4),("tan",0xd2b48c),("teal",0x008080),("thistle",0xd8bfd8),
        ("tomato",0xff6347),("turquoise",0x40e0d0),("violet",0xee82ee),("wheat",0xf5deb3),("white",0xffffff),
        ("whitesmoke",0xf5f5f5),("yellow",0xffff00),("yellowgreen",0x9acd32),
    ];
    let n = name.to_ascii_lowercase();
    T.binary_search_by(|(k, _)| k.cmp(&n.as_str())).ok()
        .map(|i| { let v = T[i].1; Rgba { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a: 255 } })
}

// ---- value parsing -------------------------------------------------------

pub const MAX_CALC_DEPTH: usize = 16;
pub const MAX_TRANSFORMS: usize = 16;
pub const MAX_TRACKS: usize = 1024;

type R<T> = Result<T, String>;

/// Component values: tokens with whitespace dropped, functions kept as a
/// (name, arguments) node so atoms can recurse without re-scanning.
#[derive(Debug, Clone, PartialEq)]
pub enum Cv { T(Tok), F(String, Vec<Cv>) }

pub fn components(toks: &[Token]) -> R<Vec<Cv>> {
    fn go(toks: &[Token], i: &mut usize, depth: usize) -> R<Vec<Cv>> {
        if depth > MAX_CALC_DEPTH + 4 { return Err("nesting too deep".into()) }
        let mut out = vec![];
        while *i < toks.len() {
            let t = &toks[*i].tok;
            *i += 1;
            match t {
                Tok::Whitespace => {}
                Tok::RParen => return Ok(out),
                Tok::Function(n) => { let args = go(toks, i, depth + 1)?; out.push(Cv::F(n.clone(), args)) }
                Tok::LParen => { let args = go(toks, i, depth + 1)?; out.push(Cv::F(String::new(), args)) }
                other => out.push(Cv::T(other.clone())),
            }
        }
        if depth > 0 { return Err("unclosed function".into()) }
        Ok(out)
    }
    let mut i = 0;
    go(toks, &mut i, 0)
}

fn unit_of(u: &str) -> Option<Unit> {
    Some(match u { "px" => Unit::Px, "em" => Unit::Em, "rem" => Unit::Rem, "ch" => Unit::Ch, "vw" => Unit::Vw, "vh" => Unit::Vh, _ => return None })
}

fn finite(v: f64) -> R<f64> { if v.is_finite() { Ok(v) } else { Err("non-finite number".into()) } }

fn length(cv: &Cv) -> R<Length> {
    match cv {
        Cv::T(Tok::Dimension { value, unit, .. }) =>
            Ok(Length { v: finite(*value)?, unit: unit_of(unit).ok_or_else(|| format!("unit `{unit}` is not admitted (§3.6)"))? }),
        // Unitless zero is a length (CSS); anything else unitless is not.
        Cv::T(Tok::Number { value, .. }) if *value == 0.0 => Ok(Length { v: 0.0, unit: Unit::Px }),
        _ => Err("expected a length".into()),
    }
}

fn number(cv: &Cv) -> R<f64> {
    match cv { Cv::T(Tok::Number { value, .. }) => finite(*value), _ => Err("expected a number".into()) }
}

fn hex_color(h: &str) -> R<Rgba> {
    let d: Vec<u8> = h.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<_>>().ok_or("bad hex colour")?;
    let (r, g, b, a) = match d.len() {
        3 => (d[0] * 17, d[1] * 17, d[2] * 17, 255),
        6 => (d[0] * 16 + d[1], d[2] * 16 + d[3], d[4] * 16 + d[5], 255),
        8 => (d[0] * 16 + d[1], d[2] * 16 + d[3], d[4] * 16 + d[5], d[6] * 16 + d[7]),
        _ => return Err(format!("#{h}: hex colours are #rgb, #rrggbb or #rrggbbaa (§3.6)")),
    };
    Ok(Rgba { r, g, b, a })
}

fn color(cv: &Cv) -> R<Color> {
    match cv {
        Cv::T(Tok::Hash(h, _)) => Ok(Color::Rgba(hex_color(h)?)),
        Cv::T(Tok::Ident(i)) if i.eq_ignore_ascii_case("currentcolor") => Ok(Color::Current),
        Cv::T(Tok::Ident(i)) if i.eq_ignore_ascii_case("transparent") => Ok(Color::Transparent),
        Cv::T(Tok::Ident(i)) => named_color(i).map(Color::Rgba).ok_or_else(|| format!("`{i}` is not a colour (system colours are excluded, §3.6)")),
        Cv::F(f, args) if matches!(f.as_str(), "rgb" | "rgba" | "hsl" | "hsla") => {
            let parts: Vec<&Cv> = args.iter().filter(|c| !matches!(c, Cv::T(Tok::Comma) | Cv::T(Tok::Delim('/')))).collect();
            if !(parts.len() == 3 || parts.len() == 4) { return Err(format!("{f}() takes 3 or 4 components")) }
            let alpha = match parts.get(3) {
                None => 1.0,
                Some(Cv::T(Tok::Number { value, .. })) => finite(*value)?.clamp(0.0, 1.0),
                Some(Cv::T(Tok::Percentage(p))) => (finite(*p)? / 100.0).clamp(0.0, 1.0),
                _ => return Err("bad alpha".into()),
            };
            let chan = |c: &Cv| -> R<f64> { match c {
                Cv::T(Tok::Number { value, .. }) => Ok(finite(*value)?.clamp(0.0, 255.0)),
                Cv::T(Tok::Percentage(p)) => Ok((finite(*p)? * 2.55).clamp(0.0, 255.0)),
                _ => Err("bad rgb component".into()) } };
            let (r, g, b) = if f.starts_with("rgb") {
                (chan(parts[0])?, chan(parts[1])?, chan(parts[2])?)
            } else {
                let h = match parts[0] { Cv::T(Tok::Number { value, .. }) => finite(*value)?,
                    Cv::T(Tok::Dimension { value, unit, .. }) if unit == "deg" => finite(*value)?, _ => return Err("bad hue".into()) };
                let pc = |c: &Cv| -> R<f64> { match c { Cv::T(Tok::Percentage(p)) => Ok((finite(*p)? / 100.0).clamp(0.0, 1.0)), _ => Err("hsl() saturation/lightness are percentages".into()) } };
                let (s, l) = (pc(parts[1])?, pc(parts[2])?);
                let h = ((h % 360.0) + 360.0) % 360.0 / 360.0;
                let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
                let p = 2.0 * l - q;
                let hue = |mut t: f64| { if t < 0.0 { t += 1.0 } if t > 1.0 { t -= 1.0 }
                    if t < 1.0 / 6.0 { p + (q - p) * 6.0 * t } else if t < 0.5 { q } else if t < 2.0 / 3.0 { p + (q - p) * (2.0 / 3.0 - t) * 6.0 } else { p } };
                (hue(h + 1.0 / 3.0) * 255.0, hue(h) * 255.0, hue(h - 1.0 / 3.0) * 255.0)
            };
            // Round half to even is avoided: one stated rule, half up.
            let q8 = |v: f64| (v + 0.5).floor() as u8;
            Ok(Color::Rgba(Rgba { r: q8(r), g: q8(g), b: q8(b), a: q8(alpha * 255.0) }))
        }
        _ => Err("expected a colour".into()),
    }
}

/// `calc()`: + - * / with the usual precedence, depth-bounded, and type-checked
/// so that `1px + 2` is a diagnostic, not a guess (§3.6).
/// A bound on `min()`/`max()` arguments, for the same reason every other
/// ceiling exists: the cost has to be stated, not discovered.
pub const MAX_CALC_ARGS: usize = 32;

fn calc(args: &[Cv], depth: usize) -> R<Calc> {
    if depth > MAX_CALC_DEPTH { return Err(format!("calc() nested deeper than {MAX_CALC_DEPTH} (§3.12)")) }
    #[derive(Clone, Copy, PartialEq)] enum K { Num, Len }
    fn kind(c: &Calc) -> R<K> {
        Ok(match c {
            Calc::Num(_) => K::Num, Calc::Len(_) | Calc::Pct(_) => K::Len,
            Calc::Add(a, b) | Calc::Sub(a, b) => { let (x, y) = (kind(a)?, kind(b)?); if x != y { return Err("calc() adds a number to a length".into()) } x }
            Calc::Mul(a, b) => { let (x, y) = (kind(a)?, kind(b)?); if x == K::Len && y == K::Len { return Err("calc() multiplies two lengths".into()) } if x == K::Len || y == K::Len { K::Len } else { K::Num } }
            Calc::Div(a, b) => { if kind(b)? != K::Num { return Err("calc() divides by a length".into()) }
                if let Calc::Num(z) = **b { if z == 0.0 { return Err("calc() divides by zero".into()) } } kind(a)? }
            // Every argument must be the same kind, as with `+`: comparing a
            // length with a number has no meaning.
            Calc::Min(v) | Calc::Max(v) => {
                let mut it = v.iter().map(kind);
                let first = it.next().ok_or("min()/max() needs an argument")??;
                for k in it { if k? != first { return Err("min()/max() compares a number with a length".into()) } }
                first
            }
            Calc::Clamp(a, b, c) => {
                let (x, y, z) = (kind(a)?, kind(b)?, kind(c)?);
                if x != y || y != z { return Err("clamp() compares a number with a length".into()) }
                x
            }
        })
    }
    let mut pos = 0;
    fn sum(a: &[Cv], p: &mut usize, d: usize) -> R<Calc> {
        let mut l = prod(a, p, d)?;
        while let Some(Cv::T(Tok::Delim(op))) = a.get(*p) {
            if *op != '+' && *op != '-' { break }
            let op = *op; *p += 1;
            let r = prod(a, p, d)?;
            l = if op == '+' { Calc::Add(Box::new(l), Box::new(r)) } else { Calc::Sub(Box::new(l), Box::new(r)) };
        }
        Ok(l)
    }
    fn prod(a: &[Cv], p: &mut usize, d: usize) -> R<Calc> {
        let mut l = atom(a, p, d)?;
        while let Some(Cv::T(Tok::Delim(op))) = a.get(*p) {
            if *op != '*' && *op != '/' { break }
            let op = *op; *p += 1;
            let r = atom(a, p, d)?;
            l = if op == '*' { Calc::Mul(Box::new(l), Box::new(r)) } else { Calc::Div(Box::new(l), Box::new(r)) };
        }
        Ok(l)
    }
    fn atom(a: &[Cv], p: &mut usize, d: usize) -> R<Calc> {
        let c = a.get(*p).ok_or("calc() ends early")?;
        *p += 1;
        match c {
            Cv::T(Tok::Number { value, .. }) => Ok(Calc::Num(finite(*value)?)),
            Cv::T(Tok::Percentage(v)) => Ok(Calc::Pct(finite(*v)?)),
            Cv::T(Tok::Dimension { .. }) => Ok(Calc::Len(length(c)?)),
            Cv::F(f, inner) if f.is_empty() || f == "calc" => calc(inner, d + 1),
            Cv::F(f, inner) if f == "min" || f == "max" || f == "clamp" => {
                let parts: Vec<Vec<Cv>> = inner.split(|c| matches!(c, Cv::T(Tok::Comma))).map(|g| g.to_vec()).collect();
                if parts.iter().any(|g| g.is_empty()) { return Err(format!("{f}() has an empty argument")) }
                if parts.len() > MAX_CALC_ARGS { return Err(format!("{f}() takes at most {MAX_CALC_ARGS} arguments (§3.11)")) }
                let args: Vec<Calc> = parts.iter().map(|g| calc(g, d + 1)).collect::<R<Vec<_>>>()?;
                match f.as_str() {
                    "min" => Ok(Calc::Min(args)),
                    "max" => Ok(Calc::Max(args)),
                    _ => {
                        let [lo, val, hi]: [Calc; 3] = args.try_into().map_err(|_| "clamp() takes exactly three arguments".to_string())?;
                        Ok(Calc::Clamp(Box::new(lo), Box::new(val), Box::new(hi)))
                    }
                }
            }
            _ => Err("unexpected token in calc()".into()),
        }
    }
    let e = sum(args, &mut pos, depth)?;
    if pos != args.len() { return Err("trailing tokens in calc()".into()) }
    kind(&e)?;
    Ok(e)
}

fn track(cv: &Cv) -> R<Track> {
    match cv {
        Cv::T(Tok::Dimension { value, unit, .. }) if unit == "fr" => {
            let v = finite(*value)?; if v < 0.0 { return Err("negative fr".into()) } Ok(Track::Fr(v)) }
        Cv::T(Tok::Percentage(p)) => Ok(Track::Pct(finite(*p)?)),
        Cv::T(Tok::Ident(i)) if i == "min-content" => Ok(Track::MinContent),
        Cv::T(Tok::Ident(i)) if i == "max-content" => Ok(Track::MaxContent),
        Cv::T(Tok::Ident(i)) if i == "auto" => Ok(Track::Auto),
        Cv::F(f, a) if f == "minmax" => {
            let p: Vec<&Cv> = a.iter().filter(|c| !matches!(c, Cv::T(Tok::Comma))).collect();
            if p.len() != 2 { return Err("minmax() takes two track sizes".into()) }
            Ok(Track::MinMax(Box::new(track(p[0])?), Box::new(track(p[1])?)))
        }
        _ => Ok(Track::Len(length(cv)?)),
    }
}

fn track_list(cvs: &[Cv], out: &mut Vec<Track>) -> R<()> {
    for cv in cvs {
        match cv {
            Cv::F(f, a) if f == "repeat" => {
                let n = match a.first() { Some(Cv::T(Tok::Number { value, int: true })) if *value >= 1.0 => *value as usize,
                    Some(Cv::T(Tok::Ident(i))) => return Err(format!("repeat({i}, …) is not admitted — the count must be an integer (§3.5)")),
                    _ => return Err("repeat() needs a positive integer count".into()) };
                if !matches!(a.get(1), Some(Cv::T(Tok::Comma))) { return Err("repeat() needs a comma".into()) }
                let mut inner = vec![];
                track_list(&a[2..], &mut inner)?;
                if inner.is_empty() { return Err("repeat() of nothing".into()) }
                for _ in 0..n {
                    out.extend(inner.iter().cloned());
                    if out.len() > MAX_TRACKS { return Err(format!("more than {MAX_TRACKS} tracks (§3.12)")) }
                }
            }
            _ => { out.push(track(cv)?); if out.len() > MAX_TRACKS { return Err(format!("more than {MAX_TRACKS} tracks (§3.12)")) } }
        }
    }
    Ok(())
}

fn one(cvs: &[Cv]) -> R<&Cv> { if cvs.len() == 1 { Ok(&cvs[0]) } else { Err("expected one value".into()) } }

fn atom(a: A, cvs: &[Cv], webfonts: &[String]) -> R<V> {
    match a {
        A::Kw(list) => { let c = one(cvs)?;
            if let Cv::T(Tok::Ident(i)) = c { if let Some(k) = list.iter().find(|k| k.eq_ignore_ascii_case(i)) { return Ok(V::Kw(k)) } }
            Err("keyword not admitted".into()) }
        A::Len | A::LenNonNeg => { let c = one(cvs)?;
            if let Cv::F(f, _) = c {
                if matches!(f.as_str(), "calc" | "min" | "max" | "clamp") {
                    // `min(…)` is a value in its own right, not only inside
                    // `calc()`; wrapping it lets one parser serve both.
                    return Ok(V::Calc(Box::new(calc(std::slice::from_ref(c), 1)?)));
                }
            }
            let l = length(c)?; if a == A::LenNonNeg && l.v < 0.0 { return Err("negative value not admitted".into()) } Ok(V::Len(l)) }
        A::Pct | A::PctNonNeg => { match one(cvs)? { Cv::T(Tok::Percentage(p)) => { let p = finite(*p)?;
            if a == A::PctNonNeg && p < 0.0 { return Err("negative value not admitted".into()) } Ok(V::Pct(p)) } _ => Err("expected a percentage".into()) } }
        A::Num | A::NumNonNeg => { let n = number(one(cvs)?)?; if a == A::NumNonNeg && n < 0.0 { return Err("negative value not admitted".into()) } Ok(V::Num(n)) }
        A::Opacity => Ok(V::Num(number(one(cvs)?)?.clamp(0.0, 1.0))),
        A::Int => match one(cvs)? { Cv::T(Tok::Number { value, int: true }) if value.abs() < 1e15 => Ok(V::Int(*value as i64)), _ => Err("expected an integer".into()) },
        A::Color => Ok(V::Color(color(one(cvs)?)?)),
        A::Url => match one(cvs)? {
            Cv::T(Tok::Url(u)) => Ok(V::Url(u.clone())),
            Cv::F(f, a) if f == "url" => match a.as_slice() { [Cv::T(Tok::Str(s))] => Ok(V::Url(s.clone())), _ => Err("url() takes one string".into()) },
            _ => Err("expected url()".into()) },
        A::Gradient => { let Cv::F(f, args) = one(cvs)? else { return Err("expected linear-gradient()".into()) };
            if f != "linear-gradient" { return Err(format!("{f}() is not admitted (§3.8 admits linear-gradient only)")) }
            let groups: Vec<Vec<Cv>> = args.split(|c| matches!(c, Cv::T(Tok::Comma))).map(|g| g.to_vec()).collect();
            let (mut angle, mut rest) = (180.0, &groups[..]);
            if let Some(first) = groups.first() {
                match first.as_slice() {
                    [Cv::T(Tok::Dimension { value, unit, .. })] if unit == "deg" => { angle = finite(*value)?; rest = &groups[1..] }
                    [Cv::T(Tok::Ident(to)), Cv::T(Tok::Ident(side))] if to == "to" => {
                        angle = match side.as_str() { "top" => 0.0, "right" => 90.0, "bottom" => 180.0, "left" => 270.0, _ => return Err("bad gradient side".into()) };
                        rest = &groups[1..] }
                    _ => {}
                }
            }
            if rest.len() < 2 { return Err("a gradient needs at least two stops".into()) }
            let stops = rest.iter().map(|g| match g.as_slice() {
                [c] => Ok(Stop { color: color(c)?, at: None }),
                [c, Cv::T(Tok::Percentage(p))] => Ok(Stop { color: color(c)?, at: Some(finite(*p)?) }),
                _ => Err("bad gradient stop".to_string()) }).collect::<R<Vec<_>>>()?;
            Ok(V::Gradient { angle_deg: angle, stops }) }
        A::Shadow => { if cvs.len() != 5 { return Err("box-shadow is exactly: x y blur spread <color> (§3.8)".into()) }
            let (x, y, blur, spread) = (length(&cvs[0])?, length(&cvs[1])?, length(&cvs[2])?, length(&cvs[3])?);
            if blur.v < 0.0 { return Err("negative blur".into()) }
            Ok(V::Shadow { x, y, blur, spread, color: color(&cvs[4])? }) }
        A::Transform => { if cvs.len() > MAX_TRANSFORMS { return Err(format!("more than {MAX_TRANSFORMS} transforms")) }
            let tfs = cvs.iter().map(|c| { let Cv::F(f, a) = c else { return Err("expected a transform function".to_string()) };
                let p: Vec<&Cv> = a.iter().filter(|c| !matches!(c, Cv::T(Tok::Comma))).collect();
                match (f.as_str(), p.len()) {
                    ("translate", 1 | 2) => { let v = |c: &Cv| -> R<V> { match c { Cv::T(Tok::Percentage(x)) => Ok(V::Pct(finite(*x)?)), _ => Ok(V::Len(length(c)?)) } };
                        let x = v(p[0])?; let y = if p.len() == 2 { v(p[1])? } else { V::Len(Length { v: 0.0, unit: Unit::Px }) };
                        Ok(Tf::Translate(Box::new(x), Box::new(y))) }
                    ("scale", 1 | 2) => { let x = number(p[0])?; Ok(Tf::Scale(x, if p.len() == 2 { number(p[1])? } else { x })) }
                    ("rotate", 1) => match p[0] { Cv::T(Tok::Dimension { value, unit, .. }) if unit == "deg" => Ok(Tf::Rotate(finite(*value)?)),
                        _ => Err("rotate() takes degrees".to_string()) },
                    _ => Err(format!("{f}() is not an admitted transform (§3.8: translate, scale, rotate)")),
                } }).collect::<R<Vec<_>>>()?;
            if tfs.is_empty() { return Err("empty transform".into()) }
            Ok(V::Transform(tfs)) }
        A::TrackList => { let mut t = vec![]; track_list(cvs, &mut t)?; if t.is_empty() { return Err("empty track list".into()) } Ok(V::Tracks(t)) }
        A::TrackSize => Ok(V::Track(track(one(cvs)?)?)),
        A::GridLine => match cvs {
            [Cv::T(Tok::Ident(i))] if i == "auto" => Ok(V::Line(GridLine::Auto)),
            [Cv::T(Tok::Number { value, int: true })] if *value != 0.0 && value.abs() <= MAX_TRACKS as f64 => Ok(V::Line(GridLine::Line(*value as i32))),
            [Cv::T(Tok::Ident(s)), Cv::T(Tok::Number { value, int: true })] if s == "span" && *value >= 1.0 && *value <= MAX_TRACKS as f64 => Ok(V::Line(GridLine::Span(*value as i32))),
            _ => Err("grid line is auto, a non-zero integer, or span <integer> — named lines are excluded (§3.5)".into()) },
        A::Family => {
            let items: Vec<&[Cv]> = cvs.split(|c| matches!(c, Cv::T(Tok::Comma))).collect();
            let (last, rest) = items.split_last().ok_or("empty font-family")?;
            let stack = match last { [Cv::T(Tok::Ident(i))] => STACKS.iter().find(|s| **s == i.as_str()).copied(), _ => None }
                .ok_or("font-family must END with a profile stack (`sans` or `mono`), so the shipped fonts always cover the text")?;
            let web = rest.iter().map(|it| match it {
                [Cv::T(Tok::Str(s))] | [Cv::T(Tok::Ident(s))] if webfonts.iter().any(|w| w == s) => Ok(s.clone()),
                [Cv::T(Tok::Str(s))] | [Cv::T(Tok::Ident(s))] => Err(format!("font family `{s}` is neither a profile stack nor declared by @font-face")),
                _ => Err("bad font family".to_string()) }).collect::<R<Vec<_>>>()?;
            Ok(V::Family { web, stack }) }
        A::FontWeight => match one(cvs)? { Cv::T(Tok::Number { value, int: true }) if (100.0..=900.0).contains(value) && (*value as i64) % 100 == 0 => Ok(V::Int(*value as i64)),
            _ => Err("font-weight is 100…900 in hundreds (§3.7)".into()) },
        A::PairLenPct | A::PairLen => {
            if cvs.len() != 2 { return Err("expected two values".into()) }
            let v = |c: &Cv| -> R<V> { match c { Cv::T(Tok::Percentage(p)) if a == A::PairLenPct => Ok(V::Pct(finite(*p)?)), _ => Ok(V::Len(length(c)?)) } };
            Ok(V::Pair(Box::new(v(&cvs[0])?), Box::new(v(&cvs[1])?))) }
        A::Ratio => match cvs {
            [c] => { let n = number(c)?; if n <= 0.0 { return Err("aspect-ratio must be positive".into()) } Ok(V::Num(n)) }
            [a1, Cv::T(Tok::Delim('/')), b1] => { let (x, y) = (number(a1)?, number(b1)?); if x <= 0.0 || y <= 0.0 { return Err("aspect-ratio must be positive".into()) } Ok(V::Num(x / y)) }
            _ => Err("aspect-ratio is <number> or <number> / <number>".into()) },
    }
}

/// Parse one declaration's value for `prop`. `Err` is the diagnostic text.
pub fn parse_value(prop: &str, toks: &[Token], webfonts: &[String]) -> R<Specified> {
    let (atoms, _, _) = grammar(prop).ok_or_else(|| format!("`{prop}` is not in the profile"))?;
    if toks.iter().any(|t| matches!(&t.tok, Tok::Function(f) if f == "var")) {
        return Ok(Specified::Unresolved(toks.to_vec()));
    }
    let cvs = components(toks)?;
    if let [Cv::T(Tok::Ident(k))] = cvs.as_slice() {
        match k.to_ascii_lowercase().as_str() {
            "inherit" => return Ok(Specified::Inherit),
            "initial" => return Ok(Specified::Initial),
            w @ ("unset" | "revert" | "revert-layer") => return Err(format!("`{w}` is excluded — inherit and initial only (§3.2)")),
            _ => {}
        }
    }
    // ★ The diagnostic names the REAL reason. Every atom that does not fit
    // the value's shape says "expected …"; one that recognised the shape and
    // then refused it (a calc() that mixes types, a unit outside §3.6) says
    // why. Reporting the last atom's complaint turned `calc(1px + 2)` into
    // "keyword not admitted".
    let mut specific: Option<String> = None;
    let mut generic = String::from("no admitted value");
    for &a in atoms {
        match atom(a, &cvs, webfonts) {
            Ok(v) => return Ok(Specified::Value(v)),
            Err(e) if e.starts_with("expected") || e == "keyword not admitted" => generic = e,
            Err(e) => { specific.get_or_insert(e); }
        }
    }
    Err(specific.unwrap_or(generic))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::tokenize;

    fn p(prop: &str, v: &str) -> R<Specified> { parse_value(prop, &tokenize(v), &["Inter".to_string()]) }
    fn val(prop: &str, v: &str) -> V { match p(prop, v) { Ok(Specified::Value(v)) => v, other => panic!("{prop}: {v} -> {other:?}") } }

    #[test]
    fn exactly_64_rows_and_no_longhand_in_two() {
        assert_eq!(ROWS.len(), 64);
        assert!(ROWS.iter().enumerate().all(|(i, r)| r.id as usize == i + 1));
        let mut all: Vec<&str> = ROWS.iter().flat_map(|r| r.props.iter().copied()).collect();
        let n = all.len(); all.sort(); all.dedup();
        assert_eq!(all.len(), n, "a longhand appears in two rows");
    }

    /// ★ Every initial value is in its own row's grammar.
    #[test]
    fn every_initial_value_parses_under_its_own_grammar() {
        for r in ROWS { for prop in r.props {
            let (_, init, _) = grammar(prop).unwrap();
            assert!(matches!(p(prop, init), Ok(Specified::Value(_))), "{prop}: initial {init:?} -> {:?}", p(prop, init));
        } }
    }

    #[test]
    fn lengths_units_and_signs() {
        assert_eq!(val("margin-top", "-1.5em"), V::Len(Length { v: -1.5, unit: Unit::Em }));
        assert_eq!(val("width", "0"), V::Len(Length { v: 0.0, unit: Unit::Px }));
        assert!(p("padding-top", "-1px").is_err(), "padding is non-negative");
        assert!(p("width", "3pt").unwrap_err().contains("not admitted"));
        assert!(p("width", "12").is_err(), "unitless non-zero is not a length");
        assert!(p("width", "1e999px").unwrap_err().contains("non-finite"));
    }

    #[test]
    fn colours() {
        assert_eq!(val("color", "#fa0"), V::Color(Color::Rgba(Rgba { r: 255, g: 170, b: 0, a: 255 })));
        assert_eq!(val("color", "rgb(255 0 0 / 50%)"), V::Color(Color::Rgba(Rgba { r: 255, g: 0, b: 0, a: 128 })));
        assert_eq!(val("color", "hsl(120, 100%, 50%)"), V::Color(Color::Rgba(Rgba { r: 0, g: 255, b: 0, a: 255 })));
        assert_eq!(val("color", "RebeccaPurple"), V::Color(Color::Rgba(Rgba { r: 0x66, g: 0x33, b: 0x99, a: 255 })));
        assert!(p("color", "ButtonText").unwrap_err().contains("system colours"));
        assert!(p("color", "#12345").is_err());
    }

    #[test]
    fn calc_is_type_checked_and_bounded() {
        assert!(matches!(val("width", "calc(100% - 2 * 8px)"), V::Calc(_)));
        assert!(p("width", "calc(1px + 2)").unwrap_err().contains("adds a number"));
        assert!(p("width", "calc(1px / 0)").unwrap_err().contains("zero"));
        assert!(p("width", "calc(1px * 2px)").unwrap_err().contains("two lengths"));
        let deep = format!("{}1px{}", "calc(".repeat(20), ")".repeat(20));
        assert!(p("width", &deep).is_err());
    }

    #[test]
    fn css_wide_keywords() {
        assert_eq!(p("color", "inherit"), Ok(Specified::Inherit));
        assert_eq!(p("color", "initial"), Ok(Specified::Initial));
        assert!(p("color", "unset").unwrap_err().contains("excluded"));
        assert!(matches!(p("color", "var(--fg, red)"), Ok(Specified::Unresolved(_))));
    }

    #[test]
    fn font_family_must_end_in_a_stack() {
        assert_eq!(val("font-family", "\"Inter\", sans"), V::Family { web: vec!["Inter".into()], stack: "sans" });
        assert!(p("font-family", "Inter").unwrap_err().contains("END with a profile stack"));
        assert!(p("font-family", "Arial, sans").unwrap_err().contains("@font-face"));
    }

    #[test]
    fn grid_tracks_and_lines() {
        assert_eq!(val("grid-template-columns", "repeat(3, 1fr) 200px").clone(), V::Tracks(vec![Track::Fr(1.0), Track::Fr(1.0), Track::Fr(1.0),
            Track::Len(Length { v: 200.0, unit: Unit::Px })]));
        assert!(p("grid-template-columns", "repeat(auto-fill, 10px)").unwrap_err().contains("integer"));
        assert!(p("grid-template-columns", "repeat(2000, 1px)").unwrap_err().contains("1024"));
        assert_eq!(val("grid-row-start", "span 2"), V::Line(GridLine::Span(2)));
        assert!(p("grid-row-start", "header").is_err());
    }

    /// ★ min()/max()/clamp() are admitted inside calc() and on their own
    /// (§3.11): they are as bounded and deterministic as the rest of calc,
    /// and modern CSS cannot be read without them.
    #[test]
    fn min_max_clamp() {
        assert!(matches!(val("width", "min(300px, 80vw)"), V::Calc(_)));
        assert!(matches!(val("width", "max(10px, 2em)"), V::Calc(_)));
        assert!(matches!(val("width", "clamp(10px, 50%, 100px)"), V::Calc(_)));
        assert!(matches!(val("width", "calc(min(300px, 80vw) + 4px)"), V::Calc(_)));
        // Type checking still applies: a number is not a length.
        assert!(p("width", "min(10px, 2)").unwrap_err().contains("compares"));
        assert!(p("width", "clamp(10px, 2, 100px)").unwrap_err().contains("compares"));
        assert!(p("width", "clamp(10px, 20px)").unwrap_err().contains("three"));
        assert!(p("width", "min()").is_err());
    }

    #[test]
    fn paint_values() {
        assert!(matches!(val("background-image", "linear-gradient(to right, red, #00f 80%)"), V::Gradient { angle_deg: 90.0, .. }));
        assert!(p("background-image", "radial-gradient(red, blue)").unwrap_err().contains("linear-gradient only"));
        assert!(matches!(val("box-shadow", "0 2px 4px 0 rgba(0,0,0,0.2)"), V::Shadow { .. }));
        assert!(matches!(val("transform", "translate(10px, 50%) rotate(45deg) scale(2)"), V::Transform(t) if t.len() == 3));
        assert!(p("transform", "skew(10deg)").unwrap_err().contains("translate, scale, rotate"));
        assert_eq!(val("opacity", "1.7"), V::Num(1.0), "opacity clamps (§3.8)");
        assert_eq!(val("aspect-ratio", "16 / 9"), V::Num(16.0 / 9.0));
    }
}
