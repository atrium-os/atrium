//! `.docx` in (ECMA-376 WordprocessingML, transitional), for the converter
//! process.
//!
//! Read: paragraphs with their styles and direct properties (alignment, indents,
//! spacing, line spacing, keeps, page breaks, widow control, outline level),
//! runs (bold, italic, underline, strike, size, colour, highlight and shading,
//! font, superscript and subscript, hyperlinks), footnotes and endnotes
//! (footnotes.xml and endnotes.xml), comments with their replies and resolved
//! state (comments.xml, commentsExtended.xml), tabs and line breaks, lists (numbering.xml: bullet or
//! numbered, level), tables (rows of cells of paragraphs, widths from the grid),
//! inline pictures (their bytes and frame), paragraph and character styles with
//! inheritance and the document defaults, page size and margins, and a header and
//! footer (page number fields become `{page}` and `{pages}`).
//!
//! Reported, not carried: floating pictures (kept in line), merged cells, nested
//! tables (their text kept), notes inside notes or headers, comments without a
//! range, tracked changes
//! (insertions kept, deletions dropped), sections after the first, fields other than
//! page numbers (their shown result kept), text boxes, charts, equations, embedded
//! objects. Kept inert: macros (`.docm`, not read).

use std::collections::HashMap;
use std::sync::Arc;

use officina_text::model::{Align, Block, CellRef, CharAttrs, Comment, CommentId, MAX_COMMENTS, MAX_COMMENT_TEXT, HeaderFooter, ImageData, LineSpacing, ListKind, ListRef, Note, NoteId, NoteKind, ObjectRef, PageSetup, ParaProps, Style, StyleId, StyleKind, TextDoc, VertAlign, MAX_LIST_LEVEL, MAX_NOTES, MAX_NOTE_PARAS, MAX_OBJECT_TWIPS, MAX_STYLES, OBJECT_CHAR};

use crate::report::{Kind, Report};
use crate::xml::{self, El};
use crate::zip::Archive;

#[derive(Debug)]
pub enum DocxError {
    Zip(crate::zip::ZipError),
    Xml(String),
    Bad(String),
}

impl std::fmt::Display for DocxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocxError::Zip(e) => write!(f, "{e}"),
            DocxError::Xml(e) => write!(f, "damaged document XML: {e}"),
            DocxError::Bad(e) => write!(f, "not a document this can read: {e}"),
        }
    }
}

impl std::error::Error for DocxError {}

impl From<crate::zip::ZipError> for DocxError {
    fn from(e: crate::zip::ZipError) -> Self {
        DocxError::Zip(e)
    }
}

/// What reading found besides the document.
#[derive(Debug, Default)]
pub struct Loaded {
    pub report: Report,
    pub paragraphs: u64,
    pub pictures: u64,
}

/// Most bytes one picture may have.
pub const MAX_PICTURE: usize = 48 << 20;
/// Most pictures a document may carry.
pub const MAX_PICTURES: usize = 1 << 16;

fn part(a: &Archive, name: &str) -> Result<Option<El>, DocxError> {
    match a.read_name(name)? {
        Some(b) => xml::parse(&b).map(Some).map_err(|e| DocxError::Xml(format!("{name}: {e}"))),
        None => Ok(None),
    }
}

/// A part's relationships: id → (type's last segment, target, external).
fn rels(a: &Archive, part: &str) -> Result<HashMap<String, (String, String, bool)>, DocxError> {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let name = if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") };
    let mut out = HashMap::new();
    if let Some(r) = self::part(a, &name)? {
        for e in r.children("Relationship") {
            let (Some(id), Some(t), Some(target)) = (e.attr("Id"), e.attr("Type"), e.attr("Target")) else { continue };
            let kind = t.rsplit('/').next().unwrap_or("").to_string();
            let external = e.attr("TargetMode") == Some("External");
            let target = if external || target.starts_with('/') { target.trim_start_matches('/').to_string() } else { resolve(dir, target) };
            out.insert(id.to_string(), (kind, target, external));
        }
    }
    Ok(out)
}

/// `target` relative to `dir`, with `..` resolved.
fn resolve(dir: &str, target: &str) -> String {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn val(e: &El) -> Option<&str> {
    e.attr("w:val")
}
/// An on/off property: present and not "0"/"false"/"off".
fn on(e: &El) -> bool {
    !matches!(val(e), Some("0" | "false" | "off" | "none"))
}
fn num(s: Option<&str>) -> Option<i64> {
    s.and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite()).map(|v| v as i64)
}
fn twips(s: Option<&str>) -> Option<i32> {
    num(s).map(|v| v.clamp(-31680, 31680) as i32)
}
fn color(s: Option<&str>) -> Option<u32> {
    let s = s?;
    if s.len() != 6 {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

/// Word's highlight colour names.
fn highlight(name: &str) -> Option<u32> {
    Some(match name {
        "yellow" => 0xFFFF00,
        "green" => 0x00FF00,
        "cyan" => 0x00FFFF,
        "magenta" => 0xFF00FF,
        "blue" => 0x0000FF,
        "red" => 0xFF0000,
        "darkBlue" => 0x000080,
        "darkCyan" => 0x008080,
        "darkGreen" => 0x008000,
        "darkMagenta" => 0x800080,
        "darkRed" => 0x800000,
        "darkYellow" => 0x808000,
        "darkGray" => 0x808080,
        "lightGray" => 0xC0C0C0,
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        _ => return None,
    })
}

/// Character properties (`w:rPr`).
fn rpr(e: &El, rep: &mut Report) -> CharAttrs {
    let mut a = CharAttrs::default();
    for p in e.els() {
        match p.name.as_str() {
            "w:b" => a.bold = Some(on(p)),
            "w:i" => a.italic = Some(on(p)),
            "w:u" => a.underline = Some(on(p)),
            "w:strike" | "w:dstrike" => a.strike = Some(on(p)),
            "w:sz" => a.size = num(val(p)).map(|v| v.clamp(2, 3276) as u16),
            "w:color" => a.color = color(val(p)),
            "w:highlight" => a.highlight = val(p).and_then(highlight),
            "w:shd" => {
                if a.highlight.is_none() {
                    a.highlight = color(p.attr("w:fill"));
                }
            }
            "w:rFonts" => {
                if let Some(f) = p.attr("w:ascii").or_else(|| p.attr("w:hAnsi")) {
                    a.family = Some(Arc::from(&f[..f.len().min(255)]));
                }
            }
            "w:bCs" | "w:iCs" | "w:szCs" | "w:lang" | "w:noProof" | "w:kern" | "w:rStyle" | "w:spacing" | "w:w" | "w:position" | "w:webHidden" | "w:specVanish" | "w:rtl" | "w:cs" | "w:eastAsianLayout" | "w:effect" | "w:ligatures" | "w:numForm" | "w:numSpacing" | "w:stylisticSets" | "w:cntxtAlts" | "w:snapToGrid" | "w:fitText" => {}
            "w:vertAlign" => {
                a.vert = match val(p) {
                    Some("superscript") => Some(VertAlign::Super),
                    Some("subscript") => Some(VertAlign::Sub),
                    Some("baseline") => Some(VertAlign::Baseline),
                    _ => None,
                }
            }
            "w:caps" | "w:smallCaps" => rep.note(Kind::Approximate, "capitals and small capitals (shown as typed)"),
            "w:vanish" => rep.note(Kind::Approximate, "hidden text (shown)"),
            "w:outline" | "w:shadow" | "w:emboss" | "w:imprint" | "w:bdr" | "w:em" => rep.note(Kind::Approximate, "text effects (dropped)"),
            _ => {}
        }
    }
    a
}

/// Paragraph properties (`w:pPr`), without the style, list and section.
fn ppr(e: &El, rep: &mut Report) -> ParaProps {
    let mut p = ParaProps::default();
    for x in e.els() {
        match x.name.as_str() {
            "w:jc" => {
                p.align = match val(x) {
                    Some("left" | "start") => Some(Align::Start),
                    Some("center") => Some(Align::Center),
                    Some("right" | "end") => Some(Align::End),
                    Some("both" | "distribute" | "lowKashida" | "mediumKashida" | "highKashida" | "thaiDistribute") => Some(Align::Justify),
                    _ => None,
                }
            }
            "w:ind" => {
                p.indent_start = twips(x.attr("w:left").or_else(|| x.attr("w:start")));
                p.indent_end = twips(x.attr("w:right").or_else(|| x.attr("w:end")));
                if let Some(h) = twips(x.attr("w:hanging")) {
                    p.first_line = Some(-h);
                } else if let Some(f) = twips(x.attr("w:firstLine")) {
                    p.first_line = Some(f);
                }
            }
            "w:spacing" => {
                p.space_before = twips(x.attr("w:before")).map(|v| v.max(0));
                p.space_after = twips(x.attr("w:after")).map(|v| v.max(0));
                if let Some(line) = num(x.attr("w:line")) {
                    p.line_spacing = Some(match x.attr("w:lineRule") {
                        Some("exact") => LineSpacing::Exact(line.clamp(20, 31680) as i32),
                        Some("atLeast") => LineSpacing::AtLeast(line.clamp(0, 31680) as i32),
                        _ => LineSpacing::Multiple((line * 100 / 240).clamp(25, 1000) as u16),
                    });
                }
            }
            "w:keepNext" => p.keep_with_next = Some(on(x)),
            "w:keepLines" => p.keep_together = Some(on(x)),
            "w:pageBreakBefore" => p.page_break_before = Some(on(x)),
            "w:widowControl" => p.widows = Some(on(x)),
            "w:outlineLvl" => p.outline_level = num(val(x)).filter(|v| (0..9).contains(v)).map(|v| v as u8 + 1),
            "w:pStyle" | "w:numPr" | "w:sectPr" | "w:rPr" | "w:contextualSpacing" | "w:snapToGrid" | "w:suppressAutoHyphens" | "w:autoSpaceDE" | "w:autoSpaceDN" | "w:adjustRightInd" | "w:bidi" | "w:textAlignment" | "w:kinsoku" | "w:wordWrap" | "w:overflowPunct" | "w:topLinePunct" | "w:suppressLineNumbers" => {}
            "w:tabs" => rep.note(Kind::Approximate, "tab stops (tabs are four spaces wide)"),
            "w:pBdr" => rep.note(Kind::Approximate, "paragraph borders (dropped)"),
            "w:shd" => rep.note(Kind::Approximate, "paragraph shading (dropped)"),
            "w:framePr" => rep.note(Kind::Approximate, "frames and drop caps (kept in line)"),
            _ => {}
        }
    }
    p
}

/// `under` with the fields `over` sets replaced.
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

/// List kinds by numbering id and level.
struct Numbering {
    kinds: HashMap<(String, u8), ListKind>,
}

impl Numbering {
    fn read(e: Option<&El>) -> Numbering {
        let mut kinds = HashMap::new();
        let Some(e) = e else { return Numbering { kinds } };
        let mut abs: HashMap<String, Vec<(u8, ListKind)>> = HashMap::new();
        for a in e.children("w:abstractNum") {
            let Some(id) = a.attr("w:abstractNumId") else { continue };
            let mut lv = Vec::new();
            for l in a.children("w:lvl") {
                let level = num(l.attr("w:ilvl")).unwrap_or(0).clamp(0, MAX_LIST_LEVEL as i64) as u8;
                let fmt = l.child("w:numFmt").and_then(val).unwrap_or("decimal");
                lv.push((level, if fmt == "bullet" || fmt == "none" { ListKind::Bullet } else { ListKind::Numbered }));
            }
            abs.insert(id.to_string(), lv);
        }
        for n in e.children("w:num") {
            let (Some(id), Some(a)) = (n.attr("w:numId"), n.child("w:abstractNumId").and_then(val)) else { continue };
            for &(l, k) in abs.get(a).map(Vec::as_slice).unwrap_or(&[]) {
                kinds.insert((id.to_string(), l), k);
            }
        }
        Numbering { kinds }
    }

    fn list(&self, numpr: &El) -> Option<Option<ListRef>> {
        let id = numpr.child("w:numId").and_then(val)?;
        if id == "0" {
            return Some(None);
        }
        let level = numpr.child("w:ilvl").and_then(|e| num(val(e))).unwrap_or(0).clamp(0, MAX_LIST_LEVEL as i64) as u8;
        let kind = self.kinds.get(&(id.to_string(), level)).copied().unwrap_or(ListKind::Numbered);
        let n: u32 = id.parse().unwrap_or(1);
        Some(Some(ListRef { id: n, kind, level }))
    }
}

struct Reader<'a> {
    archive: &'a Archive<'a>,
    doc: TextDoc,
    rep: Report,
    styles: HashMap<String, StyleId>,
    numbering: Numbering,
    rels: HashMap<String, (String, String, bool)>,
    images: HashMap<String, u32>,
    next_table: u32,
    pictures: u64,
    /// The settings turn on separate even-page headers and footers.
    even_odd: bool,
    /// Text boxes met in the paragraph being read; their paragraphs follow it.
    boxes: Vec<El>,
    box_depth: usize,
    /// Footnotes and endnotes by their `w:id`, and the relationships of their parts.
    foot_xml: HashMap<String, El>,
    end_xml: HashMap<String, El>,
    foot_rels: HashMap<String, (String, String, bool)>,
    end_rels: HashMap<String, (String, String, bool)>,
    /// Notes anchored in the text, read after it.
    pending: Vec<(NoteId, NoteKind, String)>,
    in_note: bool,
    /// Comments by their `w:id`, and (from commentsExtended.xml) the `w:id`
    /// each answers and whether it is resolved.
    comment_xml: HashMap<String, El>,
    comment_parent: HashMap<String, String>,
    comment_done: HashMap<String, bool>,
    /// Comments met in the text: `w:id` → ours.
    comment_ids: HashMap<String, CommentId>,
    /// The comments over the text being read, sorted.
    active: Vec<CommentId>,
}

/// A comment's text: a line a paragraph.
fn comment_text(e: &El) -> String {
    fn walk(e: &El, out: &mut String, depth: usize) {
        for x in e.els() {
            if out.len() > MAX_COMMENT_TEXT || depth > 32 {
                return;
            }
            match x.name.as_str() {
                "w:t" => out.push_str(&x.text()),
                "w:tab" => out.push('\t'),
                "w:br" | "w:cr" => out.push(' '),
                "w:p" => {
                    let mut line = String::new();
                    walk(x, &mut line, depth + 1);
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&line);
                }
                "w:delText" | "w:instrText" | "w:del" => {}
                _ => walk(x, out, depth + 1),
            }
        }
    }
    let mut out = String::new();
    walk(e, &mut out, 0);
    let mut end = out.len().min(MAX_COMMENT_TEXT);
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out
}

/// The elements named `name` among `e`'s children, looking through content
/// controls and custom XML wrappers (rows and cells can sit in them).
fn wrapped<'a>(e: &'a El, name: &str, out: &mut Vec<&'a El>, depth: usize) {
    for x in e.els() {
        if x.name == name {
            out.push(x);
        } else if depth < 16 {
            match x.name.as_str() {
                "w:sdt" => {
                    if let Some(c) = x.child("w:sdtContent") {
                        wrapped(c, name, out, depth + 1);
                    }
                }
                "w:customXml" | "w:ins" | "w:moveTo" => wrapped(x, name, out, depth + 1),
                _ => {}
            }
        }
    }
}

fn all_of<'a>(e: &'a El, name: &str, out: &mut Vec<&'a El>, depth: usize) {
    for x in e.els() {
        if out.len() >= 64 || depth > 64 {
            return;
        }
        if x.name == name {
            out.push(x);
        } else {
            all_of(x, name, out, depth + 1);
        }
    }
}

/// What a paragraph is being built of.
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

impl<'a> Reader<'a> {
    fn read_styles(&mut self, e: Option<&El>) {
        let Some(e) = e else { return };
        let mut defaults_p = ParaProps::default();
        let mut defaults_c = CharAttrs::default();
        if let Some(d) = e.child("w:docDefaults") {
            if let Some(p) = d.child("w:pPrDefault").and_then(|x| x.child("w:pPr")) {
                defaults_p = ppr(p, &mut self.rep);
            }
            if let Some(r) = d.child("w:rPrDefault").and_then(|x| x.child("w:rPr")) {
                defaults_c = rpr(r, &mut self.rep);
            }
        }
        // Ids first, so a style can name a parent defined after it.
        let builtin = |name: &str| -> Option<&'static str> {
            Some(match name.to_ascii_lowercase().as_str() {
                "normal" => "Normal",
                "title" => "Title",
                "heading 1" => "Heading 1",
                "heading 2" => "Heading 2",
                "heading 3" => "Heading 3",
                _ => return None,
            })
        };
        let mut list: Vec<(&El, String, StyleKind)> = Vec::new();
        for s in e.children("w:style") {
            let kind = match s.attr("w:type") {
                Some("paragraph") => StyleKind::Para,
                Some("character") => StyleKind::Char,
                Some("table") | Some("numbering") => continue,
                _ => continue,
            };
            let Some(id) = s.attr("w:styleId") else { continue };
            let name = s.child("w:name").and_then(val).unwrap_or(id).to_string();
            let sid = match builtin(&name).filter(|_| kind == StyleKind::Para).and_then(|b| self.doc.styles.by_name(b)) {
                Some(k) => k,
                None => {
                    if self.doc.styles.styles.len() >= MAX_STYLES || self.doc.styles.by_name(&name).is_some() {
                        continue;
                    }
                    let n = self.doc.styles.styles.len() as StyleId;
                    self.doc.styles.styles.push(Style { name: Arc::from(&name[..name.len().min(255)]), kind, parent: Some(0).filter(|_| kind == StyleKind::Para), next: None, para: ParaProps::default(), chars: CharAttrs::default() });
                    n
                }
            };
            self.styles.insert(id.to_string(), sid);
            list.push((s, name, kind));
        }
        for (s, name, _) in &list {
            let id = s.attr("w:styleId").unwrap_or("");
            let Some(&sid) = self.styles.get(id) else { continue };
            let mut para = s.child("w:pPr").map(|p| ppr(p, &mut self.rep)).unwrap_or_default();
            if let Some(n) = s.child("w:pPr").and_then(|p| p.child("w:numPr")) {
                para.list = self.numbering.list(n);
            }
            let mut chars = s.child("w:rPr").map(|r| rpr(r, &mut self.rep)).unwrap_or_default();
            let parent = s.child("w:basedOn").and_then(val).and_then(|b| self.styles.get(b).copied()).filter(|&p| p != sid);
            let next = s.child("w:next").and_then(val).and_then(|b| self.styles.get(b).copied());
            let st = &mut self.doc.styles.styles[sid as usize];
            if sid == 0 {
                // Normal stands on the document defaults.
                para = props_over(&para, &defaults_p);
                chars = chars.over(&defaults_c);
            }
            if st.name.starts_with("Heading") && builtin(name).is_some() {
                para.outline_level = para.outline_level.or(st.para.outline_level);
            }
            st.para = para;
            st.chars = chars;
            if sid != 0 {
                st.parent = parent.or(if st.kind == StyleKind::Para { Some(0) } else { None });
            }
            st.next = next.or(st.next);
        }
        // A parent chain that loops is cut.
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

    fn picture(&mut self, drawing: &El) -> Option<ObjectRef> {
        let blip = drawing.find("a:blip")?;
        let rid = blip.attr("r:embed")?;
        let (_, target, external) = self.rels.get(rid)?.clone();
        if external {
            self.rep.note(Kind::Dropped, "linked pictures (not fetched)");
            return None;
        }
        let image = match self.images.get(&target) {
            Some(&i) => i,
            None => {
                if self.images.len() >= MAX_PICTURES {
                    return None;
                }
                let bytes = self.archive.read_name(&target).ok().flatten()?;
                if bytes.is_empty() || bytes.len() > MAX_PICTURE {
                    return None;
                }
                let Some((w, h)) = pixel_size(&bytes) else {
                    self.rep.note_at(Kind::Dropped, "pictures in formats not shown (EMF, WMF, TIFF, SVG)", || target.clone());
                    return None;
                };
                let i = self.doc.images.add(ImageData { bytes: Arc::from(bytes), width: w, height: h });
                self.images.insert(target.clone(), i);
                i
            }
        };
        let ext = drawing.find("wp:extent");
        let emu = |k: &str| ext.and_then(|e| num(e.attr(k))).unwrap_or(0);
        let side = |v: i64| (v / 635).clamp(1, MAX_OBJECT_TWIPS as i64) as u32;
        let (w, h) = (emu("cx"), emu("cy"));
        if drawing.child("wp:anchor").is_some() {
            self.rep.note(Kind::Approximate, "floating pictures (kept in line)");
        }
        let px = self.doc.images.get(image).map(|d| (d.width, d.height)).unwrap_or((96, 96));
        let (w, h) = if w > 0 && h > 0 { (side(w), side(h)) } else { (side(px.0 as i64 * 9525), side(px.1 as i64 * 9525)) };
        self.pictures += 1;
        Some(ObjectRef { image, width: w, height: h })
    }

    /// The runs of `e` (a paragraph or something inside one) into `p`.
    fn runs(&mut self, e: &El, p: &mut Para, link: Option<&Arc<str>>, field: &mut FieldState) {
        for x in e.els() {
            match x.name.as_str() {
                "w:r" => {
                    let mut a = x.child("w:rPr").map(|r| rpr(r, &mut self.rep)).unwrap_or_default();
                    if let Some(rs) = x.child("w:rPr").and_then(|r| r.child("w:rStyle")).and_then(val).and_then(|s| self.styles.get(s)) {
                        a.style = Some(*rs);
                    }
                    if let Some(l) = link {
                        a.link = Some(l.clone());
                    }
                    if !self.active.is_empty() && !self.in_note && !field.in_margin {
                        a.comments = Some(self.active.as_slice().into());
                    }
                    // Markup compatibility: the choice we read, else the fallback.
                    let mut kids: Vec<&El> = Vec::new();
                    for k in x.els() {
                        if k.name == "mc:AlternateContent" {
                            let pick = k.children("mc:Choice").find(|c| c.find("w:drawing").is_some() || c.find("w:txbxContent").is_some()).or_else(|| k.child("mc:Fallback")).or_else(|| k.child("mc:Choice"));
                            if let Some(pk) = pick {
                                kids.extend(pk.els());
                            }
                        } else {
                            kids.push(k);
                        }
                    }
                    for k in kids {
                        if matches!(k.name.as_str(), "w:drawing" | "w:pict" | "w:object") && self.box_depth < 4 {
                            let mut b = Vec::new();
                            all_of(k, "w:txbxContent", &mut b, 0);
                            if !b.is_empty() {
                                self.rep.note(Kind::Approximate, "text boxes (their text kept after the paragraph)");
                                self.boxes.extend(b.into_iter().cloned());
                            }
                        }
                        match k.name.as_str() {
                            "w:fldChar" => match k.attr("w:fldCharType") {
                                Some("begin") => {
                                    field.stage = Stage::Instr;
                                    field.instr.clear();
                                }
                                Some("separate") if field.stage == Stage::Instr => field.stage = Stage::Result,
                                Some("end") => {
                                    if field.stage != Stage::Outside && field.in_margin {
                                        if let Some(s) = page_field(&field.instr) {
                                            p.push(s, &a);
                                        }
                                    }
                                    field.stage = Stage::Outside;
                                }
                                _ => {}
                            },
                            "w:instrText" => {
                                if field.stage == Stage::Instr && field.instr.len() < 4096 {
                                    field.instr.push_str(&k.text());
                                }
                            }
                            "w:t" | "w:delText" if k.name == "w:t" => {
                                if field.shows_result() {
                                    // Without xml:space="preserve" the ends' white space is
                                    // layout of the XML; a line end inside is a space.
                                    let t = k.text();
                                    let t = if k.attr("xml:space") == Some("preserve") { t.as_str() } else { t.trim_matches(|c: char| matches!(c, ' ' | '\n' | '\r' | '\t')) };
                                    let t: String = t.chars().map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c }).collect();
                                    p.push(&t, &a);
                                }
                            }
                            "w:tab" | "w:ptab" => p.push("\t", &a),
                            "w:br" | "w:cr" => {
                                if k.attr("w:type") == Some("page") {
                                    self.rep.note(Kind::Approximate, "page breaks inside a paragraph (a line break)");
                                }
                                p.push("\u{2028}", &a);
                            }
                            "w:noBreakHyphen" => p.push("\u{2011}", &a),
                            "w:softHyphen" => p.push("\u{AD}", &a),
                            "w:sym" => {
                                if let Some(c) = k.attr("w:char").and_then(|c| u32::from_str_radix(c, 16).ok()).and_then(char::from_u32) {
                                    p.push(&c.to_string(), &a);
                                }
                            }
                            "w:drawing" => {
                                if let Some(o) = self.picture(k) {
                                    let mut pa = a.clone();
                                    pa.object = Some(o);
                                    p.push(&OBJECT_CHAR.to_string(), &pa);
                                }
                            }
                            "w:pict" | "w:object" => {
                                if k.find("w:txbxContent").is_none() {
                                    self.rep.note(Kind::Dropped, "VML pictures and embedded objects");
                                }
                            }
                            // Ruby inside a run (Word writes it there too): the base text.
                            "w:ruby" => {
                                if let Some(b) = k.child("w:rubyBase") {
                                    self.runs(b, p, link, field);
                                }
                            }
                            "w:footnoteReference" | "w:endnoteReference" => {
                                let kind = if k.name == "w:footnoteReference" { NoteKind::Foot } else { NoteKind::End };
                                let id = k.attr("w:id").unwrap_or("").to_string();
                                let known = if kind == NoteKind::Foot { self.foot_xml.contains_key(&id) } else { self.end_xml.contains_key(&id) };
                                if !known || self.in_note || field.in_margin {
                                    self.rep.note(Kind::Dropped, "footnotes and endnotes in notes, headers and footers");
                                } else if let Some(nid) = self.doc.notes.add(Note { kind, paras: Vec::new() }) {
                                    let mut na = a.clone();
                                    na.vert = None;
                                    na.style = None;
                                    na.object = None;
                                    na.note = Some(nid);
                                    p.push(&OBJECT_CHAR.to_string(), &na);
                                    self.pending.push((nid, kind, id));
                                }
                            }
                            "w:commentReference" if !field.in_margin => {
                                if let Some(wid) = k.attr("w:id") {
                                    self.point_comment(wid, p);
                                }
                            }
                            "w:rPr" | "w:lastRenderedPageBreak" | "w:delText" => {}
                            _ => {}
                        }
                    }
                }
                "w:hyperlink" => {
                    let target = x.attr("r:id").and_then(|id| self.rels.get(id)).filter(|r| r.2).map(|r| Arc::from(&r.1[..r.1.len().min(8192)]));
                    let anchor = x.attr("w:anchor").map(|a| Arc::from(format!("#{}", &a[..a.len().min(1024)]).as_str()));
                    let l = target.or(anchor);
                    self.runs(x, p, l.as_ref().or(link), field);
                }
                "w:fldSimple" => {
                    let instr = x.attr("w:instr").unwrap_or("");
                    match page_field(instr) {
                        Some(s) if field.in_margin => {
                            let a = x.find("w:rPr").map(|r| rpr(r, &mut self.rep)).unwrap_or_default();
                            p.push(s, &a);
                        }
                        _ => self.runs(x, p, link, field),
                    }
                }
                "w:ruby" => {
                    if let Some(b) = x.child("w:rubyBase") {
                        self.runs(b, p, link, field);
                    }
                }
                "w:ins" | "w:moveTo" | "w:smartTag" | "w:customXml" | "w:sdt" | "w:sdtContent" | "w:dir" | "w:bdo" => {
                    if x.name == "w:ins" || x.name == "w:moveTo" {
                        self.rep.note(Kind::Approximate, "tracked changes (insertions kept, deletions dropped)");
                    }
                    let inner = if x.name == "w:sdt" { x.child("w:sdtContent") } else { Some(x) };
                    if let Some(i) = inner {
                        self.runs(i, p, link, field);
                    }
                }
                "w:del" | "w:moveFrom" => self.rep.note(Kind::Approximate, "tracked changes (insertions kept, deletions dropped)"),
                "m:oMath" | "m:oMathPara" => self.rep.note(Kind::Dropped, "equations"),
                "w:commentRangeStart" | "w:commentRangeEnd" => self.comment_mark(x),
                _ => {}
            }
        }
    }

    /// A comment range's start or end: the comment joins or leaves the set
    /// over the text. Replies get no range of their own.
    fn comment_mark(&mut self, x: &El) {
        if self.in_note {
            return;
        }
        let Some(wid) = x.attr("w:id") else { return };
        if x.name == "w:commentRangeEnd" {
            if let Some(&id) = self.comment_ids.get(wid) {
                self.active.retain(|&a| a != id);
            }
            return;
        }
        let Some(id) = self.comment_id(wid) else { return };
        if let Err(i) = self.active.binary_search(&id) {
            self.active.insert(i, id);
        }
    }

    /// Our comment for `w:id` `wid` (made the first time); none for a reply.
    fn comment_id(&mut self, wid: &str) -> Option<CommentId> {
        if self.comment_parent.get(wid).is_some_and(|p| self.comment_xml.contains_key(p)) {
            return None;
        }
        if let Some(&id) = self.comment_ids.get(wid) {
            return Some(id);
        }
        let c = self.comment_xml.get(wid)?;
        let cut = |s: Option<&str>, n: usize| -> Arc<str> { Arc::from(s.unwrap_or("").chars().take(n).collect::<String>().as_str()) };
        let comment = Comment { author: cut(c.attr("w:author"), 256), initials: cut(c.attr("w:initials"), 16), date: cut(c.attr("w:date"), 64), text: comment_text(c), parent: None, done: self.comment_done.get(wid).copied().unwrap_or(false) };
        let id = self.doc.comments.add(comment)?;
        self.comment_ids.insert(wid.to_string(), id);
        Some(id)
    }

    /// A comment with a reference and no range: over the character before it.
    fn point_comment(&mut self, wid: &str, p: &mut Para) {
        if self.in_note || self.comment_ids.contains_key(wid) {
            return;
        }
        let Some((at, _)) = p.text.char_indices().next_back() else { return };
        let Some(id) = self.comment_id(wid) else { return };
        let at = at as u32;
        let i = p.runs.iter().rposition(|r| r.0 <= at).unwrap_or(0);
        if p.runs[i].0 < at {
            let a = p.runs[i].1.clone();
            p.runs.insert(i + 1, (at, a));
        }
        let last = p.runs.len() - 1;
        let a = &mut p.runs[last].1;
        let mut ids: Vec<CommentId> = a.comments.as_deref().unwrap_or(&[]).to_vec();
        if let Err(k) = ids.binary_search(&id) {
            ids.insert(k, id);
        }
        a.comments = Some(ids.into());
    }

    /// Replies to the comments read, after the text: in comments.xml's order,
    /// so a reply follows what it answers.
    fn read_replies(&mut self, order: &[String]) {
        for wid in order {
            if self.comment_ids.contains_key(wid) {
                continue;
            }
            let Some(parent) = self.comment_parent.get(wid).and_then(|p| self.comment_ids.get(p)).copied() else {
                if self.comment_xml.contains_key(wid) {
                    self.rep.note(Kind::Dropped, "comments without a range in the text");
                }
                continue;
            };
            let Some(c) = self.comment_xml.get(wid) else { continue };
            let cut = |s: Option<&str>, n: usize| -> Arc<str> { Arc::from(s.unwrap_or("").chars().take(n).collect::<String>().as_str()) };
            let comment = Comment { author: cut(c.attr("w:author"), 256), initials: cut(c.attr("w:initials"), 16), date: cut(c.attr("w:date"), 64), text: comment_text(c), parent: Some(parent), done: self.comment_done.get(wid).copied().unwrap_or(false) };
            if let Some(id) = self.doc.comments.add(comment) {
                self.comment_ids.insert(wid.clone(), id);
            }
        }
    }

    /// One `w:p` into the document; returns its block.
    fn para(&mut self, e: &El, cell: Option<CellRef>, field: &mut FieldState) -> usize {
        let pp = e.child("w:pPr");
        let mut props = pp.map(|p| ppr(p, &mut self.rep)).unwrap_or_default();
        let style = pp.and_then(|p| p.child("w:pStyle")).and_then(val).and_then(|s| self.styles.get(s).copied()).filter(|&s| self.doc.styles.styles[s as usize].kind == StyleKind::Para).unwrap_or(0);
        if let Some(n) = pp.and_then(|p| p.child("w:numPr")) {
            props.list = self.numbering.list(n);
        }
        props.cell = cell;
        if cell.is_some() {
            props.space_after = props.space_after.or(Some(0));
        }
        let mut p = Para::default();
        // The paragraph mark's formatting is its typing format when it is empty.
        let mark = pp.and_then(|p| p.child("w:rPr")).map(|r| rpr(r, &mut self.rep)).unwrap_or_default();
        self.runs(e, &mut p, None, field);
        let mut runs: Vec<(u32, u32)> = p.runs.into_iter().map(|(at, a)| (at, self.doc.attrs.intern(a))).collect();
        if runs.is_empty() {
            runs.push((0, self.doc.attrs.intern(mark)));
        }
        let k = self.doc.push_para(&p.text, style, runs);
        self.doc.blocks.update(k, |b| {
            if let Some(q) = b.as_para_mut() {
                q.props = props;
            }
        });
        // Text boxes anchored in it: their paragraphs after it.
        let boxes = std::mem::take(&mut self.boxes);
        if !boxes.is_empty() {
            self.box_depth += 1;
            for b in &boxes {
                match cell {
                    Some(c) => self.cell_content(b, c, 1, field),
                    None => self.box_blocks(b, field, 0),
                }
            }
            self.box_depth -= 1;
        }
        k
    }

    /// The text of each note anchored in the text: read as blocks at the end of
    /// the document, then taken out into the note.
    fn read_notes(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return;
        }
        self.in_note = true;
        let main_rels = std::mem::take(&mut self.rels);
        for (nid, kind, id) in pending {
            let el = if kind == NoteKind::Foot { self.foot_xml.get(&id) } else { self.end_xml.get(&id) }.cloned();
            self.rels = if kind == NoteKind::Foot { self.foot_rels.clone() } else { self.end_rels.clone() };
            let before = self.doc.blocks.len();
            if let Some(el) = el {
                let mut field = FieldState::default();
                self.box_blocks(&el, &mut field, 0);
            }
            let mut paras = Vec::new();
            while self.doc.blocks.len() > before {
                if let Some(Block::Para(mut p)) = self.doc.blocks.remove(before) {
                    p.props.cell = None;
                    paras.push(p);
                }
            }
            // The space Word puts after the number.
            if let Some(p) = paras.first_mut() {
                if p.text.starts_with(' ') {
                    p.text.remove(0);
                    for r in p.runs.iter_mut().skip(1) {
                        r.0 = r.0.saturating_sub(1);
                    }
                    p.normalize();
                }
            }
            if paras.len() > MAX_NOTE_PARAS {
                paras.truncate(MAX_NOTE_PARAS);
            }
            if paras.is_empty() {
                let id = self.doc.new_id();
                paras.push(officina_text::model::Para::new(id, "", 0));
            }
            self.doc.notes.set(nid, Some(Note { kind, paras }));
        }
        self.rels = main_rels;
        self.in_note = false;
        self.doc.notes.renumber();
    }

    /// A text box's paragraphs and tables, through content controls and the like.
    fn box_blocks(&mut self, b: &El, field: &mut FieldState, depth: usize) {
        for x in b.els() {
            match x.name.as_str() {
                "w:p" => {
                    self.para(x, None, field);
                }
                "w:tbl" => self.table(x, 0, field),
                "w:sdt" if depth < 16 => {
                    if let Some(c) = x.child("w:sdtContent") {
                        self.box_blocks(c, field, depth + 1);
                    }
                }
                "w:customXml" | "w:ins" | "w:moveTo" if depth < 16 => self.box_blocks(x, field, depth + 1),
                _ => {}
            }
        }
    }

    fn table(&mut self, t: &El, depth: usize, field: &mut FieldState) {
        if depth > 0 {
            // A table in a cell: its text, a paragraph per cell paragraph.
            self.rep.note(Kind::Approximate, "nested tables (their text kept in the cell)");
        }
        let grid: Vec<u32> = t.child("w:tblGrid").map(|g| g.children("w:gridCol").map(|c| num(c.attr("w:w")).unwrap_or(1440).clamp(1, MAX_OBJECT_TWIPS as i64) as u32).collect()).unwrap_or_default();
        let table = self.next_table;
        self.next_table += 1;
        let mut rows = Vec::new();
        wrapped(t, "w:tr", &mut rows, 0);
        for (r, tr) in rows.into_iter().enumerate() {
            let mut col = 0usize;
            let mut cells = Vec::new();
            wrapped(tr, "w:tc", &mut cells, 0);
            for (c, tc) in cells.into_iter().enumerate() {
                let pr = tc.child("w:tcPr");
                let span = pr.and_then(|p| p.child("w:gridSpan")).and_then(|g| num(val(g))).unwrap_or(1).clamp(1, 63) as usize;
                if span > 1 {
                    self.rep.note(Kind::Approximate, "merged cells (kept as one wide cell)");
                }
                if pr.and_then(|p| p.child("w:vMerge")).is_some() {
                    self.rep.note(Kind::Approximate, "cells merged down (each row keeps its own cell)");
                }
                let width = (col..col + span).map(|g| grid.get(g).copied().unwrap_or(0)).sum::<u32>();
                let width = if width == 0 { pr.and_then(|p| p.child("w:tcW")).filter(|w| w.attr("w:type") == Some("dxa")).and_then(|w| num(w.attr("w:w"))).unwrap_or(2000).clamp(1, MAX_OBJECT_TWIPS as i64) as u32 } else { width.min(MAX_OBJECT_TWIPS) };
                col += span;
                let cell = CellRef { table, row: r as u32, col: c.min(u16::MAX as usize) as u16, width };
                let before = self.doc.blocks.len();
                self.cell_content(tc, cell, depth, field);
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

    fn cell_content(&mut self, tc: &El, cell: CellRef, depth: usize, field: &mut FieldState) {
        for x in tc.els() {
            match x.name.as_str() {
                "w:p" => {
                    self.para(x, Some(cell), field);
                }
                "w:tbl" if depth < 8 => {
                    let mut rows = Vec::new();
                    wrapped(x, "w:tr", &mut rows, 0);
                    for tr in rows {
                        let mut cells = Vec::new();
                        wrapped(tr, "w:tc", &mut cells, 0);
                        for inner in cells {
                            self.cell_content(inner, cell, depth + 1, field);
                        }
                    }
                    self.rep.note(Kind::Approximate, "nested tables (their text kept in the cell)");
                }
                "w:sdt" => {
                    if let Some(c) = x.child("w:sdtContent") {
                        self.cell_content(c, cell, depth, field);
                    }
                }
                "w:commentRangeStart" | "w:commentRangeEnd" => self.comment_mark(x),
                "w:customXml" | "w:ins" | "w:moveTo" => self.cell_content(x, cell, depth, field),
                _ => {}
            }
        }
    }

    fn body(&mut self, body: &El) {
        let mut field = FieldState::default();
        for x in body.els() {
            match x.name.as_str() {
                "w:p" => {
                    if x.child("w:pPr").and_then(|p| p.child("w:sectPr")).is_some() {
                        self.rep.note(Kind::Approximate, "sections after the first (one page setup)");
                    }
                    self.para(x, None, &mut field);
                }
                "w:tbl" => self.table(x, 0, &mut field),
                "w:sdt" => {
                    if let Some(c) = x.child("w:sdtContent") {
                        self.body(c);
                    }
                }
                "w:sectPr" => self.section(x),
                "w:commentRangeStart" | "w:commentRangeEnd" => self.comment_mark(x),
                "w:customXml" | "w:ins" | "w:moveTo" => self.body(x),
                _ => {}
            }
        }
    }

    fn section(&mut self, s: &El) {
        let mut g = self.doc.page;
        if let Some(sz) = s.child("w:pgSz") {
            g.width = twips(sz.attr("w:w")).unwrap_or(g.width).max(1440);
            g.height = twips(sz.attr("w:h")).unwrap_or(g.height).max(1440);
        }
        if let Some(m) = s.child("w:pgMar") {
            let t = |k: &str, d: i32| twips(m.attr(k)).map(|v| v.abs()).unwrap_or(d);
            g.margin_top = t("w:top", g.margin_top);
            g.margin_bottom = t("w:bottom", g.margin_bottom);
            g.margin_start = t("w:left", g.margin_start);
            g.margin_end = t("w:right", g.margin_end);
        }
        if s.child("w:cols").and_then(|c| num(c.attr("w:num"))).is_some_and(|n| n > 1) {
            self.rep.note(Kind::Approximate, "columns (one column)");
        }
        self.doc.page = PageSetup { ..g };
        for (tag, header) in [("w:headerReference", true), ("w:footerReference", false)] {
            let refs: Vec<&El> = s.children(tag).collect();
            let pick = refs.iter().find(|r| r.attr("w:type") == Some("default")).or(refs.first());
            // Other kinds count only where the document turns them on.
            let first = s.child("w:titlePg").is_some_and(on) && refs.iter().any(|r| r.attr("w:type") == Some("first"));
            let even = self.even_odd && refs.iter().any(|r| r.attr("w:type") == Some("even"));
            if first || even {
                self.rep.note(Kind::Approximate, "first-page and even-page headers and footers (the default one kept)");
            }
            let Some(target) = pick.and_then(|r| r.attr("r:id")).and_then(|id| self.rels.get(id)).map(|r| r.1.clone()) else { continue };
            if let Some(hf) = self.margin_text(&target) {
                if header {
                    self.doc.header = Some(hf);
                } else {
                    self.doc.footer = Some(hf);
                }
            }
        }
    }

    /// A header or footer part as one line of text, page fields as `{page}`.
    fn margin_text(&mut self, target: &str) -> Option<HeaderFooter> {
        let e = part(self.archive, target).ok().flatten()?;
        let mut field = FieldState { in_margin: true, ..Default::default() };
        let mut text = String::new();
        let mut align = Align::Start;
        let mut first = true;
        for p in e.children("w:p") {
            let mut q = Para::default();
            self.runs(p, &mut q, None, &mut field);
            if q.text.trim().is_empty() {
                continue;
            }
            if first {
                align = p.child("w:pPr").map(|x| ppr(x, &mut self.rep)).and_then(|x| x.align).unwrap_or(Align::Start);
                first = false;
            } else {
                text.push(' ');
                self.rep.note(Kind::Approximate, "headers and footers of more than one line (joined)");
            }
            text.push_str(q.text.replace(['\t', '\u{2028}'], " ").trim());
        }
        if e.find("w:tbl").is_some() || e.find("w:drawing").is_some() {
            self.rep.note(Kind::Approximate, "tables and pictures in headers and footers (their text kept)");
        }
        (!text.is_empty()).then(|| HeaderFooter { text: text.chars().take(1000).collect(), align })
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Stage {
    #[default]
    Outside,
    Instr,
    Result,
}

/// Where a complex field (`w:fldChar`) is: its instruction, then its result.
#[derive(Default)]
struct FieldState {
    stage: Stage,
    instr: String,
    /// Reading a header or footer: page number fields become placeholders.
    in_margin: bool,
}

impl FieldState {
    fn shows_result(&self) -> bool {
        match self.stage {
            Stage::Outside => true,
            Stage::Instr => false,
            Stage::Result => !(self.in_margin && page_field(&self.instr).is_some()),
        }
    }
}

/// `{page}` or `{pages}` for a page number field's instruction.
fn page_field(instr: &str) -> Option<&'static str> {
    match instr.split_whitespace().next().map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("PAGE") => Some("{page}"),
        Some("NUMPAGES") | Some("SECTIONPAGES") => Some("{pages}"),
        _ => None,
    }
}

/// A raster picture's size in pixels from its header, for the formats the
/// document process can draw (PNG, JPEG, GIF, WebP).
pub fn pixel_size(b: &[u8]) -> Option<(u32, u32)> {
    let be32 = |i: usize| b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    let le16 = |i: usize| b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]]) as u32);
    let le24 = |i: usize| -> Option<u32> { Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, 0])) };
    let size = if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        be32(16).zip(be32(20))
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let be16 = |i: usize| -> Option<u32> { Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]) as u32) };
        let mut i = 2usize;
        let mut found = None;
        while i + 9 <= b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let m = b[i + 1];
            if (0xC0..=0xCF).contains(&m) && ![0xC4, 0xC8, 0xCC].contains(&m) {
                found = be16(i + 7).zip(be16(i + 5));
                break;
            }
            let seg = be16(i + 2)? as usize;
            if seg < 2 {
                break;
            }
            i += 2 + seg;
        }
        found
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        le16(6).zip(le16(8))
    } else if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        match b.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
            b"VP8L" => {
                let bits = u32::from_le_bytes([*b.get(21)?, *b.get(22)?, *b.get(23)?, *b.get(24)?]);
                Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
            }
            _ => None,
        }
    } else {
        None
    };
    size.filter(|&(w, h)| (1..=65536).contains(&w) && (1..=65536).contains(&h))
}

/// Reads a `.docx`.
pub fn read(data: &[u8]) -> Result<(TextDoc, Loaded), DocxError> {
    let archive = Archive::open(data)?;
    let main = rels(&archive, "")?.into_values().find(|(k, _, _)| k == "officeDocument").map(|r| r.1).unwrap_or_else(|| "word/document.xml".into());
    let doc_xml = part(&archive, &main)?.ok_or_else(|| DocxError::Bad("no main document part".into()))?;
    let body = doc_xml.child("w:body").ok_or_else(|| DocxError::Bad("no document body".into()))?;
    let rels_main = rels(&archive, &main)?;
    let find = |kind: &str| rels_main.values().find(|r| r.0 == kind).map(|r| r.1.clone());
    let styles = match find("styles") {
        Some(p) => part(&archive, &p)?,
        None => None,
    };
    let numbering = match find("numbering") {
        Some(p) => part(&archive, &p)?,
        None => None,
    };
    let mut doc = TextDoc::new();
    doc.blocks.remove(0);
    let mut r = Reader { archive: &archive, doc, rep: Report::default(), styles: HashMap::new(), numbering: Numbering::read(numbering.as_ref()), rels: rels_main.clone(), images: HashMap::new(), next_table: 1, pictures: 0, even_odd: false, boxes: Vec::new(), box_depth: 0, foot_xml: HashMap::new(), end_xml: HashMap::new(), foot_rels: HashMap::new(), end_rels: HashMap::new(), pending: Vec::new(), in_note: false, comment_xml: HashMap::new(), comment_parent: HashMap::new(), comment_done: HashMap::new(), comment_ids: HashMap::new(), active: Vec::new() };
    for (kind, key) in [("footnotes", "w:footnote"), ("endnotes", "w:endnote")] {
        let Some(p) = find(kind) else { continue };
        let Some(x) = part(&archive, &p)? else { continue };
        let mut m = HashMap::new();
        // Separators and continuation notices aren't notes.
        for n in x.children(key).filter(|n| n.attr("w:type").is_none_or(|t| t == "normal")).take(MAX_NOTES) {
            if let Some(id) = n.attr("w:id") {
                m.insert(id.to_string(), n.clone());
            }
        }
        let nr = rels(&archive, &p)?;
        if kind == "footnotes" {
            r.foot_xml = m;
            r.foot_rels = nr;
        } else {
            r.end_xml = m;
            r.end_rels = nr;
        }
    }
    // Comments, and their threads and resolved state (paragraph ids tie the two).
    let mut comment_order = Vec::new();
    if let Some(x) = find("comments").map(|p| part(&archive, &p)).transpose()?.flatten() {
        let mut para_of = HashMap::new();
        for c in x.children("w:comment").take(MAX_COMMENTS) {
            let Some(id) = c.attr("w:id") else { continue };
            if let Some(pid) = c.children("w:p").last().and_then(|p| p.attr("w14:paraId")) {
                para_of.insert(pid.to_string(), id.to_string());
            }
            comment_order.push(id.to_string());
            r.comment_xml.insert(id.to_string(), c.clone());
        }
        if let Some(x) = find("commentsExtended").map(|p| part(&archive, &p)).transpose()?.flatten() {
            for e in x.children("w15:commentEx") {
                let Some(id) = e.attr("w15:paraId").and_then(|p| para_of.get(p)) else { continue };
                if e.attr("w15:done") == Some("1") {
                    r.comment_done.insert(id.clone(), true);
                }
                if let Some(parent) = e.attr("w15:paraIdParent").and_then(|p| para_of.get(p)) {
                    if parent != id {
                        r.comment_parent.insert(id.clone(), parent.clone());
                    }
                }
            }
        }
    }
    if let Some(p) = find("settings") {
        r.even_odd = part(&archive, &p)?.is_some_and(|s| s.child("w:evenAndOddHeaders").is_some_and(on));
    }
    r.read_styles(styles.as_ref());
    r.body(body);
    for more in doc_xml.children("w:body").skip(1).take(16) {
        r.body(more);
    }
    r.read_notes();
    r.read_replies(&comment_order);
    if r.doc.blocks.is_empty() {
        r.doc.push_para("", 0, vec![]);
    }
    let paragraphs = r.doc.blocks.len() as u64;
    let pictures = r.pictures;
    let mut doc = r.doc;
    doc.take_changes();
    Ok((doc, Loaded { report: r.rep, paragraphs, pictures }))
}
