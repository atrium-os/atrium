//! `.odt` out, in the document process, over our own model.
//!
//! Named styles (the built-in ones under LibreOffice's names), automatic styles
//! for direct formatting, lists as nested `text:list`s with a list style each,
//! tables with column widths, pictures in as-character frames, page size and
//! margins, and the header and footer with page number and count fields on the
//! default master page.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;

use officina_text::model::{Border, BorderStyle, ListDef, ListLevel, NumFormat, LineRestart, AttrId, Caps, EFFECT_OUTLINE, EFFECT_SHADOW, EFFECT_EMBOSS, EFFECT_ENGRAVE, ArrowEnd, Dash, Geom, ShapeData, ShapeLine, TextAnchor, MAX_GROUP_DEPTH, MAX_GROUP_SHAPES, Float, HAlign, Rel, Wrap, Section, SectionStart, BlockId, Field, Leader, TabAlign, RevId, Align, CharAttrs, CommentId, HeaderFooter, LineSpacing, ListKind, NoteKind, Para, ParaProps, StyleKind, TextDoc, VertAlign, HEADER_FROM_EDGE, OBJECT_CHAR};

use crate::xml::esc;
use crate::zip::ZipWriter;

const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" xmlns:officina="urn:atrium:officina:1.0" office:version="1.3""#;

/// Twips as points.
fn pt(t: i64) -> String {
    let v = t as f64 / 20.0;
    format!("{}pt", (v * 100.0).round() / 100.0)
}

/// RGB as `#rrggbb`.
fn hex(c: u32) -> String {
    format!("#{:06x}", c & 0xFF_FFFF)
}

/// A floating object's wrap, gap and how its position is measured, as
/// graphic properties.
fn float_props(f: &Float) -> String {
    let wrap = match f.wrap {
        Wrap::Square => r#"style:wrap="parallel" style:number-wrapped-paragraphs="no-limit" style:wrap-contour="false""#,
        Wrap::TopBottom => r#"style:wrap="none""#,
        Wrap::Behind => r#"style:wrap="run-through" style:run-through="background""#,
        Wrap::InFront => r#"style:wrap="run-through" style:run-through="foreground""#,
    };
    let hpos = match f.h_align {
        HAlign::Start => "left",
        HAlign::Center => "center",
        HAlign::End => "right",
        HAlign::Offset => "from-left",
    };
    let rel = |r: Rel| match r {
        Rel::Text => "paragraph",
        Rel::Margin => "page-content",
        Rel::Page => "page",
    };
    let g = pt(f.gap as i64);
    format!(r#" {wrap} fo:margin-left="{g}" fo:margin-right="{g}" fo:margin-top="{g}" fo:margin-bottom="{g}" style:vertical-pos="from-top" style:vertical-rel="{}" style:horizontal-pos="{hpos}" style:horizontal-rel="{}" style:flow-with-text="false""#, rel(f.v_rel), rel(f.h_rel))
}

/// A frame's size and place in twips, turned `deg` clockwise about its
/// middle: ODF turns counter-clockwise about the top left, then moves.
fn place(b: [f64; 4], deg: f64) -> String {
    let [x, y, w, h] = b;
    let p = |v: f64| pt(v.round() as i64);
    let size = format!(r#" svg:width="{}" svg:height="{}""#, p(w), p(h));
    if deg.abs() < 1e-6 || (deg - 360.0).abs() < 1e-6 {
        return format!(r#"{size} svg:x="{}" svg:y="{}""#, p(x), p(y));
    }
    let a = (360.0 - deg).rem_euclid(360.0).to_radians();
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (rx, ry) = (cx * a.cos() + cy * a.sin(), -cx * a.sin() + cy * a.cos());
    format!(r#"{size} draw:transform="rotate ({a:.9}) translate ({} {})""#, p(x + cx - rx), p(y + cy - ry))
}

/// A named style's name in the file, and its display name.
fn style_name(name: &str, i: usize) -> (String, String) {
    match name {
        "Normal" => ("Standard".into(), "Standard".into()),
        "Title" => ("Title".into(), "Title".into()),
        n if n.starts_with("Heading ") && n.len() == 9 => (n.replace(' ', "_20_"), n.to_string()),
        n => {
            let s: String = n.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).take(40).collect();
            (format!("S{i}_{s}"), n.to_string())
        }
    }
}

pub(crate) fn text_props(a: &CharAttrs, fonts: &mut Vec<String>) -> String {
    let mut s = String::new();
    if let Some(b) = a.bold {
        let w = if b { "bold" } else { "normal" };
        let _ = write!(s, r#" fo:font-weight="{w}" style:font-weight-asian="{w}" style:font-weight-complex="{w}""#);
    }
    if let Some(i) = a.italic {
        let v = if i { "italic" } else { "normal" };
        let _ = write!(s, r#" fo:font-style="{v}" style:font-style-asian="{v}" style:font-style-complex="{v}""#);
    }
    if let Some(u) = a.underline {
        if u {
            s += r#" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color""#;
        } else {
            s += r#" style:text-underline-style="none""#;
        }
    }
    match (a.strike, a.dstrike) {
        (_, Some(true)) => s += r#" style:text-line-through-style="solid" style:text-line-through-type="double""#,
        (Some(v), _) => {
            let _ = write!(s, r#" style:text-line-through-style="{}""#, if v { "solid" } else { "none" });
        }
        (None, Some(false)) => s += r#" style:text-line-through-type="single""#,
        (None, None) => {}
    }
    match a.caps {
        Some(Caps::All) => s += r#" fo:text-transform="uppercase""#,
        Some(Caps::Small) => s += r#" fo:font-variant="small-caps""#,
        Some(Caps::Normal) => s += r#" fo:text-transform="none" fo:font-variant="normal""#,
        None => {}
    }
    if let Some(h) = a.hidden {
        let _ = write!(s, r#" text:display="{}""#, if h { "none" } else { "true" });
    }
    if let Some(v) = a.spacing {
        let _ = write!(s, r#" fo:letter-spacing="{}""#, pt(v as i64));
    }
    if let Some(v) = a.scale {
        let _ = write!(s, r#" style:text-scale="{v}%""#);
    }
    if let Some(fx) = a.effects {
        let _ = write!(s, r#" style:text-outline="{}" fo:text-shadow="{}" style:font-relief="{}""#, fx & EFFECT_OUTLINE != 0, if fx & EFFECT_SHADOW != 0 { "1pt 1pt" } else { "none" }, if fx & EFFECT_EMBOSS != 0 { "embossed" } else if fx & EFFECT_ENGRAVE != 0 { "engraved" } else { "none" });
    }
    if let Some(z) = a.size {
        let p = format!("{}pt", z as f64 / 2.0);
        let _ = write!(s, r#" fo:font-size="{p}" style:font-size-asian="{p}" style:font-size-complex="{p}""#);
    }
    if let Some(v) = a.vert {
        let _ = write!(s, r#" style:text-position="{}""#, match v {
            VertAlign::Super => "super 58%",
            VertAlign::Sub => "sub 58%",
            VertAlign::Baseline => "0% 100%",
        });
    }
    if let Some(c) = a.color {
        let _ = write!(s, r##" fo:color="#{:06x}""##, c & 0xFF_FFFF);
    }
    if let Some(c) = a.highlight {
        let _ = write!(s, r##" fo:background-color="#{:06x}""##, c & 0xFF_FFFF);
    }
    if let Some(f) = &a.family {
        let name = f.to_string();
        if !fonts.contains(&name) {
            fonts.push(name.clone());
        }
        let _ = write!(s, r#" style:font-name="{}""#, esc(&name));
    }
    if let Some(l) = &a.lang_ea {
        let mut parts = l.split('-');
        let lang = parts.next().unwrap_or("");
        let _ = write!(s, r#" style:language-asian="{}""#, esc(lang));
        // ODF has no script subtag: a two-letter region, if any.
        if let Some(c) = parts.find(|p| p.len() == 2) {
            let _ = write!(s, r#" style:country-asian="{}""#, esc(c));
        }
    }
    if s.is_empty() {
        s
    } else {
        format!("<style:text-properties{s}/>")
    }
}

/// A border as `fo:border` has it: width, style, colour.
fn odf_border(b: &Border) -> String {
    let style = match b.style {
        BorderStyle::Single | BorderStyle::Thick => "solid",
        BorderStyle::Double => "double",
        BorderStyle::Dotted => "dotted",
        BorderStyle::Dashed => "dashed",
    };
    // ODF has no "thick" style: a solid rule as thick as it draws.
    let width = if b.style == BorderStyle::Thick { b.width.max(12) } else { b.width };
    format!("{}pt {style} #{:06x}", width as f64 / 8.0, b.color.unwrap_or(0) & 0xff_ffff)
}

/// Hyphenation is a text property in ODF: a paragraph's own setting goes
/// in its style's text properties.
pub(crate) fn hyphenate_props(p: &ParaProps, on: bool) -> String {
    match p.hyphenate {
        Some(v) if on => format!(r#"<style:text-properties fo:hyphenate="{v}"/>"#),
        _ => String::new(),
    }
}

/// `tab_base`: the start indent ODF measures the tab stops from.
pub(crate) fn para_props(p: &ParaProps, tab_base: i32) -> String {
    let mut s = String::new();
    if let Some(a) = p.align {
        let _ = write!(s, r#" fo:text-align="{}""#, match a {
            Align::Start => "start",
            Align::Center => "center",
            Align::End => "end",
            Align::Justify => "justify",
        });
    }
    for (k, v) in [("fo:margin-left", p.indent_start), ("fo:margin-right", p.indent_end), ("fo:text-indent", p.first_line), ("fo:margin-top", p.space_before), ("fo:margin-bottom", p.space_after)] {
        if let Some(v) = v {
            let _ = write!(s, r#" {k}="{}""#, pt(v as i64));
        }
    }
    match p.line_spacing {
        Some(LineSpacing::Multiple(m)) => {
            let _ = write!(s, r#" fo:line-height="{m}%""#);
        }
        Some(LineSpacing::Exact(t)) => {
            let _ = write!(s, r#" fo:line-height="{}""#, pt(t as i64));
        }
        Some(LineSpacing::AtLeast(t)) => {
            let _ = write!(s, r#" style:line-height-at-least="{}""#, pt(t as i64));
        }
        None => {}
    }
    if let Some(v) = p.keep_with_next {
        let _ = write!(s, r#" fo:keep-with-next="{}""#, if v { "always" } else { "auto" });
    }
    if let Some(v) = p.keep_together {
        let _ = write!(s, r#" fo:keep-together="{}""#, if v { "always" } else { "auto" });
    }
    if let Some(v) = p.rtl {
        let _ = write!(s, r#" style:writing-mode="{}""#, if v { "rl-tb" } else { "lr-tb" });
    }
    if let Some(v) = p.page_break_before {
        let _ = write!(s, r#" fo:break-before="{}""#, if v { "page" } else { "auto" });
    }
    if let Some(v) = p.widows {
        let n = if v { 2 } else { 0 };
        let _ = write!(s, r#" fo:widows="{n}" fo:orphans="{n}""#);
    }
    if let Some(b) = &p.borders {
        for (side, x) in [("top", b.top), ("bottom", b.bottom), ("left", b.start), ("right", b.end)] {
            match x {
                Some(x) => {
                    let _ = write!(s, r#" fo:border-{side}="{}" fo:padding-{side}="{}""#, odf_border(&x), pt(x.space as i64 * 20));
                    if x.style == BorderStyle::Double {
                        let w = x.width as f64 / 8.0 / 3.0;
                        let _ = write!(s, r#" style:border-line-width-{side}="{w:.2}pt {w:.2}pt {w:.2}pt""#);
                    }
                }
                None => {
                    let _ = write!(s, r#" fo:border-{side}="none""#);
                }
            }
        }
        // Paragraphs with the same borders are one box (ODF's default).
        s += r#" style:join-border="true""#;
    }
    if let Some(v) = p.shading {
        match v {
            Some(c) => {
                let _ = write!(s, r##" fo:background-color="#{:06x}""##, c & 0xFF_FFFF);
            }
            None => s += r#" fo:background-color="transparent""#,
        }
    }
    if let Some(v) = p.line_numbers {
        let _ = write!(s, r#" text:number-lines="{v}""#);
    }
    let mut tabs = String::new();
    if let Some(t) = &p.tabs {
        tabs += "<style:tab-stops>";
        for x in t {
            let ty = match x.align {
                TabAlign::Start => "left",
                TabAlign::Center => "center",
                TabAlign::End => "right",
                TabAlign::Decimal => "char",
            };
            let ch = if x.align == TabAlign::Decimal { r#" style:char=".""# } else { "" };
            let leader = match x.leader {
                Leader::None => "",
                Leader::Dots => r#" style:leader-style="dotted" style:leader-text=".""#,
                Leader::Dashes => r#" style:leader-style="dash" style:leader-text="-""#,
                Leader::Line => r#" style:leader-style="solid" style:leader-text="_""#,
            };
            let _ = write!(tabs, r#"<style:tab-stop style:position="{}" style:type="{ty}"{ch}{leader}/>"#, pt((x.pos - tab_base) as i64));
        }
        tabs += "</style:tab-stops>";
    }
    if let Some(Some(d)) = p.drop_cap {
        // ODF has no capital in the margin: it drops in the text.
        let _ = write!(tabs, r#"<style:drop-cap style:lines="{}" style:length="1" style:distance="{}"/>"#, d.lines, pt(d.distance as i64));
    }
    match (s.is_empty(), tabs.is_empty()) {
        (true, true) => s,
        (_, true) => format!("<style:paragraph-properties{s}/>"),
        _ => format!("<style:paragraph-properties{s}>{tabs}</style:paragraph-properties>"),
    }
}

/// A list level as a `text:list-level-style-*` (`l` from 1): its label
/// as prefix, the levels shown and suffix.
fn list_level_xml(l: u8, lv: &ListLevel) -> String {
    let ind = format!(r#"<style:list-level-properties text:list-level-position-and-space-mode="label-alignment"><style:list-level-label-alignment text:label-followed-by="listtab" text:list-tab-stop-position="{}" fo:text-indent="{}" fo:margin-left="{}"/></style:list-level-properties>"#, pt(lv.indent as i64), pt(-lv.hang as i64), pt(lv.indent as i64));
    if lv.format == NumFormat::Bullet {
        let c = lv.text.chars().next().unwrap_or('•');
        return format!(r#"<text:list-level-style-bullet text:level="{l}" text:bullet-char="{}">{ind}</text:list-level-style-bullet>"#, esc(&c.to_string()));
    }
    let f = match lv.format {
        NumFormat::Decimal => "1",
        NumFormat::DecimalZero => "01, 02, 03, ...",
        NumFormat::LowerLetter => "a",
        NumFormat::UpperLetter => "A",
        NumFormat::LowerRoman => "i",
        NumFormat::UpperRoman => "I",
        NumFormat::Bullet | NumFormat::None => "",
    };
    let (prefix, shown, suffix) = label_parts(&lv.text, l);
    let shown = if shown > 1 { format!(r#" text:display-levels="{shown}""#) } else { String::new() };
    let start = if lv.start != 1 { format!(r#" text:start-value="{}""#, lv.start) } else { String::new() };
    // LibreOffice's whole label, for separators other than ".".
    let mut whole = String::new();
    let mut it = lv.text.chars().peekable();
    while let Some(c) = it.next() {
        whole.push(c);
        if c == '%' && it.peek().is_some_and(|d| ('1'..='9').contains(d)) {
            whole.push(it.next().unwrap_or('1'));
            whole.push('%');
        }
    }
    format!(r#"<text:list-level-style-number text:level="{l}" loext:num-list-format="{}" style:num-prefix="{}" style:num-suffix="{}" style:num-format="{f}"{shown}{start}>{ind}</text:list-level-style-number>"#, esc(&whole), esc(prefix), esc(suffix))
}

/// A label's text before its first number and after its last, and how
/// many levels' numbers it shows (ending at its own, `l`, from 1).
pub(crate) fn label_parts(text: &str, l: u8) -> (&str, u8, &str) {
    let b = text.as_bytes();
    let marks: Vec<(usize, u8)> = (0..b.len().saturating_sub(1)).filter(|&i| b[i] == b'%' && (b'1'..=b'9').contains(&b[i + 1])).map(|i| (i, b[i + 1] - b'0')).collect();
    match (marks.first(), marks.last()) {
        (Some(&(a, first)), Some(&(z, _))) => (&text[..a], l.saturating_sub(first) + 1, &text[z + 2..]),
        _ => (text, 0, ""),
    }
}

/// The list block `b` is an item of: its own, or its style's.
fn list_of(doc: &TextDoc, b: usize) -> Option<officina_text::model::ListRef> {
    let p = doc.para(b)?;
    match p.props.list {
        Some(l) => l,
        None => doc.styles.para_format(p.style, &p.props).0.list,
    }
}

/// The bookmark name a field refers to paragraph `id` by.
fn ref_name(id: BlockId) -> String {
    format!("_Ref{id}")
}

/// What a paragraph's automatic style holds: its direct properties and the
/// formatting of an empty paragraph.
/// A section starting on a new page (but the first) starts the master page
/// again at its first paragraph, as LibreOffice has Word's section breaks
/// (its columns go on the `text:section`); `section` stands for that.
/// A section with a page of its own starts its own master page (the first
/// one too).
/// `page_change`: the section's page differs from the one before's, so it
/// starts its master page even when continuous.
fn para_key(p: &ParaProps, first: bool, page_change: bool) -> ParaProps {
    let own = p.section.and_then(|s| s.page);
    let new_page = (p.section.is_some_and(|s| s.start != SectionStart::Continuous || page_change) && !first) || (first && own.is_some());
    // An odd or even page start goes with the master page (one kept to right
    // or left pages, as LibreOffice has it).
    let start = match p.section.map(|s| s.start) {
        Some(s @ (SectionStart::OddPage | SectionStart::EvenPage)) if !first => s,
        _ => SectionStart::NextPage,
    };
    ParaProps { list: p.list.filter(|l| l.is_none()), list_start: None, cell: None, outline_level: None, section: new_page.then(|| Section { page: own, start, ..Section::default() }), ..p.clone() }
}

struct Out<'a> {
    doc: &'a TextDoc,
    /// Before its automatic style and picture names (`M` in the header and
    /// footer, whose styles live in styles.xml).
    prefix: &'static str,
    names: Vec<String>,
    fonts: Vec<String>,
    auto: String,
    para_styles: HashMap<(u16, ParaProps), String>,
    text_styles: HashMap<CharAttrs, String>,
    list_styles: HashMap<(u32, ListKind), String>,
    lists_seen: Vec<u32>,
    media: Vec<(String, Vec<u8>)>,
    image_names: HashMap<u32, String>,
    tables: u32,
    frames: u32,
    /// Writing a note's text (notes don't nest).
    in_note: u32,
    /// Comment markers by block: (byte, end?, comment id), in order; a reply
    /// goes out after the comment it answers, with no end.
    marks: HashMap<usize, Vec<(u32, bool, CommentId)>>,
    /// The block being written (none in notes).
    cur: Option<usize>,
    /// Tracked changes: the changed regions written, and the insertion open
    /// (it may run over paragraph breaks).
    regions: String,
    nchange: u32,
    ins_open: Option<(RevId, String)>,
    pending_end: Option<String>,
    /// Paragraphs fields refer to, by id: each gets a bookmark.
    targets: std::collections::HashSet<BlockId>,
    /// Graphic styles of floating pictures, by placement.
    graphic_styles: HashMap<Float, String>,
    /// Drawn shapes' graphic styles written.
    shape_styles: u32,
    /// Dashes and arrowheads (`office:styles`), by name.
    common: String,
    common_names: std::collections::HashSet<String>,
    /// Sections' own pages, each a master page (`Section_20_Page_{n}`, from 1).
    pages: Vec<officina_text::model::PageSetup>,
    /// Blocks starting a section whose page differs from the section before's.
    page_changes: std::collections::HashSet<usize>,
    /// Master pages sections start on: (own page, 0 any / 1 right / 2 left).
    sides: std::collections::HashSet<(Option<usize>, u8)>,
}

/// The name an annotation goes by.
fn annotation_name(id: CommentId) -> String {
    format!("__Annotation__{id}")
}

impl Out<'_> {
    fn para_style(&mut self, style: u16, p: &ParaProps) -> String {
        let key = (style, para_key(p, self.cur == Some(0), self.cur.is_some_and(|k| self.page_changes.contains(&k))));
        if let Some(n) = self.para_styles.get(&key) {
            return n.clone();
        }
        let base = self.doc.styles.para_format(style, p).0.indent_start.max(0);
        let body = para_props(&key.1, base) + &hyphenate_props(&key.1, self.doc.settings.hyphenate);
        let unlisted = key.1.list == Some(None) && self.doc.styles.para_format(style, &ParaProps::default()).0.list.is_some();
        if body.is_empty() && key.1.section.is_none() && !unlisted {
            return self.names.get(style as usize).cloned().unwrap_or_else(|| "Standard".into());
        }
        let n = format!("{}P{}", self.prefix, self.para_styles.len() + 1);
        let parent = self.names.get(style as usize).cloned().unwrap_or_else(|| "Standard".into());
        let side = |s: &Section| match s.start {
            SectionStart::OddPage => 1u8,
            SectionStart::EvenPage => 2,
            _ => 0,
        };
        let suffix = ["", "_Right", "_Left"];
        let master = match key.1.section {
            Some(s @ Section { page: Some(g), .. }) => {
                let k = match self.pages.iter().position(|x| *x == g) {
                    Some(k) => k,
                    None => {
                        self.pages.push(g);
                        self.pages.len() - 1
                    }
                };
                self.sides.insert((Some(k), side(&s)));
                format!(r#" style:master-page-name="Section_20_Page_{}{}""#, k + 1, suffix[side(&s) as usize])
            }
            Some(s) => {
                self.sides.insert((None, side(&s)));
                format!(r#" style:master-page-name="Section_20_Page{}""#, suffix[side(&s) as usize])
            }
            None => String::new(),
        };
        let unlist = if unlisted { " style:list-style-name=\"\"" } else { "" };
        let _ = write!(self.auto, r#"<style:style style:name="{n}" style:family="paragraph" style:parent-style-name="{}"{master}{unlist}>{body}</style:style>"#, esc(&parent));
        self.para_styles.insert(key, n.clone());
        n
    }

    fn text_style(&mut self, a: &CharAttrs) -> Option<String> {
        let mut k = a.clone();
        k.link = None;
        k.object = None;
        k.note = None;
        k.rev = None;
        k.deleted = None;
        k.fmt = None;
        if k == CharAttrs::default() {
            return None;
        }
        if let Some(n) = self.text_styles.get(&k) {
            return Some(n.clone());
        }
        let n = format!("{}T{}", self.prefix, self.text_styles.len() + 1);
        let parent = k.style.and_then(|s| self.names.get(s as usize)).map(|p| format!(r#" style:parent-style-name="{}""#, esc(p))).unwrap_or_default();
        let props = text_props(&k, &mut self.fonts);
        let _ = write!(self.auto, r#"<style:style style:name="{n}" style:family="text"{parent}>{props}</style:style>"#);
        self.text_styles.insert(k, n.clone());
        Some(n)
    }

    fn list_style(&mut self, id: u32, kind: ListKind) -> String {
        if let Some(n) = self.list_styles.get(&(id, kind)) {
            return n.clone();
        }
        let n = format!("L{}", self.list_styles.len() + 1);
        let _ = write!(self.auto, r#"<text:list-style style:name="{n}">"#);
        let def = self.doc.styles.lists.get(&id).cloned().unwrap_or_else(|| ListDef::builtin(kind));
        for l in 0..10u8 {
            let lv = def.level(l.min(8)).cloned().unwrap_or_else(|| ListLevel::builtin(kind, l.min(8)));
            self.auto += &list_level_xml(l + 1, &lv);
        }
        self.auto += "</text:list-style>";
        self.list_styles.insert((id, kind), n.clone());
        n
    }

    /// The automatic graphic style of a floating picture: its wrap, gap and
    /// how its position is measured.
    fn graphic_style(&mut self, f: &Float) -> String {
        if let Some(n) = self.graphic_styles.get(f) {
            return n.clone();
        }
        let n = format!("{}fr{}", self.prefix, self.graphic_styles.len() + 1);
        let _ = write!(self.auto, r#"<style:style style:name="{n}" style:family="graphic"><style:graphic-properties{}/></style:style>"#, float_props(f));
        self.graphic_styles.insert(*f, n.clone());
        n
    }

    /// A drawn shape's object: a `draw:custom-shape` (a `draw:line` for a
    /// line, a `draw:g` for a group) with its own graphic style, placed as
    /// a picture is, its paragraphs inside.
    fn shape_object(&mut self, sd: &ShapeData, o: &officina_text::model::ObjectRef, out: &mut String) {
        self.frames += 1;
        let (w, h) = (o.width as f64, o.height as f64);
        let (anchor, x, y) = match &o.float {
            None => (r#" text:anchor-type="as-char""#.to_string(), 0.0, 0.0),
            Some(f) => (r#" text:anchor-type="char""#.to_string(), if f.h_align == HAlign::Offset { f.x as f64 } else { 0.0 }, f.y as f64),
        };
        let mut budget = MAX_GROUP_SHAPES;
        self.drawn(sd, o.float.as_ref(), &anchor, [x, y, w, h], o.float.is_none(), 0, &mut budget, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn drawn(&mut self, sd: &ShapeData, float: Option<&Float>, anchor: &str, b: [f64; 4], inline: bool, depth: usize, budget: &mut usize, out: &mut String) {
        if depth > MAX_GROUP_DEPTH || *budget == 0 {
            return;
        }
        *budget -= 1;
        let [x, y, w, h] = b;
        let name = format!(r#" draw:name="Shape {}-{}""#, self.frames, MAX_GROUP_SHAPES - *budget);
        let wrap = float.map(float_props).unwrap_or_default();
        if !sd.kids.is_empty() {
            let st = self.shape_style(&format!(r#"{wrap} draw:fill="none" draw:stroke="none""#));
            let _ = write!(out, r#"<draw:g draw:style-name="{st}"{name}{anchor} draw:z-index="0">"#);
            if sd.fill.is_some() || sd.line.is_some() || !sd.paras.is_empty() {
                let own = ShapeData { kids: Vec::new(), ..sd.clone() };
                self.drawn(&own, None, "", b, false, depth + 1, budget, out);
            }
            for k in &sd.kids {
                let kb = [x + k.x as f64 * w / 1e5, y + k.y as f64 * h / 1e5, k.w as f64 * w / 1e5, k.h as f64 * h / 1e5];
                self.drawn(&k.shape, None, "", kb, false, depth + 1, budget, out);
            }
            out.push_str("</draw:g>");
            return;
        }
        let mut g = wrap;
        match sd.fill {
            Some(c) => {
                let _ = write!(g, r#" draw:fill="solid" draw:fill-color="{}""#, hex(c));
            }
            None => g.push_str(r#" draw:fill="none""#),
        }
        g.push_str(&self.stroke(sd.line.as_ref()));
        let [l, t, r, bt] = sd.inset;
        let _ = write!(g, r#" fo:padding-left="{}" fo:padding-top="{}" fo:padding-right="{}" fo:padding-bottom="{}" draw:textarea-horizontal-align="justify" draw:textarea-vertical-align="{}" draw:auto-grow-height="false" fo:wrap-option="wrap""#, pt(l as i64), pt(t as i64), pt(r as i64), pt(bt as i64), match sd.anchor {
            TextAnchor::Top => "top",
            TextAnchor::Middle => "middle",
            TextAnchor::Bottom => "bottom",
        });
        let st = self.shape_style(&g);
        let mut body = String::new();
        self.in_note += 1;
        for p in &sd.paras {
            let mut q = p.clone();
            q.props.list = None;
            q.props.cell = None;
            self.para_of(&q, &mut body);
        }
        self.in_note -= 1;
        let deg = sd.rot as f64 / 60_000.0;
        let is_line = matches!(&sd.geom, Geom::Preset { name, .. } if &**name == "line" || &**name == "straightConnector1");
        if is_line {
            let (mut x1, mut y1, mut x2, mut y2) = (x, y, x + w, y + h);
            if sd.flip_h {
                std::mem::swap(&mut x1, &mut x2);
            }
            if sd.flip_v {
                std::mem::swap(&mut y1, &mut y2);
            }
            if deg.abs() > 1e-6 {
                let t = deg.to_radians();
                let (cx, cy) = (x + w / 2.0, y + h / 2.0);
                let turn = |px: f64, py: f64| (cx + (px - cx) * t.cos() - (py - cy) * t.sin(), cy + (px - cx) * t.sin() + (py - cy) * t.cos());
                (x1, y1) = turn(x1, y1);
                (x2, y2) = turn(x2, y2);
            }
            let f = |v: f64| pt(v.round() as i64);
            let (tag, ty) = if matches!(&sd.geom, Geom::Preset { name, .. } if &**name == "straightConnector1") { ("draw:connector", r#" draw:type="line""#) } else { ("draw:line", "") };
            let _ = write!(out, r#"<{tag} draw:style-name="{st}"{name}{anchor} draw:z-index="0"{ty} svg:x1="{}" svg:y1="{}" svg:x2="{}" svg:y2="{}">{body}</{tag}>"#, f(x1), f(y1), f(x2), f(y2));
            return;
        }
        let place = if inline && deg.abs() < 1e-6 { format!(r#" svg:width="{}" svg:height="{}""#, pt(w.round() as i64), pt(h.round() as i64)) } else { place(b, deg) };
        let mirror = format!("{}{}", if sd.flip_h { r#" draw:mirror-horizontal="true""# } else { "" }, if sd.flip_v { r#" draw:mirror-vertical="true""# } else { "" });
        let geo = crate::odp::write::geometry(&sd.geom, w * 635.0, h * 635.0, true);
        let _ = write!(out, r#"<draw:custom-shape draw:style-name="{st}"{name}{anchor} draw:z-index="0"{place}>{body}<draw:enhanced-geometry{mirror} {geo}/></draw:custom-shape>"#);
    }

    fn shape_style(&mut self, props: &str) -> String {
        let n = format!("{}gr{}", self.prefix, self.shape_styles + 1);
        self.shape_styles += 1;
        let _ = write!(self.auto, r#"<style:style style:name="{n}" style:family="graphic"><style:graphic-properties{props}/></style:style>"#);
        n
    }

    /// A shape's outline attributes; its arrowheads' markers go to the
    /// common styles.
    fn stroke(&mut self, l: Option<&ShapeLine>) -> String {
        let Some(l) = l else { return r#" draw:stroke="none""#.into() };
        let mut s = String::new();
        let dash = match l.dash {
            Dash::Solid => None,
            Dash::Dot => Some(("Dot", r#"draw:dots1="1" draw:dots1-length="100%" draw:distance="100%""#)),
            Dash::Dash => Some(("Dash", r#"draw:dots1="1" draw:dots1-length="400%" draw:distance="300%""#)),
            Dash::LongDash => Some(("Long_Dash", r#"draw:dots1="1" draw:dots1-length="800%" draw:distance="300%""#)),
            Dash::DashDot => Some(("Dash_Dot", r#"draw:dots1="1" draw:dots1-length="400%" draw:dots2="1" draw:dots2-length="100%" draw:distance="300%""#)),
        };
        match dash {
            Some((n, attrs)) => {
                let name = format!("Officina_{n}");
                if !self.common_names.contains(&name) {
                    let _ = write!(self.common, r#"<draw:stroke-dash draw:name="{name}" draw:display-name="{n}" draw:style="rect" {attrs}/>"#);
                    self.common_names.insert(name.clone());
                }
                let _ = write!(s, r#" draw:stroke="dash" draw:stroke-dash="{name}""#);
            }
            None => s.push_str(r#" draw:stroke="solid""#),
        }
        let _ = write!(s, r#" svg:stroke-color="{}" svg:stroke-width="{}""#, hex(l.color), pt(l.width as i64));
        let mw = pt((l.width as i64 * 3).max(113));
        for (end, a) in [("start", l.head), ("end", l.tail)] {
            let m = match a {
                ArrowEnd::None => continue,
                ArrowEnd::Triangle => ("Arrow", "0 0 20 30", "M10 0 L20 30 L0 30 Z"),
                ArrowEnd::Open => ("Line_20_Arrow", "0 0 20 30", "M10 0 L20 28 L17 30 L10 8 L3 30 L0 28 Z"),
                ArrowEnd::Stealth => ("Arrow_20_concave", "0 0 20 30", "M10 0 L20 30 L10 22 L0 30 Z"),
                ArrowEnd::Diamond => ("Diamond", "0 0 20 20", "M10 0 L20 10 L10 20 L0 10 Z"),
                ArrowEnd::Oval => ("Circle", "0 0 20 20", "M20 10 C20 15.5 15.5 20 10 20 C4.5 20 0 15.5 0 10 C0 4.5 4.5 0 10 0 C15.5 0 20 4.5 20 10 Z"),
            };
            if !self.common_names.contains(m.0) {
                self.common_names.insert(m.0.to_string());
                let _ = write!(self.common, r#"<draw:marker draw:name="{}" draw:display-name="{}" svg:viewBox="{}" svg:d="{}"/>"#, m.0, m.0.replace("_20_", " "), m.1, m.2);
            }
            let _ = write!(s, r#" draw:marker-{end}="{}" draw:marker-{end}-width="{mw}""#, m.0);
        }
        s
    }

    fn image(&mut self, i: u32) -> Option<String> {
        if let Some(n) = self.image_names.get(&i) {
            return Some(n.clone());
        }
        let img = self.doc.images.get(i)?;
        let b = &img.bytes;
        let ext = if let Some(k) = officina_pict::sniff(b) { k.name() } else if b.starts_with(&[0xFF, 0xD8]) { "jpg" } else if b.starts_with(b"GIF8") { "gif" } else if b.starts_with(b"RIFF") { "webp" } else if crate::docx::write::is_svg(b) { "svg" } else { "png" };
        let n = format!("Pictures/{}image{}.{ext}", self.prefix, self.media.len() + 1);
        self.media.push((n.clone(), b.to_vec()));
        self.image_names.insert(i, n.clone());
        Some(n)
    }

    /// Text with ODF's spaces, tabs and breaks.
    fn text(s: &str, out: &mut String) {
        let mut spaces = 0usize;
        let flush = |n: &mut usize, out: &mut String, at_start: bool| {
            if *n == 0 {
                return;
            }
            // The first space of a run is a plain space unless it starts the text.
            if at_start {
                let _ = write!(out, r#"<text:s text:c="{n}"/>"#);
            } else {
                out.push(' ');
                if *n > 1 {
                    let _ = write!(out, r#"<text:s text:c="{}"/>"#, *n - 1);
                }
            }
            *n = 0;
        };
        let mut start = true;
        for c in s.chars() {
            match c {
                ' ' => spaces += 1,
                '\t' => {
                    flush(&mut spaces, out, start);
                    out.push_str("<text:tab/>");
                    start = false;
                }
                '\u{2028}' | '\n' => {
                    flush(&mut spaces, out, start);
                    out.push_str("<text:line-break/>");
                    start = true;
                }
                c => {
                    flush(&mut spaces, out, start);
                    out.push_str(&esc(&c.to_string()));
                    start = false;
                }
            }
        }
        flush(&mut spaces, out, start);
    }

    /// A changed region of kind `tag` (insertion, deletion, format-change) by
    /// revision `rev`; its id.
    fn region(&mut self, tag: &str, rev: RevId, extra: &str, content: &str) -> String {
        self.nchange += 1;
        let id = format!("ct{}", self.nchange);
        let (author, date) = self.doc.revisions.get(rev).map(|r| (r.author.to_string(), r.date.to_string())).unwrap_or_default();
        let date = if date.is_empty() { "1970-01-01T00:00:00".to_string() } else { date };
        let _ = write!(self.regions, r#"<text:changed-region xml:id="{id}" text:id="{id}"><text:{tag}{extra}><office:change-info><dc:creator>{}</dc:creator><dc:date>{}</dc:date></office:change-info>{content}</text:{tag}></text:changed-region>"#, esc(&author), esc(&date));
        id
    }

    /// Closes the insertion open, if any.
    fn close_ins(&mut self, out: &mut String) {
        if let Some((_, id)) = self.ins_open.take() {
            let _ = write!(out, r#"<text:change-end text:change-id="{id}"/>"#);
        }
    }

    /// The insertion over what comes next is `rev`'s (or none).
    fn ins_to(&mut self, rev: Option<RevId>, out: &mut String) {
        if self.ins_open.as_ref().map(|o| o.0) == rev {
            return;
        }
        self.close_ins(out);
        if let Some(r) = rev {
            let id = self.region("insertion", r, "", "");
            let _ = write!(out, r#"<text:change-start text:change-id="{id}"/>"#);
            self.ins_open = Some((r, id));
        }
    }

    /// A deletion's place in the text, its content in a region.
    fn deletion(&mut self, t: officina_text::model::DelId, style: u16, out: &mut String) {
        let doc = self.doc;
        let Some(d) = doc.deletions.get(t) else { return };
        let mut content = String::new();
        for (k, piece) in d.pieces.iter().enumerate() {
            let (st, props) = match &piece.para {
                Some((_, s, p)) if k > 0 => (*s, p.clone()),
                _ => (style, ParaProps::default()),
            };
            let name = self.para_style(st, &para_key(&props, true, false));
            let _ = write!(content, r#"<text:p text:style-name="{}">"#, esc(&name));
            for (i, &(rs, ra)) in piece.runs.iter().enumerate() {
                let re = piece.runs.get(i + 1).map_or(piece.text.len() as u32, |r| r.0);
                if rs >= re {
                    continue;
                }
                let text: String = piece.text[rs as usize..re as usize].chars().filter(|&c| c != OBJECT_CHAR).collect();
                let mut inner = String::new();
                Self::text(&text, &mut inner);
                match self.text_style(doc.attrs.get(ra)) {
                    Some(n) => {
                        let _ = write!(content, r#"<text:span text:style-name="{n}">{inner}</text:span>"#);
                    }
                    None => content.push_str(&inner),
                }
            }
            content.push_str("</text:p>");
        }
        let id = self.region("deletion", d.rev, "", &content);
        let _ = write!(out, r#"<text:change text:change-id="{id}"/>"#);
    }

    fn para(&mut self, b: usize, out: &mut String) {
        let doc = self.doc;
        let Some(p) = doc.para(b) else { return };
        self.cur = Some(b);
        if p.text.contains('\u{000C}') {
            // ODF has no page break inside a paragraph: the text after each
            // starts a paragraph of its own on a new page (as LibreOffice does).
            for (i, piece) in split_page_breaks(p).iter().enumerate() {
                if i == 1 {
                    self.cur = None;
                }
                self.para_of(piece, out);
            }
        } else {
            self.para_of(p, out);
        }
        self.cur = None;
    }

    /// An annotation's start (the comment itself) or its end.
    fn annotation(&self, out: &mut String, end: bool, id: CommentId) {
        let name = annotation_name(id);
        if end {
            let _ = write!(out, r#"<office:annotation-end office:name="{name}"/>"#);
            return;
        }
        let Some(c) = self.doc.comments.get(id) else { return };
        let parent = c.parent.map(|p| format!(r#" loext:parent-name="{}""#, annotation_name(p))).unwrap_or_default();
        let done = if c.done { r#" loext:resolved="true""# } else { "" };
        let _ = write!(out, r#"<office:annotation office:name="{name}"{parent}{done}><dc:creator>{}</dc:creator>"#, esc(&c.author));
        if !c.date.is_empty() {
            let _ = write!(out, "<dc:date>{}</dc:date>", esc(&c.date));
        }
        if !c.initials.is_empty() {
            let _ = write!(out, "<meta:creator-initials>{}</meta:creator-initials>", esc(&c.initials));
        }
        for line in c.text.split('\n') {
            out.push_str("<text:p>");
            Self::text(line, out);
            out.push_str("</text:p>");
        }
        out.push_str("</office:annotation>");
    }

    fn para_of(&mut self, p: &Para, out: &mut String) {
        let doc = self.doc;
        let mut props = p.props.clone();
        if p.text.is_empty() {
            // An empty paragraph's formatting rides on its paragraph style.
            props = props.clone();
        }
        let mut st = self.para_style(p.style, &props);
        // A contents entry in the default style: LibreOffice's Contents style of its level.
        if let (0, Some(l), None, 0) = (p.style, p.props.toc, p.props.cell, self.in_note) {
            st = if st == self.names[0] { format!("Contents_20_{}", l.clamp(1, 9)) } else { st };
            let (auto, lvl) = (st.starts_with('P'), l.clamp(1, 9));
            if auto {
                // Its automatic style's parent is the Contents style.
                let from = format!(r#"<style:style style:name="{st}" style:family="paragraph" style:parent-style-name="{}">"#, esc(&self.names[0]));
                let to = format!(r#"<style:style style:name="{st}" style:family="paragraph" style:parent-style-name="Contents_20_{lvl}">"#);
                self.auto = self.auto.replacen(&from, &to, 1);
            }
        }
        let level = doc.styles.para_format(p.style, &p.props).0.outline_level.filter(|_| doc.styles.styles.get(p.style as usize).is_some_and(|s| s.name.starts_with("Heading")));
        let tag = if level.is_some() { "text:h" } else { "text:p" };
        let lvl = level.map(|l| format!(r#" text:outline-level="{l}""#)).unwrap_or_default();
        let _ = write!(out, r#"<{tag} text:style-name="{}"{lvl}>"#, esc(&st));
        if self.in_note == 0 && self.cur.is_some() && self.targets.contains(&p.id) {
            let _ = write!(out, r#"<text:bookmark text:name="{}"/>"#, ref_name(p.id));
        }
        let marks = if self.in_note == 0 { self.cur.and_then(|b| self.marks.get(&b)).cloned().unwrap_or_default() } else { Vec::new() };
        let mut m = 0;
        for (s, e, a) in p.spans() {
            while m < marks.len() && marks[m].0 <= s {
                self.annotation(out, marks[m].1, marks[m].2);
                m += 1;
            }
            if s == e {
                continue;
            }
            let attrs = doc.attrs.get(a).clone();
            let piece = &p.text[s as usize..e as usize];
            let tracked = self.in_note == 0 && self.cur.is_some();
            let mut fmt_end = false;
            if tracked {
                if let Some(t) = attrs.deleted.filter(|_| attrs.object.is_none() && attrs.note.is_none()) {
                    for _ in piece.chars() {
                        self.deletion(t, p.style, out);
                    }
                    continue;
                }
                self.ins_to(attrs.rev, out);
                if let Some(m) = attrs.fmt {
                    let mut old = doc.attrs.get(m.old).clone();
                    old.comments = None;
                    let name = self.text_style(&old).unwrap_or_default();
                    let id = self.region("format-change", m.rev, &format!(r#" officina:old-style="{}""#, esc(&name)), "");
                    let _ = write!(out, r#"<text:change-start text:change-id="{id}"/>"#);
                    fmt_end = true;
                    self.pending_end = Some(id);
                }
            }
            let mut inner = String::new();
            let mut from = 0;
            for (i, c) in piece.char_indices().chain(std::iter::once((piece.len(), '\0'))) {
                if c != OBJECT_CHAR && i < piece.len() {
                    continue;
                }
                Self::text(&piece[from..i], &mut inner);
                if let (true, Some(f)) = (i < piece.len(), attrs.field) {
                    let (fmt, t) = match f {
                        Field::PageRef(t) => ("page", t),
                        Field::TextRef(t) => ("text", t),
                    };
                    let _ = write!(inner, r#"<text:bookmark-ref text:reference-format="{fmt}" text:ref-name="{}">"#, ref_name(t));
                    Self::text(&doc.field_text(f), &mut inner);
                    inner += "</text:bookmark-ref>";
                }
                if i < piece.len() && attrs.object.is_none() {
                    // A note: its number and text, in place.
                    if let (Some(id), 0) = (attrs.note, self.in_note) {
                        if let (Some(&(kind, n)), Some(note)) = (self.doc.note_numbers().get(&id), self.doc.notes.get(id)) {
                            let (class, pre) = if kind == NoteKind::Foot { ("footnote", "ftn") } else { ("endnote", "edn") };
                            let label = officina_text::model::note_label(kind, n);
                            let _ = write!(inner, r#"<text:note text:id="{pre}{n}" text:note-class="{class}"><text:note-citation>{label}</text:note-citation><text:note-body>"#);
                            self.in_note += 1;
                            for q in &note.paras {
                                let mut q = q.clone();
                                q.props.list = None;
                                q.props.cell = None;
                                self.para_of(&q, &mut inner);
                            }
                            self.in_note -= 1;
                            inner += "</text:note-body></text:note>";
                        }
                    }
                }
                if i < piece.len() {
                    if let Some(o) = attrs.object.filter(|o| o.shape) {
                        if let Some(sd) = doc.shapes.get(o.image) {
                            self.shape_object(&sd.clone(), &o, &mut inner);
                        }
                    } else if let Some(o) = attrs.object {
                        if let Some(href) = self.image(o.image) {
                            self.frames += 1;
                            let place = match o.float {
                                None => r#"text:anchor-type="as-char""#.to_string(),
                                Some(f) => {
                                    let st = self.graphic_style(&f);
                                    let x = if f.h_align == HAlign::Offset { format!(r#" svg:x="{}""#, pt(f.x as i64)) } else { String::new() };
                                    format!(r#"draw:style-name="{st}" text:anchor-type="char"{x} svg:y="{}""#, pt(f.y as i64))
                                }
                            };
                            let _ = write!(inner, r#"<draw:frame draw:name="Picture {}" {place} svg:width="{}" svg:height="{}" draw:z-index="0"><draw:image xlink:href="{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame>"#, self.frames, pt(o.width as i64), pt(o.height as i64), esc(&href));
                        }
                    }
                }
                from = i + c.len_utf8();
            }
            let inner = match self.text_style(&attrs) {
                Some(t) => format!(r#"<text:span text:style-name="{t}">{inner}</text:span>"#),
                None => inner,
            };
            match &attrs.link {
                Some(l) => {
                    let _ = write!(out, r#"<text:a xlink:type="simple" xlink:href="{}">{inner}</text:a>"#, esc(l));
                }
                None => out.push_str(&inner),
            }
            if fmt_end {
                if let Some(id) = self.pending_end.take() {
                    let _ = write!(out, r#"<text:change-end text:change-id="{id}"/>"#);
                }
            }
        }
        for &(_, end, id) in &marks[m..] {
            self.annotation(out, end, id);
        }
        // An insertion runs on over the break after this paragraph when that
        // break is inserted too.
        if self.in_note == 0 {
            if let Some(b) = self.cur {
                let next = doc.para(b + 1).filter(|_| doc.same_cell(b, b + 1)).and_then(|q| q.props.rev);
                match next {
                    Some(r) => self.ins_to(Some(r), out),
                    None => self.close_ins(out),
                }
            }
        }
        let _ = write!(out, "</{tag}>");
    }

    /// A run of list items from `b`; returns the block after it.
    fn list(&mut self, b: usize, end: usize, out: &mut String) -> usize {
        let doc = self.doc;
        let first = list_of(doc, b).expect("a list item");
        let style = self.list_style(first.id, first.kind);
        let cont = if self.lists_seen.contains(&first.id) { r#" text:continue-numbering="true""# } else { "" };
        self.lists_seen.push(first.id);
        let _ = write!(out, r#"<text:list text:style-name="{style}"{cont}>"#);
        let mut open: Vec<bool> = vec![false];
        let mut k = b;
        while k < end {
            let Some(l) = list_of(doc, k).filter(|l| l.id == first.id && l.kind == first.kind) else { break };
            let lv = l.level as usize;
            while open.len() - 1 > lv {
                if open.pop() == Some(true) {
                    out.push_str("</text:list-item>");
                }
                out.push_str("</text:list>");
            }
            let item = match doc.para(k).and_then(|p| p.props.list_start) {
                Some(v) => format!(r#"<text:list-item text:start-value="{v}">"#),
                None => "<text:list-item>".to_string(),
            };
            if open.len() - 1 == lv {
                if open[lv] {
                    out.push_str("</text:list-item>");
                }
                out.push_str(&item);
                open[lv] = true;
            }
            while open.len() - 1 < lv {
                let top = open.len() - 1;
                if !open[top] {
                    out.push_str("<text:list-item>");
                    open[top] = true;
                }
                out.push_str("<text:list><text:list-item>");
                open.push(true);
            }
            self.para(k, out);
            k += 1;
        }
        while let Some(o) = open.pop() {
            if o {
                out.push_str("</text:list-item>");
            }
            out.push_str("</text:list>");
        }
        k
    }

    fn table(&mut self, b: usize, out: &mut String) -> usize {
        self.table_at(b, self.doc.blocks.len(), false, out)
    }

    /// The table at a depth (`inner`: the one in a cell, ending by
    /// `limit`) starting at block `b`; returns the block after it.
    fn table_at(&mut self, b: usize, limit: usize, inner: bool, out: &mut String) -> usize {
        let doc = self.doc;
        let t = doc.cell_at(b, inner).map(|c| c.table);
        let mut end = b;
        while end < limit && doc.cell_at(end, inner).map(|c| c.table) == t {
            end += 1;
        }
        self.tables += 1;
        let name = format!("Table{}", self.tables);
        // Rows: each cell's blocks, its edges across (twips), and how it is
        // drawn (ODF has no table styles: each cell carries its look).
        let mut rows: Vec<(Vec<(std::ops::Range<usize>, u32, u32, officina_text::model::CellPaint)>, officina_text::model::RowLook)> = Vec::new();
        let mut k = b;
        while k < end {
            let row = doc.row_range_at(k, inner);
            let row = k..row.end.max(k + 1).min(end);
            let cells = cells_of(doc, row.clone(), inner);
            let paints = doc.row_paints_at(row.clone(), cells.len(), inner);
            let mut x = 0u32;
            let v = cells
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    let w = doc.cell_at(c.start, inner).map_or(1440, |x| x.width);
                    let r = (c, x, x + w, paints.get(i).copied().unwrap_or_default());
                    x += w;
                    r
                })
                .collect();
            rows.push((v, doc.cell_at(k, inner).map(|c| c.row_look).unwrap_or_default()));
            k = row.end;
        }
        let mut edges: Vec<u32> = rows.iter().flat_map(|r| r.0.iter().flat_map(|c| [c.1, c.2])).collect();
        edges.sort_unstable();
        edges.dedup_by(|a, b| a.abs_diff(*b) <= 20);
        edges.truncate(64);
        let col_of = |x: u32| edges.iter().position(|e| e.abs_diff(x) <= 20).unwrap_or(edges.len().saturating_sub(1));
        let total = edges.last().copied().unwrap_or(1440);
        let look = t.and_then(|t| doc.tables.get(&t)).cloned().unwrap_or_default();
        let align = match look.align {
            officina_text::model::HAlign::Center => "center",
            officina_text::model::HAlign::End => "right",
            _ if look.indent != 0 => "margins",
            _ => "left",
        };
        let margin = if look.indent != 0 { format!(r#" fo:margin-left="{}""#, pt(look.indent as i64)) } else { String::new() };
        let _ = write!(self.auto, r#"<style:style style:name="{name}" style:family="table"><style:table-properties style:width="{}" table:align="{align}"{margin}/></style:style>"#, pt(total as i64));
        for (i, w) in edges.windows(2).enumerate() {
            let _ = write!(self.auto, r#"<style:style style:name="{name}.C{i}" style:family="table-column"><style:table-column-properties style:column-width="{}"/></style:style>"#, pt((w[1] - w[0]) as i64));
        }
        let _ = write!(out, r#"<table:table table:name="{name}" table:style-name="{name}">"#);
        for i in 0..edges.len().saturating_sub(1) {
            let _ = write!(out, r#"<table:table-column table:style-name="{name}.C{i}"/>"#);
        }
        // One cell style per look, one row style per row look.
        let mut cell_styles: HashMap<(officina_text::model::CellPaint, Option<officina_text::model::CellPad>), String> = HashMap::new();
        let mut row_styles: HashMap<officina_text::model::RowLook, String> = HashMap::new();
        let mut in_header = false;
        let n_rows = rows.len();
        for ri in 0..n_rows {
            let (cells, rl) = &rows[ri];
            if rl.header && ri == 0 {
                out.push_str("<table:table-header-rows>");
                in_header = true;
            } else if in_header && !rl.header {
                out.push_str("</table:table-header-rows>");
                in_header = false;
            }
            let plain = officina_text::model::RowLook { header: false, ..*rl };
            let row_attr = if plain != officina_text::model::RowLook::default() {
                let n = row_styles.len();
                let rname = row_styles.entry(plain).or_insert_with(|| {
                    let rname = format!("{name}.R{n}");
                    let mut props = String::new();
                    if plain.height > 0 {
                        let _ = write!(props, r#" style:{}="{}""#, if plain.exact { "row-height" } else { "min-row-height" }, pt(plain.height as i64));
                    }
                    if plain.cant_split {
                        props += r#" fo:keep-together="always""#;
                    }
                    let _ = write!(self.auto, r#"<style:style style:name="{rname}" style:family="table-row"><style:table-row-properties{props}/></style:style>"#);
                    rname
                });
                format!(r#" table:style-name="{rname}""#)
            } else {
                String::new()
            };
            let _ = write!(out, "<table:table-row{row_attr}>");
            for (cell, x0, x1, paint) in cells {
                let c = doc.cell_at(cell.start, inner).unwrap_or_default();
                let span = col_of(*x1).saturating_sub(col_of(*x0)).max(1);
                let pad = c.look.pad.or(look.pad);
                let key = (*paint, pad);
                let n = cell_styles.len();
                let auto = &mut self.auto;
                let cname = cell_styles
                    .entry(key)
                    .or_insert_with(|| {
                        let cname = format!("{name}.A{n}");
                        let side = |b: Option<officina_text::model::Border>| b.map_or("none".to_string(), |x| odf_border(&x));
                        let pd = pad.unwrap_or_default();
                        let bg = paint.shading.map_or("transparent".to_string(), |c| format!("#{c:06x}"));
                        let va = match paint.valign {
                            officina_text::model::VAlign::Top => "top",
                            officina_text::model::VAlign::Center => "middle",
                            officina_text::model::VAlign::Bottom => "bottom",
                        };
                        let _ = write!(auto, r#"<style:style style:name="{cname}" style:family="table-cell"><style:table-cell-properties fo:padding-top="{}" fo:padding-bottom="{}" fo:padding-left="{}" fo:padding-right="{}" fo:border-top="{}" fo:border-bottom="{}" fo:border-left="{}" fo:border-right="{}" fo:background-color="{bg}" style:vertical-align="{va}"/></style:style>"#, pt(pd.top as i64), pt(pd.bottom as i64), pt(pd.start as i64), pt(pd.end as i64), side(paint.top), side(paint.bottom), side(paint.start), side(paint.end));
                        cname
                    })
                    .clone();
                // Merged down: the first of a run says how many rows it covers.
                let mut down = 1;
                while rows.get(ri + down).and_then(|r| r.0.iter().find(|d| d.1.abs_diff(*x0) <= 20)).and_then(|d| doc.cell_at(d.0.start, inner)).is_some_and(|d| d.merged_up) {
                    down += 1;
                }
                let tag = if c.merged_up { "table:covered-table-cell" } else { "table:table-cell" };
                let mut attrs = format!(r#" table:style-name="{cname}""#);
                if !c.merged_up {
                    attrs += r#" office:value-type="string""#;
                    if span > 1 {
                        let _ = write!(attrs, r#" table:number-columns-spanned="{span}""#);
                    }
                    if down > 1 {
                        let _ = write!(attrs, r#" table:number-rows-spanned="{down}""#);
                    }
                }
                let _ = write!(out, "<{tag}{attrs}>");
                let mut x = cell.start;
                while x < cell.end {
                    if !inner && doc.inner_cell(x).is_some() {
                        x = self.table_at(x, cell.end, true, out).max(x + 1);
                    } else if list_of(doc, x).is_some() {
                        // A list stops where a table in the cell starts.
                        let stop = if inner { cell.end } else { (x..cell.end).find(|&j| doc.inner_cell(j).is_some()).unwrap_or(cell.end) };
                        x = self.list(x, stop, out).max(x + 1);
                    } else {
                        self.para(x, out);
                        x += 1;
                    }
                }
                let _ = write!(out, "</{tag}>");
                // The grid columns it spans past the first are covered.
                for _ in 1..span {
                    out.push_str("<table:covered-table-cell/>");
                }
            }
            out.push_str("</table:table-row>");
        }
        if in_header {
            out.push_str("</table:table-header-rows>");
        }
        out.push_str("</table:table>");
        end
    }
}

fn cells_of(doc: &TextDoc, row: std::ops::Range<usize>, inner: bool) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for k in row {
        match out.last_mut() {
            Some(r) if doc.same_cell_at(r.start, k, inner) => r.end = k + 1,
            _ => out.push(k..k + 1),
        }
    }
    out
}

fn margin(hf: &HeaderFooter, tag: &str, style: &str) -> String {
    let mut body = String::new();
    let mut rest = hf.text.as_str();
    while !rest.is_empty() {
        let next = [("{pages}", "<text:page-count>1</text:page-count>"), ("{page}", r#"<text:page-number text:select-page="current">1</text:page-number>"#)].iter().filter_map(|(k, v)| rest.find(k).map(|i| (i, *k, *v))).min_by_key(|x| (x.0, std::cmp::Reverse(x.1.len())));
        match next {
            Some((i, k, v)) => {
                Out::text(&rest[..i], &mut body);
                body.push_str(v);
                rest = &rest[i + k.len()..];
            }
            None => {
                Out::text(rest, &mut body);
                break;
            }
        }
    }
    format!(r#"<style:{tag}><text:p text:style-name="{style}">{body}</text:p></style:{tag}>"#)
}

/// The document as an `.odt`.
pub fn write<W: io::Write>(doc: &TextDoc, out: W) -> io::Result<W> {
    // Unsettled merge conflicts: ODF has none, this copy's version goes.
    if !doc.conflicts.is_empty() {
        return write(&doc.without_conflicts(), out);
    }
    let named: Vec<(String, String)> = doc.styles.styles.iter().enumerate().map(|(i, s)| style_name(&s.name, i)).collect();
    let mut o = Out { doc, prefix: "", names: named.iter().map(|n| n.0.clone()).collect(), fonts: vec![doc.styles.default_char.family.to_string()], auto: String::new(), para_styles: HashMap::new(), text_styles: HashMap::new(), list_styles: HashMap::new(), lists_seen: Vec::new(), media: Vec::new(), image_names: HashMap::new(), tables: 0, frames: 0, in_note: 0, marks: HashMap::new(), cur: None, regions: String::new(), nchange: 0, ins_open: None, pending_end: None, targets: std::collections::HashSet::new(), graphic_styles: HashMap::new(), shape_styles: 0, common: String::new(), common_names: std::collections::HashSet::new(), pages: Vec::new(), page_changes: std::collections::HashSet::new(), sides: std::collections::HashSet::new() };
    let mut last_page = None;
    for k in 0..doc.blocks.len() {
        if let Some(x) = doc.para(k).filter(|p| p.props.cell.is_none()).and_then(|p| p.props.section) {
            if k > 0 && x.page != last_page {
                o.page_changes.insert(k);
            }
            last_page = x.page;
        }
    }
    for bl in doc.blocks.iter() {
        if let Some(p) = bl.as_para() {
            o.targets.extend(doc.fields_in(p).into_iter().map(|f| f.1.target()));
        }
    }
    // Comments: each anchored one's start and end; its replies right after its start.
    let ranges = doc.comment_ranges();
    let live: std::collections::HashSet<CommentId> = doc.live_comments().into_iter().collect();
    for &(id, (sb, so), (eb, eo)) in &ranges {
        o.marks.entry(sb).or_default().push((so, false, id));
        // The thread, in order, after the comment.
        let mut stack = doc.comments.replies(id);
        stack.reverse();
        let mut seen = 0;
        while let Some(r) = stack.pop() {
            seen += 1;
            if seen > 4096 || !live.contains(&r) {
                continue;
            }
            o.marks.entry(sb).or_default().push((so, false, r));
            let mut more = doc.comments.replies(r);
            more.reverse();
            stack.extend(more);
        }
        o.marks.entry(eb).or_default().push((eo, true, id));
    }
    for v in o.marks.values_mut() {
        v.sort_by_key(|&(at, end, _)| (at, !end));
    }
    let mut body = String::new();
    let n = doc.blocks.len();
    // Sections with columns go out as `text:section`s; a section ends where
    // the next starts (lists and contents break there too).
    let mut secs: Vec<(usize, Section)> = vec![(0, Section::default())];
    for k in 0..n {
        if let Some(x) = doc.para(k).filter(|p| p.props.cell.is_none()).and_then(|p| p.props.section) {
            if k == 0 {
                secs[0].1 = x;
            } else {
                secs.push((k, x));
            }
        }
    }
    for (i, &(from, x)) in secs.iter().enumerate() {
        let to = secs.get(i + 1).map_or(n, |s| s.0);
        let next_continuous = secs.get(i + 1).is_some_and(|s| s.1.start == SectionStart::Continuous);
        let wrap = x.columns > 1 || x.rule;
        if wrap {
            let sep = if x.rule { r##"<style:column-sep style:width="0.0071in" style:color="#000000" style:height="100%" style:vertical-align="top"/>"## } else { "" };
            // Columns balance only above a continuous break, as on screen.
            let _ = write!(o.auto, r#"<style:style style:name="Sect{i}" style:family="section"><style:section-properties text:dont-balance-text-columns="{}"><style:columns fo:column-count="{}" fo:column-gap="{}">{sep}</style:columns></style:section-properties></style:style>"#, !next_continuous, x.columns, pt(x.gap as i64));
            let _ = write!(body, r#"<text:section text:style-name="Sect{i}" text:name="Section{}">"#, i + 1);
        }
        let mut b = from;
        while b < to {
            if doc.cell(b).is_some() {
                b = o.table(b, &mut body);
            } else if list_of(doc, b).is_some() {
                b = o.list(b, to, &mut body);
            } else if doc.para(b).is_some_and(|p| p.props.toc.is_some()) {
                // A table of contents: its run of entries as the index body.
                let mut end = b;
                let mut depth = 3;
                while let Some(l) = doc.para(end).filter(|p| end < to && p.props.cell.is_none() && list_of(doc, end).is_none()).and_then(|p| p.props.toc) {
                    depth = depth.max(l.min(9));
                    end += 1;
                }
                let _ = write!(body, r#"<text:table-of-content text:protected="true" text:name="Table of Contents{}"><text:table-of-content-source text:outline-level="{depth}" text:use-index-marks="false"/><text:index-body>"#, b + 1);
                for k in b..end.max(b + 1) {
                    o.para(k, &mut body);
                }
                body += "</text:index-body></text:table-of-content>";
                b = end.max(b + 1);
            } else {
                o.para(b, &mut body);
                b += 1;
            }
        }
        if wrap {
            body += "</text:section>";
        }
    }
    // Named styles.
    let mut styles = String::new();
    for (i, st) in doc.styles.styles.iter().enumerate() {
        let (name, display) = &named[i];
        let family = if st.kind == StyleKind::Para { "paragraph" } else { "text" };
        let parent = st.parent.and_then(|p| named.get(p as usize)).map(|p| format!(r#" style:parent-style-name="{}""#, esc(&p.0))).unwrap_or_default();
        let next = st.next.and_then(|p| named.get(p as usize)).filter(|_| st.kind == StyleKind::Para).map(|p| format!(r#" style:next-style-name="{}""#, esc(&p.0))).unwrap_or_default();
        let outline = st.para.outline_level.filter(|_| st.kind == StyleKind::Para).map(|l| format!(r#" style:default-outline-level="{l}""#)).unwrap_or_default();
        let pp = if st.kind == StyleKind::Para { para_props(&st.para, 0) } else { String::new() };
        // A style's list: a list style of its own among the named ones.
        let list = match st.para.list.filter(|_| st.kind == StyleKind::Para) {
            Some(Some(l)) => {
                let name = format!("WWNum{}", l.id);
                if !styles.contains(&format!(r#"<text:list-style style:name="{name}">"#)) {
                    let def = doc.styles.lists.get(&l.id).cloned().unwrap_or_else(|| ListDef::builtin(l.kind));
                    let _ = write!(styles, r#"<text:list-style style:name="{name}">"#);
                    for k in 0..10u8 {
                        let lv = def.level(k.min(8)).cloned().unwrap_or_else(|| ListLevel::builtin(l.kind, k.min(8)));
                        styles += &list_level_xml(k + 1, &lv);
                    }
                    styles += "</text:list-style>";
                }
                format!(r#" style:list-style-name="{name}""#)
            }
            // Out of the list its parent style puts it in.
            Some(None) => " style:list-style-name=\"\"".to_string(),
            None => String::new(),
        };
        let mut chars = st.chars.clone();
        chars.style = None;
        let mut tp = text_props(&chars, &mut o.fonts);
        if let (Some(v), true, StyleKind::Para) = (st.para.hyphenate, doc.settings.hyphenate, st.kind) {
            tp = if tp.is_empty() { format!(r#"<style:text-properties fo:hyphenate="{v}"/>"#) } else { tp.replacen("<style:text-properties", &format!(r#"<style:text-properties fo:hyphenate="{v}""#), 1) };
        }
        let _ = write!(styles, r#"<style:style style:name="{}" style:display-name="{}" style:family="{family}"{parent}{next}{outline}{list}>{pp}{tp}</style:style>"#, esc(name), esc(display));
    }
    // LibreOffice's Contents styles for the entries' levels.
    let mut levels: Vec<u8> = doc.blocks.iter().filter_map(|b| b.as_para()).filter(|p| p.style == 0).filter_map(|p| p.props.toc).map(|l| l.clamp(1, 9)).collect();
    levels.sort_unstable();
    levels.dedup();
    for l in levels {
        if !named.iter().any(|n| n.0 == format!("Contents_20_{l}")) {
            let _ = write!(styles, r#"<style:style style:name="Contents_20_{l}" style:display-name="Contents {l}" style:family="paragraph" style:parent-style-name="{}" style:class="index"/>"#, esc(&named[0].0));
        }
    }
    let dc = &doc.styles.default_char;
    let hyph = if doc.settings.hyphenate { r#" fo:hyphenate="true""# } else { "" };
    let mut defaults = format!(r#"<style:default-style style:family="paragraph"><style:paragraph-properties fo:margin-bottom="8pt" fo:line-height="115%"/><style:text-properties style:font-name="{}" fo:font-size="{}pt"{hyph}/></style:default-style>"#, esc(&dc.family), dc.size as f64 / 2.0);
    // Line numbers (ODF has no first number: they count from 1).
    if let Some(n) = doc.settings.line_numbers {
        let offset = if n.distance > 0 { format!(r#" text:offset="{}""#, pt(n.distance as i64)) } else { String::new() };
        let _ = write!(defaults, r#"<text:linenumbering-configuration text:number-lines="true" text:increment="{}"{offset} style:num-format="1" text:number-position="left" text:restart-on-page="{}"/>"#, n.count_by, n.restart == LineRestart::Page);
    }

    // The page: ODF's header and footer sit inside the margins, so the page
    // margin is their distance from the edge and their height is the rest.
    let page_layout = |name: &str, g: &officina_text::model::PageSetup| {
        let mut hf_styles = String::new();
        let mut margins = (g.margin_top, g.margin_bottom);
        let any = |top: bool| {
            let pick = |m: &officina_text::model::Margins| if top { m.header.is_some() } else { m.footer.is_some() };
            (if top { doc.header.is_some() } else { doc.footer.is_some() }) || doc.first_page.as_ref().is_some_and(pick) || doc.even_pages.as_ref().is_some_and(pick)
        };
        for (tag, top) in [("header-style", true), ("footer-style", false)] {
            if any(top) {
                let m = if top { g.margin_top } else { g.margin_bottom };
                let edge = HEADER_FROM_EDGE.min(m);
                let rest = (m - edge).max(0);
                let (gap_k, gap) = if top { ("fo:margin-bottom", 0) } else { ("fo:margin-top", 0) };
                let _ = write!(hf_styles, r#"<style:{tag}><style:header-footer-properties fo:min-height="{}" {gap_k}="{}" style:dynamic-spacing="false"/></style:{tag}>"#, pt(rest as i64), pt(gap));
                if top {
                    margins.0 = edge;
                } else {
                    margins.1 = edge;
                }
            } else {
                let _ = write!(hf_styles, "<style:{tag}/>");
            }
        }
        format!(r#"<style:page-layout style:name="{name}"><style:page-layout-properties fo:page-width="{}" fo:page-height="{}" fo:margin-top="{}" fo:margin-bottom="{}" fo:margin-left="{}" fo:margin-right="{}" style:print-orientation="{}"/>{hf_styles}</style:page-layout>"#, pt(g.width as i64), pt(g.height as i64), pt(margins.0 as i64), pt(margins.1 as i64), pt(g.margin_start as i64), pt(g.margin_end as i64), if g.width > g.height { "landscape" } else { "portrait" })
    };
    let mut layout = page_layout("pm1", &doc.page);
    let mut mp_styles = String::new();
    let mut master = String::from(r#"<style:master-page style:name="Standard" style:page-layout-name="pm1">"#);
    let mut mp = 0;
    for (i, tag) in ["header", "footer"].into_iter().enumerate() {
        fn pick(m: &officina_text::model::Margins, i: usize) -> Option<&HeaderFooter> {
            if i == 0 { m.header.as_ref() } else { m.footer.as_ref() }
        }
        let main = if i == 0 { doc.header.as_ref() } else { doc.footer.as_ref() };
        // ODF's order: the region, the left (even) pages', the first page's;
        // a kind given with none shows nothing there.
        let kinds = [("", Some(main)), ("-left", doc.even_pages.as_ref().map(|m| pick(m, i))), ("-first", doc.first_page.as_ref().map(|m| pick(m, i)))];
        if kinds.iter().all(|k| k.1.flatten().is_none()) {
            continue;
        }
        for (k, (suffix, hf)) in kinds.into_iter().enumerate() {
            let el = format!("{tag}{suffix}");
            let hf = match hf {
                None => continue,
                // Blank, as LibreOffice writes it (one shown off would turn
                // the whole region off for it).
                Some(None) => {
                    let _ = write!(master, "<style:{el}><text:p/></style:{el}>");
                    continue;
                }
                Some(Some(h)) => h,
            };
            mp += 1;
            let sn = format!("MP{mp}");
            let align = match hf.align {
                Align::Start => "start",
                Align::Center => "center",
                Align::End => "end",
                Align::Justify => "justify",
            };
            let _ = write!(mp_styles, r#"<style:style style:name="{sn}" style:family="paragraph" style:parent-style-name="Standard"><style:paragraph-properties fo:text-align="{align}" fo:margin-top="0pt" fo:margin-bottom="0pt"/></style:style>"#);
            if hf.paras.is_empty() {
                master += &margin(hf, &el, &sn);
                continue;
            }
            // Its own paragraphs: their automatic styles and pictures go
            // with the master page.
            let prefix = ["MH", "MHL", "MHF", "MF", "MFL", "MFF"][i * 3 + k];
            let mut ho = Out { prefix, auto: String::new(), para_styles: HashMap::new(), text_styles: HashMap::new(), list_styles: HashMap::new(), lists_seen: Vec::new(), media: Vec::new(), image_names: HashMap::new(), marks: HashMap::new(), cur: None, regions: String::new(), nchange: 0, ins_open: None, pending_end: None, targets: std::collections::HashSet::new(), graphic_styles: HashMap::new(), shape_styles: 0, common: String::new(), common_names: std::mem::take(&mut o.common_names), pages: Vec::new(), page_changes: std::collections::HashSet::new(), sides: std::collections::HashSet::new(), in_note: 1, names: o.names.clone(), fonts: o.fonts.clone(), tables: 0, frames: 0, doc };
            let mut x = String::new();
            for p in &hf.paras {
                let mut q = p.clone();
                q.props.list = None;
                q.props.cell = None;
                q.props.section = None;
                ho.para_of(&q, &mut x);
            }
            let x = x.replace("{pages}", "<text:page-count>1</text:page-count>").replace("{page}", r#"<text:page-number text:select-page="current">1</text:page-number>"#);
            let _ = write!(master, "<style:{el}>{x}</style:{el}>");
            mp_styles += &ho.auto;
            for f in ho.fonts {
                if !o.fonts.contains(&f) {
                    o.fonts.push(f);
                }
            }
            o.media.extend(ho.media);
            o.common += &ho.common;
            o.common_names = ho.common_names;
        }
    }
    master += "</style:master-page>";
    // The same page again for sections on a new page: LibreOffice keeps a
    // change of master page as a section break, not a return to the same one.
    if o.para_styles.keys().any(|k| k.1.section.is_some()) {
        let again = master.replacen(r#"style:name="Standard""#, r#"style:name="Section_20_Page" style:display-name="Section Page" style:next-style-name="Section_20_Page""#, 1);
        master += &again;
    }
    // Sections with a page of their own: a page layout and master page each.
    let standard = master.split("</style:master-page>").next().unwrap_or("").to_string() + "</style:master-page>";
    for (k, g) in o.pages.iter().enumerate() {
        let n = k + 1;
        layout += &page_layout(&format!("pmS{n}"), g);
        master += &standard.replacen(r#"style:name="Standard" style:page-layout-name="pm1""#, &format!(r#"style:name="Section_20_Page_{n}" style:display-name="Section Page {n}" style:next-style-name="Section_20_Page_{n}" style:page-layout-name="pmS{n}""#), 1);
    }
    // Sections on odd or even pages: their master page's layout kept to right
    // or left pages, the pages after it on the plain one.
    let mut sides: Vec<(Option<usize>, u8)> = o.sides.iter().copied().filter(|x| x.1 != 0).collect();
    sides.sort();
    for (k, side) in sides {
        let (usage, suffix, word) = if side == 1 { ("right", "_Right", "Right") } else { ("left", "_Left", "Left") };
        let (plain, display, g, lname) = match k {
            Some(k) => (format!("Section_20_Page_{}", k + 1), format!("Section Page {} {word}", k + 1), o.pages[k], format!("pmS{}{}", k + 1, &word[..1])),
            None => ("Section_20_Page".to_string(), format!("Section Page {word}"), doc.page, format!("pm1{}", &word[..1])),
        };
        layout += &page_layout(&lname, &g).replacen(&format!(r#"style:name="{lname}""#), &format!(r#"style:name="{lname}" style:page-usage="{usage}""#), 1);
        master += &standard.replacen(r#"style:name="Standard" style:page-layout-name="pm1""#, &format!(r#"style:name="{plain}{suffix}" style:display-name="{display}" style:next-style-name="{plain}" style:page-layout-name="{lname}""#), 1);
    }
    let sfonts: String = o.fonts.iter().map(|f| format!(r#"<style:font-face style:name="{0}" svg:font-family="'{0}'"/>"#, esc(f))).collect();
    let styles_xml = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS}><office:font-face-decls>{sfonts}</office:font-face-decls><office:styles>{defaults}{styles}{}</office:styles><office:automatic-styles>{layout}{mp_styles}</office:automatic-styles><office:master-styles>{master}</office:master-styles></office:document-styles>"#, o.common);
    let fonts: String = o.fonts.iter().map(|f| format!(r#"<style:font-face style:name="{0}" svg:font-family="'{0}'"/>"#, esc(f))).collect();
    let tracked = if o.regions.is_empty() && !doc.tracking {
        String::new()
    } else {
        format!(r#"<text:tracked-changes text:track-changes="{}">{}</text:tracked-changes>"#, doc.tracking, o.regions)
    };
    let content = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:font-face-decls>{fonts}</office:font-face-decls><office:automatic-styles>{}</office:automatic-styles><office:body><office:text>{tracked}{body}</office:text></office:body></office:document-content>"#, o.auto);

    let mut z = ZipWriter::new(out);
    z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text")?;
    let mut manifest = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>"#);
    for (name, _) in &o.media {
        let mt = if name.ends_with(".emf") { "image/x-emf" } else if name.ends_with(".wmf") { "image/x-wmf" } else if name.ends_with(".bmp") { "image/bmp" } else if name.ends_with(".jpg") { "image/jpeg" } else if name.ends_with(".gif") { "image/gif" } else if name.ends_with(".webp") { "image/webp" } else if name.ends_with(".svg") { "image/svg+xml" } else { "image/png" };
        let _ = write!(manifest, r#"<manifest:file-entry manifest:full-path="{name}" manifest:media-type="{mt}"/>"#);
    }
    manifest += "</manifest:manifest>";
    z.add("content.xml", content.as_bytes())?;
    z.add("styles.xml", styles_xml.as_bytes())?;
    z.add("META-INF/manifest.xml", manifest.as_bytes())?;
    for (name, bytes) in &o.media {
        z.add_stored(name, bytes)?;
    }
    z.finish()
}

/// `p` cut after each page break (U+000C, dropped): the pieces after the
/// first start a page and keep the paragraph's look but not its section.
fn split_page_breaks(p: &Para) -> Vec<Para> {
    let mut out = Vec::new();
    let mut from = 0usize;
    let cuts: Vec<usize> = p.text.match_indices('\u{000C}').map(|(i, _)| i).chain(std::iter::once(p.text.len())).collect();
    for (i, &to) in cuts.iter().enumerate() {
        let runs: Vec<(u32, AttrId)> = {
            let mut v: Vec<(u32, AttrId)> = vec![(0, p.attr_at(from as u32))];
            v.extend(p.runs.iter().filter(|r| (r.0 as usize) > from && (r.0 as usize) < to).map(|r| (r.0 - from as u32, r.1)));
            v
        };
        let mut props = p.props.clone();
        if i > 0 {
            props.page_break_before = Some(true);
            props.section = None;
        }
        out.push(Para { id: p.id, text: p.text[from..to].to_string(), runs, style: p.style, props });
        from = to + 1;
    }
    out
}
