//! `.docx` out, in the document process, over our own model.
//!
//! Writes what the model holds: styles with inheritance (the built-in ones under
//! Word's names, so Word shows them as its own), paragraphs and their direct
//! properties, runs, links, tabs and line breaks, lists as numbering definitions
//! (one per list), tables with their column grid, inline pictures, page size and
//! margins, and a header and footer with page number fields.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io;

use officina_text::model::{Align, AttrId, CharAttrs, HeaderFooter, LineSpacing, ListKind, NoteId, NoteKind, Para, ParaProps, StyleKind, TextDoc, VertAlign, HEADER_FROM_EDGE, OBJECT_CHAR};

use crate::xml::esc;
use crate::zip::ZipWriter;

const W_NS: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
const DRAW_NS: &str = r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture""#;
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
    if let Some(v) = a.strike {
        s += &on("strike", v);
    }
    if let Some(c) = a.color {
        let _ = write!(s, r#"<w:color w:val="{:06X}"/>"#, c & 0xFF_FFFF);
    }
    if let Some(z) = a.size {
        let _ = write!(s, r#"<w:sz w:val="{z}"/><w:szCs w:val="{z}"/>"#);
    }
    if let Some(v) = a.vert {
        let _ = write!(s, r#"<w:vertAlign w:val="{}"/>"#, match v {
            VertAlign::Super => "superscript",
            VertAlign::Sub => "subscript",
            VertAlign::Baseline => "baseline",
        });
    }
    if let Some(h) = a.highlight {
        let _ = write!(s, r#"<w:shd w:val="clear" w:color="auto" w:fill="{:06X}"/>"#, h & 0xFF_FFFF);
    }
    match (a.underline, a.link.is_some()) {
        (Some(true), _) => s += r#"<w:u w:val="single"/>"#,
        (Some(false), _) => s += r#"<w:u w:val="none"/>"#,
        _ => {}
    }
    if s.is_empty() {
        s
    } else {
        format!("<w:rPr>{s}</w:rPr>")
    }
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

struct Out {
    rels: Vec<(String, String, String, bool)>, // id, type, target, external
    media: Vec<(String, Vec<u8>)>,
    image_rel: HashMap<u32, String>,
    link_rel: HashMap<String, String>,
    lists: HashMap<(u32, ListKind), (u32, ListKind)>,
    styles: Vec<String>,
    pic: u32,
    /// Our note id to (kind, the id the file gives it).
    notes: HashMap<NoteId, (NoteKind, u32)>,
    /// Where this part's pictures go ("image" for the document's).
    media_stem: &'static str,
}

impl Out {
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
        for (s, e, a) in p.spans() {
            if s == e {
                continue;
            }
            let attrs = doc.attrs.get(a as AttrId).clone();
            let props = rpr(&attrs, &self.styles);
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
                    OBJECT_CHAR if attrs.note.is_some() => {
                        flush(&mut text, &mut body);
                        let id = attrs.note.expect("checked");
                        if let Some(&(kind, n)) = self.notes.get(&id) {
                            let tag = if kind == NoteKind::Foot { "footnoteReference" } else { "endnoteReference" };
                            let _ = write!(body, r#"<w:r><w:rPr><w:vertAlign w:val="superscript"/></w:rPr><w:{tag} w:id="{n}"/></w:r>"#);
                        }
                    }
                    OBJECT_CHAR => {
                        flush(&mut text, &mut body);
                        if let Some(o) = attrs.object {
                            if let Some(rid) = self.image(doc, o.image) {
                                self.pic += 1;
                                let (cx, cy) = (o.width as u64 * 635, o.height as u64 * 635);
                                let n = self.pic;
                                let _ = write!(body, r#"<w:r>{props}<w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="{cx}" cy="{cy}"/><wp:docPr id="{n}" name="Picture {n}"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><pic:pic><pic:nvPicPr><pic:cNvPr id="{n}" name="Picture {n}"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="{rid}"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r>"#);
                            }
                        }
                    }
                    c => text.push(c),
                }
            }
            flush(&mut text, &mut body);
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
    }

    fn para(&mut self, doc: &TextDoc, b: usize, out: &mut String) {
        let Some(p) = doc.para(b) else { return };
        self.para_of(doc, p, out, "");
    }

    /// Paragraph `p`; `lead` goes before its runs (a note's number).
    fn para_of(&mut self, doc: &TextDoc, p: &Para, out: &mut String, lead: &str) {
        let mut pp = String::new();
        if p.style != 0 {
            if let Some(id) = self.styles.get(p.style as usize) {
                let _ = write!(pp, r#"<w:pStyle w:val="{}"/>"#, esc(id));
            }
        }
        match p.props.list {
            Some(Some(l)) => {
                let n = self.lists.len() as u32 + 1;
                let (num, _) = *self.lists.entry((l.id, l.kind)).or_insert((n, l.kind));
                let _ = write!(pp, r#"<w:numPr><w:ilvl w:val="{}"/><w:numId w:val="{num}"/></w:numPr>"#, l.level.min(8));
            }
            Some(None) => pp += r#"<w:numPr><w:ilvl w:val="0"/><w:numId w:val="0"/></w:numPr>"#,
            None => {}
        }
        pp += &ppr_body(&p.props);
        // The paragraph mark takes the formatting of an empty paragraph's run.
        if p.text.is_empty() {
            pp += &rpr(doc.attrs.get(p.attr_at(0)), &self.styles);
        }
        out.push_str("<w:p>");
        if !pp.is_empty() {
            let _ = write!(out, "<w:pPr>{pp}</w:pPr>");
        }
        out.push_str(lead);
        self.runs(doc, p, out);
        out.push_str("</w:p>");
    }

    /// The table starting at block `b`; returns the block after it.
    fn table(&mut self, doc: &TextDoc, b: usize, out: &mut String) -> usize {
        let t = doc.cell(b).map(|c| c.table);
        let mut end = b;
        while end < doc.blocks.len() && doc.cell(end).map(|c| c.table) == t {
            end += 1;
        }
        let first_row = doc.row_range(b);
        let grid: Vec<u32> = cells_of(doc, first_row).into_iter().map(|c| doc.cell(c.start).map_or(1440, |x| x.width)).collect();
        out.push_str(r#"<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/><w:tblBorders>"#);
        for side in ["top", "left", "bottom", "right", "insideH", "insideV"] {
            let _ = write!(out, r#"<w:{side} w:val="single" w:sz="4" w:space="0" w:color="8C9199"/>"#);
        }
        out.push_str(r#"</w:tblBorders><w:tblLayout w:type="fixed"/><w:tblCellMar><w:left w:w="80" w:type="dxa"/><w:right w:w="80" w:type="dxa"/></w:tblCellMar></w:tblPr><w:tblGrid>"#);
        for w in &grid {
            let _ = write!(out, r#"<w:gridCol w:w="{w}"/>"#);
        }
        out.push_str("</w:tblGrid>");
        let mut k = b;
        while k < end {
            let row = doc.row_range(k);
            let row = k..row.end.max(k + 1).min(end);
            out.push_str("<w:tr>");
            for cell in cells_of(doc, row.clone()) {
                let w = doc.cell(cell.start).map_or(1440, |c| c.width);
                let _ = write!(out, r#"<w:tc><w:tcPr><w:tcW w:w="{w}" w:type="dxa"/></w:tcPr>"#);
                for x in cell {
                    self.para(doc, x, out);
                }
                out.push_str("</w:tc>");
            }
            out.push_str("</w:tr>");
            k = row.end;
        }
        out.push_str("</w:tbl>");
        end
    }
}

/// The cells of a row as block ranges.
fn cells_of(doc: &TextDoc, row: std::ops::Range<usize>) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for k in row {
        match out.last_mut() {
            Some(r) if doc.same_cell(r.start, k) => r.end = k + 1,
            _ => out.push(k..k + 1),
        }
    }
    out
}

fn image_ext(b: &[u8]) -> &'static str {
    if b.starts_with(&[0xFF, 0xD8]) {
        "jpeg"
    } else if b.starts_with(b"GIF8") {
        "gif"
    } else if b.starts_with(b"RIFF") {
        "webp"
    } else {
        "png"
    }
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

fn styles_part(doc: &TextDoc, ids: &[String]) -> String {
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
            let body = ppr_body(&st.para);
            if !body.is_empty() {
                let _ = write!(s, "<w:pPr>{body}</w:pPr>");
            }
        }
        let mut chars = st.chars.clone();
        chars.style = None;
        s += &rpr(&chars, ids);
        s += "</w:style>";
    }
    s += "</w:styles>";
    s
}

fn numbering_part(lists: &HashMap<(u32, ListKind), (u32, ListKind)>) -> String {
    let mut s = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:numbering {W_NS}>"#);
    let mut v: Vec<(u32, ListKind)> = lists.values().copied().collect();
    v.sort_by_key(|x| x.0);
    for &(n, kind) in &v {
        let _ = write!(s, r#"<w:abstractNum w:abstractNumId="{n}"><w:multiLevelType w:val="hybridMultilevel"/>"#);
        for l in 0..9u32 {
            let (fmt, text) = match kind {
                ListKind::Bullet => ("bullet", ["•", "◦", "▪"][l as usize % 3].to_string()),
                ListKind::Numbered => (["decimal", "lowerLetter", "lowerRoman"][l as usize % 3], format!("%{}.", l + 1)),
            };
            let _ = write!(s, r#"<w:lvl w:ilvl="{l}"><w:start w:val="1"/><w:numFmt w:val="{fmt}"/><w:lvlText w:val="{text}"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="{}" w:hanging="360"/></w:pPr></w:lvl>"#, 720 * (l + 1));
        }
        s += "</w:abstractNum>";
    }
    for &(n, _) in &v {
        let _ = write!(s, r#"<w:num w:numId="{n}"><w:abstractNumId w:val="{n}"/></w:num>"#);
    }
    s += "</w:numbering>";
    s
}

/// The document as a `.docx`.
pub fn write<W: io::Write>(doc: &TextDoc, out: W) -> io::Result<W> {
    let ids: Vec<String> = doc.styles.styles.iter().enumerate().map(|(i, s)| style_id(&s.name, i)).collect();
    // Notes are numbered in the file in anchor order, each kind from 1.
    let notes: HashMap<NoteId, (NoteKind, u32)> = doc.note_numbers().iter().map(|(&id, &k)| (id, k)).collect();
    let mut o = Out { rels: Vec::new(), media: Vec::new(), image_rel: HashMap::new(), link_rel: HashMap::new(), lists: HashMap::new(), styles: ids.clone(), pic: 0, notes: notes.clone(), media_stem: "image" };
    let mut body = String::new();
    let mut b = 0;
    let n = doc.blocks.len();
    while b < n {
        if doc.cell(b).is_some() {
            b = o.table(doc, b, &mut body);
            // Word needs a paragraph between two tables and after a last one.
            if b >= n || doc.cell(b).is_some() {
                body.push_str("<w:p/>");
            }
        } else {
            o.para(doc, b, &mut body);
            b += 1;
        }
    }
    let mut sect = String::from("<w:sectPr>");
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
        let mut no = Out { rels: Vec::new(), media: Vec::new(), image_rel: HashMap::new(), link_rel: HashMap::new(), lists: HashMap::new(), styles: ids.clone(), pic: 1000 * (note_parts.len() as u32 + 1), notes: HashMap::new(), media_stem: if kind == NoteKind::Foot { "fn_image" } else { "en_image" } };
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
                no.para_of(doc, &q, &mut x, &lead);
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
    for (hf, tag, kind) in [(&doc.header, "hdr", "header"), (&doc.footer, "ftr", "footer")] {
        if let Some(hf) = hf {
            let name = format!("{kind}1.xml");
            let r = o.rel(kind, name.clone(), false);
            let _ = write!(sect, r#"<w:{kind}Reference w:type="default" r:id="{r}"/>"#);
            parts.push((format!("word/{name}"), margin_part(hf, tag)));
        }
    }
    let g = &doc.page;
    let _ = write!(sect, r#"<w:pgSz w:w="{}" w:h="{}"/><w:pgMar w:top="{}" w:right="{}" w:bottom="{}" w:left="{}" w:header="{h}" w:footer="{h}" w:gutter="0"/></w:sectPr>"#, g.width, g.height, g.margin_top, g.margin_end, g.margin_bottom, g.margin_start, h = HEADER_FROM_EDGE);
    let document = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {W_NS} {DRAW_NS}><w:body>{body}{sect}</w:body></w:document>"#);

    let mut z = ZipWriter::new(out);
    let mut types = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Default Extension="jpeg" ContentType="image/jpeg"/><Default Extension="gif" ContentType="image/gif"/><Default Extension="webp" ContentType="image/webp"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>"#);
    if !o.lists.is_empty() {
        types += r#"<Override PartName="/word/numbering.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml"/>"#;
    }
    for (part, _, _) in &note_parts {
        let _ = write!(types, r#"<Override PartName="/word/{part}.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.{part}+xml"/>"#);
    }
    for (name, _) in &parts {
        let kind = if name.contains("header") { "header" } else { "footer" };
        let _ = write!(types, r#"<Override PartName="/{name}" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.{kind}+xml"/>"#);
    }
    types += "</Types>";
    z.add("[Content_Types].xml", types.as_bytes())?;
    z.add("_rels/.rels", format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/officeDocument" Target="word/document.xml"/></Relationships>"#).as_bytes())?;
    z.add("word/document.xml", document.as_bytes())?;
    z.add("word/styles.xml", styles_part(doc, &ids).as_bytes())?;
    let mut rels = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="{REL}/styles" Target="styles.xml"/>"#);
    if !o.lists.is_empty() {
        z.add("word/numbering.xml", numbering_part(&o.lists).as_bytes())?;
        let _ = write!(rels, r#"<Relationship Id="rId2" Type="{REL}/numbering" Target="numbering.xml"/>"#);
    }
    for (id, kind, target, external) in &o.rels {
        let mode = if *external { r#" TargetMode="External""# } else { "" };
        let _ = write!(rels, r#"<Relationship Id="{id}" Type="{REL}/{kind}" Target="{}"{mode}/>"#, esc(target));
    }
    rels += "</Relationships>";
    z.add("word/_rels/document.xml.rels", rels.as_bytes())?;
    for (name, xml) in &parts {
        z.add(name, xml.as_bytes())?;
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
