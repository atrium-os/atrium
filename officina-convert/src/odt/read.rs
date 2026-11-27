//! `.odt` in (OpenDocument text, ODF 1.2/1.3), for the converter process.
//!
//! Read: headings and paragraphs with their named and automatic styles,
//! paragraph properties (alignment, margins, indent, spacing, line height, keeps,
//! page breaks, widows), spans (weight, style, underline, line-through, size,
//! colour, background, font), links, spaces, tabs and line breaks, lists (bullet
//! or numbered by their list style, level from nesting, continued lists), tables
//! (rows of cells of paragraphs, column widths), pictures in frames (kept in
//! line, with their bytes and frame size), page size and margins, and the
//! default master page's header and footer (page number and count fields become
//! `{page}` and `{pages}`).
//!
//! Footnotes and endnotes (`text:note`), superscript and subscript, and
//! annotations as comments (ranges, replies by LibreOffice's parent name, resolved)
//! are read.
//!
//! Tracked changes (`text:tracked-changes`): insertions (across paragraph
//! breaks too), deletions with what they took out, and formatting changes (the
//! formatting before from our `officina:old-style`, else plain) are read.
//!
//! Reported, not carried: notes inside notes or headers, annotations there,
//! sections and columns, frames anchored to the page (kept in line), tables
//! nested more than one deep (their text kept), fields other than page numbers (their
//! text kept), drawings and charts, embedded objects.

use officina_text::model::PageSetup;
use std::collections::HashMap;
use std::sync::Arc;

use officina_text::fields;
use officina_text::model::{CellBorders, CellLook, CellPad, RowLook, VAlign, TableBorders, TableLook, MAX_CELL_PAD, Border, BorderStyle, ParaBorders, PageBorders, MAX_BORDER_SPACE, DropCap, DROP_CAP_LINES, ListDef, ListLevel, NumFormat, LineNumbering, LineRestart, MAX_LIST_START, Caps, MAX_SPACING, SCALE_RANGE, EFFECT_OUTLINE, EFFECT_SHADOW, EFFECT_EMBOSS, EFFECT_ENGRAVE, Float, HAlign, Rel, Wrap, MAX_FLOAT_OFFSET, Section, SectionStart, MAX_COLUMNS, BlockId, Field, Leader, TabAlign, TabStop, MAX_TABS, DelPiece, Deletion, FmtChange, RevId, Revision, Align, Block, CellRef, CharAttrs, Comment, CommentId, MAX_COMMENT_TEXT, HeaderFooter, Margins, ImageData, LineSpacing, ListKind, ListRef, Note, NoteId, NoteKind, ObjectRef, ParaProps, Style, StyleId, StyleKind, TextDoc, VertAlign, MAX_LIST_LEVEL, MAX_NOTE_PARAS, MAX_MARGIN_PARAS, MAX_OBJECT_TWIPS, MAX_STYLES, OBJECT_CHAR};

use crate::report::{Kind, Report};
use crate::xml::{self, El, Node};
use crate::zip::Archive;

#[derive(Debug)]
pub enum OdtError {
    Zip(crate::zip::ZipError),
    Xml(String),
    Bad(String),
}

impl std::fmt::Display for OdtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OdtError::Zip(e) => write!(f, "{e}"),
            OdtError::Xml(e) => write!(f, "damaged document XML: {e}"),
            OdtError::Bad(e) => write!(f, "not a document this can read: {e}"),
        }
    }
}

impl std::error::Error for OdtError {}

impl From<crate::zip::ZipError> for OdtError {
    fn from(e: crate::zip::ZipError) -> Self {
        OdtError::Zip(e)
    }
}

pub use crate::docx::read::Loaded;

fn part(a: &Archive, name: &str) -> Result<Option<El>, OdtError> {
    match a.read_name(name)? {
        Some(b) => xml::parse(&b).map(Some).map_err(|e| OdtError::Xml(format!("{name}: {e}"))),
        None => Ok(None),
    }
}

/// An ODF length (`1.27cm`, `0.5in`, `12pt`, `3mm`, `1pc`, `96px`) in twips.
/// The key the default graphic style is kept under.
const DEFAULT_GRAPHIC: &str = "\u{0}default";

/// A drawing's frame (twips, from its anchor) and turn (60,000ths of a
/// degree, clockwise): `draw:transform` turns about the frame's corner and
/// moves it; ours turns about the middle.
/// A dash style's kind, by its name (ours and LibreOffice's say what they
/// are: "Dash_20_Dot", "Fine_20_Dotted", "Long_20_Dash").
fn dash_of(name: Option<&str>) -> officina_text::model::Dash {
    use officina_text::model::Dash;
    let n = name.unwrap_or("").to_ascii_lowercase();
    if n.contains("dot") && n.contains("dash") {
        Dash::DashDot
    } else if n.contains("dot") {
        Dash::Dot
    } else if n.contains("long") {
        Dash::LongDash
    } else {
        Dash::Dash
    }
}

fn odf_place(e: &El) -> (f64, f64, f64, f64, i32) {
    let l = |k: &str| e.attr(k).and_then(length).unwrap_or(0) as f64;
    let (x, y, w, h) = (l("svg:x"), l("svg:y"), l("svg:width").max(0.0), l("svg:height").max(0.0));
    let Some(tf) = e.attr("draw:transform") else { return (x, y, w, h, 0) };
    let (mut turn, mut tx, mut ty) = (0.0f64, x, y);
    let mut rest = tf;
    for _ in 0..16 {
        let Some(open) = rest.find('(') else { break };
        let Some(close) = rest[open..].find(')') else { break };
        let name = rest[..open].trim().trim_start_matches(',').trim();
        let args: Vec<&str> = rest[open + 1..open + close].split([' ', ',']).filter(|s| !s.is_empty()).collect();
        rest = &rest[open + close + 1..];
        let len = |i: usize| args.get(i).and_then(|v| length(v).map(|t| t as f64).or_else(|| v.parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| v / 1.763_888)));
        match name {
            "rotate" => turn += args.first().and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(0.0),
            "translate" => {
                tx = len(0).unwrap_or(0.0);
                ty = len(1).unwrap_or(0.0);
            }
            _ => {}
        }
    }
    // The corner at (tx, ty), the box turned by -turn (counter-clockwise
    // in ODF) about it: its middle, then our frame round the middle.
    let (s, c) = (-turn).sin_cos();
    let (cx, cy) = (tx + c * w / 2.0 - s * h / 2.0, ty + s * w / 2.0 + c * h / 2.0);
    let deg = (-turn.to_degrees()).rem_euclid(360.0);
    let rot = if deg.is_finite() { ((deg * 60_000.0).round() as i64 % 21_600_000) as i32 } else { 0 };
    (cx - w / 2.0, cy - h / 2.0, w, h, rot)
}

pub fn length(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic() || c == '%').unwrap_or(s.len());
    let v: f64 = s[..split].trim().parse().ok().filter(|v: &f64| v.is_finite())?;
    let per = match &s[split..] {
        "in" => 1440.0,
        "cm" => 1440.0 / 2.54,
        "mm" => 144.0 / 2.54,
        "pt" => 20.0,
        "pc" => 240.0,
        "px" => 15.0,
        "twip" => 1.0,
        _ => return None,
    };
    Some((v * per).round().clamp(-1e9, 1e9) as i64)
}

fn tw(s: Option<&str>) -> Option<i32> {
    s.and_then(length).map(|v| v.clamp(-31680, 31680) as i32)
}

fn color(s: Option<&str>) -> Option<u32> {
    let s = s?.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// Text properties (`style:text-properties`).
pub(crate) fn text_props(e: &El, fonts: &HashMap<String, String>) -> CharAttrs {
    let mut a = CharAttrs::default();
    if let Some(w) = e.attr("fo:font-weight") {
        a.bold = Some(matches!(w, "bold" | "600" | "700" | "800" | "900"));
    }
    if let Some(s) = e.attr("fo:font-style") {
        a.italic = Some(s == "italic" || s == "oblique");
    }
    if let Some(u) = e.attr("style:text-underline-style") {
        a.underline = Some(u != "none");
    }
    if let Some(u) = e.attr("style:text-line-through-style") {
        a.strike = Some(u != "none");
        if let (true, Some(t)) = (u != "none", e.attr("style:text-line-through-type")) {
            a.dstrike = Some(t == "double");
        }
    }
    // Upper case wins over small capitals, as in Word.
    match (e.attr("fo:text-transform"), e.attr("fo:font-variant")) {
        (Some("uppercase"), _) => a.caps = Some(Caps::All),
        (_, Some("small-caps")) => a.caps = Some(Caps::Small),
        (Some("none"), _) | (_, Some("normal")) => a.caps = Some(Caps::Normal),
        _ => {}
    }
    if let Some(d) = e.attr("text:display") {
        a.hidden = Some(d == "none");
    }
    if let Some(v) = e.attr("fo:letter-spacing") {
        a.spacing = if v == "normal" { Some(0) } else { length(v).map(|t| t.clamp(-(MAX_SPACING as i64), MAX_SPACING as i64) as i16) };
    }
    if let Some(v) = e.attr("style:text-scale").and_then(|v| v.trim_end_matches('%').trim().parse::<f64>().ok()).filter(|v| v.is_finite()) {
        a.scale = Some((v.round() as i64).clamp(*SCALE_RANGE.start() as i64, *SCALE_RANGE.end() as i64) as u16);
    }
    let mut fx = None;
    let mut set = |bit: u8, v: bool| fx = Some(if v { fx.unwrap_or(0) | bit } else { fx.unwrap_or(0) & !bit });
    if let Some(v) = e.attr("style:text-outline") {
        set(EFFECT_OUTLINE, v == "true");
    }
    if let Some(v) = e.attr("fo:text-shadow") {
        set(EFFECT_SHADOW, v != "none");
    }
    if let Some(v) = e.attr("style:font-relief") {
        set(EFFECT_EMBOSS, v == "embossed");
        set(EFFECT_ENGRAVE, v == "engraved");
    }
    a.effects = fx;
    if let Some(z) = e.attr("fo:font-size").filter(|z| !z.ends_with('%')).and_then(length) {
        a.size = Some((z / 10).clamp(2, 3276) as u16);
    }
    if let Some(t) = e.attr("style:text-position") {
        let first = t.split_whitespace().next().unwrap_or("");
        a.vert = match first {
            "super" => Some(VertAlign::Super),
            "sub" => Some(VertAlign::Sub),
            _ => match first.trim_end_matches('%').parse::<f64>() {
                Ok(v) if v > 0.0 => Some(VertAlign::Super),
                Ok(v) if v < 0.0 => Some(VertAlign::Sub),
                Ok(_) => Some(VertAlign::Baseline),
                Err(_) => None,
            },
        };
    }
    a.color = color(e.attr("fo:color"));
    a.highlight = color(e.attr("fo:background-color"));
    if let Some(f) = e.attr("style:font-name").or_else(|| e.attr("fo:font-family")) {
        let name = fonts.get(f).cloned().unwrap_or_else(|| f.trim_matches(['\'', '"']).to_string());
        a.family = Some(Arc::from(&name[..name.len().min(255)]));
    }
    // The East Asian language (style:language-asian + style:country-asian).
    if let Some(l) = e.attr("style:language-asian").filter(|l| *l != "none") {
        let tag = match e.attr("style:country-asian").filter(|c| *c != "none") {
            Some(c) => format!("{l}-{c}"),
            None => l.to_string(),
        };
        a.lang_ea = crate::docx::read::lang_tag(&tag);
    }
    a
}

/// Paragraph properties (`style:paragraph-properties`).
pub(crate) fn para_props(e: &El) -> ParaProps {
    let mut p = ParaProps::default();
    p.align = match e.attr("fo:text-align") {
        Some("start" | "left") => Some(Align::Start),
        Some("center") => Some(Align::Center),
        Some("end" | "right") => Some(Align::End),
        Some("justify") => Some(Align::Justify),
        _ => None,
    };
    p.indent_start = tw(e.attr("fo:margin-left"));
    p.indent_end = tw(e.attr("fo:margin-right"));
    p.first_line = tw(e.attr("fo:text-indent"));
    p.space_before = tw(e.attr("fo:margin-top")).map(|v| v.max(0));
    p.space_after = tw(e.attr("fo:margin-bottom")).map(|v| v.max(0));
    if let Some(h) = e.attr("fo:line-height") {
        p.line_spacing = match h.strip_suffix('%') {
            Some(pc) => pc.trim().parse::<f64>().ok().filter(|v| v.is_finite()).map(|v| LineSpacing::Multiple(v.round().clamp(25.0, 1000.0) as u16)),
            None => length(h).map(|t| LineSpacing::Exact(t.clamp(20, 31680) as i32)),
        };
    } else if let Some(h) = e.attr("style:line-height-at-least").and_then(length) {
        p.line_spacing = Some(LineSpacing::AtLeast(h.clamp(0, 31680) as i32));
    }
    if let Some(k) = e.attr("fo:keep-with-next") {
        p.keep_with_next = Some(k == "always");
    }
    if let Some(k) = e.attr("fo:keep-together") {
        p.keep_together = Some(k == "always");
    }
    match e.attr("style:writing-mode") {
        Some("rl-tb" | "rl" | "tb-rl") => p.rtl = Some(true),
        Some("lr-tb" | "lr") => p.rtl = Some(false),
        _ => {}
    }
    if let Some(b) = e.attr("fo:break-before") {
        p.page_break_before = Some(b == "page");
    }
    if let Some(w) = e.attr("fo:widows").or_else(|| e.attr("fo:orphans")) {
        p.widows = Some(w.trim().parse::<i32>().unwrap_or(0) > 0);
    }
    // Positions from the paragraph's start indent, as ODF has them (made
    // ours, from the column's edge, when a paragraph is read).
    if let Some(t) = e.child("style:tab-stops") {
        let mut v: Vec<TabStop> = Vec::new();
        for x in t.children("style:tab-stop") {
            let Some(pos) = tw(x.attr("style:position")) else { continue };
            let align = match x.attr("style:type") {
                Some("center") => TabAlign::Center,
                Some("right") => TabAlign::End,
                Some("char") => TabAlign::Decimal,
                _ => TabAlign::Start,
            };
            // The text drawn wins over the line style (LibreOffice writes "-"
            // over a solid style for dashes).
            let leader = match (x.attr("style:leader-text").and_then(|t| t.chars().next()), x.attr("style:leader-style")) {
                (_, Some("none")) => Leader::None,
                (Some('.' | '·' | '…'), _) => Leader::Dots,
                (Some('-' | '–'), _) => Leader::Dashes,
                (Some('_'), _) => Leader::Line,
                (_, Some("dotted" | "dot-dash" | "dot-dot-dash")) => Leader::Dots,
                (_, Some("dash" | "long-dash")) => Leader::Dashes,
                (_, Some("solid" | "wave")) => Leader::Line,
                _ => Leader::None,
            };
            if v.len() < MAX_TABS {
                v.push(TabStop { pos, align, leader });
            }
        }
        v.sort_by_key(|s| s.pos);
        v.dedup_by_key(|s| s.pos);
        p.tabs = Some(v);
    }
    // Borders: each side's own, else all four from fo:border; the room
    // beside each from its padding.
    let sides = ["top", "bottom", "left", "right"];
    let mut b = [None; 4];
    let mut any = false;
    for (k, side) in sides.iter().enumerate() {
        let v = e.attr(&format!("fo:border-{side}")).or_else(|| e.attr("fo:border"));
        if v.is_some() {
            any = true;
        }
        b[k] = v.and_then(odf_border).map(|mut x| {
            let pad = e.attr(&format!("fo:padding-{side}")).or_else(|| e.attr("fo:padding")).and_then(length).unwrap_or(0);
            x.space = (pad / 20).clamp(0, 31) as u16;
            x
        });
    }
    if any {
        p.borders = Some(ParaBorders { top: b[0], bottom: b[1], start: b[2], end: b[3], between: None });
    }
    match e.attr("fo:background-color") {
        Some("transparent") => p.shading = Some(None),
        c @ Some(_) => p.shading = color(c).map(Some),
        None => {}
    }
    if let Some(v) = e.attr("text:number-lines") {
        p.line_numbers = Some(v == "true");
    }
    if let Some(d) = e.child("style:drop-cap") {
        let lines = d.attr("style:lines").and_then(|v| v.parse::<u8>().ok()).unwrap_or(1);
        let length = d.attr("style:length").map_or(Some(1), |v| v.parse::<u32>().ok().or(if v == "word" { Some(1) } else { None }));
        p.drop_cap = Some((DROP_CAP_LINES.contains(&lines) && length.is_some_and(|n| n >= 1)).then(|| DropCap { lines, distance: tw(d.attr("style:distance")).unwrap_or(0).max(0), margin: false }));
    }
    p
}

/// A style's hyphenation, from its text properties.
fn hyphenation_of(s: &El) -> Option<bool> {
    s.child("style:text-properties").and_then(|t| t.attr("fo:hyphenate")).map(|v| v == "true")
}

/// A cell style's look: its borders (all round or per side), background,
/// vertical alignment and padding.
fn odf_cell_look(p: &El) -> CellLook {
    let all = p.attr("fo:border").map(odf_border);
    let side = |name: &str| p.attr(name).map(odf_border).or(all).or(Some(None));
    let pad_all = p.attr("fo:padding").and_then(length);
    let pad = |name: &str| p.attr(name).and_then(length).or(pad_all).map(|v| v.clamp(0, MAX_CELL_PAD as i64) as u16);
    let (pt, pb, ps, pe) = (pad("fo:padding-top"), pad("fo:padding-bottom"), pad("fo:padding-left"), pad("fo:padding-right"));
    let any_pad = pt.is_some() || pb.is_some() || ps.is_some() || pe.is_some();
    CellLook {
        borders: CellBorders { top: side("fo:border-top"), bottom: side("fo:border-bottom"), start: side("fo:border-left"), end: side("fo:border-right") },
        shading: Some(p.attr("fo:background-color").and_then(|c| if c == "transparent" { None } else { color(Some(c)) })),
        valign: match p.attr("style:vertical-align") {
            Some("middle") => VAlign::Center,
            Some("bottom") => VAlign::Bottom,
            _ => VAlign::Top,
        },
        pad: any_pad.then(|| CellPad { top: pt.unwrap_or(0), bottom: pb.unwrap_or(0), start: ps.unwrap_or(0), end: pe.unwrap_or(0) }),
    }
}

/// An `fo:border` value: width, style and colour in any order; `none` is none.
fn odf_border(v: &str) -> Option<Border> {
    let mut b = Border { space: 0, ..Border::default() };
    let mut seen = false;
    for t in v.split_whitespace() {
        match t {
            "none" | "hidden" => return None,
            "solid" | "groove" | "ridge" | "inset" | "outset" => seen = true,
            "double" => {
                b.style = BorderStyle::Double;
                seen = true;
            }
            "dotted" => {
                b.style = BorderStyle::Dotted;
                seen = true;
            }
            "dashed" => {
                b.style = BorderStyle::Dashed;
                seen = true;
            }
            "thin" => b.width = 2,
            "medium" => b.width = 6,
            "thick" => b.width = 12,
            _ if t.starts_with('#') => b.color = color(Some(t)).filter(|&c| c != 0),
            _ => {
                if let Some(w) = length(t) {
                    b.width = (w * 8 / 20).clamp(1, 96) as u16;
                }
            }
        }
    }
    seen.then_some(b)
}

/// A `text:list-level-style-*` as a level (`level` from 0).
fn odf_list_level(l: &El, level: u8) -> (ListKind, ListLevel) {
    let bullet = l.name == "text:list-level-style-bullet" || l.name == "text:list-level-style-image";
    let kind = if bullet { ListKind::Bullet } else { ListKind::Numbered };
    let mut lv = ListLevel::builtin(kind, level);
    if bullet {
        if let Some(c) = l.attr("text:bullet-char") {
            // Symbol-font bullets are private-use characters: a plain bullet.
            lv.text = c.chars().map(|c| if ('\u{f000}'..='\u{f0ff}').contains(&c) { '•' } else { c }).collect::<String>().into();
        }
    } else {
        lv.format = match l.attr("style:num-format") {
            Some("a") => NumFormat::LowerLetter,
            Some("A") => NumFormat::UpperLetter,
            Some("i") => NumFormat::LowerRoman,
            Some("I") => NumFormat::UpperRoman,
            Some(f) if f.starts_with("01") => NumFormat::DecimalZero,
            Some("") => NumFormat::None,
            _ => NumFormat::Decimal,
        };
        let shown = l.attr("text:display-levels").and_then(|v| v.parse::<u8>().ok()).unwrap_or(1).clamp(1, level + 1);
        let nums: Vec<String> = (level + 2 - shown..=level + 1).map(|k| format!("%{k}")).collect();
        let body = if lv.format == NumFormat::None { String::new() } else { nums.join(".") };
        lv.text = match l.attr("loext:num-list-format") {
            // LibreOffice's whole label: %1% for level 1's number.
            Some(w) => w.replace('%', "\u{1}").split('\u{1}').enumerate().map(|(i, part)| if i % 2 == 1 && part.len() == 1 && part.as_bytes()[0].is_ascii_digit() { format!("%{part}") } else { part.to_string() }).collect::<String>().into(),
            None => format!("{}{}{}", l.attr("style:num-prefix").unwrap_or(""), body, l.attr("style:num-suffix").unwrap_or("")).into(),
        };
        if let Some(v) = l.attr("text:start-value").and_then(|v| v.parse::<u32>().ok()) {
            lv.start = v.min(MAX_LIST_START);
        }
    }
    let props = l.child("style:list-level-properties");
    if let Some(a) = props.and_then(|p| p.child("style:list-level-label-alignment")) {
        if let Some(m) = tw(a.attr("fo:margin-left")) {
            lv.indent = m;
        }
        if let Some(t) = tw(a.attr("fo:text-indent")) {
            lv.hang = (-t).max(0);
        }
    } else if let Some(p) = props {
        // The older mode: the label's space from where it starts.
        let start = tw(p.attr("text:space-before")).unwrap_or(0);
        let width = tw(p.attr("text:min-label-width")).unwrap_or(LIST_HANG_ODF);
        lv.indent = start + width;
        lv.hang = width.max(0);
    }
    (kind, lv.bounded())
}

/// The label width ODF's older list mode gives when none is set.
const LIST_HANG_ODF: i32 = 360;

/// The body's first paragraph, in sections and lists too.
fn first_para(e: &El, depth: usize) -> Option<&El> {
    for x in e.els() {
        match x.name.as_str() {
            "text:p" | "text:h" => return Some(x),
            "text:section" | "text:list" | "text:list-item" | "text:list-header" if depth < 16 => {
                if let Some(p) = first_para(x, depth + 1) {
                    return Some(p);
                }
            }
            "table:table" => return None,
            _ => {}
        }
    }
    None
}

/// A master page's size and margins (its header and footer heights added to
/// them, as ODF puts those inside the margins), and its columns if any.
fn master_setup(st: &El, master: &El, base: PageSetup) -> (PageSetup, Option<Section>) {
    let mut out = base;
    let mut cols = None;
    let layout = master.attr("style:page-layout-name").and_then(|n| {
        st.child("office:automatic-styles").and_then(|a| a.children("style:page-layout").find(|l| l.attr("style:name") == Some(n)))
    });
    if let Some(pp) = layout.and_then(|l| l.child("style:page-layout-properties")) {
        let g = &mut out;
        let t = |k: &str| pp.attr(k).and_then(length).map(|v| v.clamp(0, 31680 * 2) as i32);
        g.width = t("fo:page-width").unwrap_or(g.width).max(1440);
        g.height = t("fo:page-height").unwrap_or(g.height).max(1440);
        g.margin_top = t("fo:margin-top").unwrap_or(g.margin_top);
        g.margin_bottom = t("fo:margin-bottom").unwrap_or(g.margin_bottom);
        g.margin_start = t("fo:margin-left").unwrap_or(g.margin_start);
        g.margin_end = t("fo:margin-right").unwrap_or(g.margin_end);
        if let Some(c) = pp.child("style:columns") {
            cols = Some(odf_columns(c));
        }
        // A page border sits inside the margins, the text its padding further in.
        if let Some(b) = page_borders(pp) {
            let inset = |x: &Option<Border>, pad: &str| x.map_or(0, |x| x.thickness() as i64 + pp.attr(pad).or_else(|| pp.attr("fo:padding")).and_then(length).unwrap_or(0));
            g.margin_top = (g.margin_top as i64 + inset(&b.top, "fo:padding-top")).clamp(0, 63360) as i32;
            g.margin_bottom = (g.margin_bottom as i64 + inset(&b.bottom, "fo:padding-bottom")).clamp(0, 63360) as i32;
            g.margin_start = (g.margin_start as i64 + inset(&b.start, "fo:padding-left")).clamp(0, 63360) as i32;
            g.margin_end = (g.margin_end as i64 + inset(&b.end, "fo:padding-right")).clamp(0, 63360) as i32;
        }
        // ODF puts the header and footer inside the margins; their heights add to them.
        // With dynamic spacing (LibreOffice's import of Word files) the spacing lies
        // inside the minimum height rather than after it.
        for (tag, top) in [("style:header-style", true), ("style:footer-style", false)] {
            if let Some(h) = layout.and_then(|l| l.child(tag)).and_then(|s| s.child("style:header-footer-properties")) {
                let spacing = if h.attr("style:dynamic-spacing") == Some("true") { 0 } else { h.attr(if top { "fo:margin-bottom" } else { "fo:margin-top" }).and_then(length).unwrap_or(0) };
                let extra = h.attr("fo:min-height").and_then(length).unwrap_or(0) + spacing;
                if top && master.child("style:header").is_some() {
                    g.margin_top = (g.margin_top as i64 + extra).clamp(0, 63360) as i32;
                }
                if !top && master.child("style:footer").is_some() {
                    g.margin_bottom = (g.margin_bottom as i64 + extra).clamp(0, 63360) as i32;
                }
            }
        }
    }
    (out, cols)
}

/// A page layout's borders, `space` the padding between them and the text.
fn page_borders(pp: &El) -> Option<PageBorders> {
    let side = |k: &str, pad: &str| {
        let mut b = pp.attr(k).or_else(|| pp.attr("fo:border")).and_then(odf_border)?;
        let p = pp.attr(pad).or_else(|| pp.attr("fo:padding")).and_then(length).unwrap_or(0);
        b.space = (p / 20).clamp(0, MAX_BORDER_SPACE as i64) as u16;
        Some(b)
    };
    let b = PageBorders { top: side("fo:border-top", "fo:padding-top"), bottom: side("fo:border-bottom", "fo:padding-bottom"), start: side("fo:border-left", "fo:padding-left"), end: side("fo:border-right", "fo:padding-right"), from_page: false };
    (b.top.is_some() || b.bottom.is_some() || b.start.is_some() || b.end.is_some()).then_some(b)
}

/// A master page's layout properties.
fn layout_props<'a>(st: &'a El, master: &El) -> Option<&'a El> {
    let n = master.attr("style:page-layout-name")?;
    st.child("office:automatic-styles")?.children("style:page-layout").find(|l| l.attr("style:name") == Some(n))?.child("style:page-layout-properties")
}

/// A `style:columns`: count, gap and the line between. Columns of their own
/// widths come out equal.
fn odf_columns(c: &El) -> Section {
    let n = c.attr("fo:column-count").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).clamp(1, MAX_COLUMNS as i64) as u8;
    let cols: Vec<&El> = c.children("style:column").collect();
    let gap = c.attr("fo:column-gap").and_then(length).or_else(|| {
        let a = cols.first()?.attr("fo:end-indent").and_then(length).unwrap_or(0);
        let b = cols.get(1)?.attr("fo:start-indent").and_then(length).unwrap_or(0);
        Some(a + b)
    });
    // LibreOffice writes a separator only when it draws one, and draws it
    // even styled "none" (as it writes Word's).
    let rule = c.child("style:column-sep").is_some_and(|s| s.attr("style:width").is_none_or(|w| w.trim_start().trim_start_matches(['0', '.']).starts_with(|c: char| c.is_ascii_digit())));
    Section { start: SectionStart::Continuous, columns: n, gap: gap.unwrap_or(720).clamp(0, 2880) as i32, rule, page: None }
}

/// A style as the file has it.
#[derive(Clone, Default)]
struct OStyle {
    parent: Option<String>,
    para: ParaProps,
    chars: CharAttrs,
    list_style: Option<String>,
    outline: Option<u8>,
    /// The master page a paragraph style starts.
    master: Option<String>,
}

fn props_over(over: &ParaProps, under: &ParaProps) -> ParaProps {
    over.over(under)
}

struct Reader<'a> {
    archive: &'a Archive<'a>,
    doc: TextDoc,
    rep: Report,
    /// Cross-references: bookmark names fields refer to (by index, see
    /// [`fields::UNBOUND`]), what each showed, bookmarks in the paragraph
    /// being read, the paragraph each bookmark is on; reading a table of
    /// contents' body.
    ref_names: Vec<String>,
    ref_index: HashMap<String, u64>,
    ref_shown: HashMap<Field, String>,
    open_marks: Vec<String>,
    marks: HashMap<String, BlockId>,
    in_toc: bool,
    /// Tab stops count from the start indent (ODF's way; LibreOffice turns it
    /// off for documents from Word).
    tabs_from_indent: bool,
    fonts: HashMap<String, String>,
    /// Paragraph and text styles by name, named and automatic.
    styles: HashMap<String, OStyle>,
    /// Our style for a named paragraph or text style.
    ours: HashMap<String, StyleId>,
    /// List styles: kind per level.
    lists: HashMap<String, Vec<ListKind>>,
    /// List styles as our lists' definitions.
    list_defs: HashMap<String, ListDef>,
    /// Column widths by column style name.
    columns: HashMap<String, u32>,
    /// Table cell, row and table styles' looks.
    cell_styles: HashMap<String, CellLook>,
    /// The span of the cell last started at each grid column.
    span_above: HashMap<usize, usize>,
    row_styles: HashMap<String, RowLook>,
    table_styles: HashMap<String, (officina_text::model::HAlign, i32)>,
    /// Graphic styles: parent and graphic properties, by name.
    graphic: HashMap<String, (Option<String>, Vec<(String, String)>)>,
    /// Columns of section styles by name.
    sec_styles: HashMap<String, Section>,
    /// The page's own columns, and section marks to make once all is read:
    /// (block, section, a start rather than a return to the enclosing one).
    page_cols: Section,
    sec_marks: Vec<(usize, Section, bool)>,
    /// Every master page's page setup, and paragraphs that start one.
    masters: HashMap<String, PageSetup>,
    /// Master pages kept to right (odd) or left (even) pages: a section
    /// starting one starts on such a page.
    usages: HashMap<String, SectionStart>,
    master_marks: Vec<(usize, String)>,
    sec_stack: Vec<Section>,
    /// The last list id per list style, for continued lists.
    last_list: HashMap<String, u32>,
    next_list: u32,
    next_table: u32,
    images: HashMap<String, u32>,
    pictures: u64,
    /// Text boxes and shapes with text met in the paragraph being read: their
    /// text follows the paragraph.
    boxes: Vec<El>,
    box_depth: usize,
    /// Shapes' text to read once the body is read: shape, path to it in
    /// its groups, the element holding the paragraphs.
    shape_text: Vec<(u32, Vec<usize>, El)>,
    /// Reading a shape's text (which holds no shapes).
    in_shape: bool,
    /// Notes anchored in the text, read after it.
    pending: Vec<(NoteId, NoteKind, El)>,
    in_note: bool,
    /// Reading a header or footer: page fields become placeholders.
    in_margin: bool,
    /// The header and footer, read for their paragraphs last.
    margin_els: Vec<(usize, bool, El)>,
    /// Annotations by `office:name`, for their ends and replies.
    annotations: HashMap<String, CommentId>,
    /// Replies and the names of what they answer.
    reply_to: Vec<(CommentId, String)>,
    /// The names `office:annotation-end`s carry.
    ends: std::collections::HashSet<String>,
    /// The comments over the text, sorted, as the paragraph being read began.
    active: Vec<CommentId>,
    /// Where comments start and end in the paragraph being read.
    events: Vec<(u32, CommentId, Ev)>,
    /// Tracked changes: the regions by id, our revision per author, the
    /// insertions open (they may span paragraph breaks).
    regions: HashMap<String, Region>,
    revs: HashMap<String, RevId>,
    ins_open: Vec<(String, RevId)>,
    /// The cell the last body paragraph was in (a break is only ours to mark
    /// inside one cell or outside tables).
    last_cell: Option<Option<CellRef>>,
}

/// A changed region of `text:tracked-changes`.
struct Region {
    kind: RegionKind,
    author: String,
    date: String,
    /// A deletion's content; a format change's old style name.
    el: Option<El>,
    old_style: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegionKind {
    Insert,
    Delete,
    Format,
}

/// A comment range's change at a place in a paragraph.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ev {
    Start,
    End,
    /// An annotation with no end: over the character before (else after) it.
    Point,
}

/// The names of the annotation ends in `e`.
fn annotation_ends(e: &El) -> std::collections::HashSet<String> {
    fn walk(e: &El, out: &mut std::collections::HashSet<String>, depth: usize) {
        for x in e.els() {
            if x.name == "office:annotation-end" {
                if let Some(n) = x.attr("office:name") {
                    out.insert(n.to_string());
                }
            } else if depth < 64 && x.name != "office:annotation" {
                walk(x, out, depth + 1);
            }
        }
    }
    let mut out = std::collections::HashSet::new();
    walk(e, &mut out, 0);
    out
}

/// An annotation's text: a line a paragraph.
fn annotation_text(e: &El) -> String {
    let mut lines = Vec::new();
    for q in e.els().filter(|q| q.name == "text:p" || q.name == "text:h" || q.name == "text:list") {
        lines.push(q.text().split_whitespace().collect::<Vec<_>>().join(" "));
    }
    let mut out = lines.join("\n");
    let mut end = out.len().min(MAX_COMMENT_TEXT);
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out
}

/// Puts the comments the events say over `p`'s runs: `active` over its start,
/// left as they are over its end.
fn apply_comments(p: &mut Para, active: &mut Vec<CommentId>, events: Vec<(u32, CommentId, Ev)>) {
    if active.is_empty() && events.is_empty() {
        return;
    }
    let len = p.text.len() as u32;
    // Changes as (byte, id, add), points made into a start and an end.
    let mut ch: Vec<(u32, CommentId, bool)> = Vec::new();
    for (at, id, ev) in events {
        match ev {
            Ev::Start => ch.push((at, id, true)),
            Ev::End => ch.push((at, id, false)),
            Ev::Point => {
                let at = at.min(len);
                let (s, e) = if at > 0 {
                    let s = p.text[..at as usize].char_indices().next_back().map_or(0, |(i, _)| i as u32);
                    (s, at)
                } else {
                    (0, p.text.chars().next().map_or(0, |c| c.len_utf8() as u32))
                };
                if s < e {
                    ch.push((s, id, true));
                    ch.push((e, id, false));
                }
            }
        }
    }
    ch.sort_by_key(|&(at, _, add)| (at, add));
    // Cut the runs at each change.
    let mut cuts: Vec<u32> = ch.iter().map(|c| c.0).filter(|&a| a > 0 && a < len).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut runs: Vec<(u32, CharAttrs)> = Vec::new();
    for (i, (at, a)) in p.runs.iter().enumerate() {
        let next = p.runs.get(i + 1).map_or(len, |r| r.0);
        runs.push((*at, a.clone()));
        for &c in cuts.iter().filter(|&&c| c > *at && c < next) {
            runs.push((c, a.clone()));
        }
    }
    let mut k = 0;
    let mut set = active.clone();
    for r in runs.iter_mut() {
        while k < ch.len() && ch[k].0 <= r.0 {
            let (_, id, add) = ch[k];
            match (add, set.binary_search(&id)) {
                (true, Err(i)) => set.insert(i, id),
                (false, Ok(i)) => {
                    set.remove(i);
                }
                _ => {}
            }
            k += 1;
        }
        r.1.comments = if set.is_empty() { None } else { Some(set.as_slice().into()) };
    }
    for &(_, id, add) in &ch[k..] {
        match (add, set.binary_search(&id)) {
            (true, Err(i)) => set.insert(i, id),
            (false, Ok(i)) => {
                set.remove(i);
            }
            _ => {}
        }
    }
    p.runs = runs;
    *active = set;
}

#[derive(Default)]
struct Para {
    text: String,
    runs: Vec<(u32, CharAttrs)>,
    /// Tracked: the insertion over the text being read; the formatting change
    /// (revision, index into `olds` of the attributes before).
    ins: Option<RevId>,
    fmt: Option<(RevId, CharAttrs)>,
    olds: Vec<CharAttrs>,
}

impl Para {
    fn push(&mut self, s: &str, a: &CharAttrs) {
        if s.is_empty() {
            return;
        }
        let mut a = a.clone();
        if a.deleted.is_none() {
            a.rev = self.ins;
            if let Some((rev, old)) = &self.fmt {
                // The attributes before, kept until the paragraph is interned.
                let mut o = old.clone();
                o.link = a.link.clone();
                o.rev = a.rev;
                if o != a {
                    let i = match self.olds.iter().position(|x| *x == o) {
                        Some(i) => i,
                        None => {
                            self.olds.push(o);
                            self.olds.len() - 1
                        }
                    };
                    a.fmt = Some(FmtChange { rev: *rev, old: i as u32 });
                }
            }
        }
        let at = self.text.len() as u32;
        if self.runs.last().map(|r| &r.1) != Some(&a) {
            self.runs.push((at, a));
        }
        self.text.push_str(s);
    }
}

/// Where text is being read: the body, or a header or footer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Where {
    Body,
    Margin,
}

impl<'a> Reader<'a> {
    fn font_decls(&mut self, e: Option<&El>) {
        if let Some(d) = e.and_then(|e| e.child("office:font-face-decls")) {
            for f in d.children("style:font-face") {
                if let (Some(n), Some(fam)) = (f.attr("style:name"), f.attr("svg:font-family")) {
                    self.fonts.insert(n.to_string(), fam.trim_matches(['\'', '"']).to_string());
                }
            }
        }
    }

    /// Reads `style:style`, list styles and column styles from a styles block.
    fn style_block(&mut self, e: &El) {
        for s in e.els() {
            match s.name.as_str() {
                "style:default-style" if s.attr("style:family") == Some("graphic") => {
                    let props = s.child("style:graphic-properties").map(|g| g.attrs.clone()).unwrap_or_default();
                    self.graphic.insert(DEFAULT_GRAPHIC.to_string(), (None, props));
                }
                "style:style" => {
                    let Some(name) = s.attr("style:name") else { continue };
                    match s.attr("style:family") {
                        Some("paragraph") | Some("text") => {
                            let st = OStyle {
                                parent: s.attr("style:parent-style-name").map(str::to_string),
                                // A master page named here starts a page.
                                para: {
                                    let mut q = s.child("style:paragraph-properties").map(para_props).unwrap_or_default();
                                    if s.attr("style:master-page-name").is_some_and(|m| !m.is_empty()) && s.attr("style:family") == Some("paragraph") {
                                        q.page_break_before = Some(true);
                                    }
                                    q.hyphenate = hyphenation_of(s);
                                    q
                                },
                                chars: s.child("style:text-properties").map(|t| text_props(t, &self.fonts)).unwrap_or_default(),
                                list_style: s.attr("style:list-style-name").map(str::to_string),
                                outline: s.attr("style:default-outline-level").and_then(|v| v.parse::<u8>().ok()).filter(|v| (1..=9).contains(v)),
                                master: s.attr("style:master-page-name").filter(|m| !m.is_empty() && s.attr("style:family") == Some("paragraph")).map(str::to_string),
                            };
                            self.styles.insert(name.to_string(), st);
                        }
                        Some("graphic") => {
                            let props = s.child("style:graphic-properties").map(|g| g.attrs.clone()).unwrap_or_default();
                            self.graphic.insert(name.to_string(), (s.attr("style:parent-style-name").map(str::to_string), props));
                        }
                        Some("section") => {
                            if let Some(c) = s.child("style:section-properties").and_then(|p| p.child("style:columns")) {
                                self.sec_styles.insert(name.to_string(), odf_columns(c));
                            }
                        }
                        Some("table-cell") => {
                            if let Some(p) = s.child("style:table-cell-properties") {
                                self.cell_styles.insert(name.to_string(), odf_cell_look(p));
                            }
                        }
                        Some("table-row") => {
                            if let Some(p) = s.child("style:table-row-properties") {
                                let exact = p.attr("style:row-height").and_then(length);
                                let min = p.attr("style:min-row-height").and_then(length);
                                let height = exact.or(min).map_or(0, |v| v.clamp(0, u16::MAX as i64) as u16);
                                self.row_styles.insert(name.to_string(), RowLook { header: false, cant_split: p.attr("fo:keep-together") == Some("always"), height, exact: exact.is_some() });
                            }
                        }
                        Some("table") => {
                            if let Some(p) = s.child("style:table-properties") {
                                let align = match p.attr("table:align") {
                                    Some("center") => officina_text::model::HAlign::Center,
                                    Some("right") => officina_text::model::HAlign::End,
                                    _ => officina_text::model::HAlign::Start,
                                };
                                let indent = p.attr("fo:margin-left").and_then(length).map_or(0, |v| v.clamp(-31_680, 31_680) as i32);
                                self.table_styles.insert(name.to_string(), (align, indent));
                            }
                        }
                        Some("table-column") => {
                            if let Some(w) = s.child("style:table-column-properties").and_then(|p| p.attr("style:column-width")).and_then(length) {
                                self.columns.insert(name.to_string(), w.clamp(1, MAX_OBJECT_TWIPS as i64) as u32);
                            }
                        }
                        _ => {}
                    }
                }
                "text:list-style" => {
                    let Some(name) = s.attr("style:name") else { continue };
                    let mut kinds = vec![ListKind::Numbered; MAX_LIST_LEVEL as usize + 1];
                    let mut def = ListDef::builtin(ListKind::Numbered);
                    for l in s.els() {
                        let lv = l.attr("text:level").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 10) - 1;
                        if lv > MAX_LIST_LEVEL as usize {
                            continue;
                        }
                        let (k, x) = odf_list_level(l, lv as u8);
                        kinds[lv] = k;
                        def.levels[lv] = x;
                    }
                    self.lists.insert(name.to_string(), kinds);
                    self.list_defs.insert(name.to_string(), def);
                }
                _ => {}
            }
        }
    }

    /// Our styles for the file's named paragraph and text styles.
    fn named_styles(&mut self, e: Option<&El>) {
        let Some(st) = e.and_then(|e| e.child("office:styles")) else { return };
        let builtin = |name: &str| -> Option<&'static str> {
            Some(match name {
                "Standard" | "Default_20_Paragraph_20_Style" | "Text_20_body" => "Normal",
                "Title" => "Title",
                "Heading_20_1" | "Heading 1" => "Heading 1",
                "Heading_20_2" | "Heading 2" => "Heading 2",
                "Heading_20_3" | "Heading 3" => "Heading 3",
                _ => return None,
            })
        };
        let mut named: Vec<(String, String, StyleKind)> = Vec::new();
        for s in st.children("style:style") {
            let kind = match s.attr("style:family") {
                Some("paragraph") => StyleKind::Para,
                Some("text") => StyleKind::Char,
                _ => continue,
            };
            let Some(name) = s.attr("style:name") else { continue };
            let display = s.attr("style:display-name").unwrap_or(name).to_string();
            named.push((name.to_string(), display, kind));
        }
        // "Text body" is where body text usually is; only "Standard" becomes Normal
        // when both are present.
        let has_standard = named.iter().any(|n| n.0 == "Standard");
        for (name, display, kind) in &named {
            let b = builtin(name).filter(|_| *kind == StyleKind::Para).filter(|&b| !(b == "Normal" && name != "Standard" && has_standard));
            let sid = match b.and_then(|b| self.doc.styles.by_name(b)) {
                Some(k) => k,
                None => {
                    if self.doc.styles.styles.len() >= MAX_STYLES || self.doc.styles.by_name(display).is_some() {
                        continue;
                    }
                    let n = self.doc.styles.styles.len() as StyleId;
                    self.doc.styles.styles.push(Style { name: Arc::from(&display[..display.len().min(255)]), kind: *kind, parent: Some(0).filter(|_| *kind == StyleKind::Para), next: None, para: ParaProps::default(), chars: CharAttrs::default() });
                    n
                }
            };
            self.ours.insert(name.clone(), sid);
        }
        let defaults = st.children("style:default-style").find(|d| d.attr("style:family") == Some("paragraph"));
        // Hyphenation: on for the document when any style turns it on; the
        // default style's setting is Normal's.
        let default_hyph = defaults.and_then(hyphenation_of);
        self.doc.settings.hyphenate = default_hyph == Some(true) || self.styles.values().any(|o| o.para.hyphenate == Some(true));
        if let Some(n) = st.child("text:linenumbering-configuration").filter(|n| n.attr("text:number-lines") == Some("true")) {
            let count_by = n.attr("text:increment").and_then(|v| v.parse::<u16>().ok()).unwrap_or(1).clamp(1, 100);
            let restart = if n.attr("text:restart-on-page") == Some("true") { LineRestart::Page } else { LineRestart::Continuous };
            let distance = tw(n.attr("text:offset")).unwrap_or(0).max(0);
            self.doc.settings.line_numbers = Some(LineNumbering { start: 1, count_by, distance, restart });
        }
        let (dp, dc) = defaults.map(|d| (d.child("style:paragraph-properties").map(para_props).unwrap_or_default(), d.child("style:text-properties").map(|t| text_props(t, &self.fonts)).unwrap_or_default())).unwrap_or_default();
        for (name, _, _) in &named {
            let Some(&sid) = self.ours.get(name) else { continue };
            let Some(o) = self.styles.get(name).cloned() else { continue };
            let mut para = o.para.clone();
            let mut chars = o.chars.clone();
            if sid == 0 {
                para = props_over(&para, &dp);
                chars = chars.over(&dc);
                if self.doc.settings.hyphenate {
                    para.hyphenate = para.hyphenate.or(default_hyph);
                }
            }
            para.outline_level = o.outline.or(self.doc.styles.styles[sid as usize].para.outline_level);
            if o.list_style.as_deref() == Some("") {
                para.list = Some(None);
            } else if let Some(ls) = &o.list_style {
                if let Some(k) = self.lists.get(ls).map(|k| k[0]) {
                    para.list = Some(Some(ListRef { id: self.list_id(ls, true), kind: k, level: 0 }));
                }
            }
            let parent = o.parent.as_ref().and_then(|p| self.ours.get(p).copied()).filter(|&p| p != sid);
            let s = &mut self.doc.styles.styles[sid as usize];
            s.para = para;
            s.chars = chars;
            if sid != 0 {
                s.parent = parent.or(if s.kind == StyleKind::Para { Some(0) } else { None });
            }
        }
        for i in 0..self.doc.styles.styles.len() {
            let mut seen = vec![i];
            let mut k = self.doc.styles.styles[i].parent;
            while let Some(p) = k {
                if seen.contains(&(p as usize)) {
                    self.doc.styles.styles[i].parent = None;
                    break;
                }
                seen.push(p as usize);
                k = self.doc.styles.styles.get(p as usize).and_then(|s| s.parent);
            }
        }
    }

    fn list_id(&mut self, style: &str, continued: bool) -> u32 {
        if continued {
            if let Some(&id) = self.last_list.get(style) {
                return id;
            }
        }
        self.next_list += 1;
        self.last_list.insert(style.to_string(), self.next_list);
        if let Some(d) = self.list_defs.get(style) {
            if self.doc.styles.lists.len() < officina_text::model::MAX_LIST_DEFS {
                self.doc.styles.lists.insert(self.next_list, d.clone());
            }
        }
        self.next_list
    }

    /// A paragraph's style and direct properties from its (maybe automatic) style.
    fn para_style(&self, name: Option<&str>) -> (StyleId, ParaProps, CharAttrs) {
        let Some(name) = name else { return (0, ParaProps::default(), CharAttrs::default()) };
        if let Some(&sid) = self.ours.get(name) {
            return (sid, ParaProps::default(), CharAttrs::default());
        }
        // An automatic style: direct formatting over its parent.
        match self.styles.get(name) {
            Some(o) => {
                let sid = o.parent.as_ref().and_then(|p| self.ours.get(p).copied()).unwrap_or(0);
                let mut para = o.para.clone();
                // Out of its paragraph style's list.
                if o.list_style.as_deref() == Some("") {
                    para.list = Some(None);
                }
                (sid, para, o.chars.clone())
            }
            None => (0, ParaProps::default(), CharAttrs::default()),
        }
    }

    fn span_attrs(&self, name: Option<&str>) -> CharAttrs {
        let Some(name) = name else { return CharAttrs::default() };
        let Some(o) = self.styles.get(name) else { return CharAttrs::default() };
        let mut a = o.chars.clone();
        if let Some(&sid) = self.ours.get(name) {
            a = CharAttrs { style: Some(sid), ..Default::default() };
        } else if let Some(&p) = o.parent.as_ref().and_then(|p| self.ours.get(p)) {
            if self.doc.styles.styles[p as usize].kind == StyleKind::Char {
                a.style = Some(p);
            }
        }
        a
    }

    fn picture(&mut self, frame: &El) -> Option<ObjectRef> {
        let img = frame.child("draw:image")?;
        let href = img.attr("xlink:href")?.trim_start_matches("./").to_string();
        if href.contains("://") {
            self.rep.note(Kind::Dropped, "linked pictures (not fetched)");
            return None;
        }
        let image = match self.images.get(&href) {
            Some(&i) => i,
            None => {
                let bytes = self.archive.read_name(&href).ok().flatten()?;
                if bytes.is_empty() || bytes.len() > crate::docx::read::MAX_PICTURE || self.images.len() >= crate::docx::read::MAX_PICTURES {
                    return None;
                }
                let Some((bytes, w, h)) = crate::raster::normalize(bytes) else {
                    self.rep.note_at(Kind::Dropped, "pictures in formats not shown (SVM, damaged TIFF or SVG)", || href.clone());
                    return None;
                };
                let i = self.doc.images.add(ImageData { bytes: Arc::from(bytes), width: w, height: h });
                self.images.insert(href.clone(), i);
                i
            }
        };
        let px = self.doc.images.get(image).map(|d| (d.width, d.height)).unwrap_or((96, 96));
        let side = |v: i64| v.clamp(1, MAX_OBJECT_TWIPS as i64) as u32;
        let w = frame.attr("svg:width").and_then(length).map(side).unwrap_or(side(px.0 as i64 * 15));
        let h = frame.attr("svg:height").and_then(length).map(side).unwrap_or(side(px.1 as i64 * 15));
        self.pictures += 1;
        let float = (!self.in_note && !matches!(frame.attr("text:anchor-type"), Some("as-char") | None)).then(|| self.frame_float(frame, h));
        Some(ObjectRef { image, width: w, height: h, float, shape: false })
    }

    /// A graphic property of `style`, through its parents.
    fn graphic_prop(&self, style: Option<&str>, key: &str) -> Option<String> {
        let mut cur = style.map(str::to_string);
        for _ in 0..16 {
            let Some((parent, props)) = cur.as_deref().and_then(|c| self.graphic.get(c)) else { break };
            if let Some((_, v)) = props.iter().find(|(k, _)| k == key) {
                return Some(v.clone());
            }
            cur = parent.clone();
        }
        None
    }

    /// As [`graphic_prop`], then the default graphic style (what drawings
    /// fall back on; frames have their own defaults).
    fn drawing_prop(&self, style: Option<&str>, key: &str) -> Option<String> {
        self.graphic_prop(style, key).or_else(|| self.graphic_prop(Some(DEFAULT_GRAPHIC), key))
    }

    /// Whether this reader takes drawing `e` as a shape: in the body text
    /// (not a note, header or another shape).
    fn takes_shape(&self, e: &El, at: Where) -> bool {
        at == Where::Body && !self.in_note && !self.in_shape && (e.name != "draw:frame" || e.child("draw:text-box").is_some())
    }

    /// A drawing anchored in the text as a shape; its text is read after
    /// the body.
    fn odf_shape(&mut self, e: &El) -> Option<ObjectRef> {
        use officina_text::model::{MAX_GROUP_SHAPES, MAX_SHAPES};
        if self.doc.shapes.next_id() as usize >= MAX_SHAPES {
            self.rep.note(Kind::Dropped, "shapes past the limit");
            return None;
        }
        let mut texts = Vec::new();
        let mut budget = MAX_GROUP_SHAPES;
        let (sd, [x, y, w, h]) = self.drawn(e, 0, &mut Vec::new(), &mut texts, &mut budget)?;
        let side = |v: f64| (v.round() as i64).clamp(1, MAX_OBJECT_TWIPS as i64) as u32;
        let (w, h) = (side(w), side(h));
        let id = self.doc.shapes.add(sd)?;
        for (path, el) in texts {
            self.shape_text.push((id, path, el));
        }
        let float = (!matches!(e.attr("text:anchor-type"), Some("as-char") | None)).then(|| {
            let mut f = self.frame_float(e, h);
            // Turned and drawn shapes are placed by their frame.
            let off = |v: f64| (v.round() as i64).clamp(-(MAX_FLOAT_OFFSET as i64), MAX_FLOAT_OFFSET as i64) as i32;
            if f.h_align == HAlign::Offset {
                f.x = off(x);
            }
            if e.attr("svg:y").is_none() || e.attr("draw:transform").is_some() {
                f.y = off(y);
            }
            f
        });
        Some(ObjectRef { image: id, width: w, height: h, float, shape: true })
    }

    /// A drawing element as a shape and its frame (twips, from its anchor):
    /// a group's shapes framed in their bounds.
    fn drawn(&mut self, e: &El, depth: usize, path: &mut Vec<usize>, texts: &mut Vec<(Vec<usize>, El)>, budget: &mut usize) -> Option<(officina_text::model::ShapeData, [f64; 4])> {
        use officina_text::model::{ArrowEnd, Dash, Geom, ShapeData, ShapeKid, ShapeLine, TextAnchor, MAX_GROUP_DEPTH};
        let name = e.name.as_str();
        if name == "draw:g" {
            if depth + 1 >= MAX_GROUP_DEPTH {
                self.rep.note(Kind::Dropped, "groups nested too deep");
                return None;
            }
            let mut kids: Vec<(ShapeData, [f64; 4])> = Vec::new();
            for k in e.els() {
                if *budget == 0 {
                    self.rep.note(Kind::Dropped, "shapes past the limit");
                    break;
                }
                path.push(kids.len());
                let got = self.drawn(k, depth + 1, path, texts, budget);
                path.pop();
                if let Some(x) = got {
                    *budget -= 1;
                    kids.push(x);
                }
            }
            if kids.is_empty() {
                return None;
            }
            let (x0, y0) = kids.iter().fold((f64::MAX, f64::MAX), |a, k| (a.0.min(k.1[0]), a.1.min(k.1[1])));
            let (x1, y1) = kids.iter().fold((f64::MIN, f64::MIN), |a, k| (a.0.max(k.1[0] + k.1[2]), a.1.max(k.1[1] + k.1[3])));
            let (gw, gh) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
            let mut g = ShapeData::new(Geom::preset("rect"));
            let f = |v: f64, s: f64| (v * 100_000.0 / s).round().clamp(-1e8, 1e8) as i32;
            g.kids = kids.into_iter().map(|(sd, [x, y, w, h])| ShapeKid { x: f(x - x0, gw), y: f(y - y0, gh), w: f(w, gw), h: f(h, gh), shape: sd }).collect();
            return Some((g, [x0, y0, gw, gh]));
        }
        if !matches!(name, "draw:custom-shape" | "draw:rect" | "draw:ellipse" | "draw:circle" | "draw:line" | "draw:connector" | "draw:polygon" | "draw:polyline" | "draw:path") && !(name == "draw:frame" && e.child("draw:text-box").is_some()) {
            if name == "draw:frame" {
                self.rep.note(Kind::Dropped, "pictures and objects in groups of shapes");
            }
            return None;
        }
        let st = e.attr("draw:style-name").map(str::to_string);
        let frame = name == "draw:frame";
        let keys = ["style:mirror", "draw:fill", "draw:fill-color", "fo:background-color", "draw:stroke", "draw:stroke-dash", "svg:stroke-width", "svg:stroke-color", "draw:marker-start", "draw:marker-end", "fo:border", "fo:border-top", "fo:padding", "fo:padding-left", "fo:padding-top", "fo:padding-right", "fo:padding-bottom", "draw:textarea-vertical-align"];
        let vals: HashMap<&str, String> = keys.iter().filter_map(|&k| if frame { self.graphic_prop(st.as_deref(), k) } else { self.drawing_prop(st.as_deref(), k) }.map(|v| (k, v))).collect();
        let g = |k: &str| vals.get(k).cloned();
        let (mut x, mut y, mut w, mut h, rot) = odf_place(e);
        let mut sd = ShapeData::new(Geom::preset("rect"));
        sd.rot = rot;
        if let Some(m) = g("style:mirror") {
            sd.flip_h = m.contains("horizontal");
            sd.flip_v = m.contains("vertical");
        }
        let emu = |v: f64| (v * 635.0).round() as i64;
        let mut line_shape = false;
        sd.geom = match name {
            "draw:custom-shape" => match e.child("draw:enhanced-geometry") {
                Some(eg) => {
                    let on = |k: &str| eg.attr(k) == Some("true");
                    sd.flip_h ^= on("draw:mirror-horizontal");
                    sd.flip_v ^= on("draw:mirror-vertical");
                    crate::odp::read::odf_geom(eg, emu(w), emu(h), &mut self.rep)
                }
                None => Geom::preset("rect"),
            },
            "draw:ellipse" | "draw:circle" => Geom::preset("ellipse"),
            "draw:line" | "draw:connector" => {
                let p = |k: &str| e.attr(k).and_then(length).unwrap_or(0) as f64;
                let (x1, y1, x2, y2) = (p("svg:x1"), p("svg:y1"), p("svg:x2"), p("svg:y2"));
                (x, y, w, h) = (x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs());
                sd.flip_h = x2 < x1;
                sd.flip_v = y2 < y1;
                line_shape = true;
                if name == "draw:connector" && e.attr("draw:type").is_some_and(|t| t != "line") {
                    self.rep.note(Kind::Approximate, "bent and curved connectors drawn straight");
                }
                Geom::preset(if name == "draw:connector" { "straightConnector1" } else { "line" })
            }
            "draw:polygon" | "draw:polyline" | "draw:path" => {
                let vb = crate::odp::geom::view_box(e.attr("svg:viewBox")).unwrap_or([0.0, 0.0, w.max(1.0) * 1.763_888, h.max(1.0) * 1.763_888]);
                let paths = if name == "draw:path" { e.attr("svg:d").and_then(|d| crate::odp::geom::svg_path(d, vb)) } else { e.attr("draw:points").and_then(|p| crate::odp::geom::points(p, vb, name == "draw:polygon")) };
                line_shape = name == "draw:polyline";
                match paths {
                    Some(p) => Geom::Custom(p),
                    None => {
                        self.rep.note(Kind::Dropped, "drawings with unreadable outlines");
                        return None;
                    }
                }
            }
            _ => Geom::preset("rect"),
        };
        // Fill: a text frame's background, else the drawing's fill.
        let fill_kind = g("draw:fill");
        sd.fill = match fill_kind.as_deref() {
            _ if line_shape => None,
            Some("none") => None,
            Some("solid") => g("draw:fill-color").as_deref().and_then(|c| color(Some(c))),
            Some("gradient") | Some("hatch") | Some("bitmap") => {
                self.rep.note(Kind::Approximate, "gradient, hatch and picture fills of shapes drawn in one colour");
                g("draw:fill-color").as_deref().and_then(|c| color(Some(c)))
            }
            _ if frame => g("fo:background-color").as_deref().filter(|c| *c != "transparent").and_then(|c| color(Some(c))),
            _ => g("draw:fill-color").as_deref().and_then(|c| color(Some(c))),
        };
        let marker = |m: Option<String>| -> ArrowEnd {
            let Some(m) = m.filter(|m| !m.is_empty()) else { return ArrowEnd::None };
            let n = m.to_ascii_lowercase().replace("_20_", " ");
            if n.contains("circle") || n.contains("oval") {
                ArrowEnd::Oval
            } else if n.contains("diamond") || n.contains("square 45") {
                ArrowEnd::Diamond
            } else if n.contains("concave") {
                ArrowEnd::Stealth
            } else if n.contains("line") || n.contains("open") {
                ArrowEnd::Open
            } else {
                ArrowEnd::Triangle
            }
        };
        sd.line = match g("draw:stroke").as_deref() {
            Some("none") => None,
            Some(k) => {
                let width = g("svg:stroke-width").as_deref().and_then(length).unwrap_or(0).clamp(0, 2880) as i32;
                let color = g("svg:stroke-color").as_deref().and_then(|c| color(Some(c))).unwrap_or(0);
                Some(ShapeLine { color, width, dash: if k == "dash" { dash_of(g("draw:stroke-dash").as_deref()) } else { Dash::Solid }, head: marker(g("draw:marker-start")), tail: marker(g("draw:marker-end")) })
            }
            // A text frame's border: "0.06pt solid #000000".
            None if frame => g("fo:border").or_else(|| g("fo:border-top")).filter(|b| b != "none").map(|b| {
                let mut it = b.split_whitespace();
                let width = it.next().and_then(length).unwrap_or(0).clamp(0, 2880) as i32;
                let style = it.next().unwrap_or("solid");
                let color = it.next().and_then(|c| color(Some(c))).unwrap_or(0);
                ShapeLine { color, width, dash: if matches!(style, "dashed" | "dotted") { Dash::Dash } else { Dash::Solid }, head: ArrowEnd::None, tail: ArrowEnd::None }
            }),
            None => None,
        };
        let pad = |k: &str| g(k).or_else(|| g("fo:padding")).as_deref().and_then(length).map(|v| v.clamp(0, 31_680) as i32);
        for (i, k) in ["fo:padding-left", "fo:padding-top", "fo:padding-right", "fo:padding-bottom"].into_iter().enumerate() {
            if let Some(v) = pad(k) {
                sd.inset[i] = v;
            }
        }
        sd.anchor = match g("draw:textarea-vertical-align").as_deref() {
            Some("middle") => TextAnchor::Middle,
            Some("bottom") => TextAnchor::Bottom,
            _ => TextAnchor::Top,
        };
        let text = if frame { e.child("draw:text-box") } else { Some(e) };
        if let Some(t) = text.filter(|t| t.els().any(|k| matches!(k.name.as_str(), "text:p" | "text:h" | "text:list"))) {
            texts.push((path.clone(), t.clone()));
        }
        Some((sd, [x, y, w.max(1.0), h.max(1.0)]))
    }

    /// Each shape's text: read as blocks at the end, then taken into the shape.
    fn read_shape_text(&mut self) {
        let pending = std::mem::take(&mut self.shape_text);
        if pending.is_empty() {
            return;
        }
        let was = self.in_note;
        self.in_note = true;
        self.in_shape = true;
        for (id, path, el) in pending {
            let before = self.doc.blocks.len();
            self.box_content(&el, None);
            let mut paras = Vec::new();
            while self.doc.blocks.len() > before {
                if let Some(Block::Para(mut p)) = self.doc.blocks.remove(before) {
                    p.props.section = None;
                    paras.push(p);
                }
            }
            let mut paras = officina_text::model::rows_as_lines(paras);
            if paras.len() > officina_text::model::MAX_SHAPE_PARAS {
                self.rep.note(Kind::Approximate, "very long text in shapes (cut)");
                paras.truncate(officina_text::model::MAX_SHAPE_PARAS);
            }
            if let Some(sd) = self.doc.shapes.get(id) {
                let mut sd = (**sd).clone();
                if let Some(k) = sd.kid_mut(&path) {
                    k.paras = paras;
                }
                self.doc.shapes.set(id, Some(sd));
            }
        }
        self.in_shape = false;
        self.in_note = was;
    }

    /// Where a frame anchored to its paragraph, a character or the page
    /// floats, and how text goes round it; `h` is its height (twips).
    fn frame_float(&mut self, frame: &El, h: u32) -> Float {
        let st = frame.attr("draw:style-name");
        let g = |k: &str| self.graphic_prop(st, k);
        let off = |v: Option<&str>| v.and_then(length).unwrap_or(0).clamp(-(MAX_FLOAT_OFFSET as i64), MAX_FLOAT_OFFSET as i64) as i32;
        let wrap = match g("style:wrap").as_deref() {
            Some("none") => Wrap::TopBottom,
            Some("run-through") => {
                if g("style:run-through").as_deref() == Some("background") { Wrap::Behind } else { Wrap::InFront }
            }
            _ => Wrap::Square,
        };
        let gap = ["fo:margin-left", "fo:margin-right", "fo:margin-top", "fo:margin-bottom"].iter().filter_map(|k| g(k).and_then(|v| length(&v))).max().unwrap_or(0).clamp(0, 2880) as i32;
        let page = frame.attr("text:anchor-type") == Some("page");
        let rel = |v: Option<String>, default: Rel| match v.as_deref() {
            Some("page") => Rel::Page,
            Some("page-content") | Some("page-start-margin") | Some("page-end-margin") => Rel::Margin,
            Some(_) => Rel::Text,
            None => default,
        };
        let default = if page { Rel::Page } else { Rel::Text };
        let mut f = Float { wrap, gap, h_rel: rel(g("style:horizontal-rel"), default), v_rel: rel(g("style:vertical-rel"), default), ..Float::default() };
        f.h_align = match g("style:horizontal-pos").as_deref() {
            Some("left") | Some("inside") => HAlign::Start,
            Some("center") => HAlign::Center,
            Some("right") | Some("outside") => HAlign::End,
            _ => HAlign::Offset,
        };
        if f.h_align == HAlign::Offset {
            f.x = off(frame.attr("svg:x"));
        }
        f.y = match g("style:vertical-pos").as_deref() {
            Some(p @ ("middle" | "bottom")) => {
                let pg = &self.doc.page;
                let area = if f.v_rel == Rel::Page { pg.height } else { pg.height - pg.margin_top - pg.margin_bottom };
                (if p == "middle" { (area - h as i32) / 2 } else { area - h as i32 }).max(0)
            }
            Some("top") => 0,
            _ => off(frame.attr("svg:y")),
        };
        f
    }

    /// The inline content of `e` into `p`, under attributes `a`.
    fn inline(&mut self, e: &El, p: &mut Para, a: &CharAttrs, at: Where) {
        for k in &e.kids {
            match k {
                Node::Text(t) => {
                    // XML whitespace collapses in ODF text; text:s carries kept spaces.
                    let mut s = String::with_capacity(t.len());
                    for c in t.chars() {
                        let c = if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c };
                        if c == ' ' && (s.ends_with(' ') || (s.is_empty() && p.text.ends_with(' '))) {
                            continue;
                        }
                        s.push(c);
                    }
                    p.push(&s, a);
                }
                Node::El(x) => match x.name.as_str() {
                    "text:span" => {
                        let over = self.span_attrs(x.attr("text:style-name"));
                        let mut b = over.over(a);
                        if over.style.is_some() && a.style.is_some() && over.style != a.style {
                            b.style = over.style;
                        }
                        self.inline(x, p, &b, at);
                    }
                    "text:a" => {
                        let mut b = a.clone();
                        if let Some(h) = x.attr("xlink:href") {
                            b.link = Some(Arc::from(&h[..h.len().min(8192)]));
                        }
                        self.inline(x, p, &b, at);
                    }
                    "text:s" => {
                        let n = x.attr("text:c").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 1000);
                        p.push(&" ".repeat(n), a);
                    }
                    "text:tab" => p.push("\t", a),
                    "text:line-break" => p.push("\u{2028}", a),
                    "text:page-number" if at == Where::Margin => p.push("{page}", a),
                    "text:page-count" if at == Where::Margin => p.push("{pages}", a),
                    "draw:frame" | "draw:custom-shape" | "draw:rect" | "draw:line" | "draw:g" | "draw:ellipse" | "draw:path" | "draw:polygon" | "draw:circle" | "draw:polyline" | "draw:connector" if x.child("draw:image").is_none() && self.takes_shape(x, at) => {
                        if let Some(o) = self.odf_shape(x) {
                            let mut b = a.clone();
                            b.object = Some(o);
                            p.push(&OBJECT_CHAR.to_string(), &b);
                        }
                    }
                    "draw:frame" => {
                        if let Some(o) = self.picture(x) {
                            let mut b = a.clone();
                            b.object = Some(o);
                            p.push(&OBJECT_CHAR.to_string(), &b);
                        } else if let Some(t) = x.child("draw:text-box") {
                            if at == Where::Margin {
                                self.rep.note(Kind::Approximate, "text boxes (their text kept in line)");
                                for q in t.els() {
                                    if q.name == "text:p" || q.name == "text:h" {
                                        self.inline(q, p, a, at);
                                    }
                                }
                            } else {
                                self.rep.note(Kind::Approximate, "text boxes (their text kept after the paragraph)");
                                self.boxes.push(t.clone());
                            }
                        } else if x.child("draw:object").is_some() {
                            self.rep.note(Kind::Dropped, "charts and embedded objects");
                        }
                    }
                    "draw:a" => self.inline(x, p, a, at),
                    "text:note" => {
                        let kind = if x.attr("text:note-class") == Some("endnote") { NoteKind::End } else { NoteKind::Foot };
                        match x.child("text:note-body") {
                            Some(body) if at == Where::Body && !self.in_note => {
                                if let Some(nid) = self.doc.notes.add(Note { kind, paras: Vec::new() }) {
                                    let mut na = a.clone();
                                    na.vert = None;
                                    na.object = None;
                                    na.note = Some(nid);
                                    p.push(&OBJECT_CHAR.to_string(), &na);
                                    self.pending.push((nid, kind, body.clone()));
                                }
                            }
                            _ => self.rep.note(Kind::Dropped, "footnotes and endnotes in notes, headers and footers"),
                        }
                    }
                    "office:annotation" if at == Where::Body && !self.in_note => self.annotation(x, p),
                    "office:annotation-end" if at == Where::Body && !self.in_note => {
                        if let Some(&id) = x.attr("office:name").and_then(|n| self.annotations.get(n)) {
                            self.events.push((p.text.len() as u32, id, Ev::End));
                        }
                    }
                    "office:annotation" | "office:annotation-end" => self.rep.note(Kind::Dropped, "comments in notes, headers and footers"),
                    "text:change" | "text:change-start" | "text:change-end" if at == Where::Body && !self.in_note => self.change_mark(x, p, a),
                    "text:change" | "text:change-start" | "text:change-end" => self.rep.note(Kind::Approximate, "tracked changes in notes, headers and footers (shown as they stand)"),
                    "text:bookmark" | "text:bookmark-start" if at == Where::Body && !self.in_note => {
                        if let Some(n) = x.attr("text:name").filter(|n| !n.is_empty() && n.len() <= 256) {
                            if self.open_marks.len() < 64 {
                                self.open_marks.push(n.to_string());
                            }
                        }
                    }
                    "text:bookmark-ref" if at == Where::Body && !self.in_note && matches!(x.attr("text:reference-format"), Some("page" | "text")) && x.attr("text:ref-name").is_some_and(|n| !n.is_empty()) => {
                        let page = x.attr("text:reference-format") == Some("page");
                        let shown = x.text();
                        self.push_ref(p, a, page, x.attr("text:ref-name").unwrap_or(""), &shown);
                    }
                    "text:bookmark" | "text:bookmark-start" | "text:bookmark-end" | "text:soft-page-break" | "text:reference-mark" | "text:reference-mark-start" | "text:reference-mark-end" | "text:alphabetical-index-mark" | "text:toc-mark" | "text:sequence-ref" => {}
                    "text:ruby" => {
                        if let Some(b) = x.child("text:ruby-base") {
                            self.inline(b, p, a, at);
                        }
                    }
                    "draw:custom-shape" | "draw:rect" | "draw:line" | "draw:g" | "draw:ellipse" | "draw:path" | "draw:polygon" | "draw:circle" => {
                        self.rep.note(Kind::Dropped, "drawings");
                        // A shape's own text, and a group's, follow the paragraph.
                        if at == Where::Body && (x.find("text:p").is_some() || x.find("text:h").is_some()) {
                            self.rep.note(Kind::Approximate, "text in drawings (kept after the paragraph)");
                            self.boxes.push(x.clone());
                        }
                    }
                    _ => {
                        // Fields and other inline elements: their text.
                        self.inline(x, p, a, at);
                    }
                },
            }
        }
    }

    /// Our revision for a change's author (one per author, dated by their
    /// first change).
    fn rev_for(&mut self, author: &str, date: &str) -> Option<RevId> {
        if let Some(&r) = self.revs.get(author) {
            return Some(r);
        }
        let set = self.revs.len() as u32;
        let r = self.doc.revisions.add(Revision { author: author.into(), date: date.into(), set, name: "".into() })?;
        self.revs.insert(author.to_string(), r);
        Some(r)
    }

    /// `text:tracked-changes`: the regions by id.
    fn read_regions(&mut self, e: &El) {
        self.doc.tracking = e.attr("text:track-changes") != Some("false");
        for r in e.children("text:changed-region").take(1 << 16) {
            let Some(id) = r.attr("text:id").or_else(|| r.attr("xml:id")) else { continue };
            let Some(k) = r.els().next() else { continue };
            let kind = match k.name.as_str() {
                "text:insertion" => RegionKind::Insert,
                "text:deletion" => RegionKind::Delete,
                "text:format-change" => RegionKind::Format,
                _ => continue,
            };
            let info = k.child("office:change-info");
            let text_of = |n: &str| info.and_then(|i| i.child(n)).map(|c| c.text().trim().chars().take(256).collect::<String>()).unwrap_or_default();
            let mut author = text_of("dc:creator");
            if author.is_empty() {
                author = "Unknown".into();
            }
            let old_style = k.attr("officina:old-style").map(str::to_string);
            self.regions.insert(id.to_string(), Region { kind, author, date: text_of("dc:date"), el: (kind == RegionKind::Delete).then(|| k.clone()), old_style });
        }
    }

    /// A change mark in the text: a deletion's place, or where an insertion or
    /// formatting change starts or ends.
    fn change_mark(&mut self, x: &El, p: &mut Para, a: &CharAttrs) {
        let Some(id) = x.attr("text:change-id") else { return };
        let Some(reg) = self.regions.get(id) else { return };
        let (kind, author, date) = (reg.kind, reg.author.clone(), reg.date.clone());
        let Some(rev) = self.rev_for(&author, &date) else { return };
        match (x.name.as_str(), kind) {
            ("text:change-start", RegionKind::Insert) => {
                self.ins_open.push((id.to_string(), rev));
                p.ins = Some(rev);
            }
            ("text:change-end", RegionKind::Insert) => {
                self.ins_open.retain(|o| o.0 != id);
                p.ins = self.ins_open.last().map(|o| o.1);
            }
            ("text:change-start", RegionKind::Format) => {
                let mut old = match self.regions.get(id).and_then(|r| r.old_style.clone()) {
                    Some(n) if !n.is_empty() => self.span_attrs(Some(&n)),
                    _ => CharAttrs::default(),
                };
                old.comments = None;
                let _ = a;
                p.fmt = Some((rev, old));
            }
            ("text:change-end", RegionKind::Format) => p.fmt = None,
            ("text:change", RegionKind::Delete) => {
                let Some(el) = self.regions.get(id).and_then(|r| r.el.clone()) else { return };
                let pieces = self.deleted_pieces(&el);
                if let Some(t) = self.doc.deletions.add(Deletion { rev, pieces }) {
                    p.push(&OBJECT_CHAR.to_string(), &CharAttrs { deleted: Some(t), ..Default::default() });
                }
            }
            _ => {}
        }
    }

    /// A deletion's content as pieces: a piece a paragraph.
    fn deleted_pieces(&mut self, el: &El) -> Vec<DelPiece> {
        let mut paras: Vec<&El> = Vec::new();
        fn walk<'e>(e: &'e El, out: &mut Vec<&'e El>, depth: usize) {
            for x in e.els() {
                if out.len() >= MAX_NOTE_PARAS {
                    return;
                }
                match x.name.as_str() {
                    "text:p" | "text:h" => out.push(x),
                    "office:change-info" => {}
                    _ if depth < 16 => walk(x, out, depth + 1),
                    _ => {}
                }
            }
        }
        walk(el, &mut paras, 0);
        let mut out = Vec::new();
        let outer = std::mem::take(&mut self.events);
        let open = std::mem::take(&mut self.ins_open);
        for (k, q) in paras.iter().enumerate() {
            let (style, props, chars) = self.para_style(q.attr("text:style-name"));
            let mut p = Para::default();
            self.inline(q, &mut p, &chars, Where::Margin);
            let runs: Vec<(u32, u32)> = if p.text.is_empty() { vec![(0, 0)] } else { p.runs.into_iter().map(|(at, mut a)| {
                a.fmt = None;
                a.rev = None;
                (at, self.doc.attrs.intern(a))
            }).collect() };
            let para = (k > 0).then(|| (self.doc.new_id(), style, props));
            out.push(DelPiece { para, text: p.text, runs });
        }
        self.events = outer;
        self.ins_open = open;
        if out.is_empty() {
            out.push(DelPiece { para: None, text: String::new(), runs: vec![(0, 0)] });
        }
        out
    }

    /// An annotation: a comment from here to its end (by name), or a reply
    /// (LibreOffice's parent name), or a point comment.
    fn annotation(&mut self, x: &El, p: &Para) {
        let text_of = |n: &str| x.child(n).map(|c| c.text()).unwrap_or_default();
        let cut = |s: String, n: usize| -> Arc<str> { Arc::from(s.trim().chars().take(n).collect::<String>().as_str()) };
        let initials = { let i = text_of("meta:creator-initials"); if i.is_empty() { text_of("loext:sender-initials") } else { i } };
        let c = Comment { author: cut(text_of("dc:creator"), 256), initials: cut(initials, 16), date: cut(text_of("dc:date"), 64), text: annotation_text(x), parent: None, done: x.attr("loext:resolved") == Some("true") };
        let Some(id) = self.doc.comments.add(c) else { return };
        if let Some(n) = x.attr("office:name") {
            self.annotations.insert(n.to_string(), id);
        }
        // A reply can come before what it answers: tied up after the text.
        let parent = x.attr("loext:parent-name");
        if let Some(n) = parent {
            self.reply_to.push((id, n.chars().take(256).collect()));
        } else {
            // A start when an end names it, else a point.
            let ranged = x.attr("office:name").is_some_and(|n| self.ends.contains(n));
            self.events.push((p.text.len() as u32, id, if ranged { Ev::Start } else { Ev::Point }));
        }
    }

    /// Replies to their comments; a reply to nothing known is dropped.
    fn tie_replies(&mut self) {
        for (id, name) in std::mem::take(&mut self.reply_to) {
            let parent = self.annotations.get(&name).copied().filter(|&p| p != id);
            let Some(mut c) = self.doc.comments.get(id).map(|c| (**c).clone()) else { continue };
            match parent {
                Some(p) => {
                    c.parent = Some(p);
                    self.doc.comments.set(id, Some(c));
                }
                None => {
                    self.rep.note(Kind::Dropped, "replies to comments not in the document");
                    self.doc.comments.set(id, None);
                }
            }
        }
    }

    /// A reference field read: a U+FFFC whose field names its bookmark
    /// (bound once the document is read); `shown` is what the file showed.
    fn push_ref(&mut self, p: &mut Para, a: &CharAttrs, page: bool, name: &str, shown: &str) {
        let k = match self.ref_index.get(name) {
            Some(&k) => k,
            None if self.ref_names.len() < 1 << 20 => {
                let k = self.ref_names.len() as u64;
                self.ref_names.push(name.to_string());
                self.ref_index.insert(name.to_string(), k);
                k
            }
            None => return,
        };
        let f = if page { Field::PageRef(fields::UNBOUND | k) } else { Field::TextRef(fields::UNBOUND | k) };
        self.ref_shown.entry(f).or_insert_with(|| shown.to_string());
        let mut fa = a.clone();
        fa.field = Some(f);
        fa.object = None;
        fa.note = None;
        p.push(&OBJECT_CHAR.to_string(), &fa);
    }

    /// A `text:p` or `text:h` into the document.
    fn para(&mut self, e: &El, cell: Option<CellRef>, list: Option<Option<ListRef>>) {
        let (style, mut props, chars) = self.para_style(e.attr("text:style-name"));
        if cell.is_none() && !self.in_note && self.box_depth == 0 {
            if let Some(m) = e.attr("text:style-name").and_then(|n| self.styles.get(n)).and_then(|o| o.master.clone()) {
                self.master_marks.push((self.doc.blocks.len(), m));
            }
        }
        if e.name == "text:h" {
            let lvl = e.attr("text:outline-level").and_then(|v| v.parse::<u8>().ok()).unwrap_or(1).clamp(1, 9);
            if style == 0 {
                // A heading with no named style: ours of its level.
                if let Some(h) = self.doc.styles.by_name(&format!("Heading {lvl}")) {
                    return self.para_with(e, h, props, chars, cell, list);
                }
            }
            if self.doc.styles.para_format(style, &props).0.outline_level != Some(lvl) {
                props.outline_level = Some(lvl);
            }
        }
        self.para_with(e, style, props, chars, cell, list)
    }

    fn para_with(&mut self, e: &El, style: StyleId, mut props: ParaProps, chars: CharAttrs, cell: Option<CellRef>, list: Option<Option<ListRef>>) {
        if list.is_some() {
            props.list = list;
            // The list's own indents come from its level; the style's stand.
            props.indent_start = None;
            props.first_line = None;
        }
        props.cell = cell;
        if cell.is_some() {
            props.space_after = props.space_after.or(Some(0));
        }
        let mut p = Para::default();
        if !self.in_note {
            // An insertion open across the break before this paragraph.
            if let Some(&(_, r)) = self.ins_open.last() {
                let same = match (self.last_cell, cell) {
                    (Some(None), None) => true,
                    (Some(Some(x)), Some(y)) => (x.table, x.row, x.col) == (y.table, y.row, y.col),
                    _ => false,
                };
                if same {
                    props.rev = Some(r);
                }
                p.ins = Some(r);
            }
            self.last_cell = Some(cell);
        }
        let outer = std::mem::take(&mut self.events);
        let at = if self.in_margin { Where::Margin } else { Where::Body };
        self.inline(e, &mut p, &chars, at);
        let events = std::mem::replace(&mut self.events, outer);
        // Tab stops: ODF measures them from the start indent, we from the edge.
        let pf = self.doc.styles.para_format(style, &props).0;
        if !pf.tabs.is_empty() {
            let ind = if self.tabs_from_indent { pf.indent_start.max(0) } else { 0 };
            // With no indent to measure from, the style's stops stand as they are.
            if ind != 0 || props.tabs.is_some() {
                props.tabs = Some(pf.tabs.iter().map(|t| TabStop { pos: (t.pos + ind).max(0), ..*t }).collect());
            }
        }
        if self.in_toc && cell.is_none() && !self.in_note && !p.text.is_empty() {
            let name = self.doc.styles.styles[style as usize].name.to_ascii_lowercase();
            let level = name.strip_prefix("contents").or_else(|| name.strip_prefix("toc")).and_then(|n| n.trim().parse::<u8>().ok()).filter(|l| (1..=9).contains(l)).unwrap_or(1);
            props.toc = Some(level);
            // An entry LibreOffice wrote: a link to the heading's bookmark, the
            // page number as text after the tab; the number becomes a field.
            if !p.text.contains(OBJECT_CHAR) {
                if let Some(tab) = p.text.rfind('\t') {
                    let num = p.text[tab + 1..].trim().to_string();
                    let link = p.runs.iter().rev().find(|r| (r.0 as usize) <= tab).and_then(|r| r.1.link.clone());
                    if !num.is_empty() && num.len() <= 12 && num.chars().all(|c| c.is_ascii_alphanumeric()) {
                        if let Some(name) = link.as_deref().and_then(|l| l.strip_prefix('#')).filter(|n| !n.is_empty()).map(str::to_string) {
                            let at = (tab + 1) as u32;
                            let a = p.runs.iter().rev().find(|r| r.0 <= at).map(|r| r.1.clone()).unwrap_or_default();
                            p.text.truncate(tab + 1);
                            p.runs.retain(|r| r.0 < at);
                            self.push_ref(&mut p, &a, true, &name, &num);
                        }
                    }
                }
            }
        }
        apply_comments(&mut p, &mut self.active, events);
        // Formatting changes: the attributes before, interned first.
        let olds: Vec<u32> = p.olds.iter().map(|o| self.doc.attrs.intern(o.clone())).collect();
        for r in p.runs.iter_mut() {
            if let Some(m) = &mut r.1.fmt {
                let mut o = self.doc.attrs.get(olds[m.old as usize]).clone();
                if o.comments != r.1.comments {
                    o.comments = r.1.comments.clone();
                    m.old = self.doc.attrs.intern(o);
                } else {
                    m.old = olds[m.old as usize];
                }
            }
        }
        // Trailing collapsed space.
        let mut runs: Vec<(u32, u32)> = p.runs.into_iter().map(|(at, a)| (at, self.doc.attrs.intern(a))).collect();
        if runs.is_empty() {
            runs.push((0, self.doc.attrs.intern(chars)));
        }
        let k = self.doc.push_para(&p.text, style, runs);
        self.doc.blocks.update(k, |b| {
            if let Some(q) = b.as_para_mut() {
                q.props = props;
            }
        });
        if !self.open_marks.is_empty() && !self.in_note {
            let id = self.doc.para(k).map_or(0, |q| q.id);
            for n in std::mem::take(&mut self.open_marks) {
                self.marks.entry(n).or_insert(id);
            }
        }
        self.flush_boxes(cell);
    }

    /// The text boxes and shapes the paragraph just read held, as paragraphs
    /// after it (in its cell, if it is in one).
    fn flush_boxes(&mut self, cell: Option<CellRef>) {
        if self.boxes.is_empty() || self.box_depth >= 4 {
            self.boxes.clear();
            return;
        }
        let boxes = std::mem::take(&mut self.boxes);
        self.box_depth += 1;
        for b in &boxes {
            self.box_content(b, cell);
        }
        self.box_depth -= 1;
    }

    /// Each note's text: read as blocks at the end, then taken out into the note.
    fn read_notes(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return;
        }
        self.in_note = true;
        for (nid, kind, body) in pending {
            let before = self.doc.blocks.len();
            self.box_content(&body, None);
            let mut paras = Vec::new();
            while self.doc.blocks.len() > before {
                if let Some(Block::Para(mut p)) = self.doc.blocks.remove(before) {
                    p.props.cell = None;
                    p.props.list = None;
                    paras.push(p);
                }
            }
            paras.truncate(MAX_NOTE_PARAS);
            if paras.is_empty() {
                let id = self.doc.new_id();
                paras.push(officina_text::model::Para::new(id, "", 0));
            }
            self.doc.notes.set(nid, Some(Note { kind, paras }));
        }
        self.in_note = false;
        self.doc.notes.renumber();
    }

    /// The header's and footer's own paragraphs: formatted, pictures in
    /// line, page fields as `{page}` and `{pages}`.
    fn read_margins(&mut self) {
        self.in_note = true;
        self.in_margin = true;
        for (kind, header, h) in std::mem::take(&mut self.margin_els) {
            let before = self.doc.blocks.len();
            self.box_content(&h, None);
            let mut paras = Vec::new();
            while self.doc.blocks.len() > before {
                if let Some(Block::Para(mut p)) = self.doc.blocks.remove(before) {
                    p.props.list = None;
                    p.props.section = None;
                    paras.push(p);
                }
            }
            let mut paras = officina_text::model::rows_as_lines(paras);
            while paras.len() > 1 && paras.last().is_some_and(|p| p.text.is_empty()) {
                paras.pop();
            }
            if paras.len() > MAX_MARGIN_PARAS {
                self.rep.note(Kind::Approximate, "very long headers and footers (cut)");
                paras.truncate(MAX_MARGIN_PARAS);
            }
            if let Some(h) = self.margin_slot(kind, header) {
                h.paras = paras;
            }
        }
        self.in_margin = false;
        self.in_note = false;
    }

    fn box_content(&mut self, b: &El, cell: Option<CellRef>) {
        for x in b.els() {
            match x.name.as_str() {
                "text:p" | "text:h" => self.para(x, cell, None),
                "text:list" => self.list(x, 0, None, None, cell),
                "table:table" | "loext:table" => match cell {
                    Some(c) => self.cell_content(&El { name: "x".into(), attrs: vec![], kids: vec![Node::El(x.clone())] }, c, 1),
                    None => self.table(x),
                },
                // Groups and frames in groups: their text, in order.
                "draw:g" | "draw:custom-shape" | "draw:rect" | "draw:ellipse" | "draw:polygon" | "draw:circle" | "draw:path" => self.box_content(x, cell),
                "draw:frame" => {
                    if let Some(t) = x.child("draw:text-box") {
                        self.box_content(t, cell);
                    }
                }
                _ => {}
            }
        }
    }

    fn list(&mut self, e: &El, level: u8, style: Option<String>, id: Option<u32>, cell: Option<CellRef>) {
        let style = e.attr("text:style-name").map(str::to_string).or(style);
        let id = match id {
            Some(i) => i,
            None => {
                let continued = e.attr("text:continue-numbering") == Some("true") || e.attr("text:continue-list").is_some();
                self.list_id(style.as_deref().unwrap_or(""), continued)
            }
        };
        let kind = style.as_ref().and_then(|s| self.lists.get(s)).map(|k| k[level.min(MAX_LIST_LEVEL) as usize]).unwrap_or(ListKind::Bullet);
        for item in e.els() {
            if item.name != "text:list-item" && item.name != "text:list-header" {
                continue;
            }
            let mut start = item.attr("text:start-value").and_then(|v| v.parse::<u32>().ok()).map(|v| v.min(MAX_LIST_START));
            for x in item.els() {
                match x.name.as_str() {
                    "text:p" | "text:h" => {
                        let l = if item.name == "text:list-header" { None } else { Some(Some(ListRef { id, kind, level: level.min(MAX_LIST_LEVEL) })) };
                        let before = self.doc.blocks.len();
                        self.para(x, cell, l);
                        if let (Some(v), true) = (start.take(), self.doc.blocks.len() > before) {
                            self.doc.blocks.update(before, |b| {
                                if let Some(p) = b.as_para_mut() {
                                    p.props.list_start = Some(v);
                                }
                            });
                        }
                    }
                    "text:list" => self.list(x, level.saturating_add(1), style.clone(), Some(id), cell),
                    "table:table" => self.table(x),
                    _ => {}
                }
            }
        }
    }

    fn table(&mut self, t: &El) {
        self.table_in(t, 0)
    }

    /// A table at a depth (1: in a cell, its cells' tables kept as text).
    fn table_in(&mut self, t: &El, depth: usize) {
        let mut widths: Vec<u32> = Vec::new();
        for c in t.els().flat_map(|x| if x.name == "table:table-columns" { x.els().collect::<Vec<_>>() } else { vec![x] }) {
            if c.name != "table:table-column" {
                continue;
            }
            let n = c.attr("table:number-columns-repeated").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 63);
            let w = c.attr("table:style-name").and_then(|s| self.columns.get(s)).copied().unwrap_or(1440);
            for _ in 0..n {
                if widths.len() < 63 {
                    widths.push(w);
                }
            }
        }
        let table = self.next_table;
        self.next_table += 1;
        // Cells draw their own rules in ODF: the table adds none.
        let (align, indent) = t.attr("table:style-name").and_then(|n| self.table_styles.get(n)).copied().unwrap_or_default();
        self.doc.tables.insert(table, TableLook { borders: Some(TableBorders::default()), align, indent, look: 0, ..TableLook::default() });
        let mut rows: Vec<(&El, bool)> = Vec::new();
        for x in t.els() {
            match x.name.as_str() {
                "table:table-row" => rows.push((x, false)),
                "table:table-header-rows" => rows.extend(x.children("table:table-row").map(|r| (r, true))),
                "table:table-rows" | "table:table-row-group" => rows.extend(x.children("table:table-row").map(|r| (r, false))),
                _ => {}
            }
        }
        // Grid columns a cell merged down covers, and for how many more rows.
        let mut down: HashMap<usize, usize> = HashMap::new();
        for (r, (tr, header)) in rows.into_iter().enumerate() {
            let mut col = 0usize;
            let mut c = 0u16;
            let mut row_look = tr.attr("table:style-name").and_then(|n| self.row_styles.get(n)).copied().unwrap_or_default();
            row_look.header = header;
            // Covered cells that only fill out the span of the cell before.
            let mut skip = 0usize;
            for tc in tr.els() {
                let covered = tc.name == "table:covered-table-cell";
                if !covered && tc.name != "table:table-cell" {
                    continue;
                }
                if covered && skip > 0 {
                    skip -= 1;
                    continue;
                }
                let span = tc.attr("table:number-columns-spanned").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 63);
                // A covered cell under a cell merged down is a cell of its
                // own, merged up; one beside a cell merged across is part of
                // that cell.
                let merged_up = covered && down.get(&col).is_some_and(|n| *n > 0);
                if covered && !merged_up {
                    col += 1;
                    continue;
                }
                if merged_up {
                    if let Some(n) = down.get_mut(&col) {
                        *n -= 1;
                    }
                }
                let rows_down = tc.attr("table:number-rows-spanned").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 4096);
                if rows_down > 1 {
                    down.insert(col, rows_down - 1);
                }
                // A cell merged up spans what the cell above it did.
                let span = if merged_up { self.span_above.get(&col).copied().unwrap_or(1) } else { span };
                if !merged_up {
                    self.span_above.insert(col, span);
                }
                let width = (col..col + span).map(|g| widths.get(g).copied().unwrap_or(1440)).sum::<u32>().clamp(1, MAX_OBJECT_TWIPS);
                let look = tc.attr("table:style-name").and_then(|n| self.cell_styles.get(n)).copied().unwrap_or(CellLook { borders: CellBorders { top: Some(None), bottom: Some(None), start: Some(None), end: Some(None) }, ..CellLook::default() });
                col += span;
                skip = span - 1;
                let cell = CellRef { table, row: r as u32, col: c, width, look, row_look, merged_up, span: span as u8, ..Default::default() };
                c = c.saturating_add(1);
                let before = self.doc.blocks.len();
                self.cell_content(tc, cell, depth);
                if self.doc.blocks.len() == before {
                    let k = self.doc.push_para("", 0, vec![]);
                    self.doc.blocks.update(k, |b| {
                        if let Some(q) = b.as_para_mut() {
                            q.props.cell = Some(cell);
                            q.props.space_after = Some(0);
                        }
                    });
                }
            }
        }
    }

    fn cell_content(&mut self, tc: &El, cell: CellRef, depth: usize) {
        for x in tc.els() {
            match x.name.as_str() {
                "text:p" | "text:h" => self.para(x, Some(cell), None),
                "text:list" => self.list(x, 0, None, None, Some(cell)),
                "table:table" if depth == 0 => {
                    // A table in a cell: its paragraphs are in the outer
                    // cell and in their own cell of the inner table.
                    let before = self.doc.blocks.len();
                    let spans = std::mem::take(&mut self.span_above);
                    self.table_in(x, 1);
                    self.span_above = spans;
                    for k in before..self.doc.blocks.len() {
                        self.doc.blocks.update(k, |b| {
                            if let Some(q) = b.as_para_mut() {
                                q.props.inner = q.props.cell;
                                q.props.cell = Some(cell);
                            }
                        });
                    }
                }
                "table:table" if depth < 8 => {
                    self.rep.note(Kind::Approximate, "tables three deep (their text kept in the cell)");
                    for row in x.els().filter(|r| r.name == "table:table-row") {
                        for inner in row.children("table:table-cell") {
                            self.cell_content(inner, cell, depth + 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn body(&mut self, e: &El) {
        for x in e.els() {
            match x.name.as_str() {
                "text:p" | "text:h" => self.para(x, None, None),
                "text:list" => self.list(x, 0, None, None, None),
                "table:table" => self.table(x),
                "text:section" => {
                    // A section flows on in the page; its columns start with it
                    // and the enclosing ones come back after it.
                    let outer = self.sec_stack.last().copied().unwrap_or(self.page_cols);
                    let mine = x.attr("text:style-name").and_then(|n| self.sec_styles.get(n)).copied().unwrap_or(Section { start: SectionStart::Continuous, ..Section::default() });
                    let i = self.doc.blocks.len();
                    self.sec_stack.push(mine);
                    self.body(x);
                    self.sec_stack.pop();
                    if !self.in_toc && self.doc.blocks.len() > i && (mine.columns, mine.gap, mine.rule) != (outer.columns, outer.gap, outer.rule) {
                        self.sec_marks.push((i, mine, true));
                        self.sec_marks.push((self.doc.blocks.len(), Section { start: SectionStart::Continuous, ..outer }, false));
                    }
                }
                "text:table-of-content" => {
                    if let Some(b) = x.child("text:index-body") {
                        let was = self.in_toc;
                        for y in b.els() {
                            // The title isn't an entry.
                            self.in_toc = y.name != "text:index-title";
                            self.body(&El { name: "x".into(), attrs: vec![], kids: vec![Node::El(y.clone())] });
                        }
                        self.in_toc = was;
                    }
                }
                "text:alphabetical-index" | "text:illustration-index" | "text:table-index" | "text:bibliography" | "text:user-index" | "text:object-index" => {
                    self.rep.note(Kind::Approximate, "tables of contents and indexes (their text kept)");
                    if let Some(b) = x.child("text:index-body") {
                        self.body(b);
                    }
                }
                "text:index-title" => self.body(x),
                "draw:frame" | "draw:custom-shape" | "draw:g" | "draw:rect" | "draw:ellipse" => {
                    let mut p = Para::default();
                    self.inline(&El { name: "x".into(), attrs: vec![], kids: vec![Node::El(x.clone())] }, &mut p, &CharAttrs::default(), Where::Body);
                    if !p.text.is_empty() {
                        let runs: Vec<(u32, u32)> = p.runs.into_iter().map(|(at, a)| (at, self.doc.attrs.intern(a))).collect();
                        self.doc.push_para(&p.text, 0, runs);
                    }
                    self.flush_boxes(None);
                }
                "text:tracked-changes" => self.read_regions(x),
                "text:sequence-decls" | "text:variable-decls" | "text:user-field-decls" | "office:forms" | "text:soft-page-break" => {}
                _ => {}
            }
        }
    }

    /// Section marks on the first body block (not in a cell) at or after each
    /// place; a start wins over a return, the innermost start over an outer one.
    fn place_sections(&mut self) {
        let n = self.doc.blocks.len();
        let mut at: HashMap<usize, (Section, bool)> = HashMap::new();
        for &(i, sec, start) in &self.sec_marks {
            let Some(k) = (i..n).find(|&k| self.doc.cell(k).is_none()) else { continue };
            match at.get(&k) {
                Some(&(_, true)) => {}
                Some(&(_, false)) if !start => {}
                _ => {
                    at.insert(k, (sec, start));
                }
            }
        }
        if self.page_cols.columns > 1 && n > 0 {
            at.entry(0).or_insert((self.page_cols, true));
        }
        for (k, (sec, _)) in at {
            self.doc.blocks.update(k, |b| {
                if let Some(q) = b.as_para_mut() {
                    if q.props.section.is_none() {
                        // A break before the first paragraph: the section starts a page.
                        let mut sec = sec;
                        if q.props.page_break_before == Some(true) && sec.start == SectionStart::Continuous && k > 0 {
                            sec.start = SectionStart::NextPage;
                            q.props.page_break_before = None;
                        }
                        q.props.section = Some(sec);
                    }
                }
            });
        }
    }

    /// Paragraphs that start a master page of another size: sections with
    /// that page (keeping the columns in effect), in place of a page break.
    fn place_masters(&mut self) {
        let mut cur = self.doc.page;
        for (k, name) in std::mem::take(&mut self.master_marks) {
            let Some(&g) = self.masters.get(&name) else { continue };
            let usage = self.usages.get(&name).copied().filter(|_| k > 0);
            // Our writer names a section's master page `Section_20_Page…`,
            // so one of those starts a section even on a page like the last.
            let ours = k > 0 && name.starts_with("Section_20_Page");
            if (g == cur && usage.is_none() && !ours) || k >= self.doc.blocks.len() {
                continue;
            }
            cur = g;
            let base = (0..=k).rev().find_map(|j| self.doc.para(j).and_then(|p| p.props.section.filter(|_| p.props.cell.is_none()))).unwrap_or(self.page_cols);
            let own = (g != self.doc.page).then_some(g);
            self.doc.blocks.update(k, |b| {
                if let Some(q) = b.as_para_mut() {
                    // A section already starting here keeps how it starts (a
                    // page of another size starts a new page anyway).
                    let start = match q.props.section {
                        _ if usage.is_some() => usage.unwrap(),
                        Some(x) => x.start,
                        None if k > 0 => SectionStart::NextPage,
                        None => base.start,
                    };
                    let sec = q.props.section.unwrap_or(base);
                    q.props.section = Some(Section { start, page: own, ..sec });
                    q.props.page_break_before = None;
                }
            });
        }
    }

    /// Page size, margins, header and footer from the master page the body
    /// uses; every master page's size and margins, for paragraphs that start one.
    fn page(&mut self, styles: Option<&El>, first_master: Option<String>) {
        let Some(st) = styles else { return };
        let masters = st.child("office:master-styles");
        let master = masters.and_then(|m| {
            let all: Vec<&El> = m.children("style:master-page").collect();
            first_master.as_ref().and_then(|n| all.iter().find(|x| x.attr("style:name") == Some(n.as_str())).copied()).or_else(|| all.iter().find(|x| x.attr("style:name") == Some("Standard")).copied()).or(all.first().copied())
        });
        let Some(master) = master else { return };
        if let Some(pp) = layout_props(st, master) {
            self.doc.settings.page_color = pp.attr("fo:background-color").and_then(|c| color(Some(c))).filter(|&c| c != 0xFFFFFF);
            self.doc.settings.page_borders = page_borders(pp);
        }
        let base = self.doc.page;
        let (g, cols) = master_setup(st, master, base);
        self.doc.page = g;
        // Our writer gives a first section with a page of its own a master
        // page `Section_20_Page_n`; the document's page is then `Standard`'s.
        if first_master.as_deref().is_some_and(|n| n.starts_with("Section_20_Page_")) {
            if let Some(std_page) = masters.and_then(|m| m.children("style:master-page").find(|x| x.attr("style:name") == Some("Standard"))) {
                self.doc.page = master_setup(st, std_page, base).0;
            }
        }
        if let Some(c) = cols {
            self.page_cols = Section { start: SectionStart::NextPage, ..c };
        }
        for m in masters.into_iter().flat_map(|m| m.children("style:master-page")).take(256) {
            if let Some(n) = m.attr("style:name") {
                self.masters.insert(n.to_string(), master_setup(st, m, base).0);
                let usage = m.attr("style:page-layout-name").and_then(|l| st.child("office:automatic-styles").and_then(|a| a.children("style:page-layout").find(|x| x.attr("style:name") == Some(l)))).and_then(|l| l.attr("style:page-usage"));
                match usage {
                    Some("right") => self.usages.insert(n.to_string(), SectionStart::OddPage),
                    Some("left") => self.usages.insert(n.to_string(), SectionStart::EvenPage),
                    _ => None,
                };
            }
        }
        for (tag, header) in [("style:header", true), ("style:footer", false)] {
            let Some(main) = master.child(tag) else { continue };
            if main.attr("style:display") == Some("false") {
                continue;
            }
            // Left (even) pages' and the first page's own, where given;
            // shown off, they are blank.
            let local = &tag["style:".len()..];
            let kinds = [(0usize, Some(main)), (2, master.child(&format!("{tag}-left"))), (1, master.child(&format!("{tag}-first")).or_else(|| master.child(&format!("loext:{local}-first"))))];
            for (kind, h) in kinds {
                let Some(h) = h else { continue };
                if kind == 1 {
                    self.doc.first_page.get_or_insert_with(Margins::default);
                } else if kind == 2 {
                    self.doc.even_pages.get_or_insert_with(Margins::default);
                }
                if h.attr("style:display") == Some("false") {
                    continue;
                }
                let mut text = String::new();
                let mut align = Align::Start;
                let mut first = true;
                for p in h.els().filter(|x| x.name == "text:p" || x.name == "text:h") {
                    let mut q = Para::default();
                    self.inline(p, &mut q, &CharAttrs::default(), Where::Margin);
                    if q.text.trim().is_empty() {
                        continue;
                    }
                    if first {
                        align = self.para_style(p.attr("text:style-name")).1.align.or_else(|| {
                            let (sid, _, _) = self.para_style(p.attr("text:style-name"));
                            self.doc.styles.styles.get(sid as usize).and_then(|s| s.para.align)
                        }).or_else(|| p.attr("text:style-name").and_then(|n| self.styles.get(n)).and_then(|o| o.parent.as_ref()).and_then(|pn| self.styles.get(pn)).and_then(|o| o.para.align)).unwrap_or(Align::Start);
                        first = false;
                    } else {
                        text.push(' ');
                    }
                    text.push_str(q.text.replace(['\t', '\u{2028}'], " ").trim());
                }
                if !text.is_empty() || h.find("draw:frame").is_some() {
                    self.margin_els.push((kind, header, h.clone()));
                    *self.margin_slot(kind, header) = Some(HeaderFooter { text: text.chars().take(1000).collect(), align, paras: Vec::new() });
                }
            }
        }
    }

    /// Where the header (or footer) of a kind (0 default, 1 first page, 2
    /// even pages) goes.
    fn margin_slot(&mut self, kind: usize, header: bool) -> &mut Option<HeaderFooter> {
        let m = match kind {
            1 => self.doc.first_page.get_or_insert_with(Margins::default),
            2 => self.doc.even_pages.get_or_insert_with(Margins::default),
            _ => return if header { &mut self.doc.header } else { &mut self.doc.footer },
        };
        if header { &mut m.header } else { &mut m.footer }
    }
}

/// Reads an `.odt`.
pub fn read(data: &[u8]) -> Result<(TextDoc, Loaded), OdtError> {
    let archive = Archive::open(data)?;
    if let Some(m) = archive.read_name("mimetype")? {
        if m.starts_with(b"application/vnd.oasis.opendocument.") && !m.starts_with(b"application/vnd.oasis.opendocument.text") {
            return Err(OdtError::Bad("an OpenDocument file, but not a text document".into()));
        }
    }
    let content = part(&archive, "content.xml")?.ok_or_else(|| OdtError::Bad("no content.xml".into()))?;
    let styles = part(&archive, "styles.xml")?;
    // A text document's body; failing that, the body's one element (a damaged
    // name, as LibreOffice reads it too) when it holds paragraphs.
    let body = content.child("office:body").ok_or_else(|| OdtError::Bad("no text body".into()))?;
    let text = body.child("office:text").or_else(|| body.els().next().filter(|x| body.els().count() == 1 && !x.name.starts_with("office:spreadsheet") && !x.name.starts_with("office:presentation") && !x.name.starts_with("office:drawing") && x.els().any(|p| matches!(p.name.as_str(), "text:p" | "text:h" | "text:list" | "table:table")))).ok_or_else(|| OdtError::Bad("no text body".into()))?;
    let mut doc = TextDoc::new();
    doc.blocks.remove(0);
    let mut r = Reader { archive: &archive, doc, rep: Report::default(), fonts: HashMap::new(), styles: HashMap::new(), ours: HashMap::new(), lists: HashMap::new(), list_defs: HashMap::new(), columns: HashMap::new(), cell_styles: HashMap::new(), span_above: HashMap::new(), row_styles: HashMap::new(), table_styles: HashMap::new(), graphic: HashMap::new(), sec_styles: HashMap::new(), page_cols: Section::default(), sec_marks: Vec::new(), masters: HashMap::new(), usages: HashMap::new(), master_marks: Vec::new(), sec_stack: Vec::new(), last_list: HashMap::new(), next_list: 0, next_table: 1, images: HashMap::new(), pictures: 0, boxes: Vec::new(), box_depth: 0, shape_text: Vec::new(), in_shape: false, pending: Vec::new(), in_note: false, in_margin: false, margin_els: Vec::new(), annotations: HashMap::new(), reply_to: Vec::new(), ends: annotation_ends(text), active: Vec::new(), events: Vec::new(), regions: HashMap::new(), revs: HashMap::new(), ins_open: Vec::new(), last_cell: None, ref_names: Vec::new(), ref_index: HashMap::new(), ref_shown: HashMap::new(), open_marks: Vec::new(), marks: HashMap::new(), in_toc: false, tabs_from_indent: true };
    if let Some(st) = part(&archive, "settings.xml")? {
        fn item<'e>(e: &'e El, name: &str) -> Option<&'e El> {
            if e.name == "config:config-item" && e.attr("config:name") == Some(name) {
                return Some(e);
            }
            e.els().find_map(|x| item(x, name))
        }
        if item(&st, "TabsRelativeToIndent").is_some_and(|x| x.text().trim() == "false") {
            r.tabs_from_indent = false;
        }
    }
    r.font_decls(styles.as_ref());
    r.font_decls(Some(&content));
    for s in [styles.as_ref().and_then(|s| s.child("office:styles")), styles.as_ref().and_then(|s| s.child("office:automatic-styles")), content.child("office:automatic-styles")].into_iter().flatten() {
        r.style_block(s);
    }
    r.named_styles(styles.as_ref());
    // Lists in named styles took ids; a body list starts its own.
    r.last_list.clear();
    // The master page the first paragraph's style names, if any.
    let first_master = first_para(text, 0).and_then(|p| p.attr("text:style-name")).and_then(|n| content.child("office:automatic-styles").and_then(|a| a.children("style:style").find(|s| s.attr("style:name") == Some(n)))).and_then(|s| s.attr("style:master-page-name")).map(str::to_string);
    r.page(styles.as_ref(), first_master);
    r.body(text);
    r.place_sections();
    r.place_masters();
    // The first page needs no break before it (a master page named there).
    if r.doc.para(0).is_some_and(|p| p.props.page_break_before == Some(true)) {
        r.doc.blocks.update(0, |b| {
            if let Some(q) = b.as_para_mut() {
                q.props.page_break_before = None;
            }
        });
    }
    r.read_notes();
    r.read_margins();
    r.read_shape_text();
    let lost = fields::bind(&mut r.doc, &r.ref_names, &r.marks, &r.ref_shown);
    fields::relink_toc(&mut r.doc);
    if lost > 0 {
        r.rep.note(Kind::Approximate, "cross-references to missing bookmarks (kept as text)");
    }
    r.tie_replies();
    if r.doc.blocks.is_empty() {
        r.doc.push_para("", 0, vec![]);
    }
    let paragraphs = r.doc.blocks.len() as u64;
    let pictures = r.pictures;
    let rep = r.rep;
    let mut doc = r.doc;
    doc.take_changes();
    Ok((doc, Loaded { report: rep, paragraphs, pictures }))
}
