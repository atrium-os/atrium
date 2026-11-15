//! `.odp` (OpenDocument Presentation) in: a deck with its masters, slides,
//! shapes, pictures, notes, transitions and builds (docs/spec/officina.md
//! §9.2, O6c).
//!
//! ODF styles inherit (shape style, its parents, the family default); text
//! is read with what its styles give it made direct, so a slide shows what
//! the file shows whatever master it ends up on. Each master page becomes a
//! master with one layout; its presentation styles (`-title`,
//! `-outline1`…`9`) its level styles. Shapes LibreOffice writes for
//! OOXML presets (`ooxml-…`) keep their preset; other custom shapes keep a
//! preset where there is one, else the outline their equations draw.

use std::collections::HashMap;
use std::sync::Arc;

use officina_slides::model::*;
use officina_slides::ops::{EMU_MAX_SIDE, EMU_MIN_SIDE};
use officina_text::model::{AttrId, CharAttrs, ListKind, ListRef, ParaProps, TextDoc};

use super::geom;
use crate::odt::read::{length, para_props, text_props};
use crate::pptx::read::{mime_of, Loaded, MAX_MEDIA_BYTES};
use crate::report::{Kind, Report};
use crate::xml::{self, El, Node};
use crate::zip::Archive;

#[derive(Debug)]
pub enum OdpError {
    Zip(crate::zip::ZipError),
    Xml(String),
    Bad(String),
}

impl std::fmt::Display for OdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OdpError::Zip(e) => write!(f, "{e}"),
            OdpError::Xml(e) => write!(f, "damaged presentation XML: {e}"),
            OdpError::Bad(e) => write!(f, "not a presentation this can read: {e}"),
        }
    }
}

impl std::error::Error for OdpError {}

impl From<crate::zip::ZipError> for OdpError {
    fn from(e: crate::zip::ZipError) -> Self {
        OdpError::Zip(e)
    }
}

fn part(a: &Archive, name: &str) -> Result<Option<El>, OdpError> {
    match a.read_name(name)? {
        Some(b) => xml::parse(&b).map(Some).map_err(|e| OdpError::Xml(format!("{name}: {e}"))),
        None => Ok(None),
    }
}

/// An ODF length in EMU.
pub(crate) fn emu(s: &str) -> Option<i64> {
    length(s).map(|t| (t * 635).clamp(-EMU_MAX_SIDE * 64, EMU_MAX_SIDE * 64))
}

fn rgb(s: Option<&str>) -> Option<u32> {
    let s = s?.trim().strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    u32::from_str_radix(s, 16).ok()
}

fn pct(s: Option<&str>) -> Option<f64> {
    s?.trim().strip_suffix('%')?.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A duration (`0.5s`, `500ms`, `PT0.5S`, `PT00H00M05S`) in milliseconds.
pub(crate) fn millis(s: &str) -> Option<u32> {
    let s = s.trim();
    let v = if let Some(p) = s.strip_prefix("PT").or_else(|| s.strip_prefix("pt")) {
        let mut total = 0.0;
        let mut n = String::new();
        for c in p.chars() {
            match c {
                'H' | 'h' => total += n.parse::<f64>().ok()? * 3600.0,
                'M' | 'm' => total += n.parse::<f64>().ok()? * 60.0,
                'S' | 's' => total += n.parse::<f64>().ok()?,
                _ => {
                    n.push(c);
                    continue;
                }
            }
            n.clear();
        }
        total * 1000.0
    } else if let Some(ms) = s.strip_suffix("ms") {
        ms.trim().parse::<f64>().ok()?
    } else if let Some(sec) = s.strip_suffix('s') {
        sec.trim().parse::<f64>().ok()? * 1000.0
    } else {
        s.parse::<f64>().ok()? * 1000.0
    };
    v.is_finite().then(|| v.clamp(0.0, 86_400_000.0).round() as u32)
}

/// Every style the files name.
#[derive(Default)]
struct Styles<'a> {
    by: HashMap<(String, String), &'a El>,
    defaults: HashMap<String, &'a El>,
    lists: HashMap<String, &'a El>,
    /// `draw:gradient`, `draw:fill-image`, `draw:stroke-dash`, `draw:marker`,
    /// `draw:hatch` by element and name.
    named: HashMap<(String, String), &'a El>,
    page_layouts: HashMap<String, &'a El>,
    fonts: HashMap<String, String>,
}

impl<'a> Styles<'a> {
    fn add(&mut self, container: Option<&'a El>) {
        let Some(c) = container else { return };
        for e in c.els() {
            match e.name.as_str() {
                "style:style" => {
                    if let (Some(f), Some(n)) = (e.attr("style:family"), e.attr("style:name")) {
                        self.by.insert((f.to_string(), n.to_string()), e);
                    }
                }
                "style:default-style" => {
                    if let Some(f) = e.attr("style:family") {
                        self.defaults.insert(f.to_string(), e);
                    }
                }
                "text:list-style" => {
                    if let Some(n) = e.attr("style:name") {
                        self.lists.insert(n.to_string(), e);
                    }
                }
                "draw:gradient" | "draw:fill-image" | "draw:stroke-dash" | "draw:marker" | "draw:hatch" => {
                    if let Some(n) = e.attr("draw:name") {
                        self.named.insert((e.name.clone(), n.to_string()), e);
                    }
                }
                "style:page-layout" => {
                    if let Some(n) = e.attr("style:name") {
                        self.page_layouts.insert(n.to_string(), e);
                    }
                }
                _ => {}
            }
        }
    }

    fn font_decls(&mut self, e: Option<&'a El>) {
        for f in e.into_iter().flat_map(|e| e.children("style:font-face")) {
            if let (Some(n), Some(fam)) = (f.attr("style:name"), f.attr("svg:font-family")) {
                self.fonts.insert(n.to_string(), fam.trim_matches(['\'', '"']).to_string());
            }
        }
    }

    /// A style, its parents, then its family's default: nearest first.
    fn chain(&self, family: &str, name: Option<&str>) -> Vec<&'a El> {
        let mut v = Vec::new();
        let mut cur = name.map(str::to_string);
        while let Some(n) = cur {
            if v.len() > 32 {
                break;
            }
            let Some(e) = self.by.get(&(family.to_string(), n)) else { break };
            v.push(*e);
            cur = e.attr("style:parent-style-name").map(str::to_string);
        }
        if let Some(d) = self.defaults.get(family) {
            v.push(*d);
        }
        v
    }
}

/// The first value of `key` in `section` along a chain.
fn get<'a>(chain: &[&'a El], section: &str, key: &str) -> Option<&'a str> {
    chain.iter().find_map(|e| e.child(section).and_then(|s| s.attr(key)))
}

fn graphic<'a>(chain: &[&'a El], key: &str) -> Option<&'a str> {
    get(chain, "style:graphic-properties", key)
}

/// A list style embedded in a presentation style.
fn embedded_list<'a>(chain: &[&'a El]) -> Option<&'a El> {
    chain.iter().find_map(|e| e.child("style:graphic-properties").and_then(|g| g.child("text:list-style")))
}

fn ph_kind(class: &str) -> Option<Option<PhKind>> {
    Some(Some(match class {
        "title" | "vertical_title" => PhKind::Title,
        "outline" | "vertical_outline" | "text" => PhKind::Body,
        "subtitle" => PhKind::SubTitle,
        "graphic" => PhKind::Pic,
        "object" => PhKind::Obj,
        "chart" => PhKind::Chart,
        "table" => PhKind::Table,
        "orgchart" => PhKind::Diagram,
        "header" => PhKind::Header,
        "footer" => PhKind::Footer,
        "date-time" => PhKind::Date,
        "page-number" => PhKind::SlideNum,
        "notes" | "page" | "handout" => return Some(None),
        _ => return None,
    }))
}

/// ODF custom shape types that have an OOXML preset of the same outline.
fn preset_of(t: &str) -> Option<&'static str> {
    Some(match t {
        "rectangle" | "mso-spt1" => "rect",
        "round-rectangle" | "mso-spt2" => "roundRect",
        "ellipse" | "circle" | "mso-spt3" => "ellipse",
        "isosceles-triangle" => "triangle",
        "right-triangle" => "rtTriangle",
        "diamond" => "diamond",
        "parallelogram" => "parallelogram",
        "trapezoid" => "trapezoid",
        "pentagon" => "pentagon",
        "hexagon" => "hexagon",
        "octagon" => "octagon",
        "star5" => "star5",
        "cross" => "plus",
        "right-arrow" => "rightArrow",
        "left-arrow" => "leftArrow",
        "up-arrow" => "upArrow",
        "down-arrow" => "downArrow",
        "chevron" => "chevron",
        "pentagon-right" => "homePlate",
        "ring" => "donut",
        "can" => "can",
        "cube" => "cube",
        "heart" => "heart",
        "smiley" => "smileyFace",
        "sun" => "sun",
        "moon" => "moon",
        "lightning" => "lightningBolt",
        "cloud" => "cloud",
        "star4" => "star4",
        "star8" => "star8",
        "star12" => "star12",
        "star24" => "star24",
        "block-arc" => "blockArc",
        "frame" => "frame",
        "paper" => "foldedCorner",
        "left-right-arrow" => "leftRightArrow",
        "up-down-arrow" => "upDownArrow",
        "quad-arrow" => "quadArrow",
        "notched-right-arrow" => "notchedRightArrow",
        "striped-right-arrow" => "stripedRightArrow",
        "circular-arrow" => "circularArrow",
        "flowchart-process" => "flowChartProcess",
        "flowchart-decision" => "flowChartDecision",
        "flowchart-terminator" => "flowChartTerminator",
        "flowchart-data" => "flowChartInputOutput",
        "flowchart-document" => "flowChartDocument",
        "flowchart-connector" => "flowChartConnector",
        "flowchart-alternate-process" => "flowChartAlternateProcess",
        "flowchart-predefined-process" => "flowChartPredefinedProcess",
        "flowchart-manual-input" => "flowChartManualInput",
        "flowchart-preparation" => "flowChartPreparation",
        "rectangular-callout" => "wedgeRectCallout",
        "round-rectangular-callout" => "wedgeRoundRectCallout",
        "round-callout" => "wedgeEllipseCallout",
        "cloud-callout" => "cloudCallout",
        "left-bracket" => "leftBracket",
        "right-bracket" => "rightBracket",
        "left-brace" => "leftBrace",
        "right-brace" => "rightBrace",
        "vertical-scroll" => "verticalScroll",
        "horizontal-scroll" => "horizontalScroll",
        "forbidden" => "noSmoking",
        "octagon-bevel" | "quad-bevel" => "bevel",
        _ => return None,
    })
}

/// A list level's look: its bullet and the indents it gives (twips).
struct ListLevel {
    bullet: Bullet,
    indent: Option<(i32, i32)>,
}

/// A paragraph being put together.
#[derive(Default)]
struct Runs {
    text: String,
    runs: Vec<(u32, AttrId)>,
    space: bool,
}

impl Runs {
    fn raw(&mut self, doc: &mut TextDoc, s: &str, a: &CharAttrs) {
        if s.is_empty() || self.text.len() > 1 << 20 {
            return;
        }
        let id = doc.attrs.intern(a.clone());
        if self.runs.last().map(|r| r.1) != Some(id) {
            self.runs.push((self.text.len() as u32, id));
        }
        self.text.push_str(s);
    }
    /// Text with ODF's white space rule: runs of white space are one space,
    /// none at the paragraph's start.
    fn text(&mut self, doc: &mut TextDoc, s: &str, a: &CharAttrs) {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if matches!(c, ' ' | '\n' | '\r' | '\t') {
                if !self.space && !(self.text.is_empty() && out.is_empty()) {
                    out.push(' ');
                    self.space = true;
                }
            } else {
                out.push(c);
                self.space = false;
            }
        }
        self.raw(doc, &out, a);
    }
}

struct Reader<'a> {
    archive: &'a Archive<'a>,
    st: Styles<'a>,
    deck: Deck,
    rep: Report,
    loaded: Loaded,
    media: HashMap<String, Option<MediaId>>,
    shapes: usize,
    on_slide: bool,
}

impl<'a> Reader<'a> {
    fn media(&mut self, href: &str) -> Option<MediaId> {
        let href = href.strip_prefix("vnd.sun.star.Package:").unwrap_or(href);
        let target = href.trim_start_matches("./").to_string();
        if target.contains("://") || target.starts_with("../") || target.starts_with('/') {
            self.rep.note(Kind::Dropped, "linked (not embedded) pictures and videos");
            return None;
        }
        if let Some(m) = self.media.get(&target) {
            return *m;
        }
        let got = match self.archive.find(&target) {
            Some(e) if (e.size as usize) <= MAX_MEDIA_BYTES => self.archive.read(e).ok(),
            Some(_) => {
                self.rep.note(Kind::Dropped, "pictures or videos over 512 MB");
                None
            }
            None => {
                self.rep.note(Kind::Dropped, "pictures the file names but doesn't hold");
                None
            }
        };
        let id = got.map(|b| {
            let (mime, shown) = mime_of(&target, &b);
            if !shown {
                self.rep.note_at(Kind::Approximate, "pictures in formats not drawn (TIFF, SVM) kept and shown as boxes", || target.clone());
            }
            self.loaded.pictures += 1;
            self.deck.add_media(mime, &b)
        });
        self.media.insert(target, id);
        id
    }

    /// A shape's style chain: its presentation style, then its graphic style.
    fn shape_chain(&self, e: &El) -> Vec<&'a El> {
        let mut v = Vec::new();
        if e.attr("presentation:style-name").is_some() {
            v = self.st.chain("presentation", e.attr("presentation:style-name"));
        }
        v.extend(self.st.chain("graphic", e.attr("draw:style-name")));
        v
    }

    fn chars(&self, chain: &[&'a El]) -> CharAttrs {
        let mut a = CharAttrs::default();
        for e in chain.iter().rev() {
            if let Some(t) = e.child("style:text-properties") {
                a = text_props(t, &self.st.fonts).over(&a);
            }
        }
        a
    }

    fn para(&self, chain: &[&'a El]) -> ParaProps {
        let mut p = ParaProps::default();
        for e in chain.iter().rev() {
            if let Some(t) = e.child("style:paragraph-properties") {
                p = officina_slides::resolve::para_over(&para_props(t), &p);
            }
        }
        // Tab stops and page layout mean nothing on a slide.
        p.tabs = None;
        p.keep_together = None;
        p.keep_with_next = None;
        p.page_break_before = None;
        p.widows = None;
        p
    }

    fn color(&self, c: Option<&str>, opacity: Option<&str>) -> Option<Color> {
        let mut col = Color::rgb(rgb(c)?);
        if let Some(o) = pct(opacity) {
            if o < 100.0 {
                col = col.with(ColorMod::Alpha((o.clamp(0.0, 100.0) * 1000.0) as i32));
            }
        }
        Some(col)
    }

    /// A fill from a section's properties along a chain (`None`: not said).
    fn fill(&mut self, chain: &[&'a El], section: &str) -> Option<Fill> {
        let g = |k: &str| get(chain, section, k);
        match g("draw:fill")? {
            "none" => Some(Fill::None),
            "solid" => Some(self.color(g("draw:fill-color"), g("draw:opacity")).map_or(Fill::None, Fill::Solid)),
            "gradient" => {
                let e = g("draw:fill-gradient-name").and_then(|n| self.st.named.get(&("draw:gradient".to_string(), n.to_string())).copied());
                match e.and_then(|e| self.gradient(e)) {
                    Some(f) => Some(f),
                    None => Some(self.color(g("draw:fill-color"), None).map_or(Fill::None, Fill::Solid)),
                }
            }
            "bitmap" => {
                if g("style:repeat") == Some("repeat") {
                    self.rep.note(Kind::Approximate, "tiled picture fills stretched");
                }
                let href = g("draw:fill-image-name").and_then(|n| self.st.named.get(&("draw:fill-image".to_string(), n.to_string())).copied()).and_then(|e| e.attr("xlink:href"));
                match href.and_then(|h| self.media(h)) {
                    Some(m) => Some(Fill::Picture(m)),
                    None => Some(self.color(g("draw:fill-color"), None).map_or(Fill::None, Fill::Solid)),
                }
            }
            "hatch" => {
                self.rep.note(Kind::Approximate, "hatched fills drawn solid");
                let hc = g("draw:fill-hatch-name").and_then(|n| self.st.named.get(&("draw:hatch".to_string(), n.to_string())).copied()).and_then(|e| e.attr("draw:color"));
                let solid = g("draw:fill-hatch-solid") == Some("true");
                let c = if solid { g("draw:fill-color") } else { hc.or(g("draw:fill-color")) };
                Some(self.color(c, None).map_or(Fill::None, Fill::Solid))
            }
            _ => None,
        }
    }

    fn gradient(&mut self, e: &El) -> Option<Fill> {
        let style = e.attr("draw:style").unwrap_or("linear");
        let a = rgb(e.attr("draw:start-color")).unwrap_or(0);
        let b = rgb(e.attr("draw:end-color")).unwrap_or(0xFF_FFFF);
        let angle = e.attr("draw:angle").and_then(|s| {
            let s = s.trim();
            if let Some(d) = s.strip_suffix("deg") {
                d.parse::<f64>().ok()
            } else if let Some(r) = s.strip_suffix("rad") {
                r.parse::<f64>().ok().map(f64::to_degrees)
            } else if let Some(g) = s.strip_suffix("grad") {
                g.parse::<f64>().ok().map(|g| g * 0.9)
            } else {
                s.parse::<f64>().ok().map(|v| v / 10.0)
            }
        });
        let angle = angle.filter(|v| v.is_finite()).unwrap_or(0.0);
        // Many-colour gradients (LibreOffice 7.6): stops from the start.
        let mut stops: Vec<(f64, u32)> = e.children("loext:gradient-stop").filter_map(|s| Some((s.attr("svg:offset")?.parse::<f64>().ok()?.clamp(0.0, 1.0), rgb(s.attr("loext:color-value"))?))).collect();
        if stops.len() < 2 {
            stops = vec![(0.0, a), (1.0, b)];
        }
        let stop = |p: f64, c: u32| GradientStop { pos: (p * 100_000.0).round() as u32, color: Color::rgb(c) };
        match style {
            "linear" | "axial" => {
                // ODF: 0° runs top to bottom, angles counter-clockwise;
                // ours: 0° left to right, clockwise.
                let ours = (90.0 - angle).rem_euclid(360.0);
                let list: Vec<GradientStop> = if style == "axial" {
                    if stops.len() > 2 {
                        self.rep.note(Kind::Approximate, "many-colour axial gradients drawn with their end colours");
                    }
                    let (s0, s1) = (stops[0].1, stops[stops.len() - 1].1);
                    vec![stop(0.0, s1), stop(0.5, s0), stop(1.0, s1)]
                } else {
                    stops.iter().map(|&(p, c)| stop(p, c)).collect()
                };
                Some(Fill::Gradient { stops: list, angle: (ours * 60_000.0) as i32, radial: false })
            }
            _ => {
                if style != "radial" {
                    self.rep.note(Kind::Approximate, "square, rectangular and elliptical gradients drawn radial");
                }
                // The start colour is the outside; ours run from the middle.
                let mut list: Vec<GradientStop> = stops.iter().rev().map(|&(p, c)| stop(1.0 - p, c)).collect();
                list.sort_by_key(|s| s.pos);
                Some(Fill::Gradient { stops: list, angle: 0, radial: true })
            }
        }
    }

    fn stroke(&mut self, chain: &[&'a El]) -> Option<Stroke> {
        let g = |k: &str| graphic(chain, k);
        let kind = g("draw:stroke")?;
        if kind == "none" {
            return Some(Stroke::none());
        }
        let color = self.color(g("svg:stroke-color"), g("svg:stroke-opacity")).unwrap_or(Color::rgb(0));
        let width = g("svg:stroke-width").and_then(emu).unwrap_or(0).clamp(0, 2_000_000);
        let mut s = Stroke::solid(color, if width == 0 { 9_525 } else { width });
        if kind == "dash" {
            let d = g("draw:stroke-dash").and_then(|n| self.st.named.get(&("draw:stroke-dash".to_string(), n.to_string())).copied());
            s.dash = match d {
                Some(d) => {
                    let n2 = d.attr("draw:dots2").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                    let n1 = d.attr("draw:dots1").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                    let len = |k: &str| d.attr(k).and_then(|v| pct(Some(v)).map(|p| p / 100.0 * width.max(9_525) as f64).or_else(|| emu(v).map(|e| e as f64)));
                    let l1 = len("draw:dots1-length").unwrap_or(0.0);
                    let l2 = len("draw:dots2-length").unwrap_or(0.0);
                    let unit = width.max(9_525) as f64;
                    if n1 > 0 && n2 > 0 && (l1 - l2).abs() > unit * 0.5 {
                        Dash::DashDot
                    } else if l1.max(l2) <= unit * 1.5 {
                        Dash::Dot
                    } else if l1.max(l2) >= unit * 6.0 {
                        Dash::LongDash
                    } else {
                        Dash::Dash
                    }
                }
                None => Dash::Dash,
            };
        }
        let arrow = |m: Option<&str>| -> Arrow {
            let Some(m) = m.filter(|m| !m.is_empty()) else { return Arrow::None };
            let n = m.to_ascii_lowercase().replace("_20_", " ");
            if n.contains("circle") || n.contains("oval") {
                Arrow::Oval
            } else if n.contains("diamond") || n.contains("square 45") || n.contains("square_45") {
                Arrow::Diamond
            } else if n.contains("concave") {
                Arrow::Stealth
            } else if n.contains("line") || n.contains("open") {
                Arrow::Open
            } else {
                Arrow::Triangle
            }
        };
        s.head = arrow(g("draw:marker-start"));
        s.tail = arrow(g("draw:marker-end"));
        Some(s)
    }

    /// A list level's bullet and indents, from a list style.
    fn list_level(&self, ls: Option<&'a El>, level: usize) -> ListLevel {
        let want = (level + 1).to_string();
        let e = ls.and_then(|l| l.els().find(|k| k.attr("text:level") == Some(&want)));
        let Some(e) = e else { return ListLevel { bullet: Bullet::Char { ch: '•', font: None, color: None, size_pct: None }, indent: None } };
        let bullet = match e.name.as_str() {
            "text:list-level-style-bullet" => match e.attr("text:bullet-char").and_then(|s| s.chars().next()) {
                None => Bullet::None,
                Some(ch) => {
                    let tp = e.child("style:text-properties");
                    let font = tp.and_then(|t| t.attr("style:font-name").and_then(|f| self.st.fonts.get(f).cloned()).or_else(|| t.attr("fo:font-family").map(|f| f.trim_matches(['\'', '"']).to_string()))).filter(|f| !f.is_empty()).map(|f| Arc::<str>::from(f.as_str()));
                    let color = tp.and_then(|t| if t.attr("style:use-window-font-color") == Some("true") { None } else { rgb(t.attr("fo:color")) }).map(Color::rgb);
                    let size_pct = pct(e.attr("text:bullet-relative-size")).or_else(|| pct(tp.and_then(|t| t.attr("fo:font-size")))).filter(|p| *p > 0.0 && (*p - 100.0).abs() > 0.01).map(|p| (p * 1000.0).clamp(25_000.0, 400_000.0) as u32);
                    Bullet::Char { ch, font, color, size_pct }
                }
            },
            "text:list-level-style-number" => {
                let f = e.attr("style:num-format").unwrap_or("");
                let pre = e.attr("style:num-prefix").unwrap_or("");
                let suf = e.attr("style:num-suffix").unwrap_or("");
                let base = match f {
                    "1" => "arabic",
                    "a" => "alphaLc",
                    "A" => "alphaUc",
                    "i" => "romanLc",
                    "I" => "romanUc",
                    _ => "",
                };
                if base.is_empty() {
                    Bullet::None
                } else {
                    let tail = match (pre, suf) {
                        ("(", ")") => "ParenBoth",
                        (_, ")") => "ParenR",
                        (_, ".") => "Period",
                        _ if base == "arabic" => "Plain",
                        _ => "Period",
                    };
                    Bullet::Number { scheme: format!("{base}{tail}").into(), start: e.attr("text:start-value").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, 32_767) }
                }
            }
            _ => Bullet::Char { ch: '•', font: None, color: None, size_pct: None },
        };
        let indent = e.child("style:list-level-properties").and_then(|p| {
            if let Some(a) = p.child("style:list-level-label-alignment") {
                let m = a.attr("fo:margin-left").and_then(length).unwrap_or(0);
                let t = a.attr("fo:text-indent").and_then(length).unwrap_or(0);
                return Some((m as i32, t as i32));
            }
            let sb = p.attr("text:space-before").and_then(length);
            let mw = p.attr("text:min-label-width").and_then(length);
            if sb.is_none() && mw.is_none() {
                return None;
            }
            let (sb, mw) = (sb.unwrap_or(0), mw.unwrap_or(0));
            Some(((sb + mw) as i32, -mw as i32))
        });
        ListLevel { bullet, indent: indent.map(|(a, b)| (a.clamp(-31_680, 31_680), b.clamp(-31_680, 31_680))) }
    }

    /// A text frame: its look from the shape's styles, its paragraphs.
    fn body(&mut self, e: &'a El, chain: &[&'a El], ph: bool, text_style: Option<&str>) -> TextBody {
        let mut t = TextBody::plain("");
        let g = |k: &str| graphic(chain, k);
        let pad = [g("fo:padding-left"), g("fo:padding-top"), g("fo:padding-right"), g("fo:padding-bottom")];
        if pad.iter().any(Option::is_some) || g("fo:padding").is_some() {
            let all = g("fo:padding").and_then(emu);
            let d = officina_slides::resolve::INSET;
            t.inset = Some([0, 1, 2, 3].map(|i| pad[i].and_then(emu).or(all).unwrap_or(d[i]).clamp(0, 10 * 914_400)));
        }
        t.anchor = match g("draw:textarea-vertical-align") {
            Some("top") => Some(Anchor::Top),
            Some("middle") => Some(Anchor::Middle),
            Some("bottom") => Some(Anchor::Bottom),
            _ => None,
        };
        let shrink = g("style:shrink-to-fit") == Some("true") || matches!(g("draw:fit-to-size"), Some("shrink-to-fit"));
        t.autofit = if shrink {
            Some(Autofit::Shrink { scale: 100_000, spacing_cut: 0 })
        } else {
            match g("draw:auto-grow-height") {
                Some("true") => Some(Autofit::Resize),
                Some("false") => Some(Autofit::None),
                _ => None,
            }
        };
        if g("draw:fit-to-size") == Some("true") || g("draw:fit-to-size") == Some("all") {
            self.rep.note(Kind::Approximate, "text stretched to its frame drawn at its size");
        }
        t.wrap = match g("fo:wrap-option") {
            Some("no-wrap") => Some(false),
            Some("wrap") => Some(true),
            _ => None,
        };
        if self.on_slide && (g("style:writing-mode").is_some_and(|m| m.starts_with("tb")) || get(chain, "style:paragraph-properties", "style:writing-mode").is_some_and(|m| m.starts_with("tb"))) {
            self.rep.note(Kind::Approximate, "vertical and stacked text drawn across");
        }
        if self.on_slide && chain.iter().any(|c| c.child("style:graphic-properties").and_then(|g| g.child("style:columns")).and_then(|c| c.attr("fo:column-count")).is_some_and(|n| n != "1")) {
            self.rep.note(Kind::Approximate, "text in columns drawn in one");
        }
        let mut doc = TextDoc::new();
        doc.blocks.remove(0);
        let mut bullets = Vec::new();
        // Outline levels below the first take their master's level style.
        let outline = chain.iter().find_map(|s| s.attr("style:name").and_then(|n| n.strip_suffix("-outline1")).map(str::to_string));
        let tstyle = self.st.chain("paragraph", text_style);
        let ctx = TextCtx { chain: chain.to_vec(), text_style: tstyle, outline, fallback: embedded_list(chain), ph };
        self.text_kids(e, &ctx, &mut doc, &mut bullets);
        if doc.blocks.len() == 0 {
            doc.push_para("", 0, vec![]);
        }
        t.doc = doc;
        t.bullets = bullets;
        t
    }

    fn text_kids(&mut self, e: &'a El, ctx: &TextCtx<'a>, doc: &mut TextDoc, bullets: &mut Vec<Bullet>) {
        for k in e.els() {
            match k.name.as_str() {
                "text:p" | "text:h" => self.paragraph(k, ctx, 0, if ctx.ph { Some(None) } else { None }, doc, bullets),
                "text:list" => self.list(k, ctx, 0, None, doc, bullets),
                "text:section" => self.text_kids(k, ctx, doc, bullets),
                _ => {}
            }
        }
    }

    fn list(&mut self, l: &'a El, ctx: &TextCtx<'a>, level: usize, inherited: Option<&'a El>, doc: &mut TextDoc, bullets: &mut Vec<Bullet>) {
        let own = l.attr("text:style-name").and_then(|n| self.st.lists.get(n).copied());
        let style = own.or(inherited).or(ctx.fallback);
        let level = level.min(MAX_LEVELS - 1);
        for item in l.els() {
            let header = match item.name.as_str() {
                "text:list-item" => false,
                "text:list-header" => true,
                _ => continue,
            };
            let ls = item.attr("text:style-override").and_then(|n| self.st.lists.get(n).copied()).or(style);
            for c in item.els() {
                match c.name.as_str() {
                    "text:p" | "text:h" => {
                        let lv = self.list_level(ls, level);
                        let b = if header { Bullet::None } else { lv.bullet };
                        self.paragraph_in(c, ctx, level, Some(Some(b)), lv.indent, doc, bullets);
                    }
                    "text:list" => {
                        if doc.blocks.len() < 100_000 {
                            self.list(c, ctx, level + 1, ls, doc, bullets);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn paragraph(&mut self, p: &'a El, ctx: &TextCtx<'a>, level: usize, list: Option<Option<Bullet>>, doc: &mut TextDoc, bullets: &mut Vec<Bullet>) {
        self.paragraph_in(p, ctx, level, list, None, doc, bullets)
    }

    #[allow(clippy::too_many_arguments)]
    fn paragraph_in(&mut self, p: &'a El, ctx: &TextCtx<'a>, level: usize, list: Option<Option<Bullet>>, indent: Option<(i32, i32)>, doc: &mut TextDoc, bullets: &mut Vec<Bullet>) {
        if doc.blocks.len() >= 100_000 {
            return;
        }
        let level_chain = if level > 0 { ctx.outline.as_ref().map(|o| self.st.chain("presentation", Some(&format!("{o}-outline{}", level + 1)))).filter(|c| !c.is_empty()) } else { None };
        let base: &[&'a El] = level_chain.as_deref().unwrap_or(&ctx.chain);
        let pchain = self.st.chain("paragraph", p.attr("text:style-name"));
        let chars = self.chars(&pchain).over(&self.chars(&ctx.text_style).over(&self.chars(base)));
        let mut props = officina_slides::resolve::para_over(&self.para(&pchain), &officina_slides::resolve::para_over(&self.para(&ctx.text_style), &self.para(base)));
        if let Some((start, first)) = indent {
            if props.indent_start.is_none_or(|v| v == 0) && props.first_line.is_none_or(|v| v == 0) {
                props.indent_start = Some(start);
                props.first_line = Some(first);
            }
        }
        let mut r = Runs::default();
        self.inline(p, &chars, doc, &mut r, 0);
        let Runs { text, mut runs, .. } = r;
        if runs.is_empty() || runs[0].0 != 0 {
            let id = doc.attrs.intern(chars.clone());
            runs.insert(0, (0, id));
        }
        let lvl = level.min(MAX_LEVELS - 1) as u16;
        let k = doc.push_para(&text, lvl, runs);
        match list {
            Some(None) | Some(Some(Bullet::None)) => props.list = Some(None),
            Some(Some(b)) => {
                let kind = if matches!(b, Bullet::Number { .. }) { ListKind::Numbered } else { ListKind::Bullet };
                let id = match bullets.iter().position(|x| *x == b) {
                    Some(i) => i,
                    None => {
                        bullets.push(b);
                        bullets.len() - 1
                    }
                } as u32
                    + 1;
                props.list = Some(Some(ListRef { id, kind, level: lvl as u8 }));
            }
            None => props.list = None,
        }
        if props != ParaProps::default() {
            doc.blocks.update(k, |b| {
                if let Some(p) = b.as_para_mut() {
                    p.props = props.clone();
                }
            });
        }
    }

    fn inline(&mut self, e: &'a El, a: &CharAttrs, doc: &mut TextDoc, r: &mut Runs, depth: usize) {
        if depth > 32 {
            return;
        }
        for n in &e.kids {
            match n {
                Node::Text(t) => r.text(doc, t, a),
                Node::El(c) => match c.name.as_str() {
                    "text:span" => {
                        let ch = self.st.chain("text", c.attr("text:style-name"));
                        let a2 = self.chars(&ch).over(a);
                        self.inline(c, &a2, doc, r, depth + 1);
                    }
                    "text:a" => {
                        let ch = self.st.chain("text", c.attr("text:style-name"));
                        let mut a2 = self.chars(&ch).over(a);
                        if let Some(h) = c.attr("xlink:href") {
                            let l = h.to_ascii_lowercase();
                            if l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:") {
                                a2.link = Some(h.into());
                            }
                        }
                        self.inline(c, &a2, doc, r, depth + 1);
                    }
                    "text:s" => {
                        let n = c.attr("text:c").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 1000);
                        r.raw(doc, &" ".repeat(n), a);
                        r.space = true;
                    }
                    "text:tab" => {
                        r.raw(doc, "\t", a);
                        r.space = false;
                    }
                    "text:line-break" => {
                        r.raw(doc, "\u{2028}", a);
                        r.space = true;
                    }
                    "text:page-number" => {
                        r.raw(doc, "‹#›", a);
                        r.space = false;
                    }
                    "text:date" | "text:time" => {
                        if self.on_slide && c.attr("text:fixed") != Some("true") {
                            self.rep.note(Kind::Approximate, "date fields kept as the date they showed");
                        }
                        self.inline(c, a, doc, r, depth + 1);
                    }
                    "presentation:header" | "presentation:footer" | "presentation:date-time" | "text:note" | "office:annotation" | "office:annotation-end" | "text:bookmark" | "text:bookmark-start" | "text:bookmark-end" | "text:soft-page-break" | "draw:frame" | "draw:custom-shape" => {}
                    _ => self.inline(c, a, doc, r, depth + 1),
                },
            }
        }
    }

    /// Where a shape is: its unturned box and its turn (60,000ths of a
    /// degree, clockwise).
    fn place(&mut self, e: &El) -> (Rect, i32) {
        let w = e.attr("svg:width").and_then(emu).unwrap_or(0).clamp(0, EMU_MAX_SIDE * 4);
        let h = e.attr("svg:height").and_then(emu).unwrap_or(0).clamp(0, EMU_MAX_SIDE * 4);
        let x0 = e.attr("svg:x").and_then(emu).unwrap_or(0);
        let y0 = e.attr("svg:y").and_then(emu).unwrap_or(0);
        let Some(tf) = e.attr("draw:transform") else { return (Rect::new(x0, y0, w, h), 0) };
        // x' = a x + c y + e, y' = b x + d y + f
        let mut m = [1.0f64, 0.0, 0.0, 1.0, 0.0, 0.0];
        let mut turn = 0.0f64;
        let mul = |m: [f64; 6], n: [f64; 6]| -> [f64; 6] {
            // n applied after m.
            [n[0] * m[0] + n[2] * m[1], n[1] * m[0] + n[3] * m[1], n[0] * m[2] + n[2] * m[3], n[1] * m[2] + n[3] * m[3], n[0] * m[4] + n[2] * m[5] + n[4], n[1] * m[4] + n[3] * m[5] + n[5]]
        };
        let mut rest = tf;
        for _ in 0..16 {
            let Some(open) = rest.find('(') else { break };
            let Some(close) = rest[open..].find(')') else { break };
            let name = rest[..open].trim().trim_start_matches(',').trim();
            let args: Vec<&str> = rest[open + 1..open + close].split([' ', ',']).filter(|s| !s.is_empty()).collect();
            rest = &rest[open + close + 1..];
            let n = |i: usize| args.get(i).and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite());
            let len = |i: usize| args.get(i).and_then(|v| emu(v).map(|e| e as f64).or_else(|| v.parse::<f64>().ok().map(|v| v * 360.0)));
            match name {
                "rotate" => {
                    let t = n(0).unwrap_or(0.0);
                    turn += t;
                    m = mul(m, [t.cos(), -t.sin(), t.sin(), t.cos(), 0.0, 0.0]);
                }
                "translate" => m = mul(m, [1.0, 0.0, 0.0, 1.0, len(0).unwrap_or(0.0), len(1).unwrap_or(0.0)]),
                "scale" => {
                    let sx = n(0).unwrap_or(1.0);
                    m = mul(m, [sx, 0.0, 0.0, n(1).unwrap_or(sx), 0.0, 0.0]);
                }
                "skewX" | "skewY" => self.rep.note(Kind::Approximate, "skewed shapes drawn unskewed"),
                "matrix" => {
                    let v: Vec<f64> = (0..4).map(|i| n(i).unwrap_or(0.0)).collect();
                    m = mul(m, [v[0], v[1], v[2], v[3], len(4).unwrap_or(0.0), len(5).unwrap_or(0.0)]);
                    turn += (-v[1]).atan2(v[0]);
                }
                _ => {}
            }
        }
        let (cx, cy) = (x0 as f64 + w as f64 / 2.0, y0 as f64 + h as f64 / 2.0);
        let (px, py) = (m[0] * cx + m[2] * cy + m[4], m[1] * cx + m[3] * cy + m[5]);
        let clamp = |v: f64| if v.is_finite() { v.clamp(-1e12, 1e12) as i64 } else { 0 };
        let rot = (-turn.to_degrees()).rem_euclid(360.0);
        let rot = if rot.is_finite() { (rot * 60_000.0).round() as i32 % 21_600_000 } else { 0 };
        (Rect::new(clamp(px - w as f64 / 2.0), clamp(py - h as f64 / 2.0), w, h), rot)
    }

    fn tree(&mut self, parent: &'a El, depth: usize, ids: &mut HashMap<String, ShapeId>) -> Vec<Shape> {
        let mut out = Vec::new();
        for e in parent.els() {
            if let Some(s) = self.shape(e, depth, ids) {
                out.push(s);
            }
        }
        out
    }

    fn has_text(e: &El) -> bool {
        fn any(e: &El, d: usize) -> bool {
            d < 256 && e.kids.iter().any(|k| match k {
                Node::Text(t) => !t.trim().is_empty(),
                Node::El(c) => matches!(c.name.as_str(), "text:s" | "text:tab" | "text:page-number") || any(c, d + 1),
            })
        }
        e.els().any(|k| matches!(k.name.as_str(), "text:p" | "text:h" | "text:list") && any(k, 0))
    }

    fn shape(&mut self, e: &'a El, depth: usize, ids: &mut HashMap<String, ShapeId>) -> Option<Shape> {
        let name = e.name.as_str();
        match name {
            "draw:frame" | "draw:custom-shape" | "draw:rect" | "draw:ellipse" | "draw:circle" | "draw:line" | "draw:connector" | "draw:polygon" | "draw:polyline" | "draw:path" | "draw:g" | "draw:caption" | "draw:regular-polygon" => {}
            "draw:measure" | "draw:control" | "dr3d:scene" | "draw:applet" | "draw:plugin" | "draw:floating-frame" => {
                self.rep.note_at(Kind::Dropped, "3-D scenes, measures, form controls and applets", || name.to_string());
                return None;
            }
            "office:annotation" => {
                self.rep.note(Kind::Dropped, "slide comments");
                return None;
            }
            _ => return None,
        }
        if depth > MAX_DEPTH - 1 {
            self.rep.note(Kind::Dropped, "groups nested too deep");
            return None;
        }
        self.shapes += 1;
        if self.shapes > MAX_SHAPES * 16 {
            self.rep.note(Kind::Dropped, "shapes past the deck's limit");
            return None;
        }
        let ph = match e.attr("presentation:class").map(ph_kind) {
            Some(Some(None)) => return None,
            Some(Some(Some(k))) => Some(Placeholder { kind: k, idx: 0 }),
            _ => None,
        };
        let id = self.deck.new_id();
        for k in ["draw:id", "xml:id"] {
            if let Some(v) = e.attr(k) {
                ids.insert(v.to_string(), id);
            }
        }
        let chain = self.shape_chain(e);
        let (frame, rot) = self.place(e);
        let mut s = Shape::new(id, frame, ShapeKind::Group { child: Rect::default(), shapes: Vec::new() });
        s.rot = rot;
        s.ph = ph;
        s.name = e.attr("draw:name").unwrap_or("").into();
        if let Some(d) = e.child("svg:desc").or_else(|| e.child("svg:title")) {
            s.descr = d.text().chars().take(4096).collect::<String>().into();
        }
        s.hidden = e.attr("draw:display") == Some("none") || graphic(&chain, "draw:visible") == Some("false");
        if let Some(m) = graphic(&chain, "style:mirror") {
            s.flip_h = m.contains("horizontal");
            s.flip_v = m.contains("vertical");
        }
        let fill = self.fill(&chain, "style:graphic-properties");
        let line = self.stroke(&chain);
        let text_style = e.attr("draw:text-style-name");
        match name {
            "draw:g" => {
                let kids = self.tree(e, depth + 1, ids);
                if kids.is_empty() {
                    return None;
                }
                let mut bb: Option<(i64, i64, i64, i64)> = None;
                for k in &kids {
                    if let Some(f) = k.frame {
                        bb = Some(match bb {
                            None => (f.x, f.y, f.x + f.w, f.y + f.h),
                            Some((a, b, c, d)) => (a.min(f.x), b.min(f.y), c.max(f.x + f.w), d.max(f.y + f.h)),
                        });
                    }
                }
                let (a, b, c, d) = bb.unwrap_or((0, 0, 1, 1));
                let r = Rect::new(a, b, (c - a).max(1), (d - b).max(1));
                s.frame = Some(r);
                s.rot = 0;
                s.kind = ShapeKind::Group { child: r, shapes: kids };
            }
            "draw:frame" => {
                let first = e.els().find(|k| !matches!(k.name.as_str(), "svg:title" | "svg:desc" | "draw:contour-polygon" | "draw:contour-path" | "office:event-listeners" | "draw:glue-point" | "draw:image-map"));
                match first.map(|k| k.name.as_str()) {
                    Some("draw:text-box") => {
                        let tb = first.unwrap();
                        let text = self.body(tb, &chain, ph.is_some(), text_style);
                        s.kind = ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(fill.unwrap_or(Fill::None)), line: Some(line.unwrap_or_else(Stroke::none)), text: Some(text) };
                    }
                    Some("draw:image") => {
                        if graphic(&chain, "fo:clip").is_some_and(|c| c.starts_with("rect(") && c.chars().any(|ch| ch.is_ascii_digit() && ch != '0')) {
                            self.rep.note(Kind::Approximate, "picture crops left off");
                        }
                        let media = e.children("draw:image").filter_map(|i| i.attr("xlink:href")).find_map(|h| self.media(h));
                        let Some(media) = media else {
                            if e.children("draw:image").any(|i| i.child("office:binary-data").is_some()) {
                                self.rep.note(Kind::Dropped, "pictures held inline in the XML");
                            }
                            return None;
                        };
                        let tb = e.child("draw:image").filter(|i| Self::has_text(i));
                        if tb.is_some() {
                            self.rep.note(Kind::Approximate, "text on pictures left off");
                        }
                        s.kind = ShapeKind::Picture { media, crop: [0; 4], line: line.filter(|l| l.color.is_some()) };
                    }
                    Some("draw:object") | Some("draw:object-ole") => {
                        let media = e.children("draw:image").filter_map(|i| i.attr("xlink:href")).find_map(|h| self.media(h));
                        match media {
                            Some(m) => {
                                self.rep.note(Kind::Approximate, "charts and embedded objects shown as their picture");
                                s.kind = ShapeKind::Picture { media: m, crop: [0; 4], line: None };
                            }
                            None => {
                                self.rep.note(Kind::Dropped, "charts and embedded objects without a picture");
                                if ph.is_none() {
                                    return None;
                                }
                                s.kind = ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(Fill::None), line: Some(Stroke::none()), text: Some(TextBody::plain("")) };
                            }
                        }
                    }
                    Some("table:table") => {
                        let t = first.unwrap();
                        return Some(self.table(s, t));
                    }
                    Some("draw:plugin") => {
                        let p = first.unwrap();
                        let m = p.attr("xlink:href").and_then(|h| self.media(h));
                        match m.filter(|m| self.deck.media(*m).is_some_and(|x| x.mime.starts_with("video/"))) {
                            Some(m) => {
                                let poster = e.children("draw:image").filter_map(|i| i.attr("xlink:href")).find_map(|h| self.media(h));
                                s.kind = ShapeKind::Video { media: m, poster };
                            }
                            None => {
                                self.rep.note(Kind::Dropped, "sounds and plug-ins");
                                return None;
                            }
                        }
                    }
                    _ => {
                        if ph.is_none() {
                            return None;
                        }
                        s.kind = ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(fill.unwrap_or(Fill::None)), line: Some(line.unwrap_or_else(Stroke::none)), text: Some(TextBody::plain("")) };
                    }
                }
            }
            _ => {
                let geom = match name {
                    "draw:custom-shape" => {
                        let eg = e.child("draw:enhanced-geometry");
                        if let Some(eg) = eg {
                            if eg.attr("draw:mirror-horizontal") == Some("true") {
                                s.flip_h = !s.flip_h;
                            }
                            if eg.attr("draw:mirror-vertical") == Some("true") {
                                s.flip_v = !s.flip_v;
                            }
                        }
                        eg.map(|eg| self.custom_geom(eg, frame)).unwrap_or_else(|| Geom::preset("rect"))
                    }
                    "draw:rect" | "draw:caption" => {
                        if name == "draw:caption" {
                            self.rep.note(Kind::Approximate, "callouts drawn as boxes");
                        }
                        match e.attr("draw:corner-radius").and_then(emu).filter(|r| *r > 0) {
                            Some(r) => Geom::Preset { name: "roundRect".into(), adj: vec![("adj".into(), (r * 100_000 / frame.w.min(frame.h).max(1)).clamp(0, 50_000))] },
                            None => Geom::preset("rect"),
                        }
                    }
                    "draw:ellipse" | "draw:circle" => {
                        if e.attr("draw:kind").is_some_and(|k| k != "full") {
                            self.rep.note(Kind::Approximate, "arcs, sections and cuts drawn as whole ellipses");
                        }
                        Geom::preset("ellipse")
                    }
                    "draw:regular-polygon" => {
                        let n = e.attr("draw:corners").and_then(|c| c.parse::<u32>().ok()).unwrap_or(5);
                        Geom::preset(match (e.attr("draw:concave") == Some("true"), n) {
                            (true, 4) => "star4",
                            (true, 5) => "star5",
                            (true, 6) => "star6",
                            (true, 8) => "star8",
                            (true, _) => "star5",
                            (false, 3) => "triangle",
                            (false, 4) => "diamond",
                            (false, 6) => "hexagon",
                            (false, 8) => "octagon",
                            _ => "pentagon",
                        })
                    }
                    "draw:line" | "draw:connector" => {
                        if name == "draw:connector" && e.attr("draw:type").is_some_and(|t| t != "line") {
                            self.rep.note(Kind::Approximate, "bent and curved connectors drawn straight");
                        }
                        let p = |k: &str| e.attr(k).and_then(emu).unwrap_or(0);
                        let (x1, y1, x2, y2) = (p("svg:x1"), p("svg:y1"), p("svg:x2"), p("svg:y2"));
                        s.frame = Some(Rect::new(x1.min(x2), y1.min(y2), (x2 - x1).abs(), (y2 - y1).abs()));
                        s.flip_h = x2 < x1;
                        s.flip_v = y2 < y1;
                        s.rot = 0;
                        Geom::preset(if name == "draw:connector" { "straightConnector1" } else { "line" })
                    }
                    "draw:polygon" | "draw:polyline" | "draw:path" => {
                        let vb = geom::view_box(e.attr("svg:viewBox")).unwrap_or([0.0, 0.0, frame.w.max(1) as f64 / 360.0, frame.h.max(1) as f64 / 360.0]);
                        let paths = if name == "draw:path" { e.attr("svg:d").and_then(|d| geom::svg_path(d, vb)) } else { e.attr("draw:points").and_then(|p| geom::points(p, vb, name == "draw:polygon")) };
                        match paths {
                            Some(p) => Geom::Custom(p),
                            None => return None,
                        }
                    }
                    _ => Geom::preset("rect"),
                };
                let text = if Self::has_text(e) || ph.is_some() { Some(self.body(e, &chain, ph.is_some(), text_style)) } else { None };
                let is_line = matches!(&geom, Geom::Preset { name, .. } if &**name == "line" || &**name == "straightConnector1");
                let fill = if is_line { Fill::None } else { fill.unwrap_or(Fill::None) };
                s.kind = ShapeKind::Geom { geom: Some(geom), fill: Some(fill), line: Some(line.unwrap_or_else(Stroke::none)), text };
            }
        }
        self.loaded.shapes += 1;
        Some(s)
    }

    fn custom_geom(&mut self, eg: &El, frame: Rect) -> Geom {
        let ty = eg.attr("draw:type").unwrap_or("");
        let mods: Vec<i64> = eg.attr("draw:modifiers").unwrap_or("").split([' ', ',']).filter(|s| !s.is_empty()).filter_map(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite()).map(|v| v.round().clamp(-1e9, 1e9) as i64).collect();
        let logw = frame.w as f64 / 360.0;
        let logh = frame.h as f64 / 360.0;
        if let Some(p) = ty.strip_prefix("ooxml-").filter(|p| *p != "non-primitive" && !p.is_empty()) {
            let adj = match mods.len() {
                0 => Vec::new(),
                1 => vec![("adj".into(), mods[0])],
                _ => mods.iter().enumerate().map(|(i, v)| (format!("adj{}", i + 1).into(), *v)).collect(),
            };
            return Geom::Preset { name: p.into(), adj };
        }
        let preset = preset_of(ty);
        if let Some(p) = preset {
            if officina_slides::geom::paths(&Geom::preset(p), 100, 100).1 || eg.attr("draw:enhanced-path").is_none() {
                if p == "roundRect" {
                    if let Some(m) = mods.first() {
                        return Geom::Preset { name: p.into(), adj: vec![("adj".into(), (m * 100_000 / 21_600).clamp(0, 50_000))] };
                    }
                }
                return Geom::preset(p);
            }
        }
        if let Some(paths) = geom::enhanced(eg, logw, logh) {
            return Geom::Custom(paths);
        }
        if let Some(p) = preset {
            return Geom::preset(p);
        }
        self.rep.note_at(Kind::Approximate, "shapes of kinds not in the set drawn as boxes", || ty.to_string());
        Geom::preset("rect")
    }

    /// A table drawn as a group of cells, each a rectangle with its text.
    fn table(&mut self, mut g: Shape, t: &'a El) -> Shape {
        self.rep.note(Kind::Approximate, "tables kept as groups of cells");
        let frame = g.frame.unwrap_or_default();
        let mut cols: Vec<i64> = Vec::new();
        let mut rows: Vec<&'a El> = Vec::new();
        fn walk<'a>(r: &Reader<'a>, e: &'a El, cols: &mut Vec<i64>, rows: &mut Vec<&'a El>, d: usize) {
            for k in e.els() {
                match k.name.as_str() {
                    "table:table-column" => {
                        let n = k.attr("table:number-columns-repeated").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 64);
                        let w = get(&r.st.chain("table-column", k.attr("table:style-name")), "style:table-column-properties", "style:column-width").and_then(emu).unwrap_or(0).max(0);
                        for _ in 0..n {
                            if cols.len() < 256 {
                                cols.push(w);
                            }
                        }
                    }
                    "table:table-row" => {
                        if rows.len() < 1000 {
                            rows.push(k);
                        }
                    }
                    "table:table-columns" | "table:table-header-columns" | "table:table-column-group" | "table:table-rows" | "table:table-header-rows" | "table:table-row-group" if d < 4 => walk(r, k, cols, rows, d + 1),
                    _ => {}
                }
            }
        }
        walk(self, t, &mut cols, &mut rows, 0);
        let ncols = cols.len().max(rows.iter().map(|r| r.els().filter(|c| c.name.starts_with("table:")).count()).max().unwrap_or(1)).max(1);
        cols.resize(ncols, 0);
        if cols.contains(&0) {
            let known: i64 = cols.iter().sum();
            let unknown = cols.iter().filter(|w| **w == 0).count() as i64;
            let each = ((frame.w - known).max(0) / unknown.max(1)).max(frame.w / ncols as i64).max(1);
            for c in cols.iter_mut().filter(|w| **w == 0) {
                *c = each;
            }
        }
        let heights: Vec<i64> = rows
            .iter()
            .map(|r| {
                let ch = self.st.chain("table-row", r.attr("table:style-name"));
                get(&ch, "style:table-row-properties", "style:row-height").or_else(|| get(&ch, "style:table-row-properties", "style:min-row-height")).and_then(emu).unwrap_or(0).max(0)
            })
            .collect();
        let even = frame.h / rows.len().max(1) as i64;
        let heights: Vec<i64> = heights.into_iter().map(|h| if h == 0 { even.max(1) } else { h }).collect();
        let mut cells = Vec::new();
        let mut y = 0i64;
        for (ri, tr) in rows.iter().enumerate() {
            let mut ci = 0usize;
            let mut x = 0i64;
            for tc in tr.els() {
                let rep = tc.attr("table:number-columns-repeated").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 64);
                match tc.name.as_str() {
                    "table:covered-table-cell" => {
                        for _ in 0..rep {
                            x += cols.get(ci).copied().unwrap_or(0);
                            ci += 1;
                        }
                    }
                    "table:table-cell" => {
                        for _ in 0..rep {
                            if ci >= cols.len() {
                                break;
                            }
                            let span = tc.attr("table:number-columns-spanned").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 256);
                            let rspan = tc.attr("table:number-rows-spanned").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 1000);
                            let w: i64 = (ci..ci + span).map(|i| cols.get(i).copied().unwrap_or(0)).sum();
                            let h: i64 = (ri..ri + rspan).map(|i| heights.get(i).copied().unwrap_or(0)).sum();
                            let chain = self.st.chain("table-cell", tc.attr("table:style-name"));
                            let fill = self.fill(&chain, "style:graphic-properties").or_else(|| rgb(get(&chain, "style:table-cell-properties", "fo:background-color")).map(|c| Fill::Solid(Color::rgb(c)))).unwrap_or(Fill::None);
                            let mut text = self.body(tc, &chain, false, None);
                            if text.anchor.is_none() {
                                text.anchor = Some(Anchor::Top);
                            }
                            let id = self.deck.new_id();
                            let mut s = Shape::new(id, Rect::new(x, y, w, h), ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(fill), line: Some(Stroke::solid(Color::rgb(0x40_40_40), 6_350)), text: Some(text) });
                            s.name = format!("Cell {} {}", ri + 1, ci + 1).into();
                            cells.push(s);
                            x += cols.get(ci).copied().unwrap_or(0);
                            ci += 1;
                        }
                    }
                    _ => {}
                }
            }
            y += heights.get(ri).copied().unwrap_or(0);
        }
        let total_w: i64 = cols.iter().sum();
        let h = y.max(frame.h).max(1);
        g.kind = ShapeKind::Group { child: Rect::new(0, 0, total_w.max(1), h), shapes: cells };
        g.frame = Some(Rect::new(frame.x, frame.y, total_w.max(1), h));
        self.loaded.shapes += 1;
        g
    }

    /// A master's level style from one of its presentation styles.
    fn level_style(&self, name: &str, level: usize) -> Option<LevelStyle> {
        let chain = self.st.chain("presentation", Some(name));
        if chain.is_empty() || self.st.by.get(&("presentation".to_string(), name.to_string())).is_none() {
            return None;
        }
        let ll = embedded_list(&chain).map(|l| self.list_level(Some(l), level));
        let mut para = self.para(&chain);
        if let Some((s, f)) = ll.as_ref().and_then(|l| l.indent) {
            if para.indent_start.is_none() {
                para.indent_start = Some(s);
                para.first_line = Some(f);
            }
        }
        Some(LevelStyle { chars: self.chars(&chain), color: None, para, bullet: ll.map(|l| l.bullet) })
    }

    fn transition(&mut self, chain: &[&'a El]) -> Option<Transition> {
        let g = |k: &str| get(chain, "style:drawing-page-properties", k);
        let auto = g("presentation:transition-type") == Some("automatic");
        let after_ms = if auto { g("presentation:duration").and_then(millis) } else { None };
        let ty = g("smil:type");
        if ty.is_none() && after_ms.is_none() {
            if g("presentation:transition-style").is_some_and(|s| s != "none") {
                self.rep.note(Kind::Approximate, "transitions not in the set drawn as fades");
                return Some(Transition { kind: TransitionKind::Fade, ms: 500, on_click: true, after_ms: None });
            }
            return None;
        }
        let ms = match g("presentation:transition-speed") {
            Some("slow") => 1000,
            Some("medium") => 750,
            _ => 500,
        };
        let sub = g("smil:subtype").unwrap_or("");
        let rev = g("smil:direction") == Some("reverse");
        let from = |s: &str| match s {
            "fromRight" => Dir::Left,
            "fromBottom" => Dir::Up,
            "fromLeft" => Dir::Right,
            _ => Dir::Down,
        };
        let kind = match ty.unwrap_or("") {
            "" => TransitionKind::Cut,
            "fade" => TransitionKind::Fade,
            "pushWipe" => TransitionKind::Push(from(sub)),
            "slideWipe" => {
                if rev {
                    TransitionKind::Uncover(from(sub))
                } else {
                    TransitionKind::Cover(from(sub))
                }
            }
            "barWipe" => TransitionKind::Wipe(match (sub, rev) {
                ("topToBottom", false) => Dir::Down,
                ("topToBottom", true) => Dir::Up,
                (_, false) => Dir::Right,
                (_, true) => Dir::Left,
            }),
            "barnDoorWipe" => TransitionKind::Split { vertical: sub == "vertical", out: !rev },
            "irisWipe" => TransitionKind::Zoom { out: !rev },
            "dissolve" => TransitionKind::Dissolve,
            other => {
                self.rep.note_at(Kind::Approximate, "transitions not in the set drawn as fades", || other.to_string());
                TransitionKind::Fade
            }
        };
        Some(Transition { kind, ms, on_click: !auto, after_ms })
    }

    fn builds(&mut self, page: &El, ids: &HashMap<String, ShapeId>) -> Vec<Build> {
        let mut out = Vec::new();
        let Some(root) = page.children("anim:par").find(|p| p.attr("presentation:node-type") == Some("timing-root")) else { return out };
        fn walk<'e>(e: &'e El, d: usize, out: &mut Vec<&'e El>, interactive: &mut bool) {
            if d > 16 || out.len() > 10_000 {
                return;
            }
            for k in e.els() {
                if k.attr("presentation:node-type") == Some("interactive-sequence") {
                    *interactive = true;
                    continue;
                }
                if k.attr("presentation:preset-class").is_some() {
                    out.push(k);
                } else {
                    walk(k, d + 1, out, interactive);
                }
            }
        }
        let mut nodes = Vec::new();
        let mut interactive = false;
        walk(root, 0, &mut nodes, &mut interactive);
        if interactive {
            self.rep.note(Kind::Dropped, "builds started by clicking a shape");
        }
        fn target(e: &El, d: usize) -> Option<&str> {
            if d > 8 {
                return None;
            }
            e.attr("smil:targetElement").or_else(|| e.els().find_map(|k| target(k, d + 1)))
        }
        fn longest(e: &El, d: usize) -> u32 {
            if d > 8 {
                return 0;
            }
            let own = e.attr("smil:dur").and_then(millis).unwrap_or(0);
            e.els().map(|k| longest(k, d + 1)).max().unwrap_or(0).max(own)
        }
        fn iterates(e: &El, d: usize) -> bool {
            d <= 8 && (e.name == "anim:iterate" || e.els().any(|k| iterates(k, d + 1)))
        }
        for n in nodes {
            let class = n.attr("presentation:preset-class").unwrap_or("");
            let preset = n.attr("presentation:preset-id").unwrap_or("");
            let dir = match n.attr("presentation:preset-sub-type").unwrap_or("") {
                "from-left" => Dir::Left,
                "from-top" => Dir::Up,
                "from-right" => Dir::Right,
                _ => Dir::Down,
            };
            let effect = match class {
                "entrance" => {
                    if preset == "ooo-entrance-appear" {
                        Effect::Appear
                    } else if preset.starts_with("ooo-entrance-fade") {
                        Effect::Fade
                    } else if preset.starts_with("ooo-entrance-fly-in") {
                        Effect::FlyIn(dir)
                    } else if preset == "ooo-entrance-wipe" {
                        Effect::Wipe(dir)
                    } else if preset.starts_with("ooo-entrance-zoom") {
                        Effect::Zoom
                    } else {
                        self.rep.note_at(Kind::Approximate, "builds not in the set run as appear", || preset.to_string());
                        Effect::Appear
                    }
                }
                "exit" => {
                    if preset.starts_with("ooo-exit-fade") {
                        Effect::FadeOut
                    } else {
                        if preset != "ooo-exit-disappear" {
                            self.rep.note_at(Kind::Approximate, "exits not in the set run as disappear", || preset.to_string());
                        }
                        Effect::Disappear
                    }
                }
                _ => {
                    self.rep.note(Kind::Dropped, "emphasis and motion-path builds");
                    continue;
                }
            };
            let Some(shape) = target(n, 0).and_then(|t| ids.get(t)).copied() else { continue };
            let trigger = match n.attr("presentation:node-type") {
                Some("on-click") => Trigger::OnClick,
                Some("after-previous") => Trigger::AfterPrevious,
                _ => Trigger::WithPrevious,
            };
            let ms = match longest(n, 0) {
                0 | 1 if matches!(effect, Effect::Appear | Effect::Disappear) => 0,
                0 => 500,
                v => v.min(60_000),
            };
            let delay_ms = n.attr("smil:begin").and_then(millis).unwrap_or(0).min(60_000);
            out.push(Build { shape, effect, trigger, ms, delay_ms, by_paragraph: iterates(n, 0) });
        }
        out
    }
}

struct TextCtx<'a> {
    chain: Vec<&'a El>,
    text_style: Vec<&'a El>,
    outline: Option<String>,
    fallback: Option<&'a El>,
    ph: bool,
}

pub fn read(data: &[u8]) -> Result<(Deck, Loaded), OdpError> {
    let archive = Archive::open(data)?;
    if let Some(m) = archive.read_name("mimetype")? {
        let m = String::from_utf8_lossy(&m);
        if !m.trim().starts_with("application/vnd.oasis.opendocument.presentation") {
            return Err(OdpError::Bad(format!("the package is {}", m.trim())));
        }
    }
    let content = part(&archive, "content.xml")?.ok_or_else(|| OdpError::Bad("no content.xml".into()))?;
    let styles = part(&archive, "styles.xml")?;
    let pres = content.child("office:body").and_then(|b| b.child("office:presentation")).ok_or_else(|| OdpError::Bad("no presentation body".into()))?;
    let mut st = Styles::default();
    if let Some(s) = &styles {
        st.font_decls(s.child("office:font-face-decls"));
        st.add(s.child("office:styles"));
        st.add(s.child("office:automatic-styles"));
    }
    st.font_decls(content.child("office:font-face-decls"));
    st.add(content.child("office:automatic-styles"));
    let mut r = Reader { archive: &archive, st, deck: Deck::empty(), rep: Report::default(), loaded: Loaded::default(), media: HashMap::new(), shapes: 0, on_slide: false };

    // Masters, one layout each.
    let mut layout_of: HashMap<String, LayoutId> = HashMap::new();
    let masters: Vec<&El> = styles.as_ref().and_then(|s| s.child("office:master-styles")).map(|m| m.children("style:master-page").collect()).unwrap_or_default();
    let mut sized = false;
    for mp in masters.into_iter().take(MAX_MASTERS) {
        let Some(sname) = mp.attr("style:name") else { continue };
        if !sized {
            if let Some(pl) = mp.attr("style:page-layout-name").and_then(|n| r.st.page_layouts.get(n)).and_then(|p| p.child("style:page-layout-properties")) {
                let w = pl.attr("fo:page-width").and_then(emu);
                let h = pl.attr("fo:page-height").and_then(emu);
                if let (Some(w), Some(h)) = (w, h) {
                    r.deck.size = (w.clamp(EMU_MIN_SIDE, EMU_MAX_SIDE), h.clamp(EMU_MIN_SIDE, EMU_MAX_SIDE));
                    sized = true;
                }
            }
        }
        let mid = r.deck.new_id();
        let dp = r.st.chain("drawing-page", mp.attr("draw:style-name"));
        let bgs = r.st.chain("presentation", Some(&format!("{sname}-background")));
        let background = r.fill(&dp, "style:drawing-page-properties").filter(|f| *f != Fill::None).or_else(|| r.fill(&bgs, "style:graphic-properties")).unwrap_or(Fill::Solid(Color::rgb(0xFF_FFFF)));
        let mut ids = HashMap::new();
        let shapes = r.tree(mp, 0, &mut ids);
        let title_style: ListStyle = r.level_style(&format!("{sname}-title"), 0).into_iter().collect();
        let mut body_style: ListStyle = Vec::new();
        for i in 0..MAX_LEVELS {
            match r.level_style(&format!("{sname}-outline{}", i + 1), i) {
                Some(l) => body_style.push(l),
                None => break,
            }
        }
        let name: Arc<str> = mp.attr("style:display-name").map(str::to_string).unwrap_or_else(|| sname.replace("_20_", " ")).into();
        r.deck.masters.push(Master { id: mid, name: name.clone(), background, shapes, title_style, body_style, other_style: Vec::new() });
        let lid = r.deck.new_id();
        r.deck.layouts.push(Layout { id: lid, master: mid, name, kind: "".into(), background: None, shapes: Vec::new(), hide_master_shapes: false });
        layout_of.insert(sname.to_string(), lid);
    }
    if r.deck.masters.is_empty() {
        r.rep.note(Kind::Approximate, "a deck without masters given the default ones");
        let mut fresh = Deck::new();
        fresh.size = r.deck.size;
        let wm = fresh.id_watermark();
        r.deck.masters = fresh.masters;
        r.deck.layouts = fresh.layouts;
        while r.deck.id_watermark() < wm {
            r.deck.new_id();
        }
    }

    // Slides.
    r.on_slide = true;
    for page in pres.children("draw:page") {
        if r.deck.slide_count() >= MAX_SLIDES {
            r.rep.note(Kind::Dropped, "slides past the limit");
            break;
        }
        let layout = page.attr("draw:master-page-name").and_then(|m| layout_of.get(m)).copied().unwrap_or(r.deck.layouts[0].id);
        let id = r.deck.new_id();
        let mut slide = Slide::new(id, layout);
        let dp = r.st.chain("drawing-page", page.attr("draw:style-name"));
        // A page's own fill ("none" there means the master's).
        let own: Vec<&El> = dp.iter().copied().filter(|e| e.name == "style:style").collect();
        slide.background = r.fill(&own, "style:drawing-page-properties").filter(|f| *f != Fill::None);
        let mut ids = HashMap::new();
        slide.shapes = r.tree(page, 0, &mut ids);
        let dpp = |k: &str| get(&dp, "style:drawing-page-properties", k);
        slide.hidden = dpp("presentation:visibility") == Some("hidden") || page.attr("presentation:visibility") == Some("hidden");
        slide.hide_layout_shapes = dpp("presentation:background-objects-visible") == Some("false");
        slide.transition = r.transition(&dp);
        slide.builds = r.builds(page, &ids);
        if let Some(n) = page.child("presentation:notes") {
            let frame = n.children("draw:frame").find(|f| f.attr("presentation:class") == Some("notes"));
            if let Some(tb) = frame.and_then(|f| f.child("draw:text-box")) {
                let chain = r.shape_chain(frame.unwrap());
                let t = r.body(tb, &chain, false, None);
                if !t.text().trim().is_empty() {
                    slide.notes = Some(t);
                }
            }
        }
        r.deck.push_slide(slide);
        r.loaded.slides += 1;
    }
    if r.deck.slide_count() == 0 {
        r.rep.note(Kind::Approximate, "a deck with no slides given one");
        let l = r.deck.layouts[0].id;
        let s = officina_slides::ops::new_slide(&mut r.deck, l);
        r.deck.push_slide(s);
    }
    if archive.find("Basic/script-lc.xml").is_some() || archive.find("Scripts/").is_some() {
        r.rep.note(Kind::Dropped, "macros");
    }
    Ok((r.deck, Loaded { report: r.rep, ..r.loaded }))
}
