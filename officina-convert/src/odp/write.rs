//! `.odp` out: the deck as an OpenDocument Presentation (docs/spec/officina.md
//! §9.2, O6c).
//!
//! ODF has master pages but no layouts, so each layout becomes a master
//! page holding its master's shapes and its own, with the master's level
//! styles as presentation styles. What a shape shows is written decided:
//! placeholder geometry, fills, lines and text styles come resolved through
//! the placeholder chain, theme colours as sRGB. Presets go out as
//! LibreOffice's `ooxml-…` custom shapes with their outline drawn, so a
//! reader that knows the preset keeps it and one that doesn't draws it.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::Arc;

use officina_slides::model::*;
use officina_slides::resolve::{self, Owner};
use officina_text::model::{CharAttrs, ParaProps, TextDoc};

use crate::odt::write::{para_props, text_props};
use crate::pptx::write::ext_of;
use crate::xml::esc;
use crate::zip::ZipWriter;

const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:smil="urn:oasis:names:tc:opendocument:xmlns:smil-compatible:1.0" xmlns:anim="urn:oasis:names:tc:opendocument:xmlns:animation:1.0" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" office:version="1.3""#;
const HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

fn cm(emu: f64) -> String {
    let v = emu / 360_000.0;
    let v = if v.is_finite() { (v * 1000.0).round() / 1000.0 } else { 0.0 };
    format!("{v}cm")
}

fn hex(rgba: u32) -> String {
    format!("#{:06x}", rgba >> 8)
}

/// Automatic styles of one file, each written once.
struct Auto {
    prefix: &'static str,
    xml: String,
    keys: HashMap<String, String>,
    counts: HashMap<&'static str, usize>,
}

impl Auto {
    fn new(prefix: &'static str) -> Auto {
        Auto { prefix, xml: String::new(), keys: HashMap::new(), counts: HashMap::new() }
    }
    fn style(&mut self, family: &str, short: &'static str, body: &str) -> String {
        let key = format!("{family}|{body}");
        if let Some(n) = self.keys.get(&key) {
            return n.clone();
        }
        let c = self.counts.entry(short).or_insert(0);
        *c += 1;
        let name = format!("{}{short}{c}", self.prefix);
        let _ = write!(self.xml, r#"<style:style style:name="{name}" style:family="{family}">{body}</style:style>"#);
        self.keys.insert(key, name.clone());
        name
    }
    fn list(&mut self, body: &str) -> String {
        let key = format!("list|{body}");
        if let Some(n) = self.keys.get(&key) {
            return n.clone();
        }
        let c = self.counts.entry("L").or_insert(0);
        *c += 1;
        let name = format!("{}L{c}", self.prefix);
        let _ = write!(self.xml, r#"<text:list-style style:name="{name}">{body}</text:list-style>"#);
        self.keys.insert(key, name.clone());
        name
    }
}

/// A group's placing of its children.
#[derive(Clone, Copy)]
struct GroupXf {
    frame: Rect,
    child: Rect,
    rot: i32,
}

/// A shape's box after its groups: x, y, w, h (EMU) and its turn (degrees
/// clockwise).
fn placed(r: Rect, rot: i32, stack: &[GroupXf]) -> ([f64; 4], f64) {
    let (mut x, mut y, mut w, mut h) = (r.x as f64, r.y as f64, r.w as f64, r.h as f64);
    let mut deg = rot as f64 / 60_000.0;
    for g in stack.iter().rev() {
        let sx = if g.child.w > 0 { g.frame.w as f64 / g.child.w as f64 } else { 1.0 };
        let sy = if g.child.h > 0 { g.frame.h as f64 / g.child.h as f64 } else { 1.0 };
        x = g.frame.x as f64 + (x - g.child.x as f64) * sx;
        y = g.frame.y as f64 + (y - g.child.y as f64) * sy;
        w *= sx;
        h *= sy;
        if g.rot != 0 {
            let t = (g.rot as f64 / 60_000.0).to_radians();
            let (gx, gy) = (g.frame.x as f64 + g.frame.w as f64 / 2.0, g.frame.y as f64 + g.frame.h as f64 / 2.0);
            let (dx, dy) = (x + w / 2.0 - gx, y + h / 2.0 - gy);
            let (cx, cy) = (gx + dx * t.cos() - dy * t.sin(), gy + dx * t.sin() + dy * t.cos());
            x = cx - w / 2.0;
            y = cy - h / 2.0;
            deg += g.rot as f64 / 60_000.0;
        }
    }
    ([x, y, w.max(0.0), h.max(0.0)], deg.rem_euclid(360.0))
}

/// Position attributes: `svg:x`/`svg:y`, or a `draw:transform` for a turn.
fn position(b: [f64; 4], deg: f64) -> String {
    let [x, y, w, h] = b;
    let size = format!(r#" svg:width="{}" svg:height="{}""#, cm(w), cm(h));
    if deg.abs() < 1e-6 || (deg - 360.0).abs() < 1e-6 {
        return format!(r#"{size} svg:x="{}" svg:y="{}""#, cm(x), cm(y));
    }
    // ODF turns counter-clockwise about the box's top left, then moves.
    let a = (360.0 - deg).rem_euclid(360.0).to_radians();
    let (cx, cy) = (w / 2.0, h / 2.0);
    let (rx, ry) = (cx * a.cos() + cy * a.sin(), -cx * a.sin() + cy * a.cos());
    let (tx, ty) = (x + cx - rx, y + cy - ry);
    format!(r#"{size} draw:transform="rotate ({a:.6}) translate ({} {})""#, cm(tx), cm(ty))
}

/// A number scheme as ODF's format, prefix and suffix.
fn scheme_parts(s: &str) -> (&'static str, &'static str, &'static str) {
    let f = if s.starts_with("alphaLc") {
        "a"
    } else if s.starts_with("alphaUc") {
        "A"
    } else if s.starts_with("romanLc") {
        "i"
    } else if s.starts_with("romanUc") {
        "I"
    } else {
        "1"
    };
    let (p, x) = if s.ends_with("ParenBoth") {
        ("(", ")")
    } else if s.ends_with("ParenR") {
        ("", ")")
    } else if s.ends_with("Plain") {
        ("", "")
    } else {
        ("", ".")
    };
    (f, p, x)
}

fn pt_tw(t: i32) -> String {
    format!("{}pt", (t as f64 / 20.0 * 100.0).round() / 100.0)
}

struct W<'d> {
    deck: &'d Deck,
    media: HashMap<MediaId, String>,
    files: Vec<(String, Arc<[u8]>, Arc<str>)>,
    /// Gradients, fill pictures, dashes and markers (`office:styles`).
    common: String,
    common_keys: HashMap<String, String>,
    fonts: Vec<String>,
}

impl W<'_> {
    fn theme(&self) -> &Theme {
        &self.deck.theme
    }

    fn rgba(&self, c: &Color) -> u32 {
        resolve::color(&self.deck.theme, c)
    }

    fn media_path(&mut self, id: MediaId) -> Option<String> {
        if let Some(p) = self.media.get(&id) {
            return Some(p.clone());
        }
        let mime = self.deck.media(id)?.mime.clone();
        let bytes = self.deck.media_bytes(id).ok()?;
        let dir = if mime.starts_with("video/") || mime.starts_with("audio/") { "Media" } else { "Pictures" };
        let name = format!("{dir}/media{}.{}", self.media.len() + 1, ext_of(&mime));
        self.files.push((name.clone(), bytes, mime));
        self.media.insert(id, name.clone());
        Some(name)
    }

    fn common(&mut self, kind: &str, key: String, make: impl FnOnce(&str) -> String) -> String {
        if let Some(n) = self.common_keys.get(&key) {
            return n.clone();
        }
        let name = format!("{kind}_{}", self.common_keys.len() + 1);
        let x = make(&name);
        self.common.push_str(&x);
        self.common_keys.insert(key, name.clone());
        name
    }

    /// Fill attributes for a graphic or drawing-page properties element.
    fn fill(&mut self, f: &Fill) -> String {
        match f {
            Fill::None => r#" draw:fill="none""#.into(),
            Fill::Solid(c) => {
                let v = self.rgba(c);
                let a = v & 255;
                let op = if a < 255 { format!(r#" draw:opacity="{}%""#, (a as f64 * 100.0 / 255.0).round()) } else { String::new() };
                format!(r#" draw:fill="solid" draw:fill-color="{}"{op}"#, hex(v))
            }
            Fill::Gradient { stops, angle, radial } => {
                if stops.is_empty() {
                    return r#" draw:fill="none""#.into();
                }
                let cols: Vec<(f64, u32)> = stops.iter().map(|s| (s.pos as f64 / 100_000.0, self.rgba(&s.color))).collect();
                let key = format!("g{cols:?}{angle}{radial}");
                let first = cols[0].1;
                let name = self.common("Gradient", key, |name| {
                    let (style, a, start, end, list): (&str, f64, u32, u32, Vec<(f64, u32)>) = if *radial {
                        // The start colour is the outside.
                        let l: Vec<(f64, u32)> = cols.iter().rev().map(|&(p, c)| (1.0 - p, c)).collect();
                        ("radial", 0.0, l[0].1, l[l.len() - 1].1, l)
                    } else {
                        ("linear", (90.0 - *angle as f64 / 60_000.0).rem_euclid(360.0), cols[0].1, cols[cols.len() - 1].1, cols.clone())
                    };
                    let mut s = format!(r#"<draw:gradient draw:name="{name}" draw:style="{style}" draw:cx="50%" draw:cy="50%" draw:start-color="{}" draw:end-color="{}" draw:start-intensity="100%" draw:end-intensity="100%" draw:angle="{}deg" draw:border="0%">"#, hex(start), hex(end), a.round() as i64);
                    if list.len() > 2 {
                        for (p, c) in list {
                            let _ = write!(s, r#"<loext:gradient-stop svg:offset="{p}" loext:color-type="rgb" loext:color-value="{}"/>"#, hex(c));
                        }
                    }
                    s.push_str("</draw:gradient>");
                    s
                });
                format!(r#" draw:fill="gradient" draw:fill-color="{}" draw:fill-gradient-name="{name}""#, hex(first))
            }
            Fill::Picture(m) => match self.media_path(*m) {
                Some(path) => {
                    let name = self.common("Picture", format!("p{path}"), |name| format!(r#"<draw:fill-image draw:name="{name}" xlink:href="{path}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#));
                    format!(r#" draw:fill="bitmap" draw:fill-image-name="{name}" style:repeat="stretch""#)
                }
                None => r#" draw:fill="none""#.into(),
            },
        }
    }

    fn stroke(&mut self, l: &Stroke) -> String {
        let Some(c) = &l.color else { return r#" draw:stroke="none""#.into() };
        let v = self.rgba(c);
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
                let name = self.common("Dash", format!("d{n}"), |name| format!(r#"<draw:stroke-dash draw:name="{name}" draw:display-name="{n}" draw:style="rect" {attrs}/>"#));
                let _ = write!(s, r#" draw:stroke="dash" draw:stroke-dash="{name}""#);
            }
            None => s.push_str(r#" draw:stroke="solid""#),
        }
        let _ = write!(s, r#" svg:stroke-color="{}" svg:stroke-width="{}""#, hex(v), cm(l.width as f64));
        if v & 255 < 255 {
            let _ = write!(s, r#" svg:stroke-opacity="{}%""#, ((v & 255) as f64 * 100.0 / 255.0).round());
        }
        let mw = cm((l.width * 3).max(72_000) as f64);
        for (end, a) in [("start", l.head), ("end", l.tail)] {
            let m = match a {
                Arrow::None => continue,
                Arrow::Triangle => ("Arrow", "0 0 20 30", "M10 0 L20 30 L0 30 Z"),
                Arrow::Open => ("Line_20_Arrow", "0 0 20 30", "M10 0 L20 28 L17 30 L10 8 L3 30 L0 28 Z"),
                Arrow::Stealth => ("Arrow_20_concave", "0 0 20 30", "M10 0 L20 30 L10 22 L0 30 Z"),
                Arrow::Diamond => ("Diamond", "0 0 20 20", "M10 0 L20 10 L10 20 L0 10 Z"),
                Arrow::Oval => ("Circle", "0 0 20 20", "M20 10 C20 15.5 15.5 20 10 20 C4.5 20 0 15.5 0 10 C0 4.5 4.5 0 10 0 C15.5 0 20 4.5 20 10 Z"),
            };
            if !self.common_keys.contains_key(&format!("m{}", m.0)) {
                self.common_keys.insert(format!("m{}", m.0), m.0.to_string());
                let _ = write!(self.common, r#"<draw:marker draw:name="{}" draw:display-name="{}" svg:viewBox="{}" svg:d="{}"/>"#, m.0, m.0.replace("_20_", " "), m.1, m.2);
            }
            let _ = write!(s, r#" draw:marker-{end}="{}" draw:marker-{end}-width="{mw}""#, m.0);
        }
        s
    }

    /// Text frame properties for a graphic style.
    fn frame_props(r: &resolve::Resolved) -> String {
        let [l, t, rr, b] = r.inset;
        let mut s = format!(r#" fo:padding-left="{}" fo:padding-top="{}" fo:padding-right="{}" fo:padding-bottom="{}" draw:textarea-horizontal-align="justify" draw:textarea-vertical-align="{}""#, cm(l as f64), cm(t as f64), cm(rr as f64), cm(b as f64), match r.anchor {
            Anchor::Top => "top",
            Anchor::Middle => "middle",
            Anchor::Bottom => "bottom",
        });
        s.push_str(match r.autofit {
            Autofit::Resize => r#" draw:auto-grow-height="true" draw:auto-grow-width="false""#,
            Autofit::Shrink { .. } => r#" draw:auto-grow-height="false" style:shrink-to-fit="true""#,
            Autofit::None => r#" draw:auto-grow-height="false" draw:auto-grow-width="false""#,
        });
        s.push_str(if r.wrap { r#" fo:wrap-option="wrap""# } else { r#" fo:wrap-option="no-wrap""# });
        s
    }

    /// Character attributes made complete: the level's under the run's, theme
    /// fonts and colours resolved.
    fn chars(&self, run: &CharAttrs, l: &LevelStyle) -> CharAttrs {
        let mut a = run.over(&l.chars);
        if run.color.is_none() {
            if let Some(c) = &l.color {
                a.color = Some(self.rgba(c) >> 8);
            }
        }
        if let Some(f) = &a.family {
            if f.starts_with('+') {
                a.family = Some(resolve::font(self.theme(), f));
            }
        }
        a.link = None;
        a
    }

    fn list_level(&mut self, level: usize, b: &Bullet, indent: (i32, i32)) -> String {
        // Impress places labels the older way: the label's start and width.
        let label = (-indent.1).max(0);
        let props = format!(r#"<style:list-level-properties text:space-before="{}" text:min-label-width="{}"/>"#, pt_tw(indent.0 - label), pt_tw(label));
        let lv = level + 1;
        match b {
            Bullet::Char { ch, font, color, size_pct } => {
                let mut tp = String::new();
                if let Some(f) = font {
                    let _ = write!(tp, r#" fo:font-family="{}""#, esc(f));
                }
                if let Some(c) = color {
                    let _ = write!(tp, r#" fo:color="{}""#, hex(self.rgba(c)));
                }
                let _ = write!(tp, r#" fo:font-size="{}%""#, size_pct.unwrap_or(100_000) / 1000);
                format!(r#"<text:list-level-style-bullet text:level="{lv}" text:bullet-char="{}">{props}<style:text-properties{tp}/></text:list-level-style-bullet>"#, esc(&ch.to_string()))
            }
            Bullet::Number { scheme, start } => {
                let (f, p, x) = scheme_parts(scheme);
                format!(r#"<text:list-level-style-number text:level="{lv}" style:num-prefix="{p}" style:num-suffix="{x}" style:num-format="{f}" text:start-value="{start}">{props}<style:text-properties fo:font-size="100%"/></text:list-level-style-number>"#)
            }
            Bullet::None => format!(r#"<text:list-level-style-number text:level="{lv}" style:num-format="">{props}</text:list-level-style-number>"#),
        }
    }

    /// A list style with `b` at each level given, none at the others.
    fn list_style(&mut self, levels: &[Option<(Bullet, (i32, i32))>; MAX_LEVELS], a: &mut Auto) -> String {
        let mut body = String::new();
        for (i, l) in levels.iter().enumerate() {
            let x = match l {
                Some((b, ind)) => self.list_level(i, b, *ind),
                None => self.list_level(i, &Bullet::None, (0, 0)),
            };
            body.push_str(&x);
        }
        a.list(&body)
    }

    /// Paragraph text with ODF's spaces, tabs, breaks and page numbers.
    fn text_xml(s: &str, out: &mut String) {
        let mut spaces = 0usize;
        let mut first = true;
        let flush = |n: &mut usize, out: &mut String, lead: bool| {
            if *n == 0 {
                return;
            }
            if lead {
                let _ = write!(out, r#"<text:s text:c="{}"/>"#, *n);
            } else {
                out.push(' ');
                if *n > 1 {
                    let _ = write!(out, r#"<text:s text:c="{}"/>"#, *n - 1);
                }
            }
            *n = 0;
        };
        let mut rest = s;
        while !rest.is_empty() {
            if let Some(r) = rest.strip_prefix("‹#›") {
                flush(&mut spaces, out, first);
                out.push_str("<text:page-number>&lt;number&gt;</text:page-number>");
                first = false;
                rest = r;
                continue;
            }
            let c = rest.chars().next().unwrap();
            rest = &rest[c.len_utf8()..];
            match c {
                ' ' => spaces += 1,
                '\t' => {
                    flush(&mut spaces, out, first);
                    out.push_str("<text:tab/>");
                    first = false;
                }
                '\u{2028}' | '\n' | '\r' => {
                    flush(&mut spaces, out, first);
                    out.push_str("<text:line-break/>");
                    first = true;
                }
                c => {
                    flush(&mut spaces, out, first);
                    first = false;
                    let mut b = [0u8; 4];
                    out.push_str(&esc(c.encode_utf8(&mut b)));
                }
            }
        }
        flush(&mut spaces, out, true);
    }

    /// A frame's paragraphs, lists nested for levels and bullets.
    fn text(&mut self, t: &TextBody, levels: &ListStyle, a: &mut Auto, out: &mut String) {
        let doc: &TextDoc = &t.doc;
        let paras: Vec<_> = doc.blocks.iter().filter_map(|b| b.as_para()).collect();
        let def = LevelStyle::default();
        // Each paragraph: level, bullet, props, its XML.
        struct P {
            level: usize,
            bullet: Option<Bullet>,
            indent: (i32, i32),
            xml: String,
        }
        let mut list: Vec<P> = Vec::new();
        for p in &paras {
            let level = (p.style as usize).min(MAX_LEVELS - 1);
            let l = levels.get(level).unwrap_or(&def);
            let bullet = match &p.props.list {
                Some(Some(r)) => t.bullets.get((r.id as usize).wrapping_sub(1)).cloned(),
                Some(None) => None,
                None => l.bullet.clone(),
            }
            .filter(|b| *b != Bullet::None);
            let mut props: ParaProps = resolve::para_over(&p.props, &l.para);
            props.list = None;
            props.cell = None;
            props.rev = None;
            props.toc = None;
            props.section = None;
            props.outline_level = None;
            let pchars = self.chars(&CharAttrs::default(), l);
            let indent = (props.indent_start.unwrap_or(0), props.first_line.unwrap_or(0));
            if bullet.is_some() || level > 0 {
                // As LibreOffice has it: a list paragraph's indents are its
                // list level's.
                props.indent_start = Some(0);
                props.first_line = Some(0);
            }
            let mut body = para_props(&props, props.indent_start.unwrap_or(0));
            body.push_str(&text_props(&pchars, &mut self.fonts));
            let pname = a.style("paragraph", "P", &body);
            let mut x = format!(r#"<text:p text:style-name="{pname}">"#);
            for (i, (off, aid)) in p.runs.iter().enumerate() {
                let end = p.runs.get(i + 1).map_or(p.text.len(), |r| r.0 as usize).min(p.text.len());
                let start = (*off as usize).min(end);
                let Some(seg) = p.text.get(start..end) else { continue };
                if seg.is_empty() {
                    continue;
                }
                let run = doc.attrs.get(*aid).clone();
                let c = self.chars(&run, l);
                let tp = text_props(&c, &mut self.fonts);
                let tname = a.style("text", "T", &tp);
                let mut inner = String::new();
                Self::text_xml(seg, &mut inner);
                let span = format!(r#"<text:span text:style-name="{tname}">{inner}</text:span>"#);
                match &run.link {
                    Some(h) => {
                        let _ = write!(x, r#"<text:a xlink:type="simple" xlink:href="{}">{span}</text:a>"#, esc(h));
                    }
                    None => x.push_str(&span),
                }
            }
            x.push_str("</text:p>");
            list.push(P { level, bullet, indent, xml: x });
        }
        let mut i = 0;
        while i < list.len() {
            if list[i].bullet.is_none() && list[i].level == 0 {
                out.push_str(&list[i].xml);
                i += 1;
                continue;
            }
            // A run of list paragraphs: one list, levels nested.
            let mut j = i;
            while j < list.len() && !(list[j].bullet.is_none() && list[j].level == 0) {
                j += 1;
            }
            let mut levels_used: [Option<(Bullet, (i32, i32))>; MAX_LEVELS] = Default::default();
            for p in &list[i..j] {
                if let (Some(b), None) = (&p.bullet, &levels_used[p.level]) {
                    levels_used[p.level] = Some((b.clone(), p.indent));
                }
            }
            let lname = self.list_style(&levels_used, a);
            let _ = write!(out, r#"<text:list text:style-name="{lname}">"#);
            for p in &list[i..j] {
                for _ in 0..p.level {
                    out.push_str("<text:list-item><text:list>");
                }
                match &p.bullet {
                    None => {
                        let _ = write!(out, "<text:list-header>{}</text:list-header>", p.xml);
                    }
                    Some(b) => {
                        let same = levels_used[p.level].as_ref().is_some_and(|(x, _)| x == b);
                        if same {
                            let _ = write!(out, "<text:list-item>{}</text:list-item>", p.xml);
                        } else {
                            let mut one: [Option<(Bullet, (i32, i32))>; MAX_LEVELS] = Default::default();
                            one[p.level] = Some((b.clone(), p.indent));
                            let o = self.list_style(&one, a);
                            let _ = write!(out, r#"<text:list-item text:style-override="{o}">{}</text:list-item>"#, p.xml);
                        }
                    }
                }
                for _ in 0..p.level {
                    out.push_str("</text:list></text:list-item>");
                }
            }
            out.push_str("</text:list>");
            i = j;
        }
    }

    fn ph_class(k: PhKind, has_text: bool) -> Option<&'static str> {
        Some(match k {
            PhKind::Title | PhKind::CtrTitle => "title",
            PhKind::SubTitle => "subtitle",
            PhKind::Body => "outline",
            PhKind::Obj => {
                if has_text {
                    "outline"
                } else {
                    "object"
                }
            }
            PhKind::Date => "date-time",
            PhKind::Footer => "footer",
            PhKind::SlideNum => "page-number",
            PhKind::Header => "header",
            PhKind::Pic | PhKind::ClipArt => "graphic",
            PhKind::Chart => "chart",
            PhKind::Table => "table",
            PhKind::Diagram => "orgchart",
            PhKind::Media => "object",
            PhKind::SldImg => return None,
        })
    }

    fn geometry(g: &Geom, w: f64, h: f64) -> String {
        let (vw, vh) = ((w / 360.0).round().max(1.0) as i64, (h / 360.0).round().max(1.0) as i64);
        if matches!(g, Geom::Preset { name, .. } if &**name == "ellipse") {
            // LibreOffice's own ellipse, with its own text area.
            return r#"svg:viewBox="0 0 21600 21600" draw:type="ellipse""#.into();
        }
        let (paths, _) = officina_slides::geom::paths(g, vw, vh);
        let ty = match g {
            Geom::Preset { name, .. } => match &**name {
                "rect" => "rectangle".to_string(),
                "ellipse" => "ellipse".to_string(),
                n => format!("ooxml-{}", n.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()),
            },
            Geom::Custom(_) => "ooxml-non-primitive".to_string(),
        };
        let mods = match g {
            Geom::Preset { adj, .. } if !adj.is_empty() => format!(r#" draw:modifiers="{}""#, adj.iter().map(|(_, v)| v.to_string()).collect::<Vec<_>>().join(" ")),
            _ => String::new(),
        };
        let mut d = String::new();
        for p in &paths {
            let sx = vw as f64 / p.w.max(1) as f64;
            let sy = vh as f64 / p.h.max(1) as f64;
            let x = |v: i64| (v as f64 * sx).round() as i64;
            let y = |v: i64| (v as f64 * sy).round() as i64;
            for c in &p.cmds {
                let _ = match *c {
                    PathCmd::Move(a, b) => write!(d, "M {} {} ", x(a), y(b)),
                    PathCmd::Line(a, b) => write!(d, "L {} {} ", x(a), y(b)),
                    PathCmd::Cubic(a, b, c2, d2, e, f) => write!(d, "C {} {} {} {} {} {} ", x(a), y(b), x(c2), y(d2), x(e), y(f)),
                    PathCmd::Close => write!(d, "Z "),
                };
            }
            if !p.filled {
                d.push_str("F ");
            }
            if !p.stroked {
                d.push_str("S ");
            }
            d.push_str("N ");
        }
        format!(r#"svg:viewBox="0 0 {vw} {vh}" draw:text-areas="0 0 {vw} {vh}" draw:type="{ty}"{mods} draw:enhanced-path="{}""#, d.trim_end())
    }

    #[allow(clippy::too_many_arguments)]
    fn shape(&mut self, owner: Owner, sh: &Shape, stack: &mut Vec<GroupXf>, a: &mut Auto, layer: &str, on_page: bool, out: &mut String) {
        if stack.len() > MAX_DEPTH {
            return;
        }
        let r = resolve::resolve(self.deck, owner, sh);
        let (b, deg) = placed(r.frame, sh.rot, stack);
        let name = if sh.name.is_empty() { String::new() } else { format!(r#" draw:name="{}""#, esc(&sh.name)) };
        let id = format!(r#" draw:id="id{0}" xml:id="id{0}""#, sh.id);
        let hidden = if sh.hidden { r#" draw:display="none""# } else { "" };
        let desc = if sh.descr.is_empty() { String::new() } else { format!("<svg:desc>{}</svg:desc>", esc(&sh.descr)) };
        let common = format!(r#"{name}{id}{hidden} draw:layer="{layer}""#);
        match &sh.kind {
            ShapeKind::Group { child, shapes } => {
                if shapes.is_empty() {
                    return;
                }
                let _ = write!(out, "<draw:g{name}{id}{hidden}>{desc}");
                stack.push(GroupXf { frame: r.frame, child: *child, rot: sh.rot });
                for k in shapes {
                    self.shape(owner, k, stack, a, layer, on_page, out);
                }
                stack.pop();
                out.push_str("</draw:g>");
            }
            ShapeKind::Picture { media, line, .. } => {
                let Some(path) = self.media_path(*media) else { return };
                let mut g = String::from(r#" draw:fill="none""#);
                g.push_str(&match line {
                    Some(l) => self.stroke(l),
                    None => r#" draw:stroke="none""#.into(),
                });
                if sh.flip_h || sh.flip_v {
                    let _ = write!(g, r#" style:mirror="{}""#, match (sh.flip_h, sh.flip_v) {
                        (true, true) => "horizontal vertical",
                        (true, false) => "horizontal",
                        _ => "vertical",
                    });
                }
                let st = a.style("graphic", "gr", &format!("<style:graphic-properties{g}/>"));
                let _ = write!(out, r#"<draw:frame draw:style-name="{st}"{common}{}><draw:image xlink:href="{path}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>{desc}</draw:frame>"#, position(b, deg));
            }
            ShapeKind::Video { media, poster } => {
                let Some(path) = self.media_path(*media) else { return };
                let poster = poster.and_then(|p| self.media_path(p)).map(|p| format!(r#"<draw:image xlink:href="{p}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#)).unwrap_or_default();
                let st = a.style("graphic", "gr", r#"<style:graphic-properties draw:fill="none" draw:stroke="none"/>"#);
                let _ = write!(out, r#"<draw:frame draw:style-name="{st}"{common}{}><draw:plugin xlink:href="{path}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad" draw:mime-type="application/vnd.sun.star.media"/>{poster}{desc}</draw:frame>"#, position(b, deg));
            }
            ShapeKind::Geom { text, .. } => {
                let has_text = text.as_ref().is_some_and(|t| !t.text().trim().is_empty());
                // Placeholders: frames of their class (page-level date,
                // footer and number frames go as plain text).
                let class = sh.ph.filter(|p| !on_page || !matches!(p.kind, PhKind::Date | PhKind::Footer | PhKind::SlideNum | PhKind::Header)).and_then(|p| Self::ph_class(p.kind, has_text));
                if sh.ph.is_some_and(|p| p.kind == PhKind::SldImg) {
                    return;
                }
                if let Some(class) = class {
                    let mut g = self.fill(&page_gradient(&r.fill, sh.rot));
                    g.push_str(&self.stroke(&r.line));
                    g.push_str(&Self::frame_props(&r));
                    let st = a.style("presentation", "pr", &format!("<style:graphic-properties{g}/>"));
                    let mut body = String::new();
                    if let Some(t) = text {
                        self.text(t, &r.levels, a, &mut body);
                    }
                    let empty = if has_text { "" } else { r#" presentation:placeholder="true""# };
                    let _ = write!(out, r#"<draw:frame presentation:style-name="{st}"{common}{} presentation:class="{class}"{empty} presentation:user-transformed="true"><draw:text-box>{body}</draw:text-box>{desc}</draw:frame>"#, position(b, deg));
                    return;
                }
                let is_line = matches!(&r.geom, Geom::Preset { name, .. } if &**name == "line" || &**name == "straightConnector1");
                if is_line {
                    let [x, y, w, h] = b;
                    let (mut x1, mut y1, mut x2, mut y2) = (x, y, x + w, y + h);
                    if sh.flip_h {
                        std::mem::swap(&mut x1, &mut x2);
                    }
                    if sh.flip_v {
                        std::mem::swap(&mut y1, &mut y2);
                    }
                    if deg.abs() > 1e-6 {
                        let t = deg.to_radians();
                        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
                        let turn = |px: f64, py: f64| (cx + (px - cx) * t.cos() - (py - cy) * t.sin(), cy + (px - cx) * t.sin() + (py - cy) * t.cos());
                        (x1, y1) = turn(x1, y1);
                        (x2, y2) = turn(x2, y2);
                    }
                    let mut g = String::from(r#" draw:fill="none""#);
                    g.push_str(&self.stroke(&r.line));
                    let st = a.style("graphic", "gr", &format!("<style:graphic-properties{g}/>"));
                    let tag = if matches!(&r.geom, Geom::Preset { name, .. } if &**name == "straightConnector1") { "draw:connector" } else { "draw:line" };
                    let ty = if tag == "draw:connector" { r#" draw:type="line""# } else { "" };
                    let _ = write!(out, r#"<{tag} draw:style-name="{st}"{common}{ty} svg:x1="{}" svg:y1="{}" svg:x2="{}" svg:y2="{}">{desc}<text:p/></{tag}>"#, cm(x1), cm(y1), cm(x2), cm(y2));
                    return;
                }
                let mut g = self.fill(&page_gradient(&r.fill, sh.rot));
                g.push_str(&self.stroke(&r.line));
                g.push_str(&Self::frame_props(&r));
                let st = a.style("graphic", "gr", &format!("<style:graphic-properties{g}/>"));
                let mut body = String::new();
                if let Some(t) = text {
                    self.text(t, &r.levels, a, &mut body);
                }
                let mirror = format!("{}{}", if sh.flip_h { r#" draw:mirror-horizontal="true""# } else { "" }, if sh.flip_v { r#" draw:mirror-vertical="true""# } else { "" });
                let _ = write!(out, r#"<draw:custom-shape draw:style-name="{st}"{common}{}>{desc}{body}<draw:enhanced-geometry{mirror} {}/></draw:custom-shape>"#, position(b, deg), Self::geometry(&r.geom, b[2], b[3]));
            }
        }
    }

    fn transition(t: &Transition) -> String {
        let speed = if t.ms <= 500 { "fast" } else if t.ms <= 750 { "medium" } else { "slow" };
        let from = |d: Dir| match d {
            Dir::Left => "fromRight",
            Dir::Up => "fromBottom",
            Dir::Right => "fromLeft",
            Dir::Down => "fromTop",
        };
        let (ty, sub, rev) = match t.kind {
            TransitionKind::Cut => ("", "", false),
            TransitionKind::Fade => ("fade", "crossfade", false),
            TransitionKind::Push(d) => ("pushWipe", from(d), false),
            TransitionKind::Cover(d) => ("slideWipe", from(d), false),
            TransitionKind::Uncover(d) => ("slideWipe", from(d), true),
            TransitionKind::Wipe(d) => match d {
                Dir::Right => ("barWipe", "leftToRight", false),
                Dir::Left => ("barWipe", "leftToRight", true),
                Dir::Down => ("barWipe", "topToBottom", false),
                Dir::Up => ("barWipe", "topToBottom", true),
            },
            TransitionKind::Split { vertical, out } => ("barnDoorWipe", if vertical { "vertical" } else { "horizontal" }, !out),
            TransitionKind::Zoom { out } => ("irisWipe", "rectangle", !out),
            TransitionKind::Dissolve => ("dissolve", "mosaicDissolve", false),
        };
        let mut s = String::new();
        if !ty.is_empty() {
            let _ = write!(s, r#" smil:type="{ty}" smil:subtype="{sub}"{} presentation:transition-speed="{speed}""#, if rev { r#" smil:direction="reverse""# } else { "" });
        }
        match t.after_ms {
            Some(ms) if !t.on_click || ms > 0 => {
                let _ = write!(s, r#" presentation:transition-type="automatic" presentation:duration="PT{:.3}S""#, ms as f64 / 1000.0);
            }
            _ => s.push_str(r#" presentation:transition-type="manual""#),
        }
        s
    }
}

fn secs(ms: u32) -> String {
    format!("{}s", ms as f64 / 1000.0)
}

/// Builds as a main sequence of click groups (ODF's animation tree).
fn timing(builds: &[Build]) -> String {
    if builds.is_empty() {
        return String::new();
    }
    let mut clicks: Vec<Vec<Vec<&Build>>> = Vec::new();
    for b in builds {
        match b.trigger {
            Trigger::OnClick => clicks.push(vec![vec![b]]),
            _ if clicks.is_empty() => clicks.push(vec![vec![b]]),
            Trigger::WithPrevious => clicks.last_mut().unwrap().last_mut().unwrap().push(b),
            Trigger::AfterPrevious => clicks.last_mut().unwrap().push(vec![b]),
        }
    }
    let mut s = String::from(r#"<anim:par presentation:node-type="timing-root"><anim:seq presentation:node-type="main-sequence">"#);
    for click in &clicks {
        let begin = if click[0][0].trigger == Trigger::OnClick { "indefinite" } else { "0s" };
        let _ = write!(s, r#"<anim:par smil:begin="{begin}">"#);
        let mut t = 0u32;
        for tg in click {
            let _ = write!(s, r#"<anim:par smil:begin="{}">"#, secs(t));
            let mut longest = 0;
            for b in tg {
                longest = longest.max(b.delay_ms + b.ms);
                let node = match b.trigger {
                    Trigger::OnClick => "on-click",
                    Trigger::WithPrevious => "with-previous",
                    Trigger::AfterPrevious => "after-previous",
                };
                let sub = |d: Dir| match d {
                    Dir::Left => "from-left",
                    Dir::Up => "from-top",
                    Dir::Right => "from-right",
                    Dir::Down => "from-bottom",
                };
                let tgt = format!("id{}", b.shape);
                let dur = secs(b.ms.max(1));
                let show = format!(r#"<anim:set smil:begin="0s" smil:dur="0.001s" smil:fill="hold" smil:targetElement="{tgt}" smil:attributeName="visibility" smil:to="visible"/>"#);
                let (class, preset, subtype, kids) = match b.effect {
                    Effect::Appear => ("entrance", "ooo-entrance-appear", String::new(), show),
                    Effect::Fade => ("entrance", "ooo-entrance-fade-in", String::new(), format!(r#"{show}<anim:transitionFilter smil:dur="{dur}" smil:targetElement="{tgt}" smil:type="fade" smil:subtype="crossfade"/>"#)),
                    Effect::Wipe(d) => {
                        let (st, rev) = match d {
                            Dir::Left => ("leftToRight", false),
                            Dir::Right => ("leftToRight", true),
                            Dir::Up => ("topToBottom", false),
                            Dir::Down => ("topToBottom", true),
                        };
                        ("entrance", "ooo-entrance-wipe", sub(d).to_string(), format!(r#"{show}<anim:transitionFilter smil:dur="{dur}" smil:targetElement="{tgt}" smil:type="barWipe" smil:subtype="{st}"{}/>"#, if rev { r#" smil:direction="reverse""# } else { "" }))
                    }
                    Effect::FlyIn(d) => {
                        let (xv, yv) = match d {
                            Dir::Left => ("0-width/2;x", "y;y"),
                            Dir::Right => ("1+width/2;x", "y;y"),
                            Dir::Up => ("x;x", "0-height/2;y"),
                            Dir::Down => ("x;x", "1+height/2;y"),
                        };
                        ("entrance", "ooo-entrance-fly-in", sub(d).to_string(), format!(r#"{show}<anim:animate smil:dur="{dur}" smil:fill="hold" smil:targetElement="{tgt}" smil:attributeName="x" smil:values="{xv}" smil:keyTimes="0;1" presentation:additive="base"/><anim:animate smil:dur="{dur}" smil:fill="hold" smil:targetElement="{tgt}" smil:attributeName="y" smil:values="{yv}" smil:keyTimes="0;1" presentation:additive="base"/>"#))
                    }
                    Effect::Zoom => ("entrance", "ooo-entrance-zoom", "in".to_string(), format!(r#"{show}<anim:animate smil:dur="{dur}" smil:fill="hold" smil:targetElement="{tgt}" smil:attributeName="width" smil:values="0;width" smil:keyTimes="0;1"/><anim:animate smil:dur="{dur}" smil:fill="hold" smil:targetElement="{tgt}" smil:attributeName="height" smil:values="0;height" smil:keyTimes="0;1"/>"#)),
                    Effect::Disappear => ("exit", "ooo-exit-disappear", String::new(), format!(r#"<anim:set smil:begin="0s" smil:dur="0.001s" smil:fill="hold" smil:targetElement="{tgt}" smil:attributeName="visibility" smil:to="hidden"/>"#)),
                    Effect::FadeOut => ("exit", "ooo-exit-fade-out", String::new(), format!(r#"<anim:transitionFilter smil:dur="{dur}" smil:targetElement="{tgt}" smil:type="fade" smil:subtype="crossfade" smil:mode="out"/><anim:set smil:begin="{dur}" smil:dur="0.001s" smil:fill="hold" smil:targetElement="{tgt}" smil:attributeName="visibility" smil:to="hidden"/>"#)),
                };
                let kids = if b.by_paragraph { format!(r#"<anim:iterate anim:iterate-type="by-paragraph" anim:iterate-interval="0.5s" smil:targetElement="{tgt}">{kids}</anim:iterate>"#) } else { kids };
                let subtype = if subtype.is_empty() { String::new() } else { format!(r#" presentation:preset-sub-type="{subtype}""#) };
                let _ = write!(s, r#"<anim:par smil:begin="{}" smil:fill="hold" presentation:node-type="{node}" presentation:preset-class="{class}" presentation:preset-id="{preset}"{subtype}>{kids}</anim:par>"#, secs(b.delay_ms));
            }
            s.push_str("</anim:par>");
            t += longest;
        }
        s.push_str("</anim:par>");
    }
    s.push_str("</anim:seq></anim:par>");
    s
}

/// Presentation styles for a master page: `-title` and `-outline1`…`9`.
fn master_styles(w: &mut W, name: &str, m: &Master) -> String {
    let mut s = String::new();
    let def = LevelStyle::default();
    let mut emb = String::new();
    let body: Vec<&LevelStyle> = (0..MAX_LEVELS).map(|i| m.body_style.get(i).unwrap_or(&def)).collect();
    for (i, l) in body.iter().enumerate() {
        let b = l.bullet.clone().unwrap_or(Bullet::None);
        let ind = (l.para.indent_start.unwrap_or(0), l.para.first_line.unwrap_or(0));
        emb.push_str(&w.list_level(i, &b, ind));
    }
    let one = |w: &mut W, l: &LevelStyle| -> String {
        let c = w.chars(&CharAttrs::default(), l);
        let mut p = l.para.clone();
        p.list = None;
        format!("{}{}", para_props(&p, p.indent_start.unwrap_or(0)), text_props(&c, &mut w.fonts))
    };
    let t = one(w, m.title_style.first().unwrap_or(&def));
    let _ = write!(s, r#"<style:style style:name="{name}-title" style:family="presentation"><style:graphic-properties draw:fill="none" draw:stroke="none"/>{t}</style:style>"#);
    let _ = write!(s, r#"<style:style style:name="{name}-subtitle" style:family="presentation"><style:graphic-properties draw:fill="none" draw:stroke="none"/></style:style>"#);
    for (i, l) in body.iter().enumerate() {
        let x = one(w, l);
        let parent = if i == 0 { String::new() } else { format!(r#" style:parent-style-name="{name}-outline{i}""#) };
        let g = if i == 0 { format!(r#"<style:graphic-properties draw:fill="none" draw:stroke="none"><text:list-style style:name="{name}-outline1">{emb}</text:list-style></style:graphic-properties>"#) } else { String::new() };
        let _ = write!(s, r#"<style:style style:name="{name}-outline{}" style:family="presentation"{parent}>{g}{x}</style:style>"#, i + 1);
    }
    let _ = write!(s, r#"<style:style style:name="{name}-notes" style:family="presentation"><style:graphic-properties draw:fill="none" draw:stroke="none"/></style:style>"#);
    s
}

pub fn write<O: Write>(deck: &mut Deck, out: O) -> io::Result<O> {
    deck.load_all().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.0))?;
    let deck: &Deck = deck;
    let mut w = W { deck, media: HashMap::new(), files: Vec::new(), common: String::new(), common_keys: HashMap::new(), fonts: Vec::new() };
    let (dw, dh) = deck.size;

    // Master pages: one per layout.
    let mut ma = Auto::new("M");
    let mut masters_xml = String::new();
    let mut pres_styles = String::new();
    let mut page_of: HashMap<LayoutId, String> = HashMap::new();
    let pl = format!(r#"<style:page-layout style:name="PM1"><style:page-layout-properties fo:margin-top="0cm" fo:margin-bottom="0cm" fo:margin-left="0cm" fo:margin-right="0cm" fo:page-width="{}" fo:page-height="{}" style:print-orientation="{}"/></style:page-layout>"#, cm(dw as f64), cm(dh as f64), if dw >= dh { "landscape" } else { "portrait" });
    for (i, l) in deck.layouts.iter().enumerate() {
        let Some(m) = deck.master(l.master) else { continue };
        let name = format!("Layout{}", i + 1);
        pres_styles.push_str(&master_styles(&mut w, &name, m));
        let bg = l.background.clone().unwrap_or_else(|| m.background.clone());
        let bgx = w.fill(&bg);
        let dp = ma.style("drawing-page", "dp", &format!(r#"<style:drawing-page-properties{bgx} presentation:background-visible="true" presentation:background-objects-visible="true"/>"#));
        let display = if l.name.is_empty() { name.clone() } else { format!("{} - {}", m.name, l.name) };
        let _ = write!(masters_xml, r#"<style:master-page style:name="{name}" style:display-name="{}" style:page-layout-name="PM1" draw:style-name="{dp}">"#, esc(&display));
        let mut stack = Vec::new();
        if !l.hide_master_shapes {
            for sh in &m.shapes {
                // A layout placeholder of the same kind stands for the master's.
                if let Some(p) = sh.ph {
                    if l.shapes.iter().any(|x| x.ph.is_some_and(|q| q.kind.family() == p.kind.family())) {
                        continue;
                    }
                }
                w.shape(Owner::Master, sh, &mut stack, &mut ma, "backgroundobjects", false, &mut masters_xml);
            }
        }
        for sh in &l.shapes {
            w.shape(Owner::Layout(m.id), sh, &mut stack, &mut ma, "backgroundobjects", false, &mut masters_xml);
        }
        masters_xml.push_str("</style:master-page>");
        page_of.insert(l.id, name);
    }

    // Slides.
    let mut ca = Auto::new("");
    let mut pages = String::new();
    for i in 0..deck.slide_count() {
        let Some(s) = deck.slide_if_loaded(i) else { continue };
        let mut dpp = String::new();
        if let Some(bg) = &s.background {
            dpp.push_str(&w.fill(bg));
        }
        let _ = write!(dpp, r#" presentation:background-visible="true" presentation:background-objects-visible="{}""#, !s.hide_layout_shapes);
        if s.hidden {
            dpp.push_str(r#" presentation:visibility="hidden""#);
        }
        if let Some(t) = &s.transition {
            dpp.push_str(&W::transition(t));
        }
        let dp = ca.style("drawing-page", "dp", &format!("<style:drawing-page-properties{dpp}/>"));
        let mp = page_of.get(&s.layout).cloned().or_else(|| page_of.values().next().cloned()).unwrap_or_else(|| "Layout1".into());
        let _ = write!(pages, r#"<draw:page draw:name="page{}" draw:style-name="{dp}" draw:master-page-name="{mp}">"#, i + 1);
        let mut stack = Vec::new();
        for sh in &s.shapes {
            w.shape(Owner::Slide(s.layout), sh, &mut stack, &mut ca, "layout", true, &mut pages);
        }
        pages.push_str(&timing(&s.builds));
        if let Some(n) = &s.notes {
            let mut body = String::new();
            w.text(n, &Vec::new(), &mut ca, &mut body);
            let st = ca.style("presentation", "pr", r#"<style:graphic-properties draw:fill="none" draw:stroke="none"/>"#);
            let _ = write!(pages, r#"<presentation:notes><draw:page-thumbnail draw:layer="layout" svg:width="14cm" svg:height="{}" svg:x="3.5cm" svg:y="2cm" draw:page-number="{}" presentation:class="page"/><draw:frame presentation:style-name="{st}" draw:layer="layout" svg:width="17cm" svg:height="12cm" svg:x="2cm" svg:y="14cm" presentation:class="notes"><draw:text-box>{body}</draw:text-box></draw:frame></presentation:notes>"#, cm(14.0 * 360_000.0 * dh as f64 / dw.max(1) as f64), i + 1);
        }
        pages.push_str("</draw:page>");
    }

    let fonts: String = w.fonts.iter().map(|f| format!(r#"<style:font-face style:name="{0}" svg:font-family="'{0}'"/>"#, esc(f))).collect();
    let content = format!(r#"{HEAD}<office:document-content {NS}><office:font-face-decls>{fonts}</office:font-face-decls><office:automatic-styles>{}</office:automatic-styles><office:body><office:presentation>{pages}</office:presentation></office:body></office:document-content>"#, ca.xml);
    let styles = format!(
        r#"{HEAD}<office:document-styles {NS}><office:font-face-decls>{fonts}</office:font-face-decls><office:styles><style:default-style style:family="graphic"><style:graphic-properties draw:shadow="hidden"/><style:text-properties fo:font-size="18pt"/></style:default-style>{}{pres_styles}</office:styles><office:automatic-styles>{pl}{}</office:automatic-styles><office:master-styles>{masters_xml}</office:master-styles></office:document-styles>"#,
        w.common, ma.xml
    );
    let meta = format!(r#"{HEAD}<office:document-meta {NS}><office:meta><meta:generator>Atrium Officina</meta:generator></office:meta></office:document-meta>"#);

    let mut z = ZipWriter::new(out);
    z.add_stored("mimetype", b"application/vnd.oasis.opendocument.presentation")?;
    let mut manifest = String::from(r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.presentation"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/>"#);
    for (name, _, mime) in &w.files {
        let _ = write!(manifest, r#"<manifest:file-entry manifest:full-path="{name}" manifest:media-type="{mime}"/>"#);
    }
    manifest.push_str("</manifest:manifest>");
    z.add("content.xml", content.as_bytes())?;
    z.add("styles.xml", styles.as_bytes())?;
    z.add("meta.xml", meta.as_bytes())?;
    z.add("META-INF/manifest.xml", manifest.as_bytes())?;
    for (name, bytes, _) in &w.files {
        z.add_stored(name, bytes)?;
    }
    z.finish()
}

/// A shape's gradient with its angle on the page: LibreOffice reads and
/// writes `draw:angle` there, turning it as the shape turns, while ours
/// turns with the shape.
fn page_gradient(f: &Fill, rot: i32) -> Fill {
    match f {
        Fill::Gradient { stops, angle, radial: false } => Fill::Gradient { stops: stops.clone(), angle: (*angle as i64 + rot as i64).rem_euclid(21_600_000) as i32, radial: false },
        f => f.clone(),
    }
}
