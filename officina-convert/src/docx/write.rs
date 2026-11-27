//! `.docx` out, in the document process, over our own model.
//!
//! Writes what the model holds: styles with inheritance (the built-in ones under
//! Word's names, so Word shows them as its own), paragraphs and their direct
//! properties, runs, links, tabs and line breaks, lists as numbering definitions
//! (one per list), tables with their column grid, inline pictures, page size and
//! margins, and a header and footer with page number fields.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io;

use officina_text::model::{CellPad, RegionLook, TableBorders, TableStyle, VAlign, HAlign as TableAlign, LOOK_HEADER, LOOK_TOTAL, LOOK_FIRST_COLUMN, LOOK_LAST_COLUMN, LOOK_BANDED_ROWS, LOOK_BANDED_COLUMNS, Border, BorderStyle, ListDef, ListLevel, NumFormat, LineRestart, Caps, EFFECT_OUTLINE, EFFECT_SHADOW, EFFECT_EMBOSS, EFFECT_ENGRAVE, ArrowEnd, Dash, Geom, PathCmd, ShapeData, TextAnchor, Float, HAlign, Rel, Wrap, Section, SectionStart, Field, Leader, TabAlign, RevId, Align, AttrId, CharAttrs, CommentId, HeaderFooter, LineSpacing, ListKind, NoteId, NoteKind, Para, ParaProps, StyleKind, TextDoc, VertAlign, HEADER_FROM_EDGE, OBJECT_CHAR};

use crate::xml::esc;
use crate::zip::ZipWriter;

const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
const DRAW_NS: &str = r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:wpg="http://schemas.microsoft.com/office/word/2010/wordprocessingGroup""#;
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

/// A style's id in the file: Word's for the built-in ones.
fn style_id(name: &str, i: usize) -> String {
    match name {
        "Normal" | "Title" => name.to_string(),
        n if n.starts_with("Heading ") && n.len() == 9 => n.replace(' ', ""),
        n => {
            let s: String = n.chars().filter(|c| c.is_ascii_alphanumeric()).take(32).collect();
            format!("S{i}{s}")
        }
    }
}

/// The name Word knows a built-in style by.
fn style_name(name: &str) -> String {
    match name {
        n if n.starts_with("Heading ") && n.len() == 9 => n.to_ascii_lowercase(),
        n => n.to_string(),
    }
}

fn on(tag: &str, v: bool) -> String {
    if v {
        format!("<w:{tag}/>")
    } else {
        format!(r#"<w:{tag} w:val="0"/>"#)
    }
}

fn rpr(a: &CharAttrs, styles: &[String]) -> String {
    // In the order the schema gives (Word refuses some others).
    let mut s = String::new();
    if let Some(st) = a.style.and_then(|k| styles.get(k as usize)) {
        let _ = write!(s, r#"<w:rStyle w:val="{}"/>"#, esc(st));
    }
    if let Some(f) = &a.family {
        let f = esc(f);
        let _ = write!(s, r#"<w:rFonts w:ascii="{f}" w:hAnsi="{f}" w:cs="{f}"/>"#);
    }
    if let Some(b) = a.bold {
        s += &on("b", b);
    }
    if let Some(i) = a.italic {
        s += &on("i", i);
    }
    match a.caps {
        Some(Caps::All) => s += &on("caps", true),
        Some(Caps::Small) => s += &on("smallCaps", true),
        Some(Caps::Normal) => s += &(on("caps", false) + &on("smallCaps", false)),
        None => {}
    }
    if let Some(v) = a.strike {
        s += &on("strike", v);
    }
    if let Some(v) = a.dstrike {
        s += &on("dstrike", v);
    }
    if let Some(fx) = a.effects {
        for (bit, tag) in [(EFFECT_OUTLINE, "outline"), (EFFECT_SHADOW, "shadow"), (EFFECT_EMBOSS, "emboss"), (EFFECT_ENGRAVE, "imprint")] {
            s += &on(tag, fx & bit != 0);
        }
    }
    if let Some(v) = a.hidden {
        s += &on("vanish", v);
    }
    if let Some(c) = a.color {
        let _ = write!(s, r#"<w:color w:val="{:06X}"/>"#, c & 0xFF_FFFF);
    }
    if let Some(v) = a.spacing {
        let _ = write!(s, r#"<w:spacing w:val="{v}"/>"#);
    }
    if let Some(v) = a.scale {
        let _ = write!(s, r#"<w:w w:val="{v}"/>"#);
    }
    if let Some(z) = a.size {
        let _ = write!(s, r#"<w:sz w:val="{z}"/><w:szCs w:val="{z}"/>"#);
    }
    match (a.underline, a.link.is_some()) {
        (Some(true), _) => s += r#"<w:u w:val="single"/>"#,
        (Some(false), _) => s += r#"<w:u w:val="none"/>"#,
        _ => {}
    }
    if let Some(h) = a.highlight {
        let _ = write!(s, r#"<w:shd w:val="clear" w:color="auto" w:fill="{:06X}"/>"#, h & 0xFF_FFFF);
    }
    if let Some(v) = a.vert {
        let _ = write!(s, r#"<w:vertAlign w:val="{}"/>"#, match v {
            VertAlign::Super => "superscript",
            VertAlign::Sub => "subscript",
            VertAlign::Baseline => "baseline",
        });
    }
    if let Some(l) = &a.lang_ea {
        let _ = write!(s, r#"<w:lang w:eastAsia="{}"/>"#, esc(l));
    }
    if s.is_empty() {
        s
    } else {
        format!("<w:rPr>{s}</w:rPr>")
    }
}

/// A `w:pBdr` side.
fn border_xml(name: &str, b: &Border) -> String {
    let val = match b.style {
        BorderStyle::Single => "single",
        BorderStyle::Double => "double",
        BorderStyle::Dotted => "dotted",
        BorderStyle::Dashed => "dashed",
        BorderStyle::Thick => "thick",
    };
    let color = b.color.map_or("auto".to_string(), |c| format!("{c:06X}"));
    format!(r#"<w:{name} w:val="{val}" w:sz="{}" w:space="{}" w:color="{color}"/>"#, b.width, b.space)
}

/// Paragraph properties, after `w:pStyle` and `w:numPr`.
fn ppr_body(p: &ParaProps) -> String {
    let mut s = String::new();
    if let Some(v) = p.keep_with_next {
        s += &on("keepNext", v);
    }
    if let Some(v) = p.keep_together {
        s += &on("keepLines", v);
    }
    if let Some(v) = p.page_break_before {
        s += &on("pageBreakBefore", v);
    }
    if let Some(v) = p.widows {
        s += &on("widowControl", v);
    }
    if let Some(v) = p.line_numbers {
        s += &on("suppressLineNumbers", !v);
    }
    if let Some(b) = &p.borders {
        s += "<w:pBdr>";
        for (name, x) in [("top", b.top), ("left", b.start), ("bottom", b.bottom), ("right", b.end), ("between", b.between)] {
            match x {
                Some(x) => s += &border_xml(name, &x),
                None if b.is_empty() => {
                    let _ = write!(s, r#"<w:{name} w:val="nil"/>"#);
                }
                None => {}
            }
        }
        s += "</w:pBdr>";
    }
    if let Some(v) = p.shading {
        match v {
            Some(c) => {
                let _ = write!(s, r#"<w:shd w:val="clear" w:color="auto" w:fill="{c:06X}"/>"#);
            }
            None => s += r#"<w:shd w:val="clear" w:color="auto" w:fill="auto"/>"#,
        }
    }
    if let Some(tabs) = &p.tabs {
        s += "<w:tabs>";
        for t in tabs {
            let val = match t.align {
                TabAlign::Start => "left",
                TabAlign::Center => "center",
                TabAlign::End => "right",
                TabAlign::Decimal => "decimal",
            };
            let leader = match t.leader {
                Leader::None => "",
                Leader::Dots => r#" w:leader="dot""#,
                Leader::Dashes => r#" w:leader="hyphen""#,
                Leader::Line => r#" w:leader="underscore""#,
            };
            let _ = write!(s, r#"<w:tab w:val="{val}"{leader} w:pos="{}"/>"#, t.pos);
        }
        s += "</w:tabs>";
    }
    if let Some(v) = p.hyphenate {
        s += &on("suppressAutoHyphens", !v);
    }
    // w:bidi comes before w:spacing in a w:pPr.
    if let Some(v) = p.rtl {
        s += &on("bidi", v);
    }
    if p.space_before.is_some() || p.space_after.is_some() || p.line_spacing.is_some() {
        s += "<w:spacing";
        if let Some(v) = p.space_before {
            let _ = write!(s, r#" w:before="{v}""#);
        }
        if let Some(v) = p.space_after {
            let _ = write!(s, r#" w:after="{v}""#);
        }
        match p.line_spacing {
            Some(LineSpacing::Multiple(m)) => {
                let _ = write!(s, r#" w:line="{}" w:lineRule="auto""#, m as u32 * 240 / 100);
            }
            Some(LineSpacing::Exact(t)) => {
                let _ = write!(s, r#" w:line="{t}" w:lineRule="exact""#);
            }
            Some(LineSpacing::AtLeast(t)) => {
                let _ = write!(s, r#" w:line="{t}" w:lineRule="atLeast""#);
            }
            None => {}
        }
        s += "/>";
    }
    if p.indent_start.is_some() || p.indent_end.is_some() || p.first_line.is_some() {
        s += "<w:ind";
        if let Some(v) = p.indent_start {
            let _ = write!(s, r#" w:left="{v}""#);
        }
        if let Some(v) = p.indent_end {
            let _ = write!(s, r#" w:right="{v}""#);
        }
        match p.first_line {
            Some(v) if v < 0 => {
                let _ = write!(s, r#" w:hanging="{}""#, -v);
            }
            Some(v) => {
                let _ = write!(s, r#" w:firstLine="{v}""#);
            }
            None => {}
        }
        s += "/>";
    }
    if let Some(a) = p.align {
        let _ = write!(s, r#"<w:jc w:val="{}"/>"#, match a {
            Align::Start => "left",
            Align::Center => "center",
            Align::End => "right",
            Align::Justify => "both",
        });
    }
    if let Some(l) = p.outline_level {
        let _ = write!(s, r#"<w:outlineLvl w:val="{}"/>"#, l.saturating_sub(1).min(8));
    }
    s
}

/// A floating picture's `wp:anchor` start, its position and wrap (the
/// extent and the wrap element follow; see [`anchor_wrap`]).
fn anchor_open(f: &Float, n: u32) -> String {
    let gap = f.gap.max(0) as i64 * 635;
    let behind = (f.wrap == Wrap::Behind) as u8;
    let h_from = match f.h_rel {
        Rel::Text => "column",
        Rel::Margin => "margin",
        Rel::Page => "page",
    };
    let h = match f.h_align {
        HAlign::Start => "<wp:align>left</wp:align>".to_string(),
        HAlign::Center => "<wp:align>center</wp:align>".to_string(),
        HAlign::End => "<wp:align>right</wp:align>".to_string(),
        HAlign::Offset => format!("<wp:posOffset>{}</wp:posOffset>", f.x as i64 * 635),
    };
    let v_from = match f.v_rel {
        Rel::Text => "paragraph",
        Rel::Margin => "margin",
        Rel::Page => "page",
    };
    format!(r#"<wp:anchor distT="{gap}" distB="{gap}" distL="{gap}" distR="{gap}" simplePos="0" relativeHeight="{}" behindDoc="{behind}" locked="0" layoutInCell="1" allowOverlap="1"><wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="{h_from}">{h}</wp:positionH><wp:positionV relativeFrom="{v_from}"><wp:posOffset>{}</wp:posOffset></wp:positionV>"#, 251658240 + n, f.y as i64 * 635)
}

/// The wrap element of a floating picture's `wp:anchor` (after its extent).
fn anchor_wrap(f: &Float) -> String {
    match f.wrap {
        Wrap::Square => r#"<wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapSquare wrapText="bothSides"/>"#,
        Wrap::TopBottom => r#"<wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapTopAndBottom/>"#,
        Wrap::Behind | Wrap::InFront => r#"<wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapNone/>"#,
    }
    .to_string()
}

struct Out {
    rels: Vec<(String, String, String, bool)>, // id, type, target, external
    media: Vec<(String, Vec<u8>)>,
    image_rel: HashMap<u32, String>,
    link_rel: HashMap<String, String>,
    /// Each list's abstract numbering and the `w:num` its items use now
    /// (a restart moves it to a new one).
    lists: HashMap<(u32, ListKind), (u32, u32)>,
    /// The `w:num`s that start a list again: num, list, level, number.
    restarts: Vec<(u32, (u32, ListKind), u8, u32)>,
    styles: Vec<String>,
    pic: u32,
    /// Our note id to (kind, the id the file gives it).
    notes: HashMap<NoteId, (NoteKind, u32)>,
    /// Where this part's pictures go ("image" for the document's).
    media_stem: &'static str,
    /// Comment range markers by block: (byte, end?, comment id), in order.
    marks: HashMap<usize, Vec<(u32, bool, CommentId)>>,
    /// The block being written (none in notes).
    cur: Option<usize>,
    /// Tracked changes: paragraphs whose break before was deleted, by id;
    /// the last change id given.
    del_breaks: HashMap<officina_text::model::BlockId, RevId>,
    change: u32,
    /// Paragraphs fields refer to, by id: each gets a bookmark.
    targets: std::collections::HashSet<officina_text::model::BlockId>,
    bookmark: u32,
    /// Paragraphs that end a section, by block: the section's number (its
    /// `w:sectPr`, filled in once the body is written).
    sect_end: HashMap<usize, usize>,
}

/// The bookmark name a field refers to paragraph `id` by.
pub fn ref_name(id: officina_text::model::BlockId) -> String {
    format!("_Ref{id}")
}

/// A complex field: `instr`, then `result` as it was last worked out.
fn field_runs(out: &mut String, props: &str, instr: &str, result: &str) {
    let _ = write!(out, r#"<w:r>{props}<w:fldChar w:fldCharType="begin"/></w:r><w:r>{props}<w:instrText xml:space="preserve"> {} </w:instrText></w:r><w:r>{props}<w:fldChar w:fldCharType="separate"/></w:r><w:r>{props}<w:t xml:space="preserve">{}</w:t></w:r><w:r>{props}<w:fldChar w:fldCharType="end"/></w:r>"#, esc(instr), esc(result));
}

/// A comment range's start or its end and reference.
fn comment_marker(out: &mut String, end: bool, id: CommentId) {
    if end {
        let _ = write!(out, r#"<w:commentRangeEnd w:id="{id}"/><w:r><w:commentReference w:id="{id}"/></w:r>"#);
    } else {
        let _ = write!(out, r#"<w:commentRangeStart w:id="{id}"/>"#);
    }
}

impl Out {
    /// A tracked change's attributes: id, author, date.
    fn change_attrs(&mut self, doc: &TextDoc, rev: RevId) -> String {
        self.change += 1;
        let mut s = format!(r#" w:id="{}""#, self.change);
        if let Some(r) = doc.revisions.get(rev) {
            let _ = write!(s, r#" w:author="{}""#, esc(&r.author));
            if r.date.len() >= 10 && r.date.as_bytes()[4] == b'-' {
                let _ = write!(s, r#" w:date="{}""#, esc(&r.date));
            }
        }
        s
    }

    fn rel(&mut self, kind: &str, target: String, external: bool) -> String {
        let id = format!("rId{}", self.rels.len() + 10);
        self.rels.push((id.clone(), kind.to_string(), target, external));
        id
    }

    fn image(&mut self, doc: &TextDoc, i: u32) -> Option<String> {
        if let Some(r) = self.image_rel.get(&i) {
            return Some(r.clone());
        }
        let img = doc.images.get(i)?;
        let ext = image_ext(&img.bytes);
        let name = format!("media/{}{}.{ext}", self.media_stem, self.media.len() + 1);
        self.media.push((format!("word/{name}"), img.bytes.to_vec()));
        let r = self.rel("image", name, false);
        self.image_rel.insert(i, r.clone());
        Some(r)
    }

    fn link(&mut self, url: &str) -> String {
        if let Some(r) = self.link_rel.get(url) {
            return r.clone();
        }
        let r = self.rel("hyperlink", url.to_string(), true);
        self.link_rel.insert(url.to_string(), r.clone());
        r
    }

    /// The runs of block `b`.
    fn runs(&mut self, doc: &TextDoc, p: &Para, out: &mut String) {
        let marks = self.cur.and_then(|b| self.marks.get(&b)).cloned().unwrap_or_default();
        let mut m = 0;
        for (s, e, a) in p.spans() {
            while m < marks.len() && marks[m].0 <= s {
                comment_marker(out, marks[m].1, marks[m].2);
                m += 1;
            }
            if s == e {
                continue;
            }
            let attrs = doc.attrs.get(a as AttrId).clone();
            let mut props = rpr(&attrs, &self.styles);
            // A formatting change: what it was, last in the run's properties.
            if let Some(m) = attrs.fmt {
                let old = rpr(doc.attrs.get(m.old), &self.styles);
                let old = if old.is_empty() { "<w:rPr/>".to_string() } else { old };
                let ch = format!("<w:rPrChange{}>{old}</w:rPrChange>", self.change_attrs(doc, m.rev));
                props = match props.strip_suffix("</w:rPr>") {
                    Some(head) => format!("{head}{ch}</w:rPr>"),
                    None => format!("<w:rPr>{ch}</w:rPr>"),
                };
            }
            let link = attrs.link.as_ref().map(|l| l.to_string());
            let mut body = String::new();
            let mut text = String::new();
            let flush = |text: &mut String, body: &mut String| {
                if !text.is_empty() {
                    let _ = write!(body, r#"<w:r>{props}<w:t xml:space="preserve">{}</w:t></w:r>"#, esc(text));
                    text.clear();
                }
            };
            for c in p.text[s as usize..e as usize].chars() {
                match c {
                    '\t' => {
                        flush(&mut text, &mut body);
                        let _ = write!(body, "<w:r>{props}<w:tab/></w:r>");
                    }
                    '\u{2028}' | '\n' => {
                        flush(&mut text, &mut body);
                        let _ = write!(body, "<w:r>{props}<w:br/></w:r>");
                    }
                    '\u{000C}' => {
                        flush(&mut text, &mut body);
                        let _ = write!(body, r#"<w:r>{props}<w:br w:type="page"/></w:r>"#);
                    }
                    OBJECT_CHAR if attrs.field.is_some() => {
                        flush(&mut text, &mut body);
                        let f = attrs.field.expect("checked");
                        let instr = match f {
                            Field::PageRef(t) => format!("PAGEREF {} \\h", ref_name(t)),
                            Field::TextRef(t) => format!("REF {} \\h", ref_name(t)),
                        };
                        field_runs(&mut body, &props, &instr, &doc.field_text(f));
                    }
                    OBJECT_CHAR if attrs.note.is_some() => {
                        flush(&mut text, &mut body);
                        let id = attrs.note.expect("checked");
                        if let Some(&(kind, n)) = self.notes.get(&id) {
                            let tag = if kind == NoteKind::Foot { "footnoteReference" } else { "endnoteReference" };
                            // The mark keeps its run's look, raised.
                            let mark = rpr(&CharAttrs { vert: Some(VertAlign::Super), note: None, ..attrs.clone() }, &self.styles);
                            let _ = write!(body, r#"<w:r>{mark}<w:{tag} w:id="{n}"/></w:r>"#);
                        }
                    }
                    OBJECT_CHAR => {
                        flush(&mut text, &mut body);
                        if let Some(o) = attrs.object.filter(|o| o.shape) {
                            if let Some(sd) = doc.shapes.get(o.image).cloned() {
                                self.pic += 1;
                                let (cx, cy) = (o.width as i64 * 635, o.height as i64 * 635);
                                let n = self.pic;
                                let (open, close) = match o.float {
                                    None => (r#"<wp:inline distT="0" distB="0" distL="0" distR="0">"#.to_string(), "</wp:inline>"),
                                    Some(f) => (anchor_open(&f, n), "</wp:anchor>"),
                                };
                                let (uri, inner) = if sd.kids.is_empty() {
                                    ("wordprocessingShape", self.wsp(doc, &sd, cx, cy, None))
                                } else {
                                    let mut g = format!(r#"<wpg:wgp><wpg:cNvGrpSpPr/><wpg:grpSpPr>{}</wpg:grpSpPr>"#, xfrm(&sd, 0, 0, cx, cy, true));
                                    self.group_kids(doc, &sd, cx, cy, &mut g, 0);
                                    g += "</wpg:wgp>";
                                    ("wordprocessingGroup", g)
                                };
                                let _ = write!(body, r#"<w:r>{props}<w:drawing>{open}<wp:extent cx="{cx}" cy="{cy}"/>{}<wp:docPr id="{n}" name="Shape {n}"/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/{uri}">{inner}</a:graphicData></a:graphic>{close}</w:drawing></w:r>"#, o.float.map_or(String::new(), |f| anchor_wrap(&f)));
                            }
                        } else if let Some(o) = attrs.object {
                            if let Some(rid) = self.image(doc, o.image) {
                                self.pic += 1;
                                let (cx, cy) = (o.width as u64 * 635, o.height as u64 * 635);
                                let n = self.pic;
                                let (open, close) = match o.float {
                                    None => (r#"<wp:inline distT="0" distB="0" distL="0" distR="0">"#.to_string(), "</wp:inline>"),
                                    Some(f) => (anchor_open(&f, n), "</wp:anchor>"),
                                };
                                let _ = write!(body, r#"<w:r>{props}<w:drawing>{open}<wp:extent cx="{cx}" cy="{cy}"/>{}<wp:docPr id="{n}" name="Picture {n}"/><a:graphic>"#, o.float.map_or(String::new(), |f| anchor_wrap(&f)));
                                let _ = write!(body, r#"<a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="{n}" name="Picture {n}"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic>{close}</w:drawing></w:r>"#);
                            }
                        }
                    }
                    c => text.push(c),
                }
            }
            flush(&mut text, &mut body);
            // Deleted text (put back inline by `expanded`), inserted text.
            if let Some(t) = attrs.deleted {
                if let Some(rev) = doc.deletions.get(t).map(|d| d.rev) {
                    body = body.replace(r#"<w:t xml:space="preserve">"#, r#"<w:delText xml:space="preserve">"#).replace("</w:t>", "</w:delText>");
                    body = format!("<w:del{}>{body}</w:del>", self.change_attrs(doc, rev));
                }
            }
            if let Some(rev) = attrs.rev {
                body = format!("<w:ins{}>{body}</w:ins>", self.change_attrs(doc, rev));
            }
            match link {
                Some(url) if url.starts_with('#') => {
                    let _ = write!(out, r#"<w:hyperlink w:anchor="{}">{body}</w:hyperlink>"#, esc(&url[1..]));
                }
                Some(url) => {
                    let r = self.link(&url);
                    let _ = write!(out, r#"<w:hyperlink r:id="{r}">{body}</w:hyperlink>"#);
                }
                None => out.push_str(&body),
            }
        }
        for &(_, end, id) in &marks[m..] {
            comment_marker(out, end, id);
        }
    }

    fn para(&mut self, doc: &TextDoc, b: usize, out: &mut String) {
        let Some(p) = doc.para(b) else { return };
        self.cur = Some(b);
        // A table of contents is a TOC field over its run of entries.
        let toc = |i: usize| doc.para(i).is_some_and(|q| q.props.toc.is_some() && q.props.cell.is_none());
        let mut lead = String::new();
        if toc(b) && (b == 0 || !toc(b - 1)) {
            let _ = write!(lead, r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> TOC \o "1-{}" \h \z \u </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r>"#, toc_depth(doc, b));
        }
        if self.targets.contains(&p.id) {
            self.bookmark += 1;
            let _ = write!(lead, r#"<w:bookmarkStart w:id="{}" w:name="{}"/>"#, self.bookmark, ref_name(p.id));
        }
        let mut tail = String::new();
        if self.targets.contains(&p.id) {
            let _ = write!(tail, r#"<w:bookmarkEnd w:id="{}"/>"#, self.bookmark);
        }
        if toc(b) && !toc(b + 1) {
            tail += r#"<w:r><w:fldChar w:fldCharType="end"/></w:r>"#;
        }
        self.para_of(doc, p, out, &lead, &tail);
        self.cur = None;
    }

    /// Paragraph `p`; `lead` goes before its runs (a note's number).
    fn para_of(&mut self, doc: &TextDoc, p: &Para, out: &mut String, lead: &str, tail: &str) {
        // A dropped capital: Word's frame paragraph holding the letter, then
        // the paragraph without it.
        if let (Some(Some(dc)), Some(c)) = (p.props.drop_cap, p.text.chars().next().filter(|c| c.is_alphanumeric())) {
            let n = c.len_utf8() as u32;
            let (_, cf) = doc.styles.para_format(p.style, &p.props);
            let body = doc.styles.char_format(&cf, doc.attrs.get(p.attr_at(n))).size as u32;
            let big = (body * dc.lines as u32 * 3 / 2).clamp(2, 3276) as u16;
            let line = dc.lines as u32 * body * 23 / 2;
            let letter = rpr(&CharAttrs { size: Some(big), ..doc.attrs.get(p.attr_at(0)).clone() }, &self.styles);
            let style = if p.style != 0 { self.styles.get(p.style as usize).map(|id| format!(r#"<w:pStyle w:val="{}"/>"#, esc(id))).unwrap_or_default() } else { String::new() };
            let _ = write!(out, r#"<w:p><w:pPr>{style}<w:keepNext/><w:framePr w:dropCap="{}" w:lines="{}" w:hSpace="{}" w:wrap="around" w:vAnchor="text" w:hAnchor="text"/><w:spacing w:before="0" w:after="0" w:line="{line}" w:lineRule="exact"/><w:textAlignment w:val="baseline"/></w:pPr><w:r>{letter}<w:t>{}</w:t></w:r></w:p>"#, if dc.margin { "margin" } else { "drop" }, dc.lines, dc.distance, esc(&c.to_string()));
            let mut q = p.clone();
            q.text = p.text[n as usize..].to_string();
            q.runs = p.runs.iter().map(|&(s, a)| (s.saturating_sub(n), a)).collect();
            q.runs.dedup_by(|b, a| a.0 == b.0 && {
                a.1 = b.1;
                true
            });
            q.props.drop_cap = None;
            if let Some(m) = self.cur.and_then(|b| self.marks.get_mut(&b)) {
                for x in m.iter_mut() {
                    x.0 = x.0.saturating_sub(n);
                }
            }
            return self.para_of(doc, &q, out, lead, tail);
        }
        let mut pp = String::new();
        // A contents entry in Normal goes out in Word's TOC style of its level.
        if let (0, Some(l)) = (p.style, p.props.toc.filter(|_| self.cur.is_some())) {
            let _ = write!(pp, r#"<w:pStyle w:val="TOC{}"/>"#, l.clamp(1, 9));
        } else if p.style != 0 {
            if let Some(id) = self.styles.get(p.style as usize) {
                let _ = write!(pp, r#"<w:pStyle w:val="{}"/>"#, esc(id));
            }
        }
        match p.props.list {
            Some(Some(l)) => {
                let fresh = self.lists.len() as u32 + self.restarts.len() as u32 + 1;
                let key = (l.id, l.kind);
                let (_, mut num) = *self.lists.entry(key).or_insert((fresh, fresh));
                if let Some(start) = p.props.list_start {
                    num = self.lists.len() as u32 + self.restarts.len() as u32 + 1;
                    self.restarts.push((num, key, l.level.min(8), start));
                    if let Some(e) = self.lists.get_mut(&key) {
                        e.1 = num;
                    }
                }
                let _ = write!(pp, r#"<w:numPr><w:ilvl w:val="{}"/><w:numId w:val="{num}"/></w:numPr>"#, l.level.min(8));
            }
            Some(None) => pp += r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="0"/></w:numPr>"#,
            None => {}
        }
        pp += &ppr_body(&p.props);
        // The paragraph mark: inserted or deleted with the break after it, and
        // the formatting of an empty paragraph's run.
        let mut mark = String::new();
        if let Some(b) = self.cur {
            if let Some(next) = doc.para(b + 1).filter(|_| doc.same_cell(b, b + 1)) {
                if let Some(r) = next.props.rev {
                    let _ = write!(mark, "<w:ins{}/>", self.change_attrs(doc, r));
                }
                if let Some(&r) = self.del_breaks.get(&next.id) {
                    let _ = write!(mark, "<w:del{}/>", self.change_attrs(doc, r));
                }
            }
        }
        if p.text.is_empty() {
            let r = rpr(doc.attrs.get(p.attr_at(0)), &self.styles);
            mark += r.strip_prefix("<w:rPr>").and_then(|r| r.strip_suffix("</w:rPr>")).unwrap_or("");
        }
        if !mark.is_empty() {
            let _ = write!(pp, "<w:rPr>{mark}</w:rPr>");
        }
        if let Some(i) = self.cur.and_then(|b| self.sect_end.get(&b)) {
            let _ = write!(pp, "\u{1}{i}\u{1}");
        }
        out.push_str("<w:p>");
        if !pp.is_empty() {
            let _ = write!(out, "<w:pPr>{pp}</w:pPr>");
        }
        out.push_str(lead);
        self.runs(doc, p, out);
        out.push_str(tail);
        out.push_str("</w:p>");
    }

    /// A `wps:wsp`: shape `sd` `cx` by `cy` EMU, at `off` in its group.
    fn wsp(&mut self, doc: &TextDoc, sd: &ShapeData, cx: i64, cy: i64, off: Option<(i64, i64)>) -> String {
        let (x, y) = off.unwrap_or((0, 0));
        let mut out = format!("<wps:wsp><wps:cNvSpPr/><wps:spPr>{}{}", xfrm(sd, x, y, cx, cy, false), geom_xml(&sd.geom));
        match sd.fill {
            Some(c) => {
                let _ = write!(out, r#"<a:solidFill><a:srgbClr val="{c:06X}"/></a:solidFill>"#);
            }
            None => out += "<a:noFill/>",
        }
        match sd.line {
            Some(l) => {
                let dash = match l.dash {
                    Dash::Solid => "solid",
                    Dash::Dot => "sysDot",
                    Dash::Dash => "dash",
                    Dash::DashDot => "dashDot",
                    Dash::LongDash => "lgDash",
                };
                let end = |e: ArrowEnd| match e {
                    ArrowEnd::None => "none",
                    ArrowEnd::Triangle => "triangle",
                    ArrowEnd::Open => "arrow",
                    ArrowEnd::Stealth => "stealth",
                    ArrowEnd::Diamond => "diamond",
                    ArrowEnd::Oval => "oval",
                };
                let _ = write!(out, r#"<a:ln w="{}"><a:solidFill><a:srgbClr val="{:06X}"/></a:solidFill><a:prstDash val="{dash}"/>"#, l.width as i64 * 635, l.color);
                if l.head != ArrowEnd::None {
                    let _ = write!(out, r#"<a:headEnd type="{}"/>"#, end(l.head));
                }
                if l.tail != ArrowEnd::None {
                    let _ = write!(out, r#"<a:tailEnd type="{}"/>"#, end(l.tail));
                }
                out += "</a:ln>";
            }
            None => out += "<a:ln><a:noFill/></a:ln>",
        }
        out += "</wps:spPr>";
        if !sd.paras.is_empty() {
            out += "<wps:txbx><w:txbxContent>";
            let cur = self.cur.take();
            for p in &sd.paras {
                let mut q = p.clone();
                q.props.cell = None;
                q.props.section = None;
                self.para_of(doc, &q, &mut out, "", "");
            }
            self.cur = cur;
            out += "</w:txbxContent></wps:txbx>";
        }
        let anchor = match sd.anchor {
            TextAnchor::Top => "t",
            TextAnchor::Middle => "ctr",
            TextAnchor::Bottom => "b",
        };
        let [l, t, r, b] = sd.inset.map(|v| v as i64 * 635);
        let _ = write!(out, r#"<wps:bodyPr lIns="{l}" tIns="{t}" rIns="{r}" bIns="{b}" anchor="{anchor}"/></wps:wsp>"#);
        out
    }

    /// A group's shapes into `out`, in its `cx` by `cy` EMU (its child
    /// coordinates the same).
    fn group_kids(&mut self, doc: &TextDoc, sd: &ShapeData, cx: i64, cy: i64, out: &mut String, depth: usize) {
        for k in &sd.kids {
            let f = |v: i32, s: i64| (v as i128 * s as i128 / 100_000) as i64;
            let (x, y, w, h) = (f(k.x, cx), f(k.y, cy), f(k.w, cx).max(1), f(k.h, cy).max(1));
            if k.shape.kids.is_empty() || depth + 1 >= officina_text::model::MAX_GROUP_DEPTH {
                let x2 = self.wsp(doc, &k.shape, w, h, Some((x, y)));
                out.push_str(&x2);
            } else {
                let _ = write!(out, "<wpg:grpSp><wpg:cNvGrpSpPr/><wpg:grpSpPr>{}</wpg:grpSpPr>", xfrm(&k.shape, x, y, w, h, true));
                self.group_kids(doc, &k.shape, w, h, out, depth + 1);
                out.push_str("</wpg:grpSp>");
            }
        }
    }

    /// The table starting at block `b`; returns the block after it.
    fn table(&mut self, doc: &TextDoc, b: usize, out: &mut String) -> usize {
        self.table_at(doc, b, doc.blocks.len(), false, out)
    }

    /// The table at a depth (`inner`: the one in a cell, ending by
    /// `limit`) starting at block `b`; returns the block after it.
    fn table_at(&mut self, doc: &TextDoc, b: usize, limit: usize, inner: bool, out: &mut String) -> usize {
        let t = doc.cell_at(b, inner).map(|c| c.table);
        let mut end = b;
        while end < limit && doc.cell_at(end, inner).map(|c| c.table) == t {
            end += 1;
        }
        // Rows and their cells (blocks, start and end across, in twips).
        let mut rows: Vec<Vec<(std::ops::Range<usize>, u32, u32)>> = Vec::new();
        let mut k = b;
        while k < end {
            let row = doc.row_range_at(k, inner);
            let row = k..row.end.max(k + 1).min(end);
            let mut x = 0u32;
            rows.push(
                cells_of(doc, row.clone(), inner)
                    .into_iter()
                    .map(|c| {
                        let w = doc.cell_at(c.start, inner).map_or(1440, |x| x.width);
                        let r = (c, x, x + w);
                        x += w;
                        r
                    })
                    .collect(),
            );
            k = row.end;
        }
        // The grid: every cell edge of every row (within 20 twips, one).
        let mut edges: Vec<u32> = rows.iter().flatten().flat_map(|c| [c.1, c.2]).collect();
        edges.sort_unstable();
        edges.dedup_by(|a, b| a.abs_diff(*b) <= 20);
        edges.truncate(64);
        let col_of = |x: u32| edges.iter().position(|e| e.abs_diff(x) <= 20).unwrap_or(edges.len().saturating_sub(1));
        let look = t.and_then(|t| doc.tables.get(&t)).cloned().unwrap_or_default();
        out.push_str("<w:tbl><w:tblPr>");
        if let Some(n) = &look.style {
            let _ = write!(out, r#"<w:tblStyle w:val="{}"/>"#, esc(&table_style_id(n)));
        }
        out.push_str(r#"<w:tblW w:w="0" w:type="auto"/>"#);
        match look.align {
            TableAlign::Center => out.push_str(r#"<w:jc w:val="center"/>"#),
            TableAlign::End => out.push_str(r#"<w:jc w:val="right"/>"#),
            _ => {}
        }
        if look.indent != 0 {
            let _ = write!(out, r#"<w:tblInd w:w="{}" w:type="dxa"/>"#, look.indent);
        }
        // Without a style or borders of its own, a table has rules everywhere.
        let borders = look.borders.or(if look.style.is_none() { Some(TableBorders::grid()) } else { None });
        if let Some(bs) = borders {
            out.push_str(&table_borders_xml(&bs));
        }
        if let Some(c) = look.shading {
            let _ = write!(out, r#"<w:shd w:val="clear" w:color="auto" w:fill="{c:06X}"/>"#);
        }
        if !look.autofit {
            out.push_str(r#"<w:tblLayout w:type="fixed"/>"#);
        }
        out.push_str(&pad_xml("tblCellMar", look.pad.unwrap_or_default()));
        out.push_str(&look_xml(look.look));
        out.push_str("</w:tblPr><w:tblGrid>");
        for w in edges.windows(2) {
            let _ = write!(out, r#"<w:gridCol w:w="{}"/>"#, w[1] - w[0]);
        }
        out.push_str("</w:tblGrid>");
        for (ri, row) in rows.iter().enumerate() {
            out.push_str("<w:tr>");
            let rl = row.first().and_then(|c| doc.cell_at(c.0.start, inner)).map(|c| c.row_look).unwrap_or_default();
            let mut trpr = String::new();
            if rl.cant_split {
                trpr += "<w:cantSplit/>";
            }
            if rl.height > 0 {
                let _ = write!(trpr, r#"<w:trHeight w:val="{}"{}/>"#, rl.height, if rl.exact { r#" w:hRule="exact""# } else { "" });
            }
            if rl.header {
                trpr += "<w:tblHeader/>";
            }
            if !trpr.is_empty() {
                let _ = write!(out, "<w:trPr>{trpr}</w:trPr>");
            }
            for (cell, x0, x1) in row {
                let Some(c) = doc.cell_at(cell.start, inner) else { continue };
                let span = col_of(*x1).saturating_sub(col_of(*x0)).max(1);
                let _ = write!(out, r#"<w:tc><w:tcPr><w:tcW w:w="{}" w:type="dxa"/>"#, c.width);
                if span > 1 {
                    let _ = write!(out, r#"<w:gridSpan w:val="{span}"/>"#);
                }
                // Merged down: the first of the run restarts it.
                let below = rows.get(ri + 1).and_then(|r| r.iter().find(|d| d.1.abs_diff(*x0) <= 20)).and_then(|d| doc.cell_at(d.0.start, inner)).is_some_and(|d| d.merged_up);
                if c.merged_up {
                    out.push_str("<w:vMerge/>");
                } else if below {
                    out.push_str(r#"<w:vMerge w:val="restart"/>"#);
                }
                let cb = c.look.borders;
                if !cb.is_empty() {
                    out.push_str("<w:tcBorders>");
                    for (name, side) in [("top", cb.top), ("left", cb.start), ("bottom", cb.bottom), ("right", cb.end)] {
                        match side {
                            None => {}
                            Some(None) => {
                                let _ = write!(out, r#"<w:{name} w:val="nil"/>"#);
                            }
                            Some(Some(x)) => out.push_str(&border_xml(name, &x)),
                        }
                    }
                    out.push_str("</w:tcBorders>");
                }
                match c.look.shading {
                    None => {}
                    Some(None) => out.push_str(r#"<w:shd w:val="clear" w:color="auto" w:fill="auto"/>"#),
                    Some(Some(f)) => {
                        let _ = write!(out, r#"<w:shd w:val="clear" w:color="auto" w:fill="{f:06X}"/>"#);
                    }
                }
                if let Some(pd) = c.look.pad {
                    out.push_str(&pad_xml("tcMar", pd));
                }
                match c.look.valign {
                    VAlign::Top => {}
                    VAlign::Center => out.push_str(r#"<w:vAlign w:val="center"/>"#),
                    VAlign::Bottom => out.push_str(r#"<w:vAlign w:val="bottom"/>"#),
                }
                out.push_str("</w:tcPr>");
                // A table in the cell, then (Word wants a cell to end so)
                // a paragraph after it.
                let mut x = cell.start;
                let mut last_table = false;
                while x < cell.end {
                    if !inner && doc.inner_cell(x).is_some() {
                        x = self.table_at(doc, x, cell.end, true, out).max(x + 1);
                        last_table = true;
                    } else {
                        self.para(doc, x, out);
                        x += 1;
                        last_table = false;
                    }
                }
                // A table ending the cell (or only empty paragraphs after
                // it) gets the paragraph Word wants, which reading drops.
                let mut k = cell.end;
                while k > cell.start && doc.inner_cell(k - 1).is_none() && doc.para(k - 1).is_some_and(|p| p.text.is_empty() && p.props.section.is_none()) {
                    k -= 1;
                }
                if !inner && (last_table || k > cell.start && doc.inner_cell(k - 1).is_some()) {
                    out.push_str("<w:p/>");
                }
                out.push_str("</w:tc>");
            }
            out.push_str("</w:tr>");
        }
        out.push_str("</w:tbl>");
        end
    }
}

/// A table style's id in `styles.xml`: its name's letters and digits.
fn table_style_id(name: &str) -> String {
    let id: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if id.is_empty() { "Table".to_string() } else { id }
}

fn table_borders_xml(b: &TableBorders) -> String {
    let mut s = String::from("<w:tblBorders>");
    for (name, side) in [("top", b.top), ("left", b.start), ("bottom", b.bottom), ("right", b.end), ("insideH", b.inside_h), ("insideV", b.inside_v)] {
        match side {
            Some(x) => s += &border_xml(name, &x),
            None => s += &format!(r#"<w:{name} w:val="nil"/>"#),
        }
    }
    s + "</w:tblBorders>"
}

fn pad_xml(name: &str, p: CellPad) -> String {
    format!(r#"<w:{name}><w:top w:w="{}" w:type="dxa"/><w:left w:w="{}" w:type="dxa"/><w:bottom w:w="{}" w:type="dxa"/><w:right w:w="{}" w:type="dxa"/></w:{name}>"#, p.top, p.start, p.bottom, p.end)
}

/// `w:tblLook`, with both the attributes and Word 2007's hex value.
fn look_xml(look: u8) -> String {
    let on = |bit: u8| look & bit != 0;
    let mut v = 0u16;
    for (bit, mask) in [(LOOK_HEADER, 0x20), (LOOK_TOTAL, 0x40), (LOOK_FIRST_COLUMN, 0x80), (LOOK_LAST_COLUMN, 0x100)] {
        if on(bit) {
            v |= mask;
        }
    }
    if !on(LOOK_BANDED_ROWS) {
        v |= 0x200;
    }
    if !on(LOOK_BANDED_COLUMNS) {
        v |= 0x400;
    }
    let b = |x: bool| if x { "1" } else { "0" };
    format!(r#"<w:tblLook w:val="{v:04X}" w:firstRow="{}" w:lastRow="{}" w:firstColumn="{}" w:lastColumn="{}" w:noHBand="{}" w:noVBand="{}"/>"#, b(on(LOOK_HEADER)), b(on(LOOK_TOTAL)), b(on(LOOK_FIRST_COLUMN)), b(on(LOOK_LAST_COLUMN)), b(!on(LOOK_BANDED_ROWS)), b(!on(LOOK_BANDED_COLUMNS)))
}

/// A table style as a `w:style w:type="table"`.
fn table_style_xml(name: &str, t: &TableStyle) -> String {
    let mut s = format!(r#"<w:style w:type="table" w:styleId="{}"><w:name w:val="{}"/><w:tblPr>{}</w:tblPr>"#, esc(&table_style_id(name)), esc(name), table_borders_xml(&t.borders));
    if let Some(c) = t.shading {
        let _ = write!(s, r#"<w:tcPr><w:shd w:val="clear" w:color="auto" w:fill="{c:06X}"/></w:tcPr>"#);
    }
    let region = |s: &mut String, ty: &str, r: &RegionLook, under: bool| {
        if *r == RegionLook::default() {
            return;
        }
        let _ = write!(s, r#"<w:tblStylePr w:type="{ty}">"#);
        if r.bold || r.color.is_some() {
            *s += "<w:rPr>";
            if r.bold {
                *s += "<w:b/>";
            }
            if let Some(c) = r.color {
                let _ = write!(s, r#"<w:color w:val="{c:06X}"/>"#);
            }
            *s += "</w:rPr>";
        }
        *s += "<w:tcPr>";
        if let Some(b) = r.rule {
            let _ = write!(s, "<w:tcBorders>{}</w:tcBorders>", border_xml(if under { "bottom" } else { "top" }, &b));
        }
        if let Some(c) = r.shading {
            let _ = write!(s, r#"<w:shd w:val="clear" w:color="auto" w:fill="{c:06X}"/>"#);
        }
        *s += "</w:tcPr></w:tblStylePr>";
    };
    region(&mut s, "firstRow", &t.header, true);
    region(&mut s, "lastRow", &t.total, false);
    region(&mut s, "firstCol", &t.first_col, true);
    region(&mut s, "lastCol", &t.last_col, true);
    for (ty, c) in [("band1Horz", t.band_row), ("band1Vert", t.band_col)] {
        if let Some(c) = c {
            let _ = write!(s, r#"<w:tblStylePr w:type="{ty}"><w:tcPr><w:shd w:val="clear" w:color="auto" w:fill="{c:06X}"/></w:tcPr></w:tblStylePr>"#);
        }
    }
    s + "</w:style>"
}

/// The deepest heading level the table of contents starting at `b` lists (at least 3).
fn toc_depth(doc: &TextDoc, b: usize) -> u8 {
    let mut d = 3;
    let mut i = b;
    while let Some(l) = doc.para(i).and_then(|q| q.props.toc) {
        d = d.max(l.min(9));
        i += 1;
    }
    d
}

/// The cells of a row as block ranges.
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

/// A shape's `a:xfrm`: turn, flips, place and size (a group's with child
/// coordinates the same as its own size).
fn xfrm(sd: &ShapeData, x: i64, y: i64, cx: i64, cy: i64, group: bool) -> String {
    let mut a = String::new();
    if sd.rot != 0 {
        let _ = write!(a, r#" rot="{}""#, sd.rot);
    }
    if sd.flip_h {
        a += r#" flipH="1""#;
    }
    if sd.flip_v {
        a += r#" flipV="1""#;
    }
    let ch = if group { r#"<a:chOff x="0" y="0"/>"#.to_string() + &format!(r#"<a:chExt cx="{cx}" cy="{cy}"/>"#) } else { String::new() };
    format!(r#"<a:xfrm{a}><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/>{ch}</a:xfrm>"#)
}

/// A preset's `a:prstGeom` with its adjust values, or paths as `a:custGeom`.
fn geom_xml(g: &Geom) -> String {
    match g {
        Geom::Preset { name, adj } => {
            let mut s = format!(r#"<a:prstGeom prst="{}"><a:avLst>"#, esc(name));
            for (n, v) in adj {
                let _ = write!(s, r#"<a:gd name="{}" fmla="val {v}"/>"#, esc(n));
            }
            s + "</a:avLst></a:prstGeom>"
        }
        Geom::Custom(paths) => {
            let mut s = String::from(r#"<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l="l" t="t" r="r" b="b"/><a:pathLst>"#);
            for p in paths {
                let _ = write!(s, r#"<a:path w="{}" h="{}"{}{}>"#, p.w, p.h, if p.filled { "" } else { r#" fill="none""# }, if p.stroked { "" } else { r#" stroke="0""# });
                for c in &p.cmds {
                    let pt = |x: i64, y: i64| format!(r#"<a:pt x="{x}" y="{y}"/>"#);
                    match *c {
                        PathCmd::Move(x, y) => s += &format!("<a:moveTo>{}</a:moveTo>", pt(x, y)),
                        PathCmd::Line(x, y) => s += &format!("<a:lnTo>{}</a:lnTo>", pt(x, y)),
                        PathCmd::Cubic(a, b, c2, d, e, f) => s += &format!("<a:cubicBezTo>{}{}{}</a:cubicBezTo>", pt(a, b), pt(c2, d), pt(e, f)),
                        PathCmd::Close => s += "<a:close/>",
                    }
                }
                s += "</a:path>";
            }
            s + "</a:pathLst></a:custGeom>"
        }
    }
}

fn image_ext(b: &[u8]) -> &'static str {
    if let Some(k) = officina_pict::sniff(b) {
        k.name()
    } else if b.starts_with(&[0xFF, 0xD8]) {
        "jpeg"
    } else if b.starts_with(b"GIF8") {
        "gif"
    } else if b.starts_with(b"RIFF") {
        "webp"
    } else {
        "png"
    }
}

/// A header or footer part's `{page}` and `{pages}` as page fields in the
/// run's own format.
fn page_fields(x: &str) -> String {
    let mut out = String::with_capacity(x.len());
    let mut rest = x;
    loop {
        let next = [("{pages}", "NUMPAGES"), ("{page}", "PAGE")].iter().filter_map(|&(k, f)| rest.find(k).map(|i| (i, k, f))).min_by_key(|x| (x.0, std::cmp::Reverse(x.1.len())));
        let Some((i, key, field)) = next else { break };
        let (head, tail) = (&rest[..i], &rest[i + key.len()..]);
        // Inside a run's text: close it, the field, open it again.
        let run = head.rfind("<w:r>").map(|r| &head[r + 5..]).unwrap_or("");
        let in_text = run.contains("<w:t") && !run.contains("</w:r>");
        out.push_str(head);
        if in_text {
            let props = run.find("<w:rPr>").and_then(|a| run[a..].find("</w:rPr>").map(|b| &run[a..a + b + 8])).unwrap_or("");
            let _ = write!(out, r#"</w:t></w:r><w:fldSimple w:instr=" {field} "><w:r>{props}<w:t>1</w:t></w:r></w:fldSimple><w:r>{props}<w:t xml:space="preserve">"#);
        } else {
            out.push_str(key);
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

fn margin_part(hf: &HeaderFooter, tag: &str) -> String {
    let mut body = String::new();
    let mut rest = hf.text.as_str();
    while !rest.is_empty() {
        let (field, at) = match (rest.find("{page}"), rest.find("{pages}")) {
            (Some(a), Some(b)) if b < a => ("NUMPAGES", b),
            (Some(a), _) => ("PAGE", a),
            (None, Some(b)) => ("NUMPAGES", b),
            (None, None) => ("", rest.len()),
        };
        if at > 0 {
            let _ = write!(body, r#"<w:r><w:t xml:space="preserve">{}</w:t></w:r>"#, esc(&rest[..at]));
        }
        if field.is_empty() {
            break;
        }
        let _ = write!(body, r#"<w:fldSimple w:instr=" {field} "><w:r><w:t>1</w:t></w:r></w:fldSimple>"#);
        rest = &rest[at + if field == "PAGE" { 6 } else { 7 }..];
    }
    let jc = match hf.align {
        Align::Start => "left",
        Align::Center => "center",
        Align::End => "right",
        Align::Justify => "both",
    };
    format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:{tag} {W_NS}><w:p><w:pPr><w:jc w:val="{jc}"/></w:pPr>{body}</w:p></w:{tag}>"#)
}

fn styles_part(doc: &TextDoc, ids: &[String], lists: &mut HashMap<(u32, ListKind), (u32, u32)>, restarts: usize) -> String {
    let mut s = styles_part_ours(doc, ids, lists, restarts);
    // Word's TOC styles for the entries' levels (based on Normal, the
    // entries' own properties direct).
    let mut levels: Vec<u8> = doc.blocks.iter().filter_map(|b| b.as_para()).filter(|p| p.style == 0).filter_map(|p| p.props.toc).map(|l| l.clamp(1, 9)).collect();
    levels.sort_unstable();
    levels.dedup();
    let tail = s.split_off(s.len() - "</w:styles>".len());
    for l in levels {
        if !ids.iter().any(|i| *i == format!("TOC{l}")) {
            let _ = write!(s, r#"<w:style w:type="paragraph" w:styleId="TOC{l}"><w:name w:val="toc {l}"/><w:basedOn w:val="{}"/><w:next w:val="{}"/><w:uiPriority w:val="39"/><w:unhideWhenUsed/></w:style>"#, esc(&ids[0]), esc(&ids[0]));
        }
    }
    s + &tail
}

fn styles_part_ours(doc: &TextDoc, ids: &[String], lists: &mut HashMap<(u32, ListKind), (u32, u32)>, restarts: usize) -> String {
    let mut s = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles {W_NS}><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="{f}" w:hAnsi="{f}" w:cs="{f}"/><w:sz w:val="{z}"/><w:szCs w:val="{z}"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="160" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults>"#, f = esc(&doc.styles.default_char.family), z = doc.styles.default_char.size);
    for (i, st) in doc.styles.styles.iter().enumerate() {
        let kind = if st.kind == StyleKind::Para { "paragraph" } else { "character" };
        let default = if i == 0 { r#" w:default="1""# } else { "" };
        let _ = write!(s, r#"<w:style w:type="{kind}"{default} w:styleId="{}"><w:name w:val="{}"/>"#, esc(&ids[i]), esc(&style_name(&st.name)));
        if let Some(p) = st.parent.and_then(|p| ids.get(p as usize)) {
            let _ = write!(s, r#"<w:basedOn w:val="{}"/>"#, esc(p));
        }
        if let Some(n) = st.next.and_then(|p| ids.get(p as usize)) {
            let _ = write!(s, r#"<w:next w:val="{}"/>"#, esc(n));
        }
        if st.kind == StyleKind::Para {
            s += r#"<w:qFormat/>"#;
            // A style's list: a numbering of its own, as the text's lists.
            let num = match st.para.list {
                Some(Some(l)) => {
                    let fresh = lists.len() as u32 + restarts as u32 + 1;
                    let (_, n) = *lists.entry((l.id, l.kind)).or_insert((fresh, fresh));
                    format!(r#"<w:numPr><w:ilvl w:val="{}"/><w:numId w:val="{n}"/></w:numPr>"#, l.level.min(8))
                }
                Some(None) => r#"<w:numPr><w:numId w:val="0"/></w:numPr>"#.to_string(),
                None => String::new(),
            };
            let body = num + &ppr_body(&st.para);
            if !body.is_empty() {
                let _ = write!(s, "<w:pPr>{body}</w:pPr>");
            }
        }
        let mut chars = st.chars.clone();
        chars.style = None;
        s += &rpr(&chars, ids);
        s += "</w:style>";
    }
    for (name, t) in &doc.styles.tables {
        s += &table_style_xml(name, t);
    }
    s += "</w:styles>";
    s
}

/// A list level as a `w:lvl`.
fn lvl_xml(l: u8, lv: &ListLevel) -> String {
    let fmt = match lv.format {
        NumFormat::Decimal => "decimal",
        NumFormat::DecimalZero => "decimalZero",
        NumFormat::LowerLetter => "lowerLetter",
        NumFormat::UpperLetter => "upperLetter",
        NumFormat::LowerRoman => "lowerRoman",
        NumFormat::UpperRoman => "upperRoman",
        NumFormat::Bullet => "bullet",
        NumFormat::None => "none",
    };
    format!(r#"<w:lvl w:ilvl="{l}"><w:start w:val="{}"/><w:numFmt w:val="{fmt}"/><w:lvlText w:val="{}"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="{}" w:hanging="{}"/></w:pPr></w:lvl>"#, lv.start, esc(&lv.text), lv.indent, lv.hang)
}

fn numbering_part(lists: &HashMap<(u32, ListKind), (u32, u32)>, restarts: &[(u32, (u32, ListKind), u8, u32)], doc: &TextDoc) -> String {
    let mut s = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:numbering {W_NS}>"#);
    let mut v: Vec<((u32, ListKind), u32)> = lists.iter().map(|(k, x)| (*k, x.0)).collect();
    v.sort_by_key(|x| x.1);
    for &((id, kind), n) in &v {
        let _ = write!(s, r#"<w:abstractNum w:abstractNumId="{n}"><w:multiLevelType w:val="hybridMultilevel"/>"#);
        let def = doc.styles.lists.get(&id).cloned().unwrap_or_else(|| ListDef::builtin(kind));
        for l in 0..9u8 {
            let lv = def.level(l).cloned().unwrap_or_else(|| ListLevel::builtin(kind, l));
            s += &lvl_xml(l, &lv);
        }
        s += "</w:abstractNum>";
    }
    for &(_, n) in &v {
        let _ = write!(s, r#"<w:num w:numId="{n}"><w:abstractNumId w:val="{n}"/></w:num>"#);
    }
    for &(num, key, level, start) in restarts {
        let Some(&(abs, _)) = lists.get(&key) else { continue };
        let _ = write!(s, r#"<w:num w:numId="{num}"><w:abstractNumId w:val="{abs}"/><w:lvlOverride w:ilvl="{level}"><w:startOverride w:val="{start}"/></w:lvlOverride></w:num>"#);
    }
    s += "</w:numbering>";
    s
}

/// The document as a `.docx`.
pub fn write<W: io::Write>(doc: &TextDoc, out: W) -> io::Result<W> {
    // Unsettled merge conflicts: Word has none, this copy's version goes.
    if !doc.conflicts.is_empty() {
        return write(&doc.without_conflicts(), out);
    }
    // Tracked deletions are written inline: put their text back, marked.
    let ex = (!doc.deletions.is_empty()).then(|| doc.expanded());
    let (doc, del_breaks) = match &ex {
        Some(x) => (&x.doc, x.deleted_breaks.clone()),
        None => (doc, HashMap::new()),
    };
    let ids: Vec<String> = doc.styles.styles.iter().enumerate().map(|(i, s)| style_id(&s.name, i)).collect();
    // Notes are numbered in the file in anchor order, each kind from 1.
    let notes: HashMap<NoteId, (NoteKind, u32)> = doc.note_numbers().iter().map(|(&id, &k)| (id, k)).collect();
    let mut o = Out { rels: Vec::new(), media: Vec::new(), image_rel: HashMap::new(), link_rel: HashMap::new(), lists: HashMap::new(), restarts: Vec::new(), styles: ids.clone(), pic: 0, notes: notes.clone(), media_stem: "image", marks: HashMap::new(), cur: None, del_breaks, change: 1_000_000, targets: HashSet::new(), bookmark: 0, sect_end: HashMap::new() };
    // Paragraphs that fields refer to get bookmarks.
    for bl in doc.blocks.iter() {
        if let Some(p) = bl.as_para() {
            o.targets.extend(doc.fields_in(p).into_iter().map(|f| f.1.target()));
        }
    }
    // Comments: a reply's range is the comment it answers.
    let live = doc.live_comments();
    let ranges = doc.comment_ranges();
    let mut range_of: HashMap<CommentId, ((usize, u32), (usize, u32))> = ranges.iter().map(|&(id, s, e)| (id, (s, e))).collect();
    for &id in &live {
        if !range_of.contains_key(&id) {
            let mut root = id;
            for _ in 0..64 {
                match doc.comments.get(root).and_then(|c| c.parent) {
                    Some(p) => root = p,
                    None => break,
                }
            }
            if let Some(&r) = range_of.get(&root) {
                range_of.insert(id, r);
            }
        }
    }
    for &id in &live {
        let Some(&((sb, so), (eb, eo))) = range_of.get(&id) else { continue };
        o.marks.entry(sb).or_default().push((so, false, id));
        o.marks.entry(eb).or_default().push((eo, true, id));
    }
    for v in o.marks.values_mut() {
        // Ends before starts at the same place: ranges don't touch.
        v.sort_by_key(|&(at, end, _)| (at, !end));
    }
    // Sections: where each starts and its settings; each but the last ends
    // in its last paragraph's `w:sectPr`, the last in the body's.
    let n = doc.blocks.len();
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
    let mut sect_after_table: HashMap<usize, usize> = HashMap::new();
    for i in 1..secs.len() {
        let end = secs[i].0 - 1;
        if doc.cell(end).is_some() {
            sect_after_table.insert(secs[i].0, i - 1);
        } else {
            o.sect_end.insert(end, i - 1);
        }
    }
    let mut body = String::new();
    let mut b = 0;
    while b < n {
        if doc.cell(b).is_some() {
            b = o.table(doc, b, &mut body);
            if let Some(i) = sect_after_table.get(&b) {
                // A section break needs a paragraph to hold it.
                let _ = write!(body, "<w:p><w:pPr>\u{1}{i}\u{1}</w:pPr></w:p>");
                continue;
            }
            // Word needs a paragraph between two tables and after a last one.
            if b >= n || doc.cell(b).is_some() {
                body.push_str("<w:p/>");
            }
        } else {
            o.para(doc, b, &mut body);
            b += 1;
        }
    }
    let mut sect = String::new();
    let mut parts: Vec<(String, String)> = Vec::new();
    // Footnotes and endnotes, each part with its own relationships.
    let mut note_parts: Vec<(&str, String, String)> = Vec::new();
    let mut note_media: Vec<(String, Vec<u8>)> = Vec::new();
    for (kind, tag, part) in [(NoteKind::Foot, "footnote", "footnotes"), (NoteKind::End, "endnote", "endnotes")] {
        let mut ours: Vec<(u32, NoteId)> = notes.iter().filter(|(_, k)| k.0 == kind).map(|(&id, k)| (k.1, id)).collect();
        if ours.is_empty() {
            continue;
        }
        ours.sort_unstable();
        let mut no = Out { rels: Vec::new(), media: Vec::new(), image_rel: HashMap::new(), link_rel: HashMap::new(), lists: HashMap::new(), restarts: Vec::new(), styles: ids.clone(), pic: 1000 * (note_parts.len() as u32 + 1), notes: HashMap::new(), media_stem: if kind == NoteKind::Foot { "fn_image" } else { "en_image" }, marks: HashMap::new(), cur: None, del_breaks: HashMap::new(), change: 0, targets: HashSet::new(), bookmark: 0, sect_end: HashMap::new() };
        let mut x = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:{part} {W_NS} {DRAW_NS}><w:{tag} w:type="separator" w:id="-1"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:r><w:separator/></w:r></w:p></w:{tag}><w:{tag} w:type="continuationSeparator" w:id="0"><w:p><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:r><w:continuationSeparator/></w:r></w:p></w:{tag}>"#);
        // Lists in notes come out as plain paragraphs (the numbering part is the body's).
        for (n, id) in ours {
            let Some(note) = doc.notes.get(id) else { continue };
            let _ = write!(x, r#"<w:{tag} w:id="{n}">"#);
            for (i, p) in note.paras.iter().enumerate() {
                let lead = if i == 0 { format!(r#"<w:r><w:rPr><w:vertAlign w:val="superscript"/></w:rPr><w:{tag}Ref/></w:r><w:r><w:t xml:space="preserve"> </w:t></w:r>"#) } else { String::new() };
                let mut q = p.clone();
                q.props.list = None;
                q.props.cell = None;
                no.para_of(doc, &q, &mut x, &lead, "");
            }
            let _ = write!(x, "</w:{tag}>");
        }
        let _ = write!(x, "</w:{part}>");
        let mut r = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
        for (id, k, target, external) in &no.rels {
            let mode = if *external { r#" TargetMode="External""# } else { "" };
            let _ = write!(r, r#"<Relationship Id="{id}" Type="{REL}/{k}" Target="{}"{mode}/>"#, esc(target));
        }
        r += "</Relationships>";
        note_media.extend(no.media);
        o.rel(part, format!("{part}.xml"), false);
        note_parts.push((part, x, r));
    }
    // comments.xml, and commentsExtended.xml for threads and resolved state; a
    // comment's last paragraph's id ties the two.
    let mut comment_parts: Vec<(&str, &str, String)> = Vec::new();
    if !live.is_empty() {
        let pid = |id: CommentId| format!("{:08X}", 0x1000_0000 + (id & 0x0FFF_FFFF));
        let mut x = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:comments {W_NS} xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="w14">"#);
        let mut ex = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w15:commentsEx xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" mc:Ignorable="w15">"#);
        let live_set: std::collections::HashSet<CommentId> = live.iter().copied().collect();
        let mut other = 0x2000_0000u32;
        for &id in &live {
            let Some(c) = doc.comments.get(id) else { continue };
            let _ = write!(x, r#"<w:comment w:id="{id}" w:author="{}" w:initials="{}""#, esc(&c.author), esc(&c.initials));
            if c.date.len() >= 10 && c.date.as_bytes()[4] == b'-' {
                let _ = write!(x, r#" w:date="{}""#, esc(&c.date));
            }
            x.push('>');
            let lines: Vec<&str> = c.text.split('\n').collect();
            for (i, line) in lines.iter().enumerate() {
                let p = if i + 1 == lines.len() {
                    pid(id)
                } else {
                    other += 1;
                    format!("{other:08X}")
                };
                let _ = write!(x, r#"<w:p w14:paraId="{p}" w14:textId="77777777">"#);
                if i == 0 {
                    x += r#"<w:r><w:annotationRef/></w:r>"#;
                }
                if !line.is_empty() {
                    let _ = write!(x, r#"<w:r><w:t xml:space="preserve">{}</w:t></w:r>"#, esc(line));
                }
                x += "</w:p>";
            }
            x += "</w:comment>";
            let parent = c.parent.filter(|p| live_set.contains(p)).map(|p| format!(r#" w15:paraIdParent="{}""#, pid(p))).unwrap_or_default();
            let _ = write!(ex, r#"<w15:commentEx w15:paraId="{}"{parent} w15:done="{}"/>"#, pid(id), c.done as u8);
        }
        x += "</w:comments>";
        ex += "</w15:commentsEx>";
        o.rel("comments", "comments.xml".into(), false);
        o.rel("http://schemas.microsoft.com/office/2011/relationships/commentsExtended", "commentsExtended.xml".into(), false);
        comment_parts.push(("comments", "comments", x));
        comment_parts.push(("commentsExtended", "commentsExtended", ex));
    }
    let mut margin_rels: Vec<(String, String)> = Vec::new();
    // Every header first, then every footer (the order sectPr wants); the
    // default kind, the first page's, even pages'.
    let kinds = [("default", Some((&doc.header, &doc.footer))), ("first", doc.first_page.as_ref().map(|m| (&m.header, &m.footer))), ("even", doc.even_pages.as_ref().map(|m| (&m.header, &m.footer)))];
    let mut n = 0;
    for (k, (tag, kind)) in [("hdr", "header"), ("ftr", "footer")].into_iter().enumerate() {
        for (ty, pair) in kinds {
            let Some(pair) = pair else { continue };
            let hf = if k == 0 { pair.0 } else { pair.1 };
            // A first or even kind with none: a blank part, so the default
            // one doesn't show there.
            let empty = HeaderFooter::line("", Align::Start);
            let hf = match hf {
                Some(h) => h,
                None if ty != "default" => &empty,
                None => continue,
            };
            n += 1;
            let name = format!("{kind}{n}.xml");
            let r = o.rel(kind, name.clone(), false);
            let _ = write!(sect, r#"<w:{kind}Reference w:type="{ty}" r:id="{r}"/>"#);
            if hf.paras.is_empty() {
                parts.push((format!("word/{name}"), margin_part(hf, tag)));
                continue;
            }
            // Its own paragraphs, with their pictures beside the part.
            let mut ho = Out { rels: Vec::new(), media: Vec::new(), image_rel: HashMap::new(), link_rel: HashMap::new(), lists: HashMap::new(), restarts: Vec::new(), styles: ids.clone(), pic: 10_000 * n, notes: HashMap::new(), media_stem: if k == 0 { "hdr_image" } else { "ftr_image" }, marks: HashMap::new(), cur: None, del_breaks: HashMap::new(), change: 0, targets: HashSet::new(), bookmark: 0, sect_end: HashMap::new() };
            let mut x = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:{tag} {W_NS} {DRAW_NS}>"#);
            for p in &hf.paras {
                let mut q = p.clone();
                q.props.list = None;
                q.props.cell = None;
                q.props.section = None;
                ho.para_of(doc, &q, &mut x, "", "");
            }
            let _ = write!(x, "</w:{tag}>");
            parts.push((format!("word/{name}"), page_fields(&x)));
            let mut r = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
            for (id, k, target, external) in &ho.rels {
                let mode = if *external { r#" TargetMode="External""# } else { "" };
                let _ = write!(r, r#"<Relationship Id="{id}" Type="{REL}/{k}" Target="{}"{mode}/>"#, esc(target));
            }
            r += "</Relationships>";
            margin_rels.push((format!("word/_rels/{name}.rels"), r));
            note_media.extend(ho.media);
        }
    }
    // Each section's page: its own, else the document's.
    let geo = |g: &officina_text::model::PageSetup| format!(r#"<w:pgSz w:w="{}" w:h="{}"{}/><w:pgMar w:top="{}" w:right="{}" w:bottom="{}" w:left="{}" w:header="{h}" w:footer="{h}" w:gutter="0"/>"#, g.width, g.height, if g.width > g.height { r#" w:orient="landscape""# } else { "" }, g.margin_top, g.margin_end, g.margin_bottom, g.margin_start, h = HEADER_FROM_EDGE);
    // Every section names the header and footer (a first one without would
    // have none).
    let sect_pr = |x: &Section| {
        let ty = match x.start {
            SectionStart::NextPage => "",
            SectionStart::Continuous => r#"<w:type w:val="continuous"/>"#,
            SectionStart::OddPage => r#"<w:type w:val="oddPage"/>"#,
            SectionStart::EvenPage => r#"<w:type w:val="evenPage"/>"#,
        };
        let cols = if x.columns > 1 || x.rule { format!(r#"<w:cols w:num="{}" w:space="{}"{}/>"#, x.columns, x.gap, if x.rule { r#" w:sep="1""# } else { "" }) } else { String::new() };
        let mut geo = geo(x.page.as_ref().unwrap_or(&doc.page));
        if let Some(n) = doc.settings.line_numbers {
            let restart = match n.restart {
                LineRestart::Page => "newPage",
                LineRestart::Section => "newSection",
                LineRestart::Continuous => "continuous",
            };
            let distance = if n.distance > 0 { format!(r#" w:distance="{}""#, n.distance) } else { String::new() };
            let _ = write!(geo, r#"<w:lnNumType w:countBy="{}" w:start="{}"{distance} w:restart="{restart}"/>"#, n.count_by, n.start.saturating_sub(1));
        }
        // The first page's own header: the first section's first page.
        let title = if doc.first_page.is_some() && std::ptr::eq(x, &secs[0].1) { "<w:titlePg/>" } else { "" };
        format!("<w:sectPr>{sect}{ty}{geo}{cols}{title}</w:sectPr>")
    };
    if secs.len() > 1 {
        // Text never holds U+0001 (escaped out), so the pieces alternate.
        let mut filled = String::with_capacity(body.len());
        for (j, piece) in body.split('\u{1}').enumerate() {
            if j % 2 == 0 {
                filled.push_str(piece);
            } else if let Some((_, x)) = piece.parse::<usize>().ok().and_then(|i| secs.get(i)) {
                filled.push_str(&sect_pr(x));
            }
        }
        body = filled;
    }
    let sect = sect_pr(&secs[secs.len() - 1].1);
    let document = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {W_NS} {DRAW_NS}><w:body>{body}{sect}</w:body></w:document>"#);

    // Settings: tracking on, even pages' own headers.
    if doc.tracking || doc.even_pages.is_some() || doc.settings.hyphenate {
        let even = if doc.even_pages.is_some() { "<w:evenAndOddHeaders/>" } else { "" };
        let track = if doc.tracking { "<w:trackRevisions/>" } else { "" };
        let hyph = if doc.settings.hyphenate { "<w:autoHyphenation/>" } else { "" };
        o.rel("settings", "settings.xml".into(), false);
        // Schema order: trackRevisions, autoHyphenation, evenAndOddHeaders.
        parts.push(("word/settings.xml".into(), format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:settings {W_NS}>{track}{hyph}{even}</w:settings>"#)));
    }
    let styles_xml = styles_part(doc, &ids, &mut o.lists, o.restarts.len());
    let mut z = ZipWriter::new(out);
    let mut types = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="jpeg" ContentType="image/jpeg"/><Default Extension="gif" ContentType="image/gif"/><Default Extension="webp" ContentType="image/webp"/><Default Extension="emf" ContentType="image/x-emf"/><Default Extension="wmf" ContentType="image/x-wmf"/><Default Extension="bmp" ContentType="image/bmp"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>"#);
    if !o.lists.is_empty() {
        types += r#"<Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>"#;
    }
    for (part, _, _) in &note_parts {
        let _ = write!(types, r#"<Override PartName="/word/{part}.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.{part}+xml"/>"#);
    }
    for (part, kind, _) in &comment_parts {
        let _ = write!(types, r#"<Override PartName="/word/{part}.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.{kind}+xml"/>"#);
    }
    for (name, _) in &parts {
        let kind = if name.contains("header") {
            "header"
        } else if name.contains("settings") {
            "settings"
        } else {
            "footer"
        };
        let _ = write!(types, r#"<Override PartName="/{name}" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.{kind}+xml"/>"#);
    }
    types += "</Types>";
    z.add("[Content_Types].xml", types.as_bytes())?;
    z.add("_rels/.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#).as_bytes())?;
    z.add("word/document.xml", document.as_bytes())?;
    z.add("word/styles.xml", styles_xml.as_bytes())?;
    let mut rels = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/>"#);
    if !o.lists.is_empty() {
        z.add("word/numbering.xml", numbering_part(&o.lists, &o.restarts, doc).as_bytes())?;
        let _ = write!(rels, r#"<Relationship Id="rId2" Type="{REL}/numbering" Target="numbering.xml"/>"#);
    }
    for (id, kind, target, external) in &o.rels {
        let mode = if *external { r#" TargetMode="External""# } else { "" };
        let ty = if kind.starts_with("http") { kind.clone() } else { format!("{REL}/{kind}") };
        let _ = write!(rels, r#"<Relationship Id="{id}" Type="{ty}" Target="{}"{mode}/>"#, esc(target));
    }
    rels += "</Relationships>";
    z.add("word/_rels/document.xml.rels", rels.as_bytes())?;
    for (name, xml) in &parts {
        z.add(name, xml.as_bytes())?;
    }
    for (name, r) in &margin_rels {
        z.add(name, r.as_bytes())?;
    }
    for (part, _, xml) in &comment_parts {
        z.add(&format!("word/{part}.xml"), xml.as_bytes())?;
    }
    for (part, xml, r) in &note_parts {
        z.add(&format!("word/{part}.xml"), xml.as_bytes())?;
        z.add(&format!("word/_rels/{part}.xml.rels"), r.as_bytes())?;
    }
    for (name, bytes) in o.media.iter().chain(note_media.iter()) {
        z.add_stored(name, bytes)?;
    }
    z.finish()
}
