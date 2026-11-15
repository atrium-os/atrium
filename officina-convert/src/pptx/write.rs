//! `.pptx` out: the deck as PresentationML (docs/spec/officina.md §9.2, O6c).
//!
//! Masters, layouts and slides keep their shapes, placeholders, level
//! styles, theme colours and preset geometry by name; notes go in notes
//! slides under one notes master; transitions and builds in each slide's
//! `p:transition` and `p:timing`. Pictures and videos are written once,
//! whatever uses them.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Write};

use officina_slides::model::*;
use officina_text::model::{Align, CharAttrs, LineSpacing, ParaProps, VertAlign};

use crate::xml::esc;
use crate::zip::ZipWriter;

const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const CT: &str = "application/vnd.openxmlformats-officedocument.presentationml";
const HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";

/// A part's relationships as they are made.
#[derive(Default)]
struct Rels {
    items: Vec<(String, String, String, bool)>,
}

impl Rels {
    fn add(&mut self, kind: &str, target: &str) -> String {
        if let Some(r) = self.items.iter().find(|r| r.1 == kind && r.2 == target) {
            return r.0.clone();
        }
        let id = format!("rId{}", self.items.len() + 1);
        self.items.push((id.clone(), kind.into(), target.into(), false));
        id
    }
    fn external(&mut self, kind: &str, target: &str) -> String {
        let id = format!("rId{}", self.items.len() + 1);
        self.items.push((id.clone(), kind.into(), target.into(), true));
        id
    }
    fn xml(&self) -> String {
        let mut s = format!("{HEAD}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">");
        for (id, kind, target, ext) in &self.items {
            let ty = if kind == "media" { "http://schemas.microsoft.com/office/2007/relationships/media".to_string() } else { format!("{REL}/{kind}") };
            let _ = write!(s, "<Relationship Id=\"{id}\" Type=\"{ty}\" Target=\"{}\"{}/>", esc(target), if *ext { " TargetMode=\"External\"" } else { "" });
        }
        s.push_str("</Relationships>");
        s
    }
}

pub(crate) fn ext_of(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpeg",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "image/webp" => "webp",
        "image/emf" => "emf",
        "image/wmf" => "wmf",
        "image/tiff" => "tiff",
        "image/bmp" => "bmp",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/wav" => "wav",
        _ => "bin",
    }
}

/// A one-pixel black PNG: the poster of a video that has none.
const BLACK_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x27, 0x34, 0x27, 0x0A, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn color(c: &Color) -> String {
    let mods: String = c
        .mods
        .iter()
        .map(|m| {
            let (n, v) = match *m {
                ColorMod::LumMod(v) => ("lumMod", v),
                ColorMod::LumOff(v) => ("lumOff", v),
                ColorMod::Tint(v) => ("tint", v),
                ColorMod::Shade(v) => ("shade", v),
                ColorMod::SatMod(v) => ("satMod", v),
                ColorMod::Alpha(v) => ("alpha", v),
            };
            format!("<a:{n} val=\"{v}\"/>")
        })
        .collect();
    match c.base {
        ColorBase::Rgb(v) => format!("<a:srgbClr val=\"{:06X}\">{mods}</a:srgbClr>", v & 0xFF_FFFF),
        ColorBase::Theme(s) => format!("<a:schemeClr val=\"{}\">{mods}</a:schemeClr>", s.name()),
    }
}

/// The writer's state for one package.
struct W<'d> {
    deck: &'d mut Deck,
    /// Media written: id → part name.
    media: HashMap<MediaId, String>,
    zip_media: Vec<(String, std::sync::Arc<[u8]>)>,
    poster: Option<String>,
}

impl W<'_> {
    fn media_part(&mut self, id: MediaId) -> Option<String> {
        if let Some(p) = self.media.get(&id) {
            return Some(p.clone());
        }
        let mime = self.deck.media(id)?.mime.clone();
        let bytes = self.deck.media_bytes(id).ok()?;
        let name = format!("ppt/media/media{}.{}", self.media.len() + 1, ext_of(&mime));
        self.zip_media.push((name.clone(), bytes.into()));
        self.media.insert(id, name.clone());
        Some(name)
    }

    fn fill(&mut self, f: &Fill, rels: &mut Rels, s: &mut String) {
        match f {
            Fill::None => s.push_str("<a:noFill/>"),
            Fill::Solid(c) => {
                let _ = write!(s, "<a:solidFill>{}</a:solidFill>", color(c));
            }
            Fill::Gradient { stops, angle, radial } => {
                s.push_str("<a:gradFill rotWithShape=\"1\"><a:gsLst>");
                for g in stops {
                    let _ = write!(s, "<a:gs pos=\"{}\">{}</a:gs>", g.pos, color(&g.color));
                }
                s.push_str("</a:gsLst>");
                if *radial {
                    s.push_str("<a:path path=\"circle\"><a:fillToRect l=\"50000\" t=\"50000\" r=\"50000\" b=\"50000\"/></a:path>");
                } else {
                    let _ = write!(s, "<a:lin ang=\"{angle}\" scaled=\"0\"/>");
                }
                s.push_str("</a:gradFill>");
            }
            Fill::Picture(m) => match self.media_part(*m) {
                Some(p) => {
                    let id = rels.add("image", &format!("../{}", p.trim_start_matches("ppt/")));
                    let _ = write!(s, "<a:blipFill rotWithShape=\"1\"><a:blip r:embed=\"{id}\"/><a:stretch><a:fillRect/></a:stretch></a:blipFill>");
                }
                None => s.push_str("<a:noFill/>"),
            },
        }
    }

    fn line(&mut self, l: &Stroke, s: &mut String) {
        let _ = write!(s, "<a:ln w=\"{}\">", l.width.max(0));
        match &l.color {
            Some(c) => {
                let _ = write!(s, "<a:solidFill>{}</a:solidFill>", color(c));
            }
            None => s.push_str("<a:noFill/>"),
        }
        let dash = match l.dash {
            Dash::Solid => "solid",
            Dash::Dot => "sysDot",
            Dash::Dash => "dash",
            Dash::DashDot => "dashDot",
            Dash::LongDash => "lgDash",
        };
        let _ = write!(s, "<a:prstDash val=\"{dash}\"/>");
        for (tag, a) in [("headEnd", l.head), ("tailEnd", l.tail)] {
            let t = match a {
                Arrow::None => continue,
                Arrow::Triangle => "triangle",
                Arrow::Open => "arrow",
                Arrow::Stealth => "stealth",
                Arrow::Diamond => "diamond",
                Arrow::Oval => "oval",
            };
            let _ = write!(s, "<a:{tag} type=\"{t}\"/>");
        }
        s.push_str("</a:ln>");
    }

    fn geom(&self, g: &Geom, s: &mut String) {
        match g {
            Geom::Preset { name, adj } => {
                let _ = write!(s, "<a:prstGeom prst=\"{}\"><a:avLst>", esc(name));
                for (n, v) in adj {
                    let _ = write!(s, "<a:gd name=\"{}\" fmla=\"val {v}\"/>", esc(n));
                }
                s.push_str("</a:avLst></a:prstGeom>");
            }
            Geom::Custom(paths) => {
                s.push_str("<a:custGeom><a:avLst/><a:gdLst/><a:ahLst/><a:cxnLst/><a:rect l=\"0\" t=\"0\" r=\"r\" b=\"b\"/><a:pathLst>");
                for p in paths {
                    let _ = write!(s, "<a:path w=\"{}\" h=\"{}\"{}{}>", p.w, p.h, if p.filled { "" } else { " fill=\"none\"" }, if p.stroked { "" } else { " stroke=\"0\"" });
                    for c in &p.cmds {
                        match *c {
                            PathCmd::Move(x, y) => {
                                let _ = write!(s, "<a:moveTo><a:pt x=\"{x}\" y=\"{y}\"/></a:moveTo>");
                            }
                            PathCmd::Line(x, y) => {
                                let _ = write!(s, "<a:lnTo><a:pt x=\"{x}\" y=\"{y}\"/></a:lnTo>");
                            }
                            PathCmd::Cubic(a, b, c, d, e, f) => {
                                let _ = write!(s, "<a:cubicBezTo><a:pt x=\"{a}\" y=\"{b}\"/><a:pt x=\"{c}\" y=\"{d}\"/><a:pt x=\"{e}\" y=\"{f}\"/></a:cubicBezTo>");
                            }
                            PathCmd::Close => s.push_str("<a:close/>"),
                        }
                    }
                    s.push_str("</a:path>");
                }
                s.push_str("</a:pathLst></a:custGeom>");
            }
        }
    }

    fn xfrm(sh: &Shape, tag: &str, child: Option<Rect>, s: &mut String) {
        let Some(f) = sh.frame else { return };
        let _ = write!(s, "<{tag}{}{}{}>", if sh.rot != 0 { format!(" rot=\"{}\"", sh.rot) } else { String::new() }, if sh.flip_h { " flipH=\"1\"" } else { "" }, if sh.flip_v { " flipV=\"1\"" } else { "" });
        let _ = write!(s, "<a:off x=\"{}\" y=\"{}\"/><a:ext cx=\"{}\" cy=\"{}\"/>", f.x, f.y, f.w.max(0), f.h.max(0));
        if let Some(c) = child {
            let _ = write!(s, "<a:chOff x=\"{}\" y=\"{}\"/><a:chExt cx=\"{}\" cy=\"{}\"/>", c.x, c.y, c.w.max(0), c.h.max(0));
        }
        let _ = write!(s, "</{tag}>");
    }

    fn rpr(a: &CharAttrs, color_override: Option<&Color>, tag: &str, rels: &mut Rels, s: &mut String) {
        let mut at = String::from(" lang=\"en-US\"");
        if let Some(v) = a.size {
            let _ = write!(at, " sz=\"{}\"", v as u32 * 50);
        }
        if let Some(b) = a.bold {
            let _ = write!(at, " b=\"{}\"", b as u8);
        }
        if let Some(i) = a.italic {
            let _ = write!(at, " i=\"{}\"", i as u8);
        }
        if let Some(u) = a.underline {
            at.push_str(if u { " u=\"sng\"" } else { " u=\"none\"" });
        }
        if let Some(st) = a.strike {
            at.push_str(if st { " strike=\"sngStrike\"" } else { " strike=\"noStrike\"" });
        }
        match a.vert {
            Some(VertAlign::Super) => at.push_str(" baseline=\"30000\""),
            Some(VertAlign::Sub) => at.push_str(" baseline=\"-25000\""),
            _ => {}
        }
        let mut kids = String::new();
        if let Some(c) = color_override {
            let _ = write!(kids, "<a:solidFill>{}</a:solidFill>", color(c));
        } else if let Some(c) = a.color {
            let _ = write!(kids, "<a:solidFill><a:srgbClr val=\"{:06X}\"/></a:solidFill>", c & 0xFF_FFFF);
        }
        if let Some(h) = a.highlight {
            let _ = write!(kids, "<a:highlight><a:srgbClr val=\"{:06X}\"/></a:highlight>", h & 0xFF_FFFF);
        }
        if let Some(f) = &a.family {
            let _ = write!(kids, "<a:latin typeface=\"{}\"/>", esc(f));
        }
        if let Some(l) = &a.link {
            let id = rels.external("hyperlink", l);
            let _ = write!(kids, "<a:hlinkClick r:id=\"{id}\"/>");
        }
        if kids.is_empty() {
            let _ = write!(s, "<a:{tag}{at}/>");
        } else {
            let _ = write!(s, "<a:{tag}{at}>{kids}</a:{tag}>");
        }
    }

    fn bullet(b: &Bullet, s: &mut String) {
        match b {
            Bullet::None => s.push_str("<a:buNone/>"),
            Bullet::Char { ch, font, color: c, size_pct } => {
                if let Some(c) = c {
                    let _ = write!(s, "<a:buClr>{}</a:buClr>", color(c));
                }
                if let Some(p) = size_pct {
                    let _ = write!(s, "<a:buSzPct val=\"{p}\"/>");
                }
                if let Some(f) = font {
                    let _ = write!(s, "<a:buFont typeface=\"{}\"/>", esc(f));
                }
                let _ = write!(s, "<a:buChar char=\"{}\"/>", esc(&ch.to_string()));
            }
            Bullet::Number { scheme, start } => {
                let _ = write!(s, "<a:buAutoNum type=\"{}\"{}/>", esc(scheme), if *start != 1 { format!(" startAt=\"{start}\"") } else { String::new() });
            }
        }
    }

    /// Paragraph properties as `a:pPr`/`a:lvlNpPr` attributes and the
    /// spacing children (bullets and defaults follow, in schema order).
    fn ppr_parts(p: &ParaProps) -> (String, String) {
        let mut at = String::new();
        let emu = |tw: i32| tw as i64 * 635;
        if let Some(v) = p.indent_start {
            let _ = write!(at, " marL=\"{}\"", emu(v));
        }
        if let Some(v) = p.indent_end {
            let _ = write!(at, " marR=\"{}\"", emu(v));
        }
        if let Some(v) = p.first_line {
            let _ = write!(at, " indent=\"{}\"", emu(v));
        }
        if let Some(a) = p.align {
            let _ = write!(at, " algn=\"{}\"", match a {
                Align::Start => "l",
                Align::Center => "ctr",
                Align::End => "r",
                Align::Justify => "just",
            });
        }
        let mut kids = String::new();
        match p.line_spacing {
            Some(LineSpacing::Multiple(m)) => {
                let _ = write!(kids, "<a:lnSpc><a:spcPct val=\"{}\"/></a:lnSpc>", m as u32 * 1000);
            }
            Some(LineSpacing::Exact(t)) | Some(LineSpacing::AtLeast(t)) => {
                let _ = write!(kids, "<a:lnSpc><a:spcPts val=\"{}\"/></a:lnSpc>", t.max(0) * 5);
            }
            None => {}
        }
        if let Some(v) = p.space_before {
            let _ = write!(kids, "<a:spcBef><a:spcPts val=\"{}\"/></a:spcBef>", v.max(0) * 5);
        }
        if let Some(v) = p.space_after {
            let _ = write!(kids, "<a:spcAft><a:spcPts val=\"{}\"/></a:spcAft>", v.max(0) * 5);
        }
        (at, kids)
    }

    fn levels(levels: &ListStyle, rels: &mut Rels, s: &mut String) {
        for (i, l) in levels.iter().enumerate().take(MAX_LEVELS) {
            if *l == LevelStyle::default() {
                continue;
            }
            let (at, kids) = Self::ppr_parts(&l.para);
            let _ = write!(s, "<a:lvl{}pPr{at}>{kids}", i + 1);
            if let Some(b) = &l.bullet {
                Self::bullet(b, s);
            }
            if l.chars != CharAttrs::default() || l.color.is_some() {
                Self::rpr(&l.chars, l.color.as_ref(), "defRPr", rels, s);
            }
            let _ = write!(s, "</a:lvl{}pPr>", i + 1);
        }
    }

    fn body(t: &TextBody, tag: &str, rels: &mut Rels, s: &mut String) {
        let _ = write!(s, "<{tag}><a:bodyPr");
        if let Some(i) = t.inset {
            let _ = write!(s, " lIns=\"{}\" tIns=\"{}\" rIns=\"{}\" bIns=\"{}\"", i[0], i[1], i[2], i[3]);
        }
        if let Some(a) = t.anchor {
            let _ = write!(s, " anchor=\"{}\"", match a {
                Anchor::Top => "t",
                Anchor::Middle => "ctr",
                Anchor::Bottom => "b",
            });
        }
        if let Some(w) = t.wrap {
            s.push_str(if w { " wrap=\"square\"" } else { " wrap=\"none\"" });
        }
        s.push_str(" rtlCol=\"0\">");
        match t.autofit {
            Some(Autofit::None) => s.push_str("<a:noAutofit/>"),
            Some(Autofit::Shrink { scale, spacing_cut }) => {
                let _ = write!(s, "<a:normAutofit{}{}/>", if scale < 100_000 { format!(" fontScale=\"{scale}\"") } else { String::new() }, if spacing_cut > 0 { format!(" lnSpcReduction=\"{spacing_cut}\"") } else { String::new() });
            }
            Some(Autofit::Resize) => s.push_str("<a:spAutoFit/>"),
            None => {}
        }
        s.push_str("</a:bodyPr><a:lstStyle>");
        Self::levels(&t.levels, rels, s);
        s.push_str("</a:lstStyle>");
        let d = &t.doc;
        let mut any = false;
        for b in d.blocks.iter() {
            let Some(p) = b.as_para() else { continue };
            any = true;
            s.push_str("<a:p>");
            let (mut at, kids) = Self::ppr_parts(&p.props);
            if p.style > 0 {
                at = format!(" lvl=\"{}\"{at}", p.style.min(8));
            }
            let mut bul = String::new();
            match &p.props.list {
                Some(None) => bul.push_str("<a:buNone/>"),
                Some(Some(r)) => {
                    if let Some(b) = t.bullets.get(r.id as usize - 1) {
                        Self::bullet(b, &mut bul);
                    }
                }
                None => {}
            }
            if !at.is_empty() || !kids.is_empty() || !bul.is_empty() {
                let _ = write!(s, "<a:pPr{at}>{kids}{bul}</a:pPr>");
            }
            // Runs, cut at line breaks and slide-number fields.
            let text = &p.text;
            let bounds: Vec<(usize, usize, u32)> = {
                let mut v = Vec::new();
                for (i, r) in p.runs.iter().enumerate() {
                    let start = (r.0 as usize).min(text.len());
                    let end = p.runs.get(i + 1).map_or(text.len(), |n| (n.0 as usize).min(text.len()));
                    v.push((start, end, r.1));
                }
                v
            };
            let mut last = CharAttrs::default();
            for (start, end, aid) in bounds {
                let a = d.attrs.get(aid as _).clone();
                last = a.clone();
                let seg = &text[start..end];
                let mut rest = seg;
                while !rest.is_empty() {
                    let br = rest.find('\u{2028}');
                    let fld = rest.find("‹#›");
                    let cut = match (br, fld) {
                        (Some(b), Some(f)) => b.min(f),
                        (Some(b), None) => b,
                        (None, Some(f)) => f,
                        (None, None) => rest.len(),
                    };
                    if cut > 0 {
                        s.push_str("<a:r>");
                        Self::rpr(&a, None, "rPr", rels, s);
                        let _ = write!(s, "<a:t>{}</a:t></a:r>", esc(&rest[..cut]));
                    }
                    rest = &rest[cut..];
                    if rest.starts_with('\u{2028}') {
                        s.push_str("<a:br>");
                        Self::rpr(&a, None, "rPr", rels, s);
                        s.push_str("</a:br>");
                        rest = &rest['\u{2028}'.len_utf8()..];
                    } else if rest.starts_with("‹#›") {
                        s.push_str("<a:fld id=\"{B6F15528-21DE-4FAA-801E-634DDDAF4B2B}\" type=\"slidenum\">");
                        Self::rpr(&a, None, "rPr", rels, s);
                        s.push_str("<a:t>‹#›</a:t></a:fld>");
                        rest = &rest["‹#›".len()..];
                    }
                }
            }
            Self::rpr(&last, None, "endParaRPr", rels, s);
            s.push_str("</a:p>");
        }
        if !any {
            s.push_str("<a:p><a:endParaRPr lang=\"en-US\"/></a:p>");
        }
        let _ = write!(s, "</{tag}>");
    }

    fn nv(sh: &Shape, kind: &str, s: &mut String) {
        let _ = write!(s, "<p:cNvPr id=\"{}\" name=\"{}\"{}{}/>", sh.id as u64 + 1, esc(&sh.name), if sh.descr.is_empty() { String::new() } else { format!(" descr=\"{}\"", esc(&sh.descr)) }, if sh.hidden { " hidden=\"1\"" } else { "" });
        s.push_str(kind);
    }

    fn nvpr(sh: &Shape, extra: &str, s: &mut String) {
        s.push_str("<p:nvPr>");
        if let Some(ph) = sh.ph {
            s.push_str("<p:ph");
            if ph.kind != PhKind::Obj {
                let _ = write!(s, " type=\"{}\"", ph.kind.name());
            }
            if ph.idx != 0 {
                let _ = write!(s, " idx=\"{}\"", ph.idx);
            }
            s.push_str("/>");
        }
        s.push_str(extra);
        s.push_str("</p:nvPr>");
    }

    fn shapes(&mut self, list: &[Shape], rels: &mut Rels, s: &mut String) {
        for sh in list {
            self.shape(sh, rels, s);
        }
    }

    fn shape(&mut self, sh: &Shape, rels: &mut Rels, s: &mut String) {
        match &sh.kind {
            ShapeKind::Geom { geom, fill, line, text } => {
                s.push_str("<p:sp><p:nvSpPr>");
                let text_box = sh.ph.is_none() && text.is_some() && matches!(fill, Some(Fill::None) | None) && geom.as_ref().is_some_and(|g| matches!(g, Geom::Preset { name, .. } if &**name == "rect"));
                Self::nv(sh, if text_box { "<p:cNvSpPr txBox=\"1\"/>" } else { "<p:cNvSpPr/>" }, s);
                Self::nvpr(sh, "", s);
                s.push_str("</p:nvSpPr><p:spPr>");
                Self::xfrm(sh, "a:xfrm", None, s);
                match geom {
                    Some(g) => self.geom(g, s),
                    None if sh.ph.is_none() => self.geom(&Geom::preset("rect"), s),
                    None => {}
                }
                if let Some(f) = fill {
                    self.fill(f, rels, s);
                }
                if let Some(l) = line {
                    self.line(l, s);
                }
                s.push_str("</p:spPr>");
                if let Some(t) = text {
                    Self::body(t, "p:txBody", rels, s);
                }
                s.push_str("</p:sp>");
            }
            ShapeKind::Picture { media, crop, line } => {
                let Some(p) = self.media_part(*media) else { return };
                let rid = rels.add("image", &format!("../{}", p.trim_start_matches("ppt/")));
                s.push_str("<p:pic><p:nvPicPr>");
                Self::nv(sh, "<p:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></p:cNvPicPr>", s);
                Self::nvpr(sh, "", s);
                let _ = write!(s, "</p:nvPicPr><p:blipFill><a:blip r:embed=\"{rid}\"/>");
                if *crop != [0; 4] {
                    let _ = write!(s, "<a:srcRect l=\"{}\" t=\"{}\" r=\"{}\" b=\"{}\"/>", crop[0], crop[1], crop[2], crop[3]);
                }
                s.push_str("<a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>");
                Self::xfrm(sh, "a:xfrm", None, s);
                s.push_str("<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>");
                if let Some(l) = line {
                    self.line(l, s);
                }
                s.push_str("</p:spPr></p:pic>");
            }
            ShapeKind::Video { media, poster } => {
                let Some(v) = self.media_part(*media) else { return };
                let target = format!("../{}", v.trim_start_matches("ppt/"));
                let vid = rels.add("video", &target);
                let mid = rels.add("media", &target);
                let poster = match poster.and_then(|p| self.media_part(p)) {
                    Some(p) => p,
                    None => {
                        let name = "ppt/media/poster.png".to_string();
                        if self.poster.is_none() {
                            self.zip_media.push((name.clone(), BLACK_PNG.into()));
                            self.poster = Some(name.clone());
                        }
                        name
                    }
                };
                let rid = rels.add("image", &format!("../{}", poster.trim_start_matches("ppt/")));
                s.push_str("<p:pic><p:nvPicPr>");
                Self::nv(sh, "<p:cNvPicPr><a:picLocks noChangeAspect=\"1\"/></p:cNvPicPr>", s);
                Self::nvpr(sh, &format!("<a:videoFile r:link=\"{vid}\"/><p:extLst><p:ext uri=\"{{DAA4B4D4-6D71-4841-9C94-3DE7FCFB9230}}\"><p14:media xmlns:p14=\"http://schemas.microsoft.com/office/powerpoint/2010/main\" r:embed=\"{mid}\"/></p:ext></p:extLst>"), s);
                let _ = write!(s, "</p:nvPicPr><p:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr>");
                Self::xfrm(sh, "a:xfrm", None, s);
                s.push_str("<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr></p:pic>");
            }
            ShapeKind::Group { child, shapes } => {
                s.push_str("<p:grpSp><p:nvGrpSpPr>");
                Self::nv(sh, "<p:cNvGrpSpPr/>", s);
                Self::nvpr(sh, "", s);
                s.push_str("</p:nvGrpSpPr><p:grpSpPr>");
                Self::xfrm(sh, "a:xfrm", Some(*child), s);
                s.push_str("</p:grpSpPr>");
                self.shapes(shapes, rels, s);
                s.push_str("</p:grpSp>");
            }
        }
    }

    fn tree(&mut self, shapes: &[Shape], rels: &mut Rels, s: &mut String) {
        s.push_str("<p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"0\" cy=\"0\"/><a:chOff x=\"0\" y=\"0\"/><a:chExt cx=\"0\" cy=\"0\"/></a:xfrm></p:grpSpPr>");
        self.shapes(shapes, rels, s);
        s.push_str("</p:spTree>");
    }

    fn background(&mut self, bg: Option<&Fill>, rels: &mut Rels, s: &mut String) {
        if let Some(f) = bg {
            s.push_str("<p:bg><p:bgPr>");
            self.fill(f, rels, s);
            s.push_str("<a:effectLst/></p:bgPr></p:bg>");
        }
    }
}

fn transition(t: &Transition) -> String {
    let spd = if t.ms <= 500 { "fast" } else if t.ms <= 750 { "med" } else { "slow" };
    let dir = |d: Dir| match d {
        Dir::Left => "l",
        Dir::Up => "u",
        Dir::Right => "r",
        Dir::Down => "d",
    };
    let kid = match t.kind {
        TransitionKind::Cut => "<p:cut/>".to_string(),
        TransitionKind::Fade => "<p:fade/>".to_string(),
        TransitionKind::Push(d) => format!("<p:push dir=\"{}\"/>", dir(d)),
        TransitionKind::Wipe(d) => format!("<p:wipe dir=\"{}\"/>", dir(d)),
        TransitionKind::Cover(d) => format!("<p:cover dir=\"{}\"/>", dir(d)),
        TransitionKind::Uncover(d) => format!("<p:pull dir=\"{}\"/>", dir(d)),
        TransitionKind::Split { vertical, out } => format!("<p:split orient=\"{}\" dir=\"{}\"/>", if vertical { "vert" } else { "horz" }, if out { "out" } else { "in" }),
        TransitionKind::Zoom { out } => format!("<p:zoom dir=\"{}\"/>", if out { "out" } else { "in" }),
        TransitionKind::Dissolve => "<p:dissolve/>".to_string(),
    };
    format!("<p:transition spd=\"{spd}\"{}{}>{kid}</p:transition>", if t.on_click { "" } else { " advClick=\"0\"" }, t.after_ms.map(|a| format!(" advTm=\"{a}\"")).unwrap_or_default())
}

/// Builds as a main sequence of click groups (PowerPoint's timing tree).
fn timing(builds: &[Build]) -> String {
    if builds.is_empty() {
        return String::new();
    }
    let mut n = 2u32;
    let mut next = || {
        n += 1;
        n
    };
    let mut groups: Vec<Vec<Vec<&Build>>> = Vec::new();
    for b in builds {
        match b.trigger {
            Trigger::OnClick => groups.push(vec![vec![b]]),
            Trigger::WithPrevious => match groups.last_mut().and_then(|g| g.last_mut()) {
                Some(p) => p.push(b),
                None => groups.push(vec![vec![b]]),
            },
            Trigger::AfterPrevious => match groups.last_mut() {
                Some(g) => g.push(vec![b]),
                None => groups.push(vec![vec![b]]),
            },
        }
    }
    let first_auto = builds[0].trigger != Trigger::OnClick;
    let mut s = String::from("<p:timing><p:tnLst><p:par><p:cTn id=\"1\" dur=\"indefinite\" restart=\"never\" nodeType=\"tmRoot\"><p:childTnLst><p:seq concurrent=\"1\" nextAc=\"seek\"><p:cTn id=\"2\" dur=\"indefinite\" nodeType=\"mainSeq\"><p:childTnLst>");
    for (gi, g) in groups.iter().enumerate() {
        let cond = if gi == 0 && first_auto { "<p:cond delay=\"indefinite\"/><p:cond evt=\"onBegin\" delay=\"0\"><p:tn val=\"2\"/></p:cond>" } else { "<p:cond delay=\"indefinite\"/>" };
        let _ = write!(s, "<p:par><p:cTn id=\"{}\" fill=\"hold\"><p:stCondLst>{cond}</p:stCondLst><p:childTnLst>", next());
        let mut at = 0u32;
        for (pi, p) in g.iter().enumerate() {
            let _ = write!(s, "<p:par><p:cTn id=\"{}\" fill=\"hold\"><p:stCondLst><p:cond delay=\"{at}\"/></p:stCondLst><p:childTnLst>", next());
            let mut longest = 0;
            for (bi, b) in p.iter().enumerate() {
                let node = if pi > 0 && bi == 0 { "afterEffect" } else if bi > 0 || (gi == 0 && first_auto && pi == 0) { "withEffect" } else { "clickEffect" };
                let (class, preset, sub) = match b.effect {
                    Effect::Appear => ("entr", 1, 0),
                    Effect::Fade => ("entr", 10, 0),
                    Effect::FlyIn(d) => ("entr", 2, dir_sub(d)),
                    Effect::Wipe(d) => ("entr", 22, dir_sub(d)),
                    Effect::Zoom => ("entr", 53, 16),
                    Effect::Disappear => ("exit", 1, 0),
                    Effect::FadeOut => ("exit", 10, 0),
                };
                let spid = b.shape as u64 + 1;
                let ms = b.ms.max(1);
                longest = longest.max(ms + b.delay_ms);
                let _ = write!(s, "<p:par><p:cTn id=\"{}\" presetID=\"{preset}\" presetClass=\"{class}\" presetSubtype=\"{sub}\" fill=\"hold\" grpId=\"0\" nodeType=\"{node}\"><p:stCondLst><p:cond delay=\"{}\"/></p:stCondLst><p:childTnLst>", next(), b.delay_ms);
                let tgt = format!("<p:tgtEl><p:spTgt spid=\"{spid}\"/></p:tgtEl>");
                let set = |s: &mut String, id: u32, val: &str, delay: u32| {
                    let _ = write!(s, "<p:set><p:cBhvr><p:cTn id=\"{id}\" dur=\"1\" fill=\"hold\"><p:stCondLst><p:cond delay=\"{delay}\"/></p:stCondLst></p:cTn>{tgt}<p:attrNameLst><p:attrName>style.visibility</p:attrName></p:attrNameLst></p:cBhvr><p:to><p:strVal val=\"{val}\"/></p:to></p:set>");
                };
                match b.effect {
                    Effect::Appear => set(&mut s, next(), "visible", 0),
                    Effect::Fade | Effect::Wipe(_) => {
                        set(&mut s, next(), "visible", 0);
                        let filter = match b.effect {
                            Effect::Wipe(d) => format!("wipe({})", match d {
                                Dir::Left => "left",
                                Dir::Up => "up",
                                Dir::Right => "right",
                                Dir::Down => "down",
                            }),
                            _ => "fade".into(),
                        };
                        let _ = write!(s, "<p:animEffect transition=\"in\" filter=\"{filter}\"><p:cBhvr><p:cTn id=\"{}\" dur=\"{ms}\"/>{tgt}</p:cBhvr></p:animEffect>", next());
                    }
                    Effect::FlyIn(d) => {
                        set(&mut s, next(), "visible", 0);
                        let (fx, fy) = match d {
                            Dir::Left => ("0-#ppt_w/2", "#ppt_y"),
                            Dir::Right => ("1+#ppt_w/2", "#ppt_y"),
                            Dir::Up => ("#ppt_x", "0-#ppt_h/2"),
                            Dir::Down => ("#ppt_x", "1+#ppt_h/2"),
                        };
                        for (attr, from, to) in [("ppt_x", fx, "#ppt_x"), ("ppt_y", fy, "#ppt_y")] {
                            let _ = write!(s, "<p:anim calcmode=\"lin\" valueType=\"num\"><p:cBhvr additive=\"base\"><p:cTn id=\"{}\" dur=\"{ms}\" fill=\"hold\"/>{tgt}<p:attrNameLst><p:attrName>{attr}</p:attrName></p:attrNameLst></p:cBhvr><p:tavLst><p:tav tm=\"0\"><p:val><p:strVal val=\"{from}\"/></p:val></p:tav><p:tav tm=\"100000\"><p:val><p:strVal val=\"{to}\"/></p:val></p:tav></p:tavLst></p:anim>", next());
                        }
                    }
                    Effect::Zoom => {
                        set(&mut s, next(), "visible", 0);
                        for (attr, to) in [("ppt_w", "#ppt_w"), ("ppt_h", "#ppt_h")] {
                            let _ = write!(s, "<p:anim calcmode=\"lin\" valueType=\"num\"><p:cBhvr><p:cTn id=\"{}\" dur=\"{ms}\" fill=\"hold\"/>{tgt}<p:attrNameLst><p:attrName>{attr}</p:attrName></p:attrNameLst></p:cBhvr><p:tavLst><p:tav tm=\"0\"><p:val><p:fltVal val=\"0\"/></p:val></p:tav><p:tav tm=\"100000\"><p:val><p:strVal val=\"{to}\"/></p:val></p:tav></p:tavLst></p:anim>", next());
                        }
                        let _ = write!(s, "<p:animEffect transition=\"in\" filter=\"fade\"><p:cBhvr><p:cTn id=\"{}\" dur=\"{ms}\"/>{tgt}</p:cBhvr></p:animEffect>", next());
                    }
                    Effect::Disappear => set(&mut s, next(), "hidden", 0),
                    Effect::FadeOut => {
                        let _ = write!(s, "<p:animEffect transition=\"out\" filter=\"fade\"><p:cBhvr><p:cTn id=\"{}\" dur=\"{ms}\"/>{tgt}</p:cBhvr></p:animEffect>", next());
                        set(&mut s, next(), "hidden", ms.saturating_sub(1));
                    }
                }
                s.push_str("</p:childTnLst></p:cTn></p:par>");
            }
            s.push_str("</p:childTnLst></p:cTn></p:par>");
            at += longest;
        }
        s.push_str("</p:childTnLst></p:cTn></p:par>");
    }
    s.push_str("</p:childTnLst></p:cTn><p:prevCondLst><p:cond evt=\"onPrev\" delay=\"0\"><p:tgtEl><p:sldTgt/></p:tgtEl></p:cond></p:prevCondLst><p:nextCondLst><p:cond evt=\"onNext\" delay=\"0\"><p:tgtEl><p:sldTgt/></p:tgtEl></p:cond></p:nextCondLst></p:seq></p:childTnLst></p:cTn></p:par></p:tnLst><p:bldLst>");
    let mut seen = std::collections::HashSet::new();
    for b in builds {
        if seen.insert(b.shape) {
            let _ = write!(s, "<p:bldP spid=\"{}\" grpId=\"0\"{}/>", b.shape as u64 + 1, if b.by_paragraph { " build=\"p\"" } else { "" });
        }
    }
    s.push_str("</p:bldLst></p:timing>");
    s
}

fn dir_sub(d: Dir) -> u32 {
    match d {
        Dir::Left => 8,
        Dir::Up => 1,
        Dir::Right => 2,
        Dir::Down => 4,
    }
}

fn theme_xml(t: &Theme) -> String {
    let tags = ["dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink"];
    let mut s = format!("{HEAD}<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"{}\"><a:themeElements><a:clrScheme name=\"{}\">", esc(&t.name), esc(&t.name));
    for (i, tag) in tags.iter().enumerate() {
        let _ = write!(s, "<a:{tag}><a:srgbClr val=\"{:06X}\"/></a:{tag}>", t.colors[i] & 0xFF_FFFF);
    }
    let _ = write!(s, "</a:clrScheme><a:fontScheme name=\"{}\"><a:majorFont><a:latin typeface=\"{}\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont><a:minorFont><a:latin typeface=\"{}\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme>", esc(&t.name), esc(&t.major), esc(&t.minor));
    let grad = "<a:gradFill rotWithShape=\"1\"><a:gsLst><a:gs pos=\"0\"><a:schemeClr val=\"phClr\"><a:lumMod val=\"110000\"/><a:satMod val=\"105000\"/><a:tint val=\"67000\"/></a:schemeClr></a:gs><a:gs pos=\"50000\"><a:schemeClr val=\"phClr\"><a:lumMod val=\"105000\"/><a:satMod val=\"103000\"/><a:tint val=\"73000\"/></a:schemeClr></a:gs><a:gs pos=\"100000\"><a:schemeClr val=\"phClr\"><a:lumMod val=\"105000\"/><a:satMod val=\"109000\"/><a:tint val=\"81000\"/></a:schemeClr></a:gs></a:gsLst><a:lin ang=\"5400000\" scaled=\"0\"/></a:gradFill>";
    let _ = write!(s, "<a:fmtScheme name=\"Office\"><a:fillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>{grad}{grad}</a:fillStyleLst><a:lnStyleLst>");
    for w in [6350, 12700, 19050] {
        let _ = write!(s, "<a:ln w=\"{w}\" cap=\"flat\" cmpd=\"sng\" algn=\"ctr\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:prstDash val=\"solid\"/><a:miter lim=\"800000\"/></a:ln>");
    }
    let _ = write!(s, "</a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:solidFill><a:schemeClr val=\"phClr\"><a:tint val=\"95000\"/><a:satMod val=\"170000\"/></a:schemeClr></a:solidFill>{grad}</a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>");
    s
}

const CLR_MAP: &str = r#"bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink""#;

/// Writes `deck` as a `.pptx`.
pub fn write<O: Write>(deck: &mut Deck, out: O) -> io::Result<O> {
    deck.load_all().map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.0))?;
    let mut z = ZipWriter::new(out);
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut w = W { deck, media: HashMap::new(), zip_media: Vec::new(), poster: None };
    let mut prels = Rels::default();
    let mut parts: Vec<(String, String)> = Vec::new();
    let mut ids = 2_147_483_648u64;

    // Masters and layouts.
    let masters = w.deck.masters.clone();
    let layouts = w.deck.layouts.clone();
    let mut layout_part: HashMap<LayoutId, String> = HashMap::new();
    let mut master_list = String::new();
    let mut lcount = 0;
    for (mi, m) in masters.iter().enumerate() {
        let mname = format!("ppt/slideMasters/slideMaster{}.xml", mi + 1);
        let mut mrels = Rels::default();
        let mut lids = String::new();
        let mine: Vec<&Layout> = layouts.iter().filter(|l| l.master == m.id).collect();
        let master_ids = ids;
        ids += 1;
        for l in &mine {
            lcount += 1;
            let lname = format!("ppt/slideLayouts/slideLayout{lcount}.xml");
            let rid = mrels.add("slideLayout", &format!("../slideLayouts/slideLayout{lcount}.xml"));
            let _ = write!(lids, "<p:sldLayoutId id=\"{ids}\" r:id=\"{rid}\"/>");
            ids += 1;
            let mut lrels = Rels::default();
            lrels.add("slideMaster", &format!("../slideMasters/slideMaster{}.xml", mi + 1));
            let valid_kind = ["title", "tx", "twoColTx", "tbl", "txAndChart", "chartAndTx", "dgm", "chart", "txAndClipArt", "clipArtAndTx", "titleOnly", "blank", "txAndObj", "objAndTx", "objOnly", "obj", "txAndMedia", "mediaAndTx", "objOverTx", "txOverObj", "txAndTwoObj", "twoObjAndTx", "twoObjOverTx", "fourObj", "vertTx", "clipArtAndVertTx", "vertTitleAndTx", "vertTitleAndTxOverChart", "twoObj", "objAndTwoObj", "twoObjAndObj", "cust", "secHead", "twoTxTwoObj", "objTx", "picTx"].contains(&&*l.kind);
            let mut s = format!("{HEAD}<p:sldLayout {NS}{}{} preserve=\"1\"><p:cSld name=\"{}\">", if valid_kind { format!(" type=\"{}\"", l.kind) } else { String::new() }, if l.hide_master_shapes { " showMasterSp=\"0\"" } else { "" }, esc(&l.name));
            w.background(l.background.as_ref(), &mut lrels, &mut s);
            w.tree(&l.shapes, &mut lrels, &mut s);
            s.push_str("</p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>");
            parts.push((lname.clone(), s));
            parts.push((format!("ppt/slideLayouts/_rels/slideLayout{lcount}.xml.rels"), lrels.xml()));
            overrides.push((format!("/{lname}"), format!("{CT}.slideLayout+xml")));
            layout_part.insert(l.id, format!("../slideLayouts/slideLayout{lcount}.xml"));
        }
        mrels.add("theme", "../theme/theme1.xml");
        let mut s = format!("{HEAD}<p:sldMaster {NS}><p:cSld name=\"{}\">", esc(&m.name));
        w.background(Some(&m.background), &mut mrels, &mut s);
        w.tree(&m.shapes, &mut mrels, &mut s);
        let _ = write!(s, "</p:cSld><p:clrMap {CLR_MAP}/><p:sldLayoutIdLst>{lids}</p:sldLayoutIdLst><p:txStyles>");
        for (tag, st) in [("p:titleStyle", &m.title_style), ("p:bodyStyle", &m.body_style), ("p:otherStyle", &m.other_style)] {
            let _ = write!(s, "<{tag}>");
            W::levels(st, &mut mrels, &mut s);
            let _ = write!(s, "</{tag}>");
        }
        s.push_str("</p:txStyles></p:sldMaster>");
        parts.push((mname.clone(), s));
        parts.push((format!("ppt/slideMasters/_rels/slideMaster{}.xml.rels", mi + 1), mrels.xml()));
        overrides.push((format!("/{mname}"), format!("{CT}.slideMaster+xml")));
        let rid = prels.add("slideMaster", &format!("slideMasters/slideMaster{}.xml", mi + 1));
        let _ = write!(master_list, "<p:sldMasterId id=\"{master_ids}\" r:id=\"{rid}\"/>");
    }

    // Slides.
    let mut slide_list = String::new();
    let mut any_notes = false;
    let n = w.deck.slide_count();
    for i in 0..n {
        let slide = w.deck.slide(i).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.0))?.clone();
        let mut srels = Rels::default();
        let lp = layout_part.get(&slide.layout).cloned().or_else(|| layout_part.values().next().cloned()).unwrap_or_default();
        srels.add("slideLayout", &lp);
        let mut s = format!("{HEAD}<p:sld {NS}{}{}><p:cSld>", if slide.hidden { " show=\"0\"" } else { "" }, if slide.hide_layout_shapes { " showMasterSp=\"0\"" } else { "" });
        w.background(slide.background.as_ref(), &mut srels, &mut s);
        w.tree(&slide.shapes, &mut srels, &mut s);
        s.push_str("</p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>");
        if let Some(t) = &slide.transition {
            s.push_str(&transition(t));
        }
        s.push_str(&timing(&slide.builds));
        s.push_str("</p:sld>");
        if let Some(notes) = &slide.notes {
            any_notes = true;
            srels.add("notesSlide", &format!("../notesSlides/notesSlide{}.xml", i + 1));
            let mut nrels = Rels::default();
            nrels.add("notesMaster", "../notesMasters/notesMaster1.xml");
            nrels.add("slide", &format!("../slides/slide{}.xml", i + 1));
            let mut ns = format!("{HEAD}<p:notes {NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Slide Image\"/><p:cNvSpPr><a:spLocks noGrp=\"1\" noRot=\"1\" noChangeAspect=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"sldImg\"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp><p:sp><p:nvSpPr><p:cNvPr id=\"3\" name=\"Notes\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"body\" idx=\"1\"/></p:nvPr></p:nvSpPr><p:spPr/>");
            W::body(notes, "p:txBody", &mut nrels, &mut ns);
            ns.push_str("</p:sp></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>");
            let nn = format!("ppt/notesSlides/notesSlide{}.xml", i + 1);
            parts.push((nn.clone(), ns));
            parts.push((format!("ppt/notesSlides/_rels/notesSlide{}.xml.rels", i + 1), nrels.xml()));
            overrides.push((format!("/{nn}"), format!("{CT}.notesSlide+xml")));
        }
        let name = format!("ppt/slides/slide{}.xml", i + 1);
        parts.push((name.clone(), s));
        parts.push((format!("ppt/slides/_rels/slide{}.xml.rels", i + 1), srels.xml()));
        overrides.push((format!("/{name}"), format!("{CT}.slide+xml")));
        let rid = prels.add("slide", &format!("slides/slide{}.xml", i + 1));
        let _ = write!(slide_list, "<p:sldId id=\"{}\" r:id=\"{rid}\"/>", 256 + i);
    }

    // The notes master, the theme, the presentation.
    let mut notes_list = String::new();
    if any_notes {
        let rid = prels.add("notesMaster", "notesMasters/notesMaster1.xml");
        let _ = write!(notes_list, "<p:notesMasterIdLst><p:notesMasterId r:id=\"{rid}\"/></p:notesMasterIdLst>");
        let s = format!("{HEAD}<p:notesMaster {NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:sp><p:nvSpPr><p:cNvPr id=\"2\" name=\"Notes\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph type=\"body\" idx=\"1\"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"685800\" y=\"4400550\"/><a:ext cx=\"5486400\" cy=\"3600450\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang=\"en-US\"/></a:p></p:txBody></p:sp></p:spTree></p:cSld><p:clrMap {CLR_MAP}/></p:notesMaster>");
        parts.push(("ppt/notesMasters/notesMaster1.xml".into(), s));
        let mut r = Rels::default();
        r.add("theme", "../theme/theme2.xml");
        parts.push(("ppt/notesMasters/_rels/notesMaster1.xml.rels".into(), r.xml()));
        parts.push(("ppt/theme/theme2.xml".into(), theme_xml(&w.deck.theme)));
        overrides.push(("/ppt/notesMasters/notesMaster1.xml".into(), format!("{CT}.notesMaster+xml")));
        overrides.push(("/ppt/theme/theme2.xml".into(), "application/vnd.openxmlformats-officedocument.theme+xml".into()));
    }
    parts.push(("ppt/theme/theme1.xml".into(), theme_xml(&w.deck.theme)));
    overrides.push(("/ppt/theme/theme1.xml".into(), "application/vnd.openxmlformats-officedocument.theme+xml".into()));
    prels.add("theme", "theme/theme1.xml");
    for (kind, file, body) in [
        ("presProps", "presProps.xml", format!("{HEAD}<p:presentationPr {NS}/>")),
        ("viewProps", "viewProps.xml", format!("{HEAD}<p:viewPr {NS}><p:normalViewPr><p:restoredLeft sz=\"15620\"/><p:restoredTop sz=\"94660\"/></p:normalViewPr><p:gridSpacing cx=\"76200\" cy=\"76200\"/></p:viewPr>")),
        ("tableStyles", "tableStyles.xml", format!("{HEAD}<a:tblStyleLst xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" def=\"{{5C22544A-7EE6-4342-B048-85BDC9FD1C3A}}\"/>")),
    ] {
        prels.add(kind, file);
        parts.push((format!("ppt/{file}"), body));
        overrides.push((format!("/ppt/{file}"), format!("{CT}.{}+xml", if kind == "tableStyles" { "tableStyles" } else { kind })));
    }
    let (cx, cy) = w.deck.size;
    // Text in shapes that aren't placeholders: the first master's other style.
    let mut dts = String::new();
    if let Some(m) = w.deck.masters.first() {
        let other = m.other_style.clone();
        W::levels(&other, &mut prels, &mut dts);
    }
    let pres = format!("{HEAD}<p:presentation {NS} saveSubsetFonts=\"1\"><p:sldMasterIdLst>{master_list}</p:sldMasterIdLst>{notes_list}<p:sldIdLst>{slide_list}</p:sldIdLst><p:sldSz cx=\"{cx}\" cy=\"{cy}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/><p:defaultTextStyle>{dts}</p:defaultTextStyle></p:presentation>");
    parts.push(("ppt/presentation.xml".into(), pres));
    parts.push(("ppt/_rels/presentation.xml.rels".into(), prels.xml()));
    overrides.push(("/ppt/presentation.xml".into(), format!("{CT}.presentation.main+xml")));
    parts.push(("docProps/app.xml".into(), format!("{HEAD}<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>Atrium Slides</Application><Slides>{n}</Slides></Properties>")));
    parts.push(("docProps/core.xml".into(), format!("{HEAD}<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"/>")));
    overrides.push(("/docProps/app.xml".into(), "application/vnd.openxmlformats-officedocument.extended-properties+xml".into()));
    overrides.push(("/docProps/core.xml".into(), "application/vnd.openxmlformats-package.core-properties+xml".into()));

    let mut ct = format!("{HEAD}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>");
    let mut exts: Vec<(&str, String)> = Vec::new();
    for (name, _) in &w.zip_media {
        let e = name.rsplit('.').next().unwrap_or("bin");
        let mime = match e {
            "png" => "image/png",
            "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            "emf" => "image/x-emf",
            "wmf" => "image/x-wmf",
            "tiff" => "image/tiff",
            "bmp" => "image/bmp",
            "mp4" => "video/mp4",
            "mov" => "video/quicktime",
            "webm" => "video/webm",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            _ => "application/octet-stream",
        };
        if !exts.iter().any(|x| x.1 == e) {
            exts.push((mime, e.to_string()));
        }
    }
    for (mime, e) in exts {
        let _ = write!(ct, "<Default Extension=\"{e}\" ContentType=\"{mime}\"/>");
    }
    for (p, t) in &overrides {
        let _ = write!(ct, "<Override PartName=\"{p}\" ContentType=\"{t}\"/>");
    }
    ct.push_str("</Types>");
    z.add("[Content_Types].xml", ct.as_bytes())?;
    let mut root = Rels::default();
    root.add("officeDocument", "ppt/presentation.xml");
    let mut root_xml = root.xml();
    root_xml = root_xml.replace("</Relationships>", "<Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/><Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties\" Target=\"docProps/app.xml\"/></Relationships>");
    z.add("_rels/.rels", root_xml.as_bytes())?;
    for (name, body) in &parts {
        z.add(name, body.as_bytes())?;
    }
    for (name, bytes) in &w.zip_media {
        // Pictures are compressed already.
        z.add_stored(name, bytes)?;
    }
    z.finish()
}
