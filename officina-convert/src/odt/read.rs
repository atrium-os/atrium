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
//! Reported, not carried: notes inside notes or headers, annotations there, tracked changes,
//! sections and columns, frames anchored to the page (kept in line), merged
//! cells, nested tables (their text kept), fields other than page numbers (their
//! text kept), drawings and charts, embedded objects.

use std::collections::HashMap;
use std::sync::Arc;

use officina_text::model::{Align, Block, CellRef, CharAttrs, Comment, CommentId, MAX_COMMENT_TEXT, HeaderFooter, ImageData, LineSpacing, ListKind, ListRef, Note, NoteId, NoteKind, ObjectRef, ParaProps, Style, StyleId, StyleKind, TextDoc, VertAlign, MAX_LIST_LEVEL, MAX_NOTE_PARAS, MAX_OBJECT_TWIPS, MAX_STYLES, OBJECT_CHAR};

use crate::docx::read::pixel_size;
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
fn text_props(e: &El, fonts: &HashMap<String, String>) -> CharAttrs {
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
    }
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
    a
}

/// Paragraph properties (`style:paragraph-properties`).
fn para_props(e: &El) -> ParaProps {
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
    if let Some(b) = e.attr("fo:break-before") {
        p.page_break_before = Some(b == "page");
    }
    if let Some(w) = e.attr("fo:widows").or_else(|| e.attr("fo:orphans")) {
        p.widows = Some(w.trim().parse::<i32>().unwrap_or(0) > 0);
    }
    p
}

/// A style as the file has it.
#[derive(Clone, Default)]
struct OStyle {
    parent: Option<String>,
    para: ParaProps,
    chars: CharAttrs,
    list_style: Option<String>,
    outline: Option<u8>,
}

fn props_over(over: &ParaProps, under: &ParaProps) -> ParaProps {
    ParaProps {
        align: over.align.or(under.align),
        indent_start: over.indent_start.or(under.indent_start),
        indent_end: over.indent_end.or(under.indent_end),
        first_line: over.first_line.or(under.first_line),
        space_before: over.space_before.or(under.space_before),
        space_after: over.space_after.or(under.space_after),
        line_spacing: over.line_spacing.or(under.line_spacing),
        keep_with_next: over.keep_with_next.or(under.keep_with_next),
        keep_together: over.keep_together.or(under.keep_together),
        page_break_before: over.page_break_before.or(under.page_break_before),
        widows: over.widows.or(under.widows),
        outline_level: over.outline_level.or(under.outline_level),
        list: over.list.or(under.list),
        cell: over.cell.or(under.cell),
    }
}

struct Reader<'a> {
    archive: &'a Archive<'a>,
    doc: TextDoc,
    rep: Report,
    fonts: HashMap<String, String>,
    /// Paragraph and text styles by name, named and automatic.
    styles: HashMap<String, OStyle>,
    /// Our style for a named paragraph or text style.
    ours: HashMap<String, StyleId>,
    /// List styles: kind per level.
    lists: HashMap<String, Vec<ListKind>>,
    /// Column widths by column style name.
    columns: HashMap<String, u32>,
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
    /// Notes anchored in the text, read after it.
    pending: Vec<(NoteId, NoteKind, El)>,
    in_note: bool,
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
}

impl Para {
    fn push(&mut self, s: &str, a: &CharAttrs) {
        if s.is_empty() {
            return;
        }
        let at = self.text.len() as u32;
        if self.runs.last().map(|r| &r.1) != Some(a) {
            self.runs.push((at, a.clone()));
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
                "style:style" => {
                    let Some(name) = s.attr("style:name") else { continue };
                    match s.attr("style:family") {
                        Some("paragraph") | Some("text") => {
                            let st = OStyle {
                                parent: s.attr("style:parent-style-name").map(str::to_string),
                                para: s.child("style:paragraph-properties").map(para_props).unwrap_or_default(),
                                chars: s.child("style:text-properties").map(|t| text_props(t, &self.fonts)).unwrap_or_default(),
                                list_style: s.attr("style:list-style-name").map(str::to_string),
                                outline: s.attr("style:default-outline-level").and_then(|v| v.parse::<u8>().ok()).filter(|v| (1..=9).contains(v)),
                            };
                            self.styles.insert(name.to_string(), st);
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
                    for l in s.els() {
                        let lv = l.attr("text:level").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, MAX_LIST_LEVEL as usize + 1) - 1;
                        kinds[lv] = if l.name == "text:list-level-style-bullet" || l.name == "text:list-level-style-image" { ListKind::Bullet } else { ListKind::Numbered };
                    }
                    self.lists.insert(name.to_string(), kinds);
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
        let (dp, dc) = defaults.map(|d| (d.child("style:paragraph-properties").map(para_props).unwrap_or_default(), d.child("style:text-properties").map(|t| text_props(t, &self.fonts)).unwrap_or_default())).unwrap_or_default();
        for (name, _, _) in &named {
            let Some(&sid) = self.ours.get(name) else { continue };
            let Some(o) = self.styles.get(name).cloned() else { continue };
            let mut para = o.para.clone();
            let mut chars = o.chars.clone();
            if sid == 0 {
                para = props_over(&para, &dp);
                chars = chars.over(&dc);
            }
            para.outline_level = o.outline.or(self.doc.styles.styles[sid as usize].para.outline_level);
            if let Some(ls) = &o.list_style {
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
                (sid, o.para.clone(), o.chars.clone())
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
                let Some((w, h)) = pixel_size(&bytes) else {
                    self.rep.note_at(Kind::Dropped, "pictures in formats not shown (EMF, WMF, TIFF, SVG)", || href.clone());
                    return None;
                };
                let i = self.doc.images.add(ImageData { bytes: Arc::from(bytes), width: w, height: h });
                self.images.insert(href.clone(), i);
                i
            }
        };
        if !matches!(frame.attr("text:anchor-type"), Some("as-char") | Some("char") | None) {
            self.rep.note(Kind::Approximate, "pictures anchored to the paragraph or page (kept in line)");
        }
        let px = self.doc.images.get(image).map(|d| (d.width, d.height)).unwrap_or((96, 96));
        let side = |v: i64| v.clamp(1, MAX_OBJECT_TWIPS as i64) as u32;
        let w = frame.attr("svg:width").and_then(length).map(side).unwrap_or(side(px.0 as i64 * 15));
        let h = frame.attr("svg:height").and_then(length).map(side).unwrap_or(side(px.1 as i64 * 15));
        self.pictures += 1;
        Some(ObjectRef { image, width: w, height: h })
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
                    "text:change" | "text:change-start" | "text:change-end" => self.rep.note(Kind::Approximate, "tracked changes (shown as they stand)"),
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

    /// A `text:p` or `text:h` into the document.
    fn para(&mut self, e: &El, cell: Option<CellRef>, list: Option<Option<ListRef>>) {
        let (style, mut props, chars) = self.para_style(e.attr("text:style-name"));
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
        let outer = std::mem::take(&mut self.events);
        self.inline(e, &mut p, &chars, Where::Body);
        let events = std::mem::replace(&mut self.events, outer);
        apply_comments(&mut p, &mut self.active, events);
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
            for x in item.els() {
                match x.name.as_str() {
                    "text:p" | "text:h" => {
                        let l = if item.name == "text:list-header" { None } else { Some(Some(ListRef { id, kind, level: level.min(MAX_LIST_LEVEL) })) };
                        self.para(x, cell, l);
                    }
                    "text:list" => self.list(x, level.saturating_add(1), style.clone(), Some(id), cell),
                    "table:table" => self.table(x),
                    _ => {}
                }
            }
        }
    }

    fn table(&mut self, t: &El) {
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
        let mut rows: Vec<&El> = Vec::new();
        for x in t.els() {
            match x.name.as_str() {
                "table:table-row" => rows.push(x),
                "table:table-header-rows" | "table:table-rows" | "table:table-row-group" => rows.extend(x.children("table:table-row")),
                _ => {}
            }
        }
        for (r, tr) in rows.into_iter().enumerate() {
            let mut col = 0usize;
            let mut c = 0u16;
            for tc in tr.els() {
                if tc.name == "table:covered-table-cell" {
                    col += 1;
                    continue;
                }
                if tc.name != "table:table-cell" {
                    continue;
                }
                let span = tc.attr("table:number-columns-spanned").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 63);
                if span > 1 || tc.attr("table:number-rows-spanned").is_some_and(|v| v != "1") {
                    self.rep.note(Kind::Approximate, "merged cells (kept as one wide cell)");
                }
                let width = (col..col + span).map(|g| widths.get(g).copied().unwrap_or(1440)).sum::<u32>().clamp(1, MAX_OBJECT_TWIPS);
                col += span;
                let cell = CellRef { table, row: r as u32, col: c, width };
                c = c.saturating_add(1);
                let before = self.doc.blocks.len();
                self.cell_content(tc, cell, 0);
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
                "table:table" if depth < 8 => {
                    self.rep.note(Kind::Approximate, "nested tables (their text kept in the cell)");
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
                    self.rep.note(Kind::Approximate, "sections (their content kept)");
                    self.body(x);
                }
                "text:table-of-content" | "text:alphabetical-index" | "text:illustration-index" | "text:table-index" | "text:bibliography" | "text:user-index" | "text:object-index" => {
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
                "text:sequence-decls" | "text:variable-decls" | "text:user-field-decls" | "office:forms" | "text:tracked-changes" | "text:soft-page-break" => {
                    if x.name == "text:tracked-changes" && !x.kids.is_empty() {
                        self.rep.note(Kind::Approximate, "tracked changes (shown as they stand)");
                    }
                }
                _ => {}
            }
        }
    }

    /// Page size, margins, header and footer from the master page the body uses.
    fn page(&mut self, styles: Option<&El>, first_master: Option<String>) {
        let Some(st) = styles else { return };
        let masters = st.child("office:master-styles");
        let master = masters.and_then(|m| {
            let all: Vec<&El> = m.children("style:master-page").collect();
            first_master.as_ref().and_then(|n| all.iter().find(|x| x.attr("style:name") == Some(n.as_str())).copied()).or_else(|| all.iter().find(|x| x.attr("style:name") == Some("Standard")).copied()).or(all.first().copied())
        });
        let Some(master) = master else { return };
        let layout = master.attr("style:page-layout-name").and_then(|n| {
            st.child("office:automatic-styles").and_then(|a| a.children("style:page-layout").find(|l| l.attr("style:name") == Some(n)))
        });
        if let Some(pp) = layout.and_then(|l| l.child("style:page-layout-properties")) {
            let g = &mut self.doc.page;
            let t = |k: &str| pp.attr(k).and_then(length).map(|v| v.clamp(0, 31680 * 2) as i32);
            g.width = t("fo:page-width").unwrap_or(g.width).max(1440);
            g.height = t("fo:page-height").unwrap_or(g.height).max(1440);
            g.margin_top = t("fo:margin-top").unwrap_or(g.margin_top);
            g.margin_bottom = t("fo:margin-bottom").unwrap_or(g.margin_bottom);
            g.margin_start = t("fo:margin-left").unwrap_or(g.margin_start);
            g.margin_end = t("fo:margin-right").unwrap_or(g.margin_end);
            if pp.child("style:columns").and_then(|c| c.attr("fo:column-count")).and_then(|v| v.parse::<u32>().ok()).is_some_and(|n| n > 1) {
                self.rep.note(Kind::Approximate, "columns (one column)");
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
        for (tag, header) in [("style:header", true), ("style:footer", false)] {
            let Some(h) = master.child(tag) else { continue };
            if h.attr("style:display") == Some("false") {
                continue;
            }
            if master.child(&format!("{tag}-left")).is_some() || master.child(&format!("{tag}-first")).is_some() {
                self.rep.note(Kind::Approximate, "first-page and even-page headers and footers (the default one kept)");
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
                    self.rep.note(Kind::Approximate, "headers and footers of more than one line (joined)");
                }
                text.push_str(q.text.replace(['\t', '\u{2028}'], " ").trim());
            }
            if !text.is_empty() {
                let hf = HeaderFooter { text: text.chars().take(1000).collect(), align };
                if header {
                    self.doc.header = Some(hf);
                } else {
                    self.doc.footer = Some(hf);
                }
            }
        }
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
    let mut r = Reader { archive: &archive, doc, rep: Report::default(), fonts: HashMap::new(), styles: HashMap::new(), ours: HashMap::new(), lists: HashMap::new(), columns: HashMap::new(), last_list: HashMap::new(), next_list: 0, next_table: 1, images: HashMap::new(), pictures: 0, boxes: Vec::new(), box_depth: 0, pending: Vec::new(), in_note: false, annotations: HashMap::new(), reply_to: Vec::new(), ends: annotation_ends(text), active: Vec::new(), events: Vec::new() };
    r.font_decls(styles.as_ref());
    r.font_decls(Some(&content));
    for s in [styles.as_ref().and_then(|s| s.child("office:styles")), styles.as_ref().and_then(|s| s.child("office:automatic-styles")), content.child("office:automatic-styles")].into_iter().flatten() {
        r.style_block(s);
    }
    r.named_styles(styles.as_ref());
    // Lists in named styles took ids; a body list starts its own.
    r.last_list.clear();
    // The master page the first paragraph's style names, if any.
    let first_master = text.els().find(|x| x.name == "text:p" || x.name == "text:h").and_then(|p| p.attr("text:style-name")).and_then(|n| content.child("office:automatic-styles").and_then(|a| a.children("style:style").find(|s| s.attr("style:name") == Some(n)))).and_then(|s| s.attr("style:master-page-name")).map(str::to_string);
    r.page(styles.as_ref(), first_master);
    r.body(text);
    r.read_notes();
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
