//! App icons (forum.md §6.1, dev-tools.md §3.5).
//!
//! [`check`] is the dock's own test of an icon: navigator-svg's strict
//! profile, then filled shapes only, at most
//! [`MAX_SHAPES`](portcullis_toml::icon::MAX_ICON_SHAPES) of them and
//! [`MAX_POINTS`](portcullis_toml::icon::MAX_ICON_POINTS) points, in at most
//! 32 KB. What passes here, the dock draws.
//!
//! [`import`] takes an SVG drawn in another program and makes it one: editor
//! data, metadata and hidden layers go, strokes become the fills they paint,
//! shapes become paths, and what an icon can't keep (script, text, embedded
//! pictures, filters, masks, patterns, outside references) is left out and
//! named, so the person can decide whether the rest is still their icon.

use std::fmt::Write as _;

use navigator_svg::vector::{self, Fill, Paint};
use portcullis_toml::icon::{MAX_ICON_BYTES, MAX_ICON_POINTS, MAX_ICON_SHAPES};

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// The dock's verdict on an icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub shapes: usize,
    pub points: usize,
    pub bytes: usize,
    /// Why the dock would refuse it; `None`: it draws it.
    pub problem: Option<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.problem.is_none()
    }
}

/// The fills an icon draws as, or why it can't be drawn.
fn fills_of(svg: &[u8]) -> Result<(usvg::Tree, Vec<Fill>), String> {
    let (tree, _) = navigator_svg::load(svg).map_err(|e| match e {
        navigator_svg::LoadError::Refused(why) => why.to_string(),
        navigator_svg::LoadError::Corrupt(why) => why,
    })?;
    let s = tree.size();
    if !(s.width().is_finite() && s.height().is_finite() && s.width() > 0.0 && s.height() > 0.0) {
        return Err("no size".into());
    }
    let fills = vector::fills(&tree).map_err(|e| e.to_string())?;
    Ok((tree, fills))
}

/// The dock's check (forum-dock's `bundle_icon::parse`, without drawing).
pub fn check(svg: &[u8]) -> Report {
    let bytes = svg.len();
    if bytes as u64 > MAX_ICON_BYTES {
        return Report { shapes: 0, points: 0, bytes, problem: Some(format!("{} KB: an icon is at most {} KB", bytes.div_ceil(1024), MAX_ICON_BYTES / 1024)) };
    }
    match fills_of(svg) {
        Err(why) => Report { shapes: 0, points: 0, bytes, problem: Some(why) },
        Ok((_, fills)) => {
            let points = fills.iter().map(|f| f.points.len()).sum();
            let problem = if fills.is_empty() {
                Some("nothing to draw".into())
            } else if fills.len() > MAX_ICON_SHAPES {
                Some(format!("{} shapes: the dock draws at most {MAX_ICON_SHAPES}", fills.len()))
            } else if points > MAX_ICON_POINTS {
                Some(format!("{points} points: the dock draws at most {MAX_ICON_POINTS}"))
            } else {
                None
            };
            Report { shapes: fills.len(), points, bytes, problem }
        }
    }
}

/// An SVG made into an icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The icon, as the dock will read it.
    pub svg: String,
    pub report: Report,
    /// What was changed to fit (strokes to fills, …).
    pub converted: Vec<String>,
    /// What was left out, and why.
    pub dropped: Vec<String>,
}

/// Why an element or attribute can't be in an icon, by name.
fn dropped_element(name: &str) -> Option<&'static str> {
    Some(match name {
        "script" => "a script",
        "foreignObject" | "iframe" | "video" | "audio" | "canvas" | "object" | "embed" => "embedded content",
        "image" | "feImage" => "an embedded picture",
        "text" | "tspan" | "textPath" | "tref" | "font" | "font-face" | "glyph" | "altGlyph" =>
            "text (turn it into outlines in the drawing program)",
        "filter" => "a filter effect (blur, shadow)",
        n if n.starts_with("fe") => "a filter effect (blur, shadow)",
        "animate" | "animateMotion" | "animateTransform" | "animateColor" | "set" | "mpath" | "discard" => "animation",
        "marker" => "line markers (arrow heads)",
        "mask" => "a mask",
        "pattern" => "a pattern fill",
        "cursor" => "a cursor",
        _ => return None,
    })
}

/// Elements that carry nothing drawn, left out without a word.
fn quiet_element(name: &str) -> bool {
    matches!(name, "metadata" | "title" | "desc" | "view")
}

fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
}

fn local_refs_only(v: &str) -> bool {
    let mut rest = v;
    while let Some(i) = rest.find("url(") {
        let after = rest[i + 4..].trim_start_matches([' ', '"', '\'']);
        if !after.starts_with('#') {
            return false;
        }
        rest = &rest[i + 4..];
    }
    true
}

struct Clean {
    out: String,
    converted: std::collections::BTreeSet<&'static str>,
    dropped: std::collections::BTreeSet<&'static str>,
}

/// The source with what an icon can't keep taken out.
fn clean(n: roxmltree::Node, c: &mut Clean, depth: usize) {
    if depth > navigator_svg::profile::MAX_DEPTH {
        c.dropped.insert("layers nested too deeply");
        return;
    }
    if !n.is_element() {
        return;
    }
    let ns = n.tag_name().namespace();
    if ns.is_some_and(|ns| ns != SVG_NS) {
        c.converted.insert("the drawing program's own data removed");
        return;
    }
    let name = n.tag_name().name();
    if quiet_element(name) {
        return;
    }
    if let Some(why) = dropped_element(name) {
        c.dropped.insert(why);
        return;
    }
    let hidden = n.attribute("display").is_some_and(|v| v.trim() == "none")
        || n.attribute("visibility").is_some_and(|v| matches!(v.trim(), "hidden" | "collapse"))
        || n.attribute("style").is_some_and(|v| v.replace(' ', "").contains("display:none"));
    if hidden && depth > 1 {
        c.converted.insert("hidden layers left out");
        return;
    }
    if name == "style" {
        let css: String = n.children().filter_map(|t| t.text()).collect();
        if css.contains("@import") || css.contains("@font-face") || css.contains("filter") || !local_refs_only(&css) {
            c.dropped.insert("a style sheet with outside references or filters");
            return;
        }
    }
    c.out.push('<');
    c.out.push_str(name);
    if depth == 1 {
        c.out.push_str(" xmlns=\"");
        c.out.push_str(SVG_NS);
        c.out.push('"');
    }
    for a in n.attributes() {
        let (an, v) = (a.name(), a.value());
        let href = an == "href" && a.namespace().is_none_or(|ns| ns == XLINK_NS);
        if a.namespace().is_some() && !href {
            c.converted.insert("the drawing program's own data removed");
            continue;
        }
        if an.starts_with("on") {
            c.dropped.insert("a script");
            continue;
        }
        if href && !v.starts_with('#') {
            c.dropped.insert("an outside reference");
            continue;
        }
        if an == "filter" {
            if v.trim() != "none" {
                c.dropped.insert("a filter effect (blur, shadow)");
            }
            continue;
        }
        if an == "mask" {
            c.dropped.insert("a mask");
            continue;
        }
        if an.starts_with("marker") {
            c.dropped.insert("line markers (arrow heads)");
            continue;
        }
        let v = if an == "style" {
            // Declarations one by one: the ones an icon can't keep go.
            let kept: Vec<&str> = v.split(';').filter(|d| {
                let k = d.split(':').next().unwrap_or("").trim();
                let keep = !matches!(k, "filter" | "mask") && !k.starts_with("marker") && local_refs_only(d);
                if !keep && !d.trim().is_empty() {
                    c.dropped.insert(if k == "mask" { "a mask" } else if k == "filter" { "a filter effect (blur, shadow)" } else { "an outside reference" });
                }
                keep
            }).collect();
            kept.join(";")
        } else if !local_refs_only(v) {
            c.dropped.insert("an outside reference");
            continue;
        } else {
            v.to_string()
        };
        if an == "stroke" && v.trim() != "none" || an == "style" && v.contains("stroke:") && !v.contains("stroke:none") {
            c.converted.insert("strokes turned into the fills they paint");
        }
        c.out.push(' ');
        c.out.push_str(an);
        c.out.push_str("=\"");
        esc(&v, &mut c.out);
        c.out.push('"');
    }
    c.out.push('>');
    for ch in n.children() {
        if ch.is_text() && name == "style" {
            esc(ch.text().unwrap_or(""), &mut c.out);
        } else {
            clean(ch, c, depth + 1);
        }
    }
    c.out.push_str("</");
    c.out.push_str(name);
    c.out.push('>');
}

fn num(v: f32, out: &mut String) {
    let r = (v * 1000.0).round() / 1000.0;
    let r = if r == 0.0 { 0.0 } else { r };
    let _ = write!(out, "{r}");
}

fn color(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Fills as plain SVG: one `path` each, in its own transform, with its
/// paint; clip rectangles and gradients in `defs`.
fn write_fills(w: f32, h: f32, fills: &[Fill]) -> String {
    let mut defs = String::new();
    let mut body = String::new();
    for (i, f) in fills.iter().enumerate() {
        let mut d = String::new();
        let mut k = 0;
        for &v in &f.verbs {
            let take = match v {
                vector::VERB_MOVE => { d.push('M'); 1 }
                vector::VERB_LINE => { d.push('L'); 1 }
                vector::VERB_QUAD => { d.push('Q'); 2 }
                vector::VERB_CUBIC => { d.push('C'); 3 }
                _ => { d.push('Z'); 0 }
            };
            for p in f.points.iter().skip(k).take(take) {
                num(p[0], &mut d);
                d.push(' ');
                num(p[1], &mut d);
                d.push(' ');
            }
            k += take;
        }
        let (fill, mut alpha) = match &f.paint {
            Paint::Solid(c) => (color(*c), c[3] as f32 / 255.0),
            Paint::Linear { p0, p1, stops } => {
                let _ = write!(defs, "<linearGradient id=\"g{i}\" gradientUnits=\"userSpaceOnUse\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\">",
                    p0[0], p0[1], p1[0], p1[1]);
                for (o, c) in stops {
                    let _ = write!(defs, "<stop offset=\"{o}\" stop-color=\"{}\" stop-opacity=\"{}\"/>", color(*c), c[3] as f32 / 255.0);
                }
                defs.push_str("</linearGradient>");
                (format!("url(#g{i})"), 1.0)
            }
            Paint::Radial { c, r, stops } => {
                let _ = write!(defs, "<radialGradient id=\"g{i}\" gradientUnits=\"userSpaceOnUse\" cx=\"{}\" cy=\"{}\" r=\"{r}\">", c[0], c[1]);
                for (o, col) in stops {
                    let _ = write!(defs, "<stop offset=\"{o}\" stop-color=\"{}\" stop-opacity=\"{}\"/>", color(*col), col[3] as f32 / 255.0);
                }
                defs.push_str("</radialGradient>");
                (format!("url(#g{i})"), 1.0)
            }
        };
        alpha *= f.opacity;
        if let Some([x0, y0, x1, y1]) = f.clip {
            let _ = write!(defs, "<clipPath id=\"c{i}\"><rect x=\"{x0}\" y=\"{y0}\" width=\"{}\" height=\"{}\"/></clipPath>", x1 - x0, y1 - y0);
            let _ = write!(body, "<g clip-path=\"url(#c{i})\">");
        }
        let t = f.transform;
        let _ = write!(body, "<path");
        if t != [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] {
            let _ = write!(body, " transform=\"matrix({} {} {} {} {} {})\"", t[0], t[1], t[2], t[3], t[4], t[5]);
        }
        let _ = write!(body, " d=\"{}\" fill=\"{fill}\"", d.trim_end());
        if alpha < 1.0 {
            let _ = write!(body, " fill-opacity=\"{}\"", (alpha * 1000.0).round() / 1000.0);
        }
        if f.even_odd {
            body.push_str(" fill-rule=\"evenodd\"");
        }
        body.push_str("/>");
        if f.clip.is_some() {
            body.push_str("</g>");
        }
    }
    let mut out = format!("<svg xmlns=\"{SVG_NS}\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\">");
    if !defs.is_empty() {
        out.push_str("<defs>");
        out.push_str(&defs);
        out.push_str("</defs>");
    }
    out.push_str(&body);
    out.push_str("</svg>\n");
    out
}

/// `src` (an SVG from anywhere) made into an icon the dock draws, or why it
/// can't be one.
pub fn import(src: &[u8]) -> Result<Import, String> {
    if src.len() > navigator_svg::profile::MAX_SVG_BYTES {
        return Err(format!("{} KB: larger than any SVG this takes ({} KB)", src.len() / 1024, navigator_svg::profile::MAX_SVG_BYTES / 1024));
    }
    let text = std::str::from_utf8(src).map_err(|_| "not UTF-8 text: is it an SVG?".to_string())?;
    let text = navigator_svg::profile::without_doctype(text).map_err(|e| e.to_string())?;
    let opt = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: (navigator_svg::profile::MAX_ELEMENTS as u32) * 8,
        ..Default::default()
    };
    let doc = roxmltree::Document::parse_with_options(&text, opt).map_err(|e| format!("not an SVG: {e}"))?;
    if doc.root_element().tag_name().name() != "svg" {
        return Err("not an SVG: its root is not <svg>".into());
    }
    let mut c = Clean { out: String::new(), converted: Default::default(), dropped: Default::default() };
    clean(doc.root_element(), &mut c, 1);
    let (tree, fills) = fills_of(c.out.as_bytes()).map_err(|why| format!("even without what an icon can't keep: {why}"))?;
    if fills.is_empty() {
        return Err(if c.dropped.is_empty() { "nothing to draw".into() } else {
            format!("nothing is left to draw without {}", c.dropped.iter().copied().collect::<Vec<_>>().join(", "))
        });
    }
    let size = tree.size();
    let svg = write_fills(size.width(), size.height(), &fills);
    let report = check(svg.as_bytes());
    if let Some(p) = &report.problem {
        return Err(format!("as an icon it is {p}: simplify it in the drawing program"));
    }
    c.converted.insert("shapes written as paths");
    Ok(Import {
        svg,
        report,
        converted: c.converted.into_iter().map(String::from).collect(),
        dropped: c.dropped.into_iter().map(String::from).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dock_check() {
        let ok = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect width="24" height="24" rx="5" fill="#2050a0"/><circle cx="12" cy="12" r="5" fill="#fff"/></svg>"##;
        let r = check(ok);
        assert!(r.ok(), "{r:?}");
        assert_eq!(r.shapes, 2);
        let text = br#"<svg xmlns="http://www.w3.org/2000/svg"><text>hi</text></svg>"#;
        assert!(!check(text).ok());
        let many: String = (0..65).map(|i| format!("<rect x=\"{i}\" width=\"1\" height=\"1\"/>")).collect();
        let r = check(format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 65 1\">{many}</svg>").as_bytes());
        assert_eq!(r.problem.as_deref(), Some("65 shapes: the dock draws at most 64"));
        assert!(!check(&vec![b' '; 40 * 1024]).ok());
    }

    /// What a drawing program writes: its own namespaces and metadata, a
    /// stroke, a hidden layer, text, a blur and a linked picture.
    const FROM_ELSEWHERE: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"
     xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd"
     width="48" height="48" viewBox="0 0 48 48" sodipodi:docname="icon.svg" inkscape:version="1.3">
  <metadata><rdf xmlns="http://www.w3.org/1999/02/22-rdf-syntax-ns#"/></metadata>
  <sodipodi:namedview pagecolor="#ffffff"/>
  <defs><filter id="blur"><feGaussianBlur stdDeviation="2"/></filter>
        <linearGradient id="sky" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#4080ff"/><stop offset="1" stop-color="#2040a0"/></linearGradient></defs>
  <g inkscape:label="Layer 1" inkscape:groupmode="layer">
    <rect x="4" y="4" width="40" height="40" rx="8" fill="url(#sky)" filter="url(#blur)"/>
    <path d="M14 24 L22 32 L34 16" fill="none" stroke="#ffffff" stroke-width="4" stroke-linecap="round"/>
    <text x="4" y="46" font-size="6">Done</text>
    <image xlink:href="https://example.com/x.png" width="8" height="8"/>
  </g>
  <g style="display:none" inkscape:label="Old"><circle cx="24" cy="24" r="20" fill="red"/></g>
</svg>"##;

    #[test]
    fn an_svg_from_elsewhere_becomes_an_icon() {
        assert!(!check(FROM_ELSEWHERE.as_bytes()).ok(), "the source itself is not an icon");
        let i = import(FROM_ELSEWHERE.as_bytes()).unwrap();
        assert!(i.report.ok(), "{:?}", i.report);
        // The rect and the tick's outline.
        assert_eq!(i.report.shapes, 2, "{}", i.svg);
        for d in ["text (turn it into outlines in the drawing program)", "a filter effect (blur, shadow)", "an embedded picture"] {
            assert!(i.dropped.iter().any(|x| x == d), "{d}: {:?}", i.dropped);
        }
        for c in ["strokes turned into the fills they paint", "hidden layers left out", "the drawing program's own data removed"] {
            assert!(i.converted.iter().any(|x| x == c), "{c}: {:?}", i.converted);
        }
        assert!(!i.svg.contains("inkscape") && !i.svg.contains("stroke") && !i.svg.contains("example.com"), "{}", i.svg);
        assert!(i.svg.contains("linearGradient"), "{}", i.svg);
        // What it writes is what the dock takes, and it reads back the same.
        assert_eq!(check(i.svg.as_bytes()), i.report);
        assert_eq!(import(i.svg.as_bytes()).unwrap().svg, i.svg);
    }

    #[test]
    fn what_it_refuses() {
        assert!(import(b"not xml").is_err());
        assert!(import(br#"<html/>"#).is_err());
        let only_text = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><text>A</text></svg>"#;
        let e = import(only_text).unwrap_err();
        assert!(e.contains("nothing is left to draw without text"), "{e}");
        let entity = br#"<!DOCTYPE svg [<!ENTITY x "y">]><svg xmlns="http://www.w3.org/2000/svg"/>"#;
        assert!(import(entity).is_err());
        let many: String = (0..80).map(|i| format!("<rect x=\"{i}\" width=\"1\" height=\"1\"/>")).collect();
        let e = import(format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 80 1\">{many}</svg>").as_bytes()).unwrap_err();
        assert!(e.contains("80 shapes"), "{e}");
    }
}
