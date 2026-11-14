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

use officina_text::model::{Align, CharAttrs, CommentId, HeaderFooter, LineSpacing, ListKind, NoteKind, Para, ParaProps, StyleKind, TextDoc, VertAlign, HEADER_FROM_EDGE, OBJECT_CHAR};

use crate::xml::esc;
use crate::zip::ZipWriter;

const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" office:version="1.3""#;

/// Twips as points.
fn pt(t: i64) -> String {
    let v = t as f64 / 20.0;
    format!("{}pt", (v * 100.0).round() / 100.0)
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

fn text_props(a: &CharAttrs, fonts: &mut Vec<String>) -> String {
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
    if let Some(v) = a.strike {
        let _ = write!(s, r#" style:text-line-through-style="{}""#, if v { "solid" } else { "none" });
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
    if s.is_empty() {
        s
    } else {
        format!("<style:text-properties{s}/>")
    }
}

fn para_props(p: &ParaProps) -> String {
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
    if let Some(v) = p.page_break_before {
        let _ = write!(s, r#" fo:break-before="{}""#, if v { "page" } else { "auto" });
    }
    if let Some(v) = p.widows {
        let n = if v { 2 } else { 0 };
        let _ = write!(s, r#" fo:widows="{n}" fo:orphans="{n}""#);
    }
    if s.is_empty() {
        s
    } else {
        format!("<style:paragraph-properties{s}/>")
    }
}

/// What a paragraph's automatic style holds: its direct properties and the
/// formatting of an empty paragraph.
fn para_key(p: &ParaProps) -> ParaProps {
    ParaProps { list: None, cell: None, outline_level: None, ..p.clone() }
}

struct Out<'a> {
    doc: &'a TextDoc,
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
}

/// The name an annotation goes by.
fn annotation_name(id: CommentId) -> String {
    format!("__Annotation__{id}")
}

impl Out<'_> {
    fn para_style(&mut self, style: u16, p: &ParaProps) -> String {
        let key = (style, para_key(p));
        if let Some(n) = self.para_styles.get(&key) {
            return n.clone();
        }
        let body = para_props(&key.1);
        if body.is_empty() {
            return self.names.get(style as usize).cloned().unwrap_or_else(|| "Standard".into());
        }
        let n = format!("P{}", self.para_styles.len() + 1);
        let parent = self.names.get(style as usize).cloned().unwrap_or_else(|| "Standard".into());
        let _ = write!(self.auto, r#"<style:style style:name="{n}" style:family="paragraph" style:parent-style-name="{}">{body}</style:style>"#, esc(&parent));
        self.para_styles.insert(key, n.clone());
        n
    }

    fn text_style(&mut self, a: &CharAttrs) -> Option<String> {
        let mut k = a.clone();
        k.link = None;
        k.object = None;
        k.note = None;
        if k == CharAttrs::default() {
            return None;
        }
        if let Some(n) = self.text_styles.get(&k) {
            return Some(n.clone());
        }
        let n = format!("T{}", self.text_styles.len() + 1);
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
        for l in 1..=10u32 {
            let ind = format!(r#"<style:list-level-properties text:list-level-position-and-space-mode="label-alignment"><style:list-level-label-alignment text:label-followed-by="listtab" text:list-tab-stop-position="{p}" fo:text-indent="-18pt" fo:margin-left="{p}"/></style:list-level-properties>"#, p = pt(720 * l as i64));
            match kind {
                ListKind::Bullet => {
                    let c = ["•", "◦", "▪"][(l as usize - 1) % 3];
                    let _ = write!(self.auto, r#"<text:list-level-style-bullet text:level="{l}" text:bullet-char="{c}">{ind}</text:list-level-style-bullet>"#);
                }
                ListKind::Numbered => {
                    let f = ["1", "a", "i"][(l as usize - 1) % 3];
                    let _ = write!(self.auto, r#"<text:list-level-style-number text:level="{l}" style:num-suffix="." style:num-format="{f}">{ind}</text:list-level-style-number>"#);
                }
            }
        }
        self.auto += "</text:list-style>";
        self.list_styles.insert((id, kind), n.clone());
        n
    }

    fn image(&mut self, i: u32) -> Option<String> {
        if let Some(n) = self.image_names.get(&i) {
            return Some(n.clone());
        }
        let img = self.doc.images.get(i)?;
        let b = &img.bytes;
        let ext = if b.starts_with(&[0xFF, 0xD8]) { "jpg" } else if b.starts_with(b"GIF8") { "gif" } else if b.starts_with(b"RIFF") { "webp" } else { "png" };
        let n = format!("Pictures/image{}.{ext}", self.media.len() + 1);
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

    fn para(&mut self, b: usize, out: &mut String) {
        let doc = self.doc;
        let Some(p) = doc.para(b) else { return };
        self.cur = Some(b);
        self.para_of(p, out);
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
        let st = self.para_style(p.style, &props);
        let level = doc.styles.para_format(p.style, &p.props).0.outline_level.filter(|_| doc.styles.styles.get(p.style as usize).is_some_and(|s| s.name.starts_with("Heading")));
        let tag = if level.is_some() { "text:h" } else { "text:p" };
        let lvl = level.map(|l| format!(r#" text:outline-level="{l}""#)).unwrap_or_default();
        let _ = write!(out, r#"<{tag} text:style-name="{}"{lvl}>"#, esc(&st));
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
            let mut inner = String::new();
            let piece = &p.text[s as usize..e as usize];
            let mut from = 0;
            for (i, c) in piece.char_indices().chain(std::iter::once((piece.len(), '\0'))) {
                if c != OBJECT_CHAR && i < piece.len() {
                    continue;
                }
                Self::text(&piece[from..i], &mut inner);
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
                    if let Some(o) = attrs.object {
                        if let Some(href) = self.image(o.image) {
                            self.frames += 1;
                            let _ = write!(inner, r#"<draw:frame draw:name="Picture {}" text:anchor-type="as-char" svg:width="{}" svg:height="{}" draw:z-index="0"><draw:image xlink:href="{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame>"#, self.frames, pt(o.width as i64), pt(o.height as i64), esc(&href));
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
        }
        for &(_, end, id) in &marks[m..] {
            self.annotation(out, end, id);
        }
        let _ = write!(out, "</{tag}>");
    }

    /// A run of list items from `b`; returns the block after it.
    fn list(&mut self, b: usize, end: usize, out: &mut String) -> usize {
        let doc = self.doc;
        let first = doc.para(b).and_then(|p| p.props.list.flatten()).expect("a list item");
        let style = self.list_style(first.id, first.kind);
        let cont = if self.lists_seen.contains(&first.id) { r#" text:continue-numbering="true""# } else { "" };
        self.lists_seen.push(first.id);
        let _ = write!(out, r#"<text:list text:style-name="{style}"{cont}>"#);
        let mut open: Vec<bool> = vec![false];
        let mut k = b;
        while k < end {
            let Some(l) = doc.para(k).and_then(|p| p.props.list.flatten()).filter(|l| l.id == first.id && l.kind == first.kind) else { break };
            let lv = l.level as usize;
            while open.len() - 1 > lv {
                if open.pop() == Some(true) {
                    out.push_str("</text:list-item>");
                }
                out.push_str("</text:list>");
            }
            if open.len() - 1 == lv {
                if open[lv] {
                    out.push_str("</text:list-item>");
                }
                out.push_str("<text:list-item>");
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
        let doc = self.doc;
        let t = doc.cell(b).map(|c| c.table);
        let mut end = b;
        while end < doc.blocks.len() && doc.cell(end).map(|c| c.table) == t {
            end += 1;
        }
        self.tables += 1;
        let name = format!("Table{}", self.tables);
        let first = cells_of(doc, doc.row_range(b));
        let widths: Vec<u32> = first.iter().map(|c| doc.cell(c.start).map_or(1440, |x| x.width)).collect();
        let total: u32 = widths.iter().sum();
        let _ = write!(self.auto, r#"<style:style style:name="{name}" style:family="table"><style:table-properties style:width="{}" table:align="left"/></style:style>"#, pt(total as i64));
        for (i, w) in widths.iter().enumerate() {
            let _ = write!(self.auto, r#"<style:style style:name="{name}.C{i}" style:family="table-column"><style:table-column-properties style:column-width="{}"/></style:style>"#, pt(*w as i64));
        }
        let cell_style = format!("{name}.A1");
        let _ = write!(self.auto, r#"<style:style style:name="{cell_style}" style:family="table-cell"><style:table-cell-properties fo:padding="4pt" fo:border="0.5pt solid #8c9199"/></style:style>"#);
        let _ = write!(out, r#"<table:table table:name="{name}" table:style-name="{name}">"#);
        for i in 0..widths.len() {
            let _ = write!(out, r#"<table:table-column table:style-name="{name}.C{i}"/>"#);
        }
        let mut k = b;
        while k < end {
            let row = doc.row_range(k);
            let row = k..row.end.max(k + 1).min(end);
            out.push_str("<table:table-row>");
            for cell in cells_of(doc, row.clone()) {
                let _ = write!(out, r#"<table:table-cell table:style-name="{cell_style}" office:value-type="string">"#);
                for x in cell {
                    self.para(x, out);
                }
                out.push_str("</table:table-cell>");
            }
            out.push_str("</table:table-row>");
            k = row.end;
        }
        out.push_str("</table:table>");
        end
    }
}

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
    let named: Vec<(String, String)> = doc.styles.styles.iter().enumerate().map(|(i, s)| style_name(&s.name, i)).collect();
    let mut o = Out { doc, names: named.iter().map(|n| n.0.clone()).collect(), fonts: vec![doc.styles.default_char.family.to_string()], auto: String::new(), para_styles: HashMap::new(), text_styles: HashMap::new(), list_styles: HashMap::new(), lists_seen: Vec::new(), media: Vec::new(), image_names: HashMap::new(), tables: 0, frames: 0, in_note: 0, marks: HashMap::new(), cur: None };
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
    let mut b = 0;
    while b < n {
        if doc.cell(b).is_some() {
            b = o.table(b, &mut body);
        } else if doc.para(b).and_then(|p| p.props.list.flatten()).is_some() {
            b = o.list(b, n, &mut body);
        } else {
            o.para(b, &mut body);
            b += 1;
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
        let pp = if st.kind == StyleKind::Para { para_props(&st.para) } else { String::new() };
        let mut chars = st.chars.clone();
        chars.style = None;
        let tp = text_props(&chars, &mut o.fonts);
        let _ = write!(styles, r#"<style:style style:name="{}" style:display-name="{}" style:family="{family}"{parent}{next}{outline}>{pp}{tp}</style:style>"#, esc(name), esc(display));
    }
    let fonts: String = o.fonts.iter().map(|f| format!(r#"<style:font-face style:name="{0}" svg:font-family="'{0}'"/>"#, esc(f))).collect();
    let dc = &doc.styles.default_char;
    let defaults = format!(r#"<style:default-style style:family="paragraph"><style:paragraph-properties fo:margin-bottom="8pt" fo:line-height="115%"/><style:text-properties style:font-name="{}" fo:font-size="{}pt"/></style:default-style>"#, esc(&dc.family), dc.size as f64 / 2.0);

    // The page: ODF's header and footer sit inside the margins, so the page
    // margin is their distance from the edge and their height is the rest.
    let g = &doc.page;
    let mut hf_styles = String::new();
    let mut margins = (g.margin_top, g.margin_bottom);
    for (hf, tag, top) in [(&doc.header, "header-style", true), (&doc.footer, "footer-style", false)] {
        if hf.is_some() {
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
    let layout = format!(r#"<style:page-layout style:name="pm1"><style:page-layout-properties fo:page-width="{}" fo:page-height="{}" fo:margin-top="{}" fo:margin-bottom="{}" fo:margin-left="{}" fo:margin-right="{}" style:print-orientation="{}"/>{hf_styles}</style:page-layout>"#, pt(g.width as i64), pt(g.height as i64), pt(margins.0 as i64), pt(margins.1 as i64), pt(g.margin_start as i64), pt(g.margin_end as i64), if g.width > g.height { "landscape" } else { "portrait" });
    let mut mp_styles = String::new();
    let mut master = String::from(r#"<style:master-page style:name="Standard" style:page-layout-name="pm1">"#);
    for (i, (hf, tag)) in [(&doc.header, "header"), (&doc.footer, "footer")].into_iter().enumerate() {
        if let Some(hf) = hf {
            let sn = format!("MP{}", i + 1);
            let align = match hf.align {
                Align::Start => "start",
                Align::Center => "center",
                Align::End => "end",
                Align::Justify => "justify",
            };
            let _ = write!(mp_styles, r#"<style:style style:name="{sn}" style:family="paragraph" style:parent-style-name="Standard"><style:paragraph-properties fo:text-align="{align}" fo:margin-top="0pt" fo:margin-bottom="0pt"/></style:style>"#);
            master += &margin(hf, tag, &sn);
        }
    }
    master += "</style:master-page>";
    let styles_xml = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS}><office:font-face-decls>{fonts}</office:font-face-decls><office:styles>{defaults}{styles}</office:styles><office:automatic-styles>{layout}{mp_styles}</office:automatic-styles><office:master-styles>{master}</office:master-styles></office:document-styles>"#);
    let fonts: String = o.fonts.iter().map(|f| format!(r#"<style:font-face style:name="{0}" svg:font-family="'{0}'"/>"#, esc(f))).collect();
    let content = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:font-face-decls>{fonts}</office:font-face-decls><office:automatic-styles>{}</office:automatic-styles><office:body><office:text>{body}</office:text></office:body></office:document-content>"#, o.auto);

    let mut z = ZipWriter::new(out);
    z.add_stored("mimetype", b"application/vnd.oasis.opendocument.text")?;
    let mut manifest = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>"#);
    for (name, _) in &o.media {
        let mt = if name.ends_with(".jpg") { "image/jpeg" } else if name.ends_with(".gif") { "image/gif" } else if name.ends_with(".webp") { "image/webp" } else { "image/png" };
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
