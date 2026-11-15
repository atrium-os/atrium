//! `.pptx` (PresentationML) in: a deck with its theme, masters, layouts,
//! slides, pictures, notes and transitions (docs/spec/officina.md §9.2, O6c).
//!
//! Shapes keep their OOXML preset names and adjust values, colours keep their
//! theme slots and changes, placeholders keep their kind and index, so a deck
//! read here and written back says what it said. What has no place in the
//! model is reported: charts and SmartArt, effects, 3-D, vertical text.

use std::collections::HashMap;
use std::sync::Arc;

use officina_slides::model::*;
use officina_slides::ops::{EMU_MAX_SIDE, EMU_MIN_SIDE};
use officina_text::model::{Align, AttrId, CharAttrs, LineSpacing, ListKind, ListRef, ParaProps, TextDoc, VertAlign};

use crate::report::{Kind, Report};
use crate::xml::{self, El};
use crate::zip::Archive;

#[derive(Debug)]
pub enum PptxError {
    Zip(crate::zip::ZipError),
    Xml(String),
    Bad(String),
}

impl std::fmt::Display for PptxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PptxError::Zip(e) => write!(f, "{e}"),
            PptxError::Xml(e) => write!(f, "damaged presentation XML: {e}"),
            PptxError::Bad(e) => write!(f, "not a presentation this can read: {e}"),
        }
    }
}

impl std::error::Error for PptxError {}

impl From<crate::zip::ZipError> for PptxError {
    fn from(e: crate::zip::ZipError) -> Self {
        PptxError::Zip(e)
    }
}

/// What reading found besides the deck.
#[derive(Debug, Default)]
pub struct Loaded {
    pub report: Report,
    pub slides: u64,
    pub shapes: u64,
    pub pictures: u64,
}

/// Most bytes one picture or video may have.
pub const MAX_MEDIA_BYTES: usize = 512 << 20;

type Rels = HashMap<String, (String, String, bool)>;

fn part(a: &Archive, name: &str) -> Result<Option<El>, PptxError> {
    match a.read_name(name)? {
        Some(b) => xml::parse(&b).map(Some).map_err(|e| PptxError::Xml(format!("{name}: {e}"))),
        None => Ok(None),
    }
}

/// A part's relationships: id → (type's last segment, target, external).
pub(crate) fn rels(a: &Archive, part: &str) -> Result<Rels, PptxError> {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let name = if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") };
    let mut out = HashMap::new();
    if let Some(r) = self::part(a, &name)? {
        for e in r.children("Relationship") {
            let (Some(id), Some(t), Some(target)) = (e.attr("Id"), e.attr("Type"), e.attr("Target")) else { continue };
            let kind = t.rsplit('/').next().unwrap_or("").to_string();
            let external = e.attr("TargetMode") == Some("External");
            let target = if external || target.starts_with('/') { target.trim_start_matches('/').to_string() } else { resolve_path(dir, target) };
            out.insert(id.to_string(), (kind, target, external));
        }
    }
    Ok(out)
}

fn resolve_path(dir: &str, target: &str) -> String {
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

/// A child by local name, in PresentationML (`p:`) or a diagram drawing
/// (`dsp:`).
fn kid<'e>(e: &'e El, local: &str) -> Option<&'e El> {
    e.els().find(|k| k.name.strip_prefix("p:").or_else(|| k.name.strip_prefix("dsp:")) == Some(local))
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn num(e: &El, name: &str) -> Option<i64> {
    e.attr(name).and_then(|v| v.trim().parse::<i64>().ok())
}

fn on(e: &El, name: &str) -> bool {
    matches!(e.attr(name), Some("1") | Some("true") | Some("on"))
}

/// The master's colour map: `bg1`, `tx1`, ... to theme slots.
#[derive(Clone)]
struct ClrMap(HashMap<String, Slot>);

impl ClrMap {
    fn standard() -> ClrMap {
        let mut m = HashMap::new();
        for (k, v) in [("bg1", Slot::Lt1), ("tx1", Slot::Dk1), ("bg2", Slot::Lt2), ("tx2", Slot::Dk2)] {
            m.insert(k.to_string(), v);
        }
        ClrMap(m)
    }
    fn read(e: &El) -> ClrMap {
        let mut m = ClrMap::standard();
        for (k, v) in &e.attrs {
            if let Some(s) = Slot::from_name(v) {
                m.0.insert(k.clone(), s);
            }
        }
        m
    }
    fn slot(&self, name: &str) -> Option<Slot> {
        self.0.get(name).copied().or_else(|| Slot::from_name(name))
    }
}

/// What a shape tree is read for.
struct Ctx<'a> {
    rels: &'a Rels,
    map: &'a ClrMap,
    /// Colours named by the shape's style references (`phClr`).
    ph_color: Option<Color>,
    /// File shape ids to ours, for builds.
    ids: HashMap<u32, ShapeId>,
}

struct Reader<'a> {
    archive: &'a Archive<'a>,
    deck: Deck,
    rep: Report,
    loaded: Loaded,
    /// Media read so far, by part name.
    media: HashMap<String, Option<MediaId>>,
    /// Theme line widths (`lnStyleLst`), for style references.
    line_widths: Vec<Emu>,
    shapes: usize,
    /// Reading slides (not masters or layouts): some notes are only worth
    /// making for what a slide shows.
    on_slide: bool,
}

pub(crate) fn mime_of(name: &str, b: &[u8]) -> (&'static str, bool) {
    let n = name.to_ascii_lowercase();
    let ext = n.rsplit('.').next().unwrap_or("");
    match ext {
        "png" => ("image/png", true),
        "jpg" | "jpeg" | "jpe" => ("image/jpeg", true),
        "gif" => ("image/gif", true),
        "svg" => ("image/svg+xml", true),
        "webp" => ("image/webp", true),
        "emf" => ("image/emf", true),
        "wmf" => ("image/wmf", true),
        "tif" | "tiff" => ("image/tiff", false),
        "bmp" => ("image/bmp", true),
        "mp4" | "m4v" => ("video/mp4", true),
        "mov" => ("video/quicktime", true),
        "webm" => ("video/webm", true),
        "mp3" => ("audio/mpeg", true),
        "wav" => ("audio/wav", true),
        _ => {
            if let Some(k) = officina_pict::sniff(b) {
                (k.mime(), true)
            } else if b.starts_with(b"\x89PNG") {
                ("image/png", true)
            } else if b.starts_with(&[0xFF, 0xD8]) {
                ("image/jpeg", true)
            } else {
                ("application/octet-stream", false)
            }
        }
    }
}

impl<'a> Reader<'a> {
    /// The media a relationship names, read once.
    fn media(&mut self, rels: &Rels, rid: &str) -> Option<MediaId> {
        let (_, target, external) = rels.get(rid)?;
        if *external {
            self.rep.note(Kind::Dropped, "linked (not embedded) pictures and videos");
            return None;
        }
        if let Some(m) = self.media.get(target) {
            return *m;
        }
        let got = match self.archive.find(target) {
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
            let (mime, shown) = mime_of(target, &b);
            if !shown {
                self.rep.note_at(Kind::Approximate, "pictures in formats not drawn (TIFF) kept and shown as boxes", || target.clone());
            }
            self.loaded.pictures += 1;
            self.deck.add_media(mime, &b)
        });
        self.media.insert(target.clone(), id);
        id
    }

    fn color(&mut self, ctx: &Ctx, holder: &El) -> Option<Color> {
        for c in holder.els() {
            let base = match c.name.as_str() {
                "a:srgbClr" => ColorBase::Rgb(u32::from_str_radix(c.attr("val")?, 16).ok()? & 0xFF_FFFF),
                "a:sysClr" => ColorBase::Rgb(c.attr("lastClr").and_then(|v| u32::from_str_radix(v, 16).ok()).unwrap_or(if c.attr("val") == Some("window") { 0xFFFFFF } else { 0 })),
                "a:schemeClr" => {
                    let v = c.attr("val")?;
                    if v == "phClr" {
                        let mut base = ctx.ph_color.clone().unwrap_or(Color::theme(Slot::Dk1));
                        base.mods.extend(mods(c));
                        return Some(base);
                    }
                    ColorBase::Theme(ctx.map.slot(v)?)
                }
                "a:prstClr" => ColorBase::Rgb(preset_color(c.attr("val").unwrap_or(""))),
                "a:scrgbClr" => {
                    let ch = |n: &str| {
                        let v = num(c, n).unwrap_or(0).clamp(0, 100_000) as f64 / 100_000.0;
                        let s = if v <= 0.003_130_8 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
                        (s * 255.0).round() as u32
                    };
                    ColorBase::Rgb((ch("r") << 16) | (ch("g") << 8) | ch("b"))
                }
                "a:hslClr" => {
                    self.rep.note(Kind::Approximate, "HSL colours");
                    ColorBase::Rgb(0x808080)
                }
                _ => continue,
            };
            return Some(Color { base, mods: mods(c) });
        }
        None
    }

    fn fill(&mut self, ctx: &Ctx, sp_pr: &El) -> Option<Fill> {
        for f in sp_pr.els() {
            return Some(match f.name.as_str() {
                "a:noFill" => Fill::None,
                "a:solidFill" => Fill::Solid(self.color(ctx, f)?),
                "a:gradFill" => {
                    let mut stops = Vec::new();
                    if let Some(l) = f.child("a:gsLst") {
                        for g in l.children("a:gs") {
                            if let Some(c) = self.color(ctx, g) {
                                stops.push(GradientStop { pos: num(g, "pos").unwrap_or(0).clamp(0, 100_000) as u32, color: c });
                            }
                        }
                    }
                    stops.sort_by_key(|s| s.pos);
                    if stops.is_empty() {
                        return None;
                    }
                    let radial = f.child("a:path").is_some();
                    if f.child("a:path").is_some_and(|p| p.attr("path") != Some("circle")) {
                        self.rep.note(Kind::Approximate, "rectangular and shape gradients drawn as radial");
                    }
                    let mut angle = f.child("a:lin").and_then(|l| num(l, "ang")).unwrap_or(0).rem_euclid(21_600_000);
                    // A gradient that stays put on the page while the shape
                    // turns: ours turns with the shape.
                    if matches!(f.attr("rotWithShape"), Some("0") | Some("false")) {
                        let rot = sp_pr.child("a:xfrm").and_then(|x| num(x, "rot")).unwrap_or(0);
                        angle = (angle - rot).rem_euclid(21_600_000);
                    }
                    Fill::Gradient { stops, angle: angle as i32, radial }
                }
                "a:blipFill" => {
                    let rid = f.child("a:blip").and_then(|b| b.attr("r:embed")).map(str::to_string);
                    match rid.and_then(|r| self.media(ctx.rels, &r)) {
                        Some(m) => Fill::Picture(m),
                        None => Fill::None,
                    }
                }
                "a:pattFill" => {
                    self.rep.note(Kind::Approximate, "pattern fills drawn in their foreground colour");
                    match f.child("a:fgClr").and_then(|c| self.color(ctx, c)) {
                        Some(c) => Fill::Solid(c),
                        None => Fill::None,
                    }
                }
                "a:grpFill" => {
                    self.rep.note(Kind::Approximate, "group fills");
                    Fill::None
                }
                _ => continue,
            });
        }
        None
    }

    fn line(&mut self, ctx: &Ctx, ln: &El) -> Stroke {
        let mut s = Stroke::none();
        s.width = num(ln, "w").unwrap_or(9_525).clamp(0, 20 * 914_400);
        let mut has_fill = false;
        for c in ln.els() {
            match c.name.as_str() {
                "a:noFill" => {
                    has_fill = true;
                    s.color = None;
                }
                "a:solidFill" => {
                    has_fill = true;
                    s.color = self.color(ctx, c);
                }
                "a:gradFill" => {
                    has_fill = true;
                    self.rep.note(Kind::Approximate, "gradient lines drawn in their first colour");
                    s.color = c.child("a:gsLst").and_then(|l| l.child("a:gs")).and_then(|g| self.color(ctx, g));
                }
                "a:prstDash" => {
                    s.dash = match c.attr("val").unwrap_or("solid") {
                        "solid" => Dash::Solid,
                        "dot" | "sysDot" => Dash::Dot,
                        "dash" | "sysDash" => Dash::Dash,
                        "lgDash" => Dash::LongDash,
                        _ => Dash::DashDot,
                    }
                }
                "a:custDash" => {
                    self.rep.note(Kind::Approximate, "custom dashes");
                    s.dash = Dash::Dash;
                }
                "a:headEnd" | "a:tailEnd" => {
                    let a = match c.attr("type").unwrap_or("none") {
                        "triangle" => Arrow::Triangle,
                        "arrow" => Arrow::Open,
                        "stealth" => Arrow::Stealth,
                        "diamond" => Arrow::Diamond,
                        "oval" => Arrow::Oval,
                        _ => Arrow::None,
                    };
                    if c.name == "a:headEnd" {
                        s.head = a;
                    } else {
                        s.tail = a;
                    }
                }
                _ => {}
            }
        }
        if !has_fill {
            // A line with no fill of its own takes the style's colour.
            s.color = ctx.ph_color.clone();
        }
        s
    }

    fn geom(&mut self, sp_pr: &El) -> Option<Geom> {
        if let Some(p) = sp_pr.child("a:prstGeom") {
            let name = p.attr("prst").unwrap_or("rect");
            let mut adj = Vec::new();
            if let Some(av) = p.child("a:avLst") {
                for g in av.children("a:gd") {
                    let (Some(n), Some(f)) = (g.attr("name"), g.attr("fmla")) else { continue };
                    if let Some(v) = f.strip_prefix("val ").and_then(|v| v.trim().parse::<i64>().ok()) {
                        adj.push((Arc::<str>::from(n), v));
                    }
                }
            }
            return Some(Geom::Preset { name: name.into(), adj });
        }
        let c = sp_pr.child("a:custGeom")?;
        // Guide formulas: worked out for the shape's own size.
        let guided = c.child("a:gdLst").is_some_and(|g| g.children("a:gd").next().is_some());
        if guided || c.child("a:avLst").is_some_and(|g| g.children("a:gd").next().is_some()) {
            let ext = sp_pr.child("a:xfrm").and_then(|x| x.child("a:ext"));
            let (w, h) = ext.map_or((0, 0), |e| (num(e, "cx").unwrap_or(0).clamp(0, EMU_MAX_SIDE), num(e, "cy").unwrap_or(0).clamp(0, EMU_MAX_SIDE)));
            return Some(Geom::Custom(guided_geom(c).paths(w, h, &[])));
        }
        let mut out = Vec::new();
        for p in c.child("a:pathLst")?.children("a:path") {
            let (w, h) = (num(p, "w").unwrap_or(0).max(0), num(p, "h").unwrap_or(0).max(0));
            let mut path = Path { w, h, cmds: Vec::new(), filled: p.attr("fill") != Some("none"), stroked: p.attr("stroke") != Some("0") && p.attr("stroke") != Some("false") };
            let mut odd = false;
            let mut v = |e: &El, a: &str, size: i64| -> i64 {
                match e.attr(a) {
                    Some(t) => t.parse::<i64>().unwrap_or_else(|_| {
                        odd = true;
                        match t {
                            "r" | "w" | "b" | "h" => size,
                            "hc" | "vc" => size / 2,
                            _ => 0,
                        }
                    }),
                    None => 0,
                }
            };
            let mut cur = (0i64, 0i64);
            for cmd in p.els() {
                let pts: Vec<(i64, i64)> = cmd.children("a:pt").map(|pt| (v(pt, "x", w), v(pt, "y", h))).collect();
                match cmd.name.as_str() {
                    "a:moveTo" if !pts.is_empty() => {
                        path.cmds.push(PathCmd::Move(pts[0].0, pts[0].1));
                        cur = pts[0];
                    }
                    "a:lnTo" if !pts.is_empty() => {
                        path.cmds.push(PathCmd::Line(pts[0].0, pts[0].1));
                        cur = pts[0];
                    }
                    "a:cubicBezTo" if pts.len() == 3 => {
                        path.cmds.push(PathCmd::Cubic(pts[0].0, pts[0].1, pts[1].0, pts[1].1, pts[2].0, pts[2].1));
                        cur = pts[2];
                    }
                    "a:quadBezTo" if pts.len() == 2 => {
                        let c1 = (cur.0 + 2 * (pts[0].0 - cur.0) / 3, cur.1 + 2 * (pts[0].1 - cur.1) / 3);
                        let c2 = (pts[1].0 + 2 * (pts[0].0 - pts[1].0) / 3, pts[1].1 + 2 * (pts[0].1 - pts[1].1) / 3);
                        path.cmds.push(PathCmd::Cubic(c1.0, c1.1, c2.0, c2.1, pts[1].0, pts[1].1));
                        cur = pts[1];
                    }
                    "a:arcTo" => {
                        let (wr, hr) = (v(cmd, "wR", w) as f64, v(cmd, "hR", h) as f64);
                        let (st, sw) = (v(cmd, "stAng", 0) as f64, v(cmd, "swAng", 0) as f64);
                        cur = arc(&mut path.cmds, cur, wr, hr, st, sw);
                    }
                    "a:close" => path.cmds.push(PathCmd::Close),
                    _ => {}
                }
            }
            if odd {
                self.rep.note(Kind::Approximate, "custom shapes with guide formulas");
            }
            out.push(path);
        }
        Some(Geom::Custom(out))
    }

    /// A shape's text frame.
    fn body(&mut self, ctx: &Ctx, tx: &El) -> TextBody {
        let mut t = TextBody::plain("");
        if let Some(b) = tx.child("a:bodyPr") {
            let ins = [num(b, "lIns"), num(b, "tIns"), num(b, "rIns"), num(b, "bIns")];
            if ins.iter().any(Option::is_some) {
                let d = officina_slides::resolve::INSET;
                t.inset = Some([0, 1, 2, 3].map(|i| ins[i].unwrap_or(d[i]).clamp(0, 10 * 914_400)));
            }
            t.anchor = match b.attr("anchor") {
                Some("t") => Some(Anchor::Top),
                Some("ctr") => Some(Anchor::Middle),
                Some("b") => Some(Anchor::Bottom),
                Some(_) => {
                    self.rep.note(Kind::Approximate, "justified and distributed text anchors");
                    Some(Anchor::Top)
                }
                None => None,
            };
            t.wrap = match b.attr("wrap") {
                Some("none") => Some(false),
                Some("square") => Some(true),
                _ => None,
            };
            if b.child("a:normAutofit").is_some() {
                let n = b.child("a:normAutofit").unwrap();
                t.autofit = Some(Autofit::Shrink { scale: num(n, "fontScale").unwrap_or(100_000).clamp(1_000, 100_000) as u32, spacing_cut: num(n, "lnSpcReduction").unwrap_or(0).clamp(0, 100_000) as u32 });
            } else if b.child("a:spAutoFit").is_some() {
                t.autofit = Some(Autofit::Resize);
            } else if b.child("a:noAutofit").is_some() {
                t.autofit = Some(Autofit::None);
            }
            if self.on_slide && b.attr("vert").is_some_and(|v| v != "horz") {
                self.rep.note(Kind::Approximate, "vertical and stacked text drawn across");
            }
            if num(b, "numCol").is_some_and(|n| n > 1) {
                self.rep.note(Kind::Approximate, "text in columns drawn in one");
            }
            if b.attr("rot").is_some_and(|r| r != "0") {
                self.rep.note(Kind::Approximate, "turned text frames");
            }
        }
        if let Some(l) = tx.child("a:lstStyle") {
            t.levels = self.list_style(ctx, l);
        }
        let mut doc = TextDoc::new();
        doc.blocks.remove(0);
        for p in tx.children("a:p") {
            self.para(ctx, p, &mut doc, &mut t.bullets);
        }
        if doc.blocks.len() == 0 {
            doc.push_para("", 0, vec![]);
        }
        t.doc = doc;
        t
    }

    fn list_style(&mut self, ctx: &Ctx, l: &El) -> ListStyle {
        let mut out: ListStyle = Vec::new();
        for i in 0..MAX_LEVELS {
            let Some(e) = l.child(&format!("a:lvl{}pPr", i + 1)) else { continue };
            while out.len() < i {
                out.push(LevelStyle::default());
            }
            out.push(self.level(ctx, e));
        }
        out
    }

    /// A level's style from an `a:lvlNpPr` (or a paragraph's `a:pPr`).
    fn level(&mut self, ctx: &Ctx, e: &El) -> LevelStyle {
        let mut l = LevelStyle { para: self.para_props(e), ..Default::default() };
        if let Some(r) = e.child("a:defRPr") {
            let (a, c) = self.chars(ctx, r);
            l.chars = a;
            l.color = c;
        }
        l.bullet = self.bullet(ctx, e);
        l
    }

    fn para_props(&mut self, e: &El) -> ParaProps {
        let tw = |v: i64| (v / 635).clamp(-31_680 * 4, 31_680 * 4) as i32;
        let mut p = ParaProps {
            align: match e.attr("algn") {
                Some("l") => Some(Align::Start),
                Some("ctr") => Some(Align::Center),
                Some("r") => Some(Align::End),
                Some("just") | Some("dist") | Some("justLow") | Some("thaiDist") => Some(Align::Justify),
                _ => None,
            },
            indent_start: num(e, "marL").map(tw),
            first_line: num(e, "indent").map(tw),
            indent_end: num(e, "marR").map(tw),
            ..Default::default()
        };
        let spacing = |s: &El| -> Option<(bool, i64)> {
            if let Some(v) = s.child("a:spcPct").and_then(|x| num(x, "val")) {
                return Some((true, v));
            }
            s.child("a:spcPts").and_then(|x| num(x, "val")).map(|v| (false, v))
        };
        if let Some((pct, v)) = e.child("a:lnSpc").and_then(spacing) {
            p.line_spacing = Some(if pct { LineSpacing::Multiple((v / 1000).clamp(10, 1000) as u16) } else { LineSpacing::Exact((v / 5).clamp(20, 31_680) as i32) });
        }
        for (name, before) in [("a:spcBef", true), ("a:spcAft", false)] {
            if let Some((pct, v)) = e.child(name).and_then(spacing) {
                // Points are exact; a percentage is of the line (taken at 18 pt).
                let twips = if pct {
                    self.rep.note(Kind::Approximate, "paragraph spacing as a percentage of the line");
                    v * 360 / 100_000
                } else {
                    v / 5
                };
                let twips = twips.clamp(0, 31_680) as i32;
                if before {
                    p.space_before = Some(twips);
                } else {
                    p.space_after = Some(twips);
                }
            }
        }
        p
    }

    fn bullet(&mut self, ctx: &Ctx, e: &El) -> Option<Bullet> {
        if e.child("a:buNone").is_some() {
            return Some(Bullet::None);
        }
        if let Some(c) = e.child("a:buChar") {
            let ch = c.attr("char").and_then(|s| s.chars().next()).unwrap_or('•');
            let font = e.child("a:buFont").and_then(|f| f.attr("typeface")).map(Arc::<str>::from);
            let color = e.child("a:buClr").and_then(|c| self.color(ctx, c));
            let size_pct = e.child("a:buSzPct").and_then(|s| num(s, "val")).map(|v| v.clamp(25_000, 400_000) as u32);
            return Some(Bullet::Char { ch, font, color, size_pct });
        }
        if let Some(n) = e.child("a:buAutoNum") {
            return Some(Bullet::Number { scheme: n.attr("type").unwrap_or("arabicPeriod").into(), start: num(n, "startAt").unwrap_or(1).clamp(1, 32_767) as u32 });
        }
        if e.child("a:buBlip").is_some() {
            self.rep.note(Kind::Approximate, "picture bullets drawn as dots");
            return Some(Bullet::Char { ch: '•', font: None, color: None, size_pct: None });
        }
        None
    }

    /// Character attributes from an `a:rPr`, `a:defRPr` or `a:endParaRPr`;
    /// the colour apart (a level keeps it as a theme colour).
    fn chars(&mut self, ctx: &Ctx, r: &El) -> (CharAttrs, Option<Color>) {
        let mut a = CharAttrs::default();
        if let Some(sz) = num(r, "sz") {
            a.size = Some((sz / 50).clamp(2, 8000) as u16);
        }
        if r.attr("b").is_some() {
            a.bold = Some(on(r, "b"));
        }
        if r.attr("i").is_some() {
            a.italic = Some(on(r, "i"));
        }
        if let Some(u) = r.attr("u") {
            a.underline = Some(u != "none");
        }
        if let Some(s) = r.attr("strike") {
            a.strike = Some(s != "noStrike");
        }
        if let Some(b) = num(r, "baseline") {
            a.vert = match b {
                b if b > 0 => Some(VertAlign::Super),
                b if b < 0 => Some(VertAlign::Sub),
                _ => None,
            };
        }
        if r.attr("cap").is_some_and(|c| c != "none") {
            self.rep.note(Kind::Approximate, "all caps and small caps");
        }
        if let Some(l) = r.child("a:latin").and_then(|l| l.attr("typeface")) {
            if !l.is_empty() {
                a.family = Some(l.into());
            }
        }
        let mut color = None;
        if let Some(f) = r.child("a:solidFill") {
            color = self.color(ctx, f);
        } else if let Some(f) = r.child("a:gradFill") {
            self.rep.note(Kind::Approximate, "gradient text drawn in its first colour");
            color = f.child("a:gsLst").and_then(|l| l.child("a:gs")).and_then(|g| self.color(ctx, g));
        }
        if let Some(h) = r.child("a:highlight").and_then(|h| self.color(ctx, h)) {
            a.highlight = Some(officina_slides::resolve::color(&self.deck.theme, &h) >> 8);
        }
        if let Some(h) = r.child("a:hlinkClick").and_then(|h| h.attr("r:id")) {
            if let Some((_, target, true)) = ctx.rels.get(h) {
                let t = target.to_ascii_lowercase();
                if t.starts_with("http://") || t.starts_with("https://") || t.starts_with("mailto:") {
                    a.link = Some(target.as_str().into());
                }
            }
        }
        if r.child("a:effectLst").is_some_and(|e| e.els().next().is_some()) {
            self.rep.note(Kind::Approximate, "text shadows and glow left off");
        }
        (a, color)
    }

    fn para(&mut self, ctx: &Ctx, p: &El, doc: &mut TextDoc, bullets: &mut Vec<Bullet>) {
        let ppr = p.child("a:pPr");
        let lvl = ppr.and_then(|e| num(e, "lvl")).unwrap_or(0).clamp(0, MAX_LEVELS as i64 - 1) as u16;
        let mut text = String::new();
        let mut runs: Vec<(u32, AttrId)> = Vec::new();
        let mut push = |doc: &mut TextDoc, text: &mut String, s: &str, a: CharAttrs| {
            if s.is_empty() {
                return;
            }
            let id = doc.attrs.intern(a);
            if runs.last().map(|r| r.1) != Some(id) {
                runs.push((text.len() as u32, id));
            }
            text.push_str(s);
        };
        for k in p.els() {
            match k.name.as_str() {
                "a:r" => {
                    let (mut a, c) = k.child("a:rPr").map(|r| self.chars(ctx, r)).unwrap_or_default();
                    if let Some(c) = c {
                        a.color = Some(officina_slides::resolve::color(&self.deck.theme, &c) >> 8);
                    }
                    let t = k.child("a:t").map(|t| t.text()).unwrap_or_default();
                    let t: String = t.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
                    push(doc, &mut text, &t, a);
                }
                "a:br" => {
                    let (a, _) = k.child("a:rPr").map(|r| self.chars(ctx, r)).unwrap_or_default();
                    push(doc, &mut text, "\u{2028}", a);
                }
                "a:fld" => {
                    let (mut a, c) = k.child("a:rPr").map(|r| self.chars(ctx, r)).unwrap_or_default();
                    if let Some(c) = c {
                        a.color = Some(officina_slides::resolve::color(&self.deck.theme, &c) >> 8);
                    }
                    let t = if k.attr("type") == Some("slidenum") { "‹#›".to_string() } else { k.child("a:t").map(|t| t.text()).unwrap_or_default() };
                    if self.on_slide && k.attr("type").is_some_and(|t| t.starts_with("datetime")) {
                        self.rep.note(Kind::Approximate, "date fields kept as the date they showed");
                    }
                    push(doc, &mut text, &t, a);
                }
                "m:oMathPara" | "a14:m" | "mc:AlternateContent" => {
                    self.rep.note(Kind::Dropped, "equations");
                }
                _ => {}
            }
        }
        if runs.is_empty() || runs[0].0 != 0 {
            // An empty paragraph keeps its end mark's size.
            let (a, _) = p.child("a:endParaRPr").map(|r| self.chars(ctx, r)).unwrap_or_default();
            let id = doc.attrs.intern(a);
            runs.insert(0, (0, id));
        }
        let k = doc.push_para(&text, lvl, runs);
        if let Some(e) = ppr {
            let mut props = self.para_props(e);
            match self.bullet(ctx, e) {
                Some(Bullet::None) => props.list = Some(None),
                Some(b) => {
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
                None => {}
            }
            if props != ParaProps::default() {
                doc.blocks.update(k, |b| {
                    if let Some(p) = b.as_para_mut() {
                        p.props = props;
                    }
                });
            }
        }
    }

    fn xfrm(e: &El) -> Option<(Rect, i32, bool, bool, Option<Rect>)> {
        let off = e.child("a:off")?;
        let ext = e.child("a:ext")?;
        let c = |v: Option<i64>| v.unwrap_or(0).clamp(-(1 << 40), 1 << 40);
        let r = Rect::new(c(num(off, "x")), c(num(off, "y")), c(num(ext, "cx")).max(0), c(num(ext, "cy")).max(0));
        let child = match (e.child("a:chOff"), e.child("a:chExt")) {
            (Some(o), Some(x)) => Some(Rect::new(c(num(o, "x")), c(num(o, "y")), c(num(x, "cx")).max(0), c(num(x, "cy")).max(0))),
            _ => None,
        };
        Some((r, num(e, "rot").unwrap_or(0).rem_euclid(21_600_000) as i32, on(e, "flipH"), on(e, "flipV"), child))
    }

    /// The common non-visual properties: name, description, hidden, the
    /// file's id, the placeholder.
    fn nv(&mut self, nv: Option<&El>) -> (Arc<str>, Arc<str>, bool, Option<u32>, Option<Placeholder>) {
        let Some(nv) = nv else { return ("".into(), "".into(), false, None, None) };
        let c = nv.els().find(|e| e.name.ends_with(":cNvPr"));
        let name = c.and_then(|c| c.attr("name")).unwrap_or("").into();
        let descr = c.and_then(|c| c.attr("descr")).unwrap_or("").into();
        let hidden = c.is_some_and(|c| on(c, "hidden"));
        let fid = c.and_then(|c| num(c, "id")).map(|v| v as u32);
        let ph = nv.child("p:nvPr").and_then(|n| n.child("p:ph")).map(|p| Placeholder { kind: PhKind::from_name(p.attr("type").unwrap_or("obj")).unwrap_or(PhKind::Obj), idx: num(p, "idx").unwrap_or(0).clamp(0, u32::MAX as i64) as u32 });
        (name, descr, hidden, fid, ph)
    }

    /// What a `p:style` gives: its fill, line and text colour, and the
    /// colours its fill and line references name (`phClr` in what the
    /// shape sets for itself).
    #[allow(clippy::type_complexity)]
    fn style_refs(&mut self, ctx: &Ctx, st: Option<&El>) -> (Option<Fill>, Option<Stroke>, Option<Color>, Option<Color>, Option<Color>) {
        let Some(st) = st else { return (None, None, None, None, None) };
        let mut fill = None;
        let mut line = None;
        let fc = st.child("a:fillRef").and_then(|f| self.color(ctx, f));
        if let Some(f) = st.child("a:fillRef") {
            let i = num(f, "idx").unwrap_or(0);
            if i > 1 {
                self.rep.note(Kind::Approximate, "theme fill styles drawn as solid colours");
            }
            if i > 0 {
                fill = fc.clone().map(Fill::Solid);
            }
        }
        let lc = st.child("a:lnRef").and_then(|l| self.color(ctx, l));
        if let Some(l) = st.child("a:lnRef") {
            let i = num(l, "idx").unwrap_or(0);
            if i > 0 {
                let w = self.line_widths.get(i as usize - 1).copied().unwrap_or(12_700 * i);
                line = lc.clone().map(|c| Stroke::solid(c, w));
            }
        }
        let font = st.child("a:fontRef").and_then(|f| self.color(ctx, f));
        if st.child("a:effectRef").is_some_and(|e| num(e, "idx").unwrap_or(0) > 0) {
            self.rep.note(Kind::Approximate, "shadows and glow left off");
        }
        (fill, line, font, fc, lc)
    }

    /// The shapes of a shape tree (`p:spTree` or a group's).
    fn tree(&mut self, ctx: &mut Ctx, tree: &El, depth: usize) -> Vec<Shape> {
        let mut out = Vec::new();
        for e in tree.els() {
            let e = match e.name.as_str() {
                "mc:AlternateContent" => match e.child("mc:Fallback").or_else(|| e.child("mc:Choice")) {
                    Some(alt) => {
                        out.extend(self.tree(ctx, alt, depth));
                        continue;
                    }
                    None => continue,
                },
                _ => e,
            };
            if let Some(s) = self.shape(ctx, e, depth) {
                out.push(s);
            }
        }
        out
    }

    fn shape(&mut self, ctx: &mut Ctx, e: &El, depth: usize) -> Option<Shape> {
        if depth > MAX_DEPTH - 1 {
            self.rep.note(Kind::Dropped, "groups nested too deep");
            return None;
        }
        self.shapes += 1;
        if self.shapes > MAX_SHAPES * 16 {
            self.rep.note(Kind::Dropped, "shapes past the deck's limit");
            return None;
        }
        let id = self.deck.new_id();
        let nv = e.els().find(|k| local(&k.name).starts_with("nv"));
        let (name, descr, hidden, fid, ph) = self.nv(nv);
        if let Some(f) = fid {
            ctx.ids.insert(f, id);
        }
        let sp_pr = kid(e, "spPr").or_else(|| kid(e, "grpSpPr"));
        let x = sp_pr.and_then(|s| s.child("a:xfrm")).or_else(|| kid(e, "xfrm")).and_then(Self::xfrm);
        let mut shape = Shape::new(id, Rect::default(), ShapeKind::Group { child: Rect::default(), shapes: Vec::new() });
        shape.frame = x.map(|x| x.0);
        if let Some((_, rot, fh, fv, _)) = x {
            shape.rot = rot;
            shape.flip_h = fh;
            shape.flip_v = fv;
        }
        shape.name = name;
        shape.descr = descr;
        shape.hidden = hidden;
        shape.ph = ph;
        if sp_pr.is_some_and(|s| s.child("a:effectLst").is_some_and(|x| x.els().next().is_some())) {
            self.rep.note(Kind::Approximate, "shadows and glow left off");
        }
        if sp_pr.is_some_and(|s| s.child("a:scene3d").is_some() || s.child("a:sp3d").is_some()) {
            self.rep.note(Kind::Approximate, "3-D effects drawn flat");
        }
        shape.kind = match local(&e.name) {
            "sp" | "cxnSp" => {
                let (sfill, sline, sfont, fc, lc) = self.style_refs(ctx, kid(e, "style"));
                let saved = ctx.ph_color.take();
                let geom = sp_pr.and_then(|s| self.geom(s));
                ctx.ph_color = fc;
                let fill = sp_pr.and_then(|s| self.fill(ctx, s)).or(sfill);
                ctx.ph_color = lc;
                let line = match sp_pr.and_then(|s| s.child("a:ln")) {
                    Some(l) => {
                        let mut st = self.line(ctx, l);
                        if l.attr("w").is_none() {
                            if let Some(sl) = &sline {
                                st.width = sl.width;
                            }
                        }
                        Some(st)
                    }
                    None => sline,
                };
                ctx.ph_color = sfont.clone();
                let mut text = kid(e, "txBody").map(|t| self.body(ctx, t));
                ctx.ph_color = saved;
                if let (Some(t), Some(c)) = (&mut text, sfont) {
                    // The style's text colour, under the frame's own levels.
                    for i in 0..MAX_LEVELS {
                        if t.levels.len() <= i {
                            t.levels.push(LevelStyle::default());
                        }
                        t.levels[i].color.get_or_insert(c.clone());
                    }
                }
                ShapeKind::Geom { geom, fill, line, text }
            }
            "pic" => {
                let bf = kid(e, "blipFill");
                let rid = bf.and_then(|b| b.child("a:blip")).and_then(|b| b.attr("r:embed")).map(str::to_string);
                let poster = rid.and_then(|r| self.media(ctx.rels, &r));
                let crop = bf.and_then(|b| b.child("a:srcRect")).map(|s| ["l", "t", "r", "b"].map(|k| num(s, k).unwrap_or(0).clamp(-1_000_000, 1_000_000) as i32)).unwrap_or([0; 4]);
                let line = sp_pr.and_then(|s| s.child("a:ln")).map(|l| self.line(ctx, l));
                // A video: the movie and its poster frame.
                let nvpr = nv.and_then(|n| n.child("p:nvPr"));
                let video = nvpr.and_then(|n| {
                    let embed = n.find("p14:media").and_then(|m| m.attr("r:embed")).map(str::to_string);
                    let link = n.child("a:videoFile").and_then(|v| v.attr("r:link")).map(str::to_string);
                    embed.or(link)
                });
                match (video, poster) {
                    (Some(v), p) => match self.media(ctx.rels, &v) {
                        Some(m) => ShapeKind::Video { media: m, poster: p },
                        None => match p {
                            Some(p) => ShapeKind::Picture { media: p, crop, line },
                            None => return None,
                        },
                    },
                    (None, Some(p)) => ShapeKind::Picture { media: p, crop, line },
                    (None, None) => {
                        if nvpr.is_some_and(|n| n.child("a:audioFile").is_some() || n.child("a:wavAudioFile").is_some()) {
                            self.rep.note(Kind::Dropped, "sounds");
                        }
                        self.rep.note(Kind::Dropped, "pictures with nothing to show");
                        return None;
                    }
                }
            }
            "grpSp" => {
                let kids = self.tree(ctx, e, depth + 1);
                let child = x.and_then(|x| x.4).or(x.map(|x| x.0)).unwrap_or_default();
                ShapeKind::Group { child, shapes: kids }
            }
            "graphicFrame" => {
                let data = e.find("a:graphicData");
                let uri = data.and_then(|d| d.attr("uri")).unwrap_or("");
                if let Some(tbl) = data.and_then(|d| d.child("a:tbl")) {
                    return Some(self.table(ctx, shape, tbl));
                }
                if let Some(ole) = data.and_then(|d| d.find("p:pic")) {
                    // An embedded object's picture.
                    let mut s = self.shape(ctx, ole, depth + 1)?;
                    if s.frame.is_none() {
                        s.frame = shape.frame;
                    }
                    self.rep.note(Kind::Approximate, "embedded objects shown as their picture");
                    return Some(s);
                }
                if uri.ends_with("/chart") {
                    self.rep.note(Kind::Dropped, "charts");
                } else if uri.ends_with("/diagram") {
                    if let Some(g) = data.and_then(|d| d.child("dgm:relIds")).and_then(|r| self.diagram(ctx, shape.clone(), r, depth)) {
                        return Some(g);
                    }
                    self.rep.note(Kind::Dropped, "SmartArt diagrams");
                } else {
                    self.rep.note_at(Kind::Dropped, "embedded objects", || uri.to_string());
                }
                return None;
            }
            "contentPart" => {
                self.rep.note(Kind::Dropped, "ink");
                return None;
            }
            _ => return None,
        };
        self.loaded.shapes += 1;
        Some(shape)
    }

    /// SmartArt: the shapes PowerPoint drew for it (its drawing part), else
    /// its text in a frame.
    fn diagram(&mut self, ctx: &mut Ctx, mut g: Shape, ids: &El, depth: usize) -> Option<Shape> {
        let frame = g.frame?;
        let data_part = ids.attr("r:dm").and_then(|r| ctx.rels.get(r)).map(|t| t.1.clone())?;
        let data = part(self.archive, &data_part).ok()??;
        let drawing = data.find("dsp:dataModelExt").and_then(|x| x.attr("relId")).and_then(|r| ctx.rels.get(r)).map(|t| t.1.clone());
        if let Some(dp) = drawing {
            if let Some(d) = part(self.archive, &dp).ok().flatten() {
                if let Some(tree) = d.find("dsp:spTree") {
                    let drels = rels(self.archive, &dp).unwrap_or_default();
                    let mut dctx = Ctx { rels: &drels, map: ctx.map, ph_color: None, ids: HashMap::new() };
                    let kids = self.tree(&mut dctx, tree, depth + 1);
                    if kids.is_empty() {
                        // An empty drawing: fall back to the data model's text.
                        return self.diagram_text(g, &data);
                    }
                    self.rep.note(Kind::Approximate, "SmartArt kept as the shapes it was drawn with");
                    g.kind = ShapeKind::Group { child: Rect::new(0, 0, frame.w.max(1), frame.h.max(1)), shapes: kids };
                    self.loaded.shapes += 1;
                    return Some(g);
                }
            }
        }
        self.diagram_text(g, &data)
    }

    /// No drawing: the diagram's text, one paragraph per node.
    fn diagram_text(&mut self, mut g: Shape, data: &El) -> Option<Shape> {
        let mut lines = Vec::new();
        if let Some(pts) = data.find("dgm:ptLst") {
            for pt in pts.children("dgm:pt") {
                if pt.attr("type").is_some_and(|t| t != "node") {
                    continue;
                }
                if let Some(t) = pt.child("dgm:t") {
                    for p in t.children("a:p") {
                        let s = p.text();
                        if !s.trim().is_empty() {
                            lines.push(s);
                        }
                    }
                }
            }
        }
        if lines.is_empty() {
            return None;
        }
        self.rep.note(Kind::Approximate, "SmartArt with no drawing kept as its text");
        let mut t = TextBody::plain(&lines.join("\n"));
        t.autofit = Some(Autofit::Shrink { scale: 100_000, spacing_cut: 0 });
        g.kind = ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(Fill::None), line: None, text: Some(t) };
        self.loaded.shapes += 1;
        Some(g)
    }

    /// A table drawn as a group of cells, each a rectangle with its text.
    fn table(&mut self, ctx: &mut Ctx, mut g: Shape, tbl: &El) -> Shape {
        self.rep.note(Kind::Approximate, "tables kept as groups of cells");
        let cols: Vec<i64> = tbl.child("a:tblGrid").map(|t| t.children("a:gridCol").map(|c| num(c, "w").unwrap_or(0).max(0)).collect()).unwrap_or_default();
        let frame = g.frame.unwrap_or_default();
        let mut cells = Vec::new();
        let mut y = 0i64;
        let rows: Vec<&El> = tbl.children("a:tr").collect();
        let heights: Vec<i64> = rows.iter().map(|r| num(r, "h").unwrap_or(0).max(0)).collect();
        for (ri, tr) in rows.iter().enumerate() {
            let mut x = 0i64;
            for (ci, tc) in tr.children("a:tc").enumerate() {
                let w0 = cols.get(ci).copied().unwrap_or(0);
                if on(tc, "hMerge") || on(tc, "vMerge") {
                    x += w0;
                    continue;
                }
                let span = num(tc, "gridSpan").unwrap_or(1).clamp(1, 1000) as usize;
                let rspan = num(tc, "rowSpan").unwrap_or(1).clamp(1, 1000) as usize;
                let w: i64 = (ci..ci + span).map(|i| cols.get(i).copied().unwrap_or(0)).sum();
                let h: i64 = (ri..ri + rspan).map(|i| heights.get(i).copied().unwrap_or(0)).sum();
                let id = self.deck.new_id();
                let pr = tc.child("a:tcPr");
                let fill = pr.and_then(|p| self.fill(ctx, p)).unwrap_or(Fill::None);
                let line = pr.and_then(|p| ["a:lnT", "a:lnB", "a:lnL", "a:lnR"].iter().find_map(|n| p.child(n))).map(|l| self.line(ctx, l));
                let mut text = tc.child("a:txBody").map(|t| self.body(ctx, t)).unwrap_or_else(|| TextBody::plain(""));
                if let Some(p) = pr {
                    let ins = [num(p, "marL"), num(p, "marT"), num(p, "marR"), num(p, "marB")];
                    text.inset = Some([0, 1, 2, 3].map(|i| ins[i].unwrap_or(if i % 2 == 0 { 91_440 } else { 45_720 }).clamp(0, 914_400)));
                    text.anchor = match p.attr("anchor") {
                        Some("ctr") => Some(Anchor::Middle),
                        Some("b") => Some(Anchor::Bottom),
                        _ => Some(Anchor::Top),
                    };
                }
                let mut s = Shape::new(id, Rect::new(x, y, w, h), ShapeKind::Geom { geom: Some(Geom::preset("rect")), fill: Some(fill), line: Some(line.unwrap_or_else(|| Stroke::solid(Color::theme(Slot::Dk1).with(ColorMod::Tint(40_000)), 6_350))), text: Some(text) });
                s.name = format!("Cell {} {}", ri + 1, ci + 1).into();
                cells.push(s);
                x += w0;
            }
            y += heights.get(ri).copied().unwrap_or(0);
        }
        let total_w: i64 = cols.iter().sum();
        g.kind = ShapeKind::Group { child: Rect::new(0, 0, total_w.max(1), y.max(1)), shapes: cells };
        // The frame keeps the table's own size (rows grow with their text).
        g.frame = Some(Rect::new(frame.x, frame.y, total_w.max(1), y.max(frame.h).max(1)));
        if let ShapeKind::Group { child, .. } = &mut g.kind {
            child.h = y.max(frame.h).max(1);
        }
        self.loaded.shapes += 1;
        g
    }

    fn background(&mut self, ctx: &Ctx, c_sld: &El) -> Option<Fill> {
        let bg = c_sld.child("p:bg")?;
        if let Some(pr) = bg.child("p:bgPr") {
            return self.fill(ctx, pr);
        }
        if let Some(r) = bg.child("p:bgRef") {
            if num(r, "idx").unwrap_or(0) > 1001 {
                self.rep.note(Kind::Approximate, "theme background styles drawn as solid colours");
            }
            return self.color(ctx, r).map(Fill::Solid);
        }
        None
    }
}

fn mods(c: &El) -> Vec<ColorMod> {
    c.els()
        .filter_map(|m| {
            let v = num(m, "val")?.clamp(-10_000_000, 10_000_000) as i32;
            Some(match m.name.as_str() {
                "a:lumMod" => ColorMod::LumMod(v),
                "a:lumOff" => ColorMod::LumOff(v),
                "a:tint" => ColorMod::Tint(v),
                "a:shade" => ColorMod::Shade(v),
                "a:satMod" => ColorMod::SatMod(v),
                "a:alpha" => ColorMod::Alpha(v),
                _ => return None,
            })
        })
        .collect()
}

fn preset_color(n: &str) -> u32 {
    match n {
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        "red" => 0xFF0000,
        "green" => 0x008000,
        "lime" => 0x00FF00,
        "blue" => 0x0000FF,
        "yellow" => 0xFFFF00,
        "cyan" | "aqua" => 0x00FFFF,
        "magenta" | "fuchsia" => 0xFF00FF,
        "gray" | "grey" => 0x808080,
        "silver" => 0xC0C0C0,
        "maroon" => 0x800000,
        "navy" => 0x000080,
        "orange" => 0xFFA500,
        "purple" => 0x800080,
        "teal" => 0x008080,
        "olive" => 0x808000,
        "ltGray" | "lightGray" => 0xD3D3D3,
        "dkGray" | "darkGray" => 0xA9A9A9,
        _ => 0x000000,
    }
}

/// An elliptical arc from `cur`: radii `wr`, `hr`, start and sweep angles in
/// 60,000ths of a degree; cubic pieces of at most 90°. Returns the end.
/// A custom geometry with guides, as a definition (the evaluator the
/// preset shapes use).
fn guided_geom(c: &El) -> officina_slides::presets::Def {
    use officina_slides::presets::{Builder, Cmd};
    let mut b = Builder::new();
    for (list, adj) in [("a:avLst", true), ("a:gdLst", false)] {
        for g in c.child(list).into_iter().flat_map(|l| l.children("a:gd")) {
            let (Some(n), Some(f)) = (g.attr("name"), g.attr("fmla")) else { continue };
            if adj {
                b.adj(n, f);
            } else {
                b.guide(n, f);
            }
        }
    }
    for p in c.child("a:pathLst").into_iter().flat_map(|l| l.children("a:path")) {
        let size = |a: &str| p.attr(a).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0).unwrap_or(0.0);
        let stroked = !matches!(p.attr("stroke"), Some("0") | Some("false"));
        b.path(size("w"), size("h"), p.attr("fill") != Some("none"), stroked);
        for cmd in p.els() {
            let pts: Vec<_> = cmd.children("a:pt").map(|pt| (b.arg(pt.attr("x").unwrap_or("0")), b.arg(pt.attr("y").unwrap_or("0")))).collect();
            let c = match (cmd.name.as_str(), &pts[..]) {
                ("a:moveTo", [p, ..]) => Cmd::Move(p.0, p.1),
                ("a:lnTo", [p, ..]) => Cmd::Line(p.0, p.1),
                ("a:quadBezTo", [p, q, ..]) => Cmd::Quad(p.0, p.1, q.0, q.1),
                ("a:cubicBezTo", [p, q, r, ..]) => Cmd::Cubic(p.0, p.1, q.0, q.1, r.0, r.1),
                ("a:arcTo", _) => {
                    let mut a = |n: &str| b.arg(cmd.attr(n).unwrap_or("0"));
                    Cmd::Arc(a("wR"), a("hR"), a("stAng"), a("swAng"))
                }
                ("a:close", _) => Cmd::Close,
                _ => continue,
            };
            b.cmd(c);
        }
    }
    b.finish()
}

fn arc(out: &mut Vec<PathCmd>, cur: (i64, i64), wr: f64, hr: f64, st: f64, sw: f64) -> (i64, i64) {
    let a0 = (st / 60_000.0).to_radians();
    let da = (sw / 60_000.0).to_radians();
    // The centre, from the start point on the ellipse.
    let (cx, cy) = (cur.0 as f64 - wr * a0.cos(), cur.1 as f64 - hr * a0.sin());
    let n = ((da.abs() / std::f64::consts::FRAC_PI_2).ceil() as usize).clamp(1, 16);
    let step = da / n as f64;
    let mut a = a0;
    let mut end = cur;
    for _ in 0..n {
        let b = a + step;
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let (x0, y0) = (cx + wr * a.cos(), cy + hr * a.sin());
        let (x3, y3) = (cx + wr * b.cos(), cy + hr * b.sin());
        let (x1, y1) = (x0 - k * wr * a.sin(), y0 + k * hr * a.cos());
        let (x2, y2) = (x3 + k * wr * b.sin(), y3 - k * hr * b.cos());
        end = (x3.round() as i64, y3.round() as i64);
        out.push(PathCmd::Cubic(x1.round() as i64, y1.round() as i64, x2.round() as i64, y2.round() as i64, end.0, end.1));
        a = b;
    }
    end
}

fn theme(t: &El, line_widths: &mut Vec<Emu>) -> Theme {
    let mut th = Theme::default();
    if let Some(n) = t.attr("name") {
        th.name = n.into();
    }
    if let Some(cs) = t.find("a:clrScheme") {
        for (i, slot) in Slot::ALL.iter().enumerate() {
            let tag = match slot {
                Slot::Dk1 => "a:dk1",
                Slot::Lt1 => "a:lt1",
                Slot::Dk2 => "a:dk2",
                Slot::Lt2 => "a:lt2",
                Slot::Accent1 => "a:accent1",
                Slot::Accent2 => "a:accent2",
                Slot::Accent3 => "a:accent3",
                Slot::Accent4 => "a:accent4",
                Slot::Accent5 => "a:accent5",
                Slot::Accent6 => "a:accent6",
                Slot::Hlink => "a:hlink",
                Slot::FolHlink => "a:folHlink",
            };
            let Some(e) = cs.child(tag) else { continue };
            let v = e.child("a:srgbClr").and_then(|c| c.attr("val")).or_else(|| e.child("a:sysClr").and_then(|c| c.attr("lastClr")));
            if let Some(v) = v.and_then(|v| u32::from_str_radix(v, 16).ok()) {
                th.colors[i] = v & 0xFF_FFFF;
            }
        }
    }
    if let Some(fs) = t.find("a:fontScheme") {
        if let Some(f) = fs.child("a:majorFont").and_then(|m| m.child("a:latin")).and_then(|l| l.attr("typeface")) {
            th.major = f.into();
        }
        if let Some(f) = fs.child("a:minorFont").and_then(|m| m.child("a:latin")).and_then(|l| l.attr("typeface")) {
            th.minor = f.into();
        }
    }
    if let Some(ls) = t.find("a:lnStyleLst") {
        *line_widths = ls.children("a:ln").map(|l| num(l, "w").unwrap_or(9_525).clamp(0, 914_400)).collect();
    }
    th
}

fn transition(rep: &mut Report, t: &El) -> Option<Transition> {
    let ms = num(t, "p14:dur").or_else(|| num(t, "dur")).map(|v| v.clamp(0, 60_000) as u32).unwrap_or(match t.attr("spd") {
        Some("slow") => 1000,
        Some("med") => 750,
        _ => 500,
    });
    let on_click = t.attr("advClick") != Some("0");
    let after_ms = num(t, "advTm").map(|v| v.clamp(0, 86_400_000) as u32);
    let dir = |e: &El, def: Dir| match e.attr("dir") {
        Some("l") => Dir::Left,
        Some("u") => Dir::Up,
        Some("r") => Dir::Right,
        Some("d") => Dir::Down,
        _ => def,
    };
    let mut kind = TransitionKind::Cut;
    let mut any = false;
    for e in t.els() {
        any = true;
        let name = e.name.rsplit(':').next().unwrap_or("");
        kind = match name {
            "fade" => {
                if on(e, "thruBlk") {
                    rep.note(Kind::Approximate, "fades through black drawn as fades");
                }
                TransitionKind::Fade
            }
            "push" => TransitionKind::Push(dir(e, Dir::Up)),
            "wipe" => TransitionKind::Wipe(dir(e, Dir::Left)),
            "cover" => TransitionKind::Cover(dir(e, Dir::Left)),
            "pull" => TransitionKind::Uncover(dir(e, Dir::Left)),
            "split" => TransitionKind::Split { vertical: e.attr("orient") == Some("vert"), out: e.attr("dir") != Some("in") },
            "zoom" => TransitionKind::Zoom { out: e.attr("dir") == Some("out") },
            "dissolve" => TransitionKind::Dissolve,
            "cut" => TransitionKind::Cut,
            "sndAc" | "extLst" => {
                if name == "sndAc" {
                    rep.note(Kind::Dropped, "transition sounds");
                }
                continue;
            }
            other => {
                rep.note_at(Kind::Approximate, "transitions not in the set drawn as fades", || other.to_string());
                TransitionKind::Fade
            }
        };
        break;
    }
    if !any && after_ms.is_none() && on_click {
        return None;
    }
    Some(Transition { kind, ms, on_click, after_ms })
}

/// Builds: the entrance and exit effects a slide's timing tree names.
fn builds(rep: &mut Report, timing: &El, ids: &HashMap<u32, ShapeId>) -> Vec<Build> {
    let mut out = Vec::new();
    fn walk<'e>(e: &'e El, f: &mut dyn FnMut(&'e El)) {
        for k in e.els() {
            if k.name == "p:cTn" && k.attr("presetClass").is_some() {
                f(k);
            } else {
                walk(k, f);
            }
        }
    }
    walk(timing, &mut |c| {
        let class = c.attr("presetClass").unwrap_or("");
        let preset = num(c, "presetID").unwrap_or(0);
        let sub = num(c, "presetSubtype").unwrap_or(0);
        let trigger = match c.attr("nodeType") {
            Some("withEffect") => Trigger::WithPrevious,
            Some("afterEffect") => Trigger::AfterPrevious,
            _ => Trigger::OnClick,
        };
        let dir = match sub {
            8 => Dir::Left,
            1 => Dir::Up,
            2 => Dir::Right,
            4 => Dir::Down,
            _ => Dir::Down,
        };
        let effect = match (class, preset) {
            ("entr", 1) => Effect::Appear,
            ("entr", 10) => Effect::Fade,
            ("entr", 2) => Effect::FlyIn(dir),
            ("entr", 22) => Effect::Wipe(dir),
            ("entr", 53) | ("entr", 23) => Effect::Zoom,
            ("exit", 1) => Effect::Disappear,
            ("exit", 10) => Effect::FadeOut,
            ("entr", _) => {
                rep.note(Kind::Approximate, "entrance effects not in the set run as fades");
                Effect::Fade
            }
            ("exit", _) => {
                rep.note(Kind::Approximate, "exit effects not in the set run as fade-outs");
                Effect::FadeOut
            }
            _ => {
                rep.note(Kind::Dropped, "emphasis and motion-path effects");
                return;
            }
        };
        let Some(tgt) = c.find("p:spTgt") else { return };
        let Some(sid) = num(tgt, "spid").and_then(|v| ids.get(&(v as u32)).copied()) else { return };
        let by_paragraph = tgt.child("p:txEl").is_some();
        // The effect's own length: the longest animation under it.
        let mut ms = 0u32;
        fn durs(e: &El, ms: &mut u32) {
            for k in e.els() {
                if k.name == "p:cTn" {
                    if let Some(d) = k.attr("dur").and_then(|d| d.parse::<u32>().ok()) {
                        *ms = (*ms).max(d.min(60_000));
                    }
                }
                durs(k, ms);
            }
        }
        durs(c, &mut ms);
        let delay_ms = c.child("p:stCondLst").and_then(|s| s.child("p:cond")).and_then(|d| d.attr("delay")).and_then(|d| d.parse::<u32>().ok()).unwrap_or(0).min(60_000);
        // One build per shape and effect: paragraph-by-paragraph effects
        // repeat per paragraph in the file.
        if by_paragraph && out.last().is_some_and(|b: &Build| b.shape == sid && b.by_paragraph && b.effect == effect) {
            return;
        }
        out.push(Build { shape: sid, effect, trigger, ms: if ms == 0 { 500 } else { ms }, delay_ms, by_paragraph });
    });
    out
}

/// Reads a `.pptx`.
pub fn read(data: &[u8]) -> Result<(Deck, Loaded), PptxError> {
    let archive = Archive::open(data)?;
    let root = rels(&archive, "")?;
    let pres_name = root.values().find(|(k, _, _)| k == "officeDocument").map(|r| r.1.clone()).unwrap_or_else(|| "ppt/presentation.xml".into());
    let pres = part(&archive, &pres_name)?.ok_or_else(|| PptxError::Bad("no presentation part".into()))?;
    if pres.name != "p:presentation" {
        return Err(PptxError::Bad(format!("the main part is {}", pres.name)));
    }
    let prels = rels(&archive, &pres_name)?;
    let mut r = Reader { archive: &archive, deck: Deck::empty(), rep: Report::default(), loaded: Loaded::default(), media: HashMap::new(), line_widths: Vec::new(), shapes: 0, on_slide: false };
    if let Some(sz) = pres.child("p:sldSz") {
        let (w, h) = (num(sz, "cx").unwrap_or(12_192_000), num(sz, "cy").unwrap_or(6_858_000));
        r.deck.size = (w.clamp(EMU_MIN_SIDE, EMU_MAX_SIDE), h.clamp(EMU_MIN_SIDE, EMU_MAX_SIDE));
    }

    // Masters and their layouts.
    let mut layout_ids: HashMap<String, (LayoutId, ClrMap)> = HashMap::new();
    let mut first_theme = true;
    let master_parts: Vec<String> = pres.child("p:sldMasterIdLst").map(|l| l.children("p:sldMasterId").filter_map(|m| m.attr("r:id")).filter_map(|id| prels.get(id).map(|t| t.1.clone())).collect()).unwrap_or_default();
    for mp in master_parts.iter().take(MAX_MASTERS) {
        let Some(m) = part(&archive, mp)? else { continue };
        let mrels = rels(&archive, mp)?;
        if let Some(tp) = mrels.values().find(|(k, _, _)| k == "theme").map(|t| t.1.clone()) {
            if let Some(t) = part(&archive, &tp)? {
                if first_theme {
                    let mut lw = Vec::new();
                    r.deck.theme = theme(&t, &mut lw);
                    r.line_widths = lw;
                    first_theme = false;
                } else {
                    r.rep.note(Kind::Approximate, "masters with themes of their own use the first master's theme");
                }
            }
        }
        let map = m.child("p:clrMap").map(ClrMap::read).unwrap_or_else(ClrMap::standard);
        let mid = r.deck.new_id();
        let c_sld = m.child("p:cSld");
        let mut ctx = Ctx { rels: &mrels, map: &map, ph_color: None, ids: HashMap::new() };
        let background = c_sld.and_then(|c| r.background(&ctx, c)).unwrap_or(Fill::Solid(Color::theme(map.slot("bg1").unwrap_or(Slot::Lt1))));
        let shapes = c_sld.and_then(|c| c.child("p:spTree")).map(|t| r.tree(&mut ctx, t, 0)).unwrap_or_default();
        let styles = m.child("p:txStyles");
        let ls = |r: &mut Reader, name: &str| styles.and_then(|s| s.child(name)).map(|l| r.list_style(&ctx, l)).unwrap_or_default();
        let (title_style, body_style, other_style) = (ls(&mut r, "p:titleStyle"), ls(&mut r, "p:bodyStyle"), ls(&mut r, "p:otherStyle"));
        let name = c_sld.and_then(|c| c.attr("name")).unwrap_or("Master").into();
        r.deck.masters.push(Master { id: mid, name, background, shapes, title_style, body_style, other_style });
        let lparts: Vec<String> = m.child("p:sldLayoutIdLst").map(|l| l.children("p:sldLayoutId").filter_map(|x| x.attr("r:id")).filter_map(|id| mrels.get(id).map(|t| t.1.clone())).collect()).unwrap_or_default();
        for lp in lparts {
            if r.deck.layouts.len() >= MAX_LAYOUTS {
                r.rep.note(Kind::Dropped, "layouts past the limit");
                break;
            }
            let Some(l) = part(&archive, &lp)? else { continue };
            let lrels = rels(&archive, &lp)?;
            let lmap = match l.child("p:clrMapOvr").and_then(|o| o.child("a:overrideClrMapping")) {
                Some(o) => ClrMap::read(o),
                None => map.clone(),
            };
            let lid = r.deck.new_id();
            let c_sld = l.child("p:cSld");
            let mut ctx = Ctx { rels: &lrels, map: &lmap, ph_color: None, ids: HashMap::new() };
            let background = c_sld.and_then(|c| r.background(&ctx, c));
            let shapes = c_sld.and_then(|c| c.child("p:spTree")).map(|t| r.tree(&mut ctx, t, 0)).unwrap_or_default();
            let name = c_sld.and_then(|c| c.attr("name")).unwrap_or("Layout").into();
            let kind = l.attr("type").unwrap_or("").into();
            r.deck.layouts.push(Layout { id: lid, master: mid, name, kind, background, shapes, hide_master_shapes: l.attr("showMasterSp") == Some("0") });
            layout_ids.insert(lp, (lid, lmap));
        }
    }
    if r.deck.masters.is_empty() || r.deck.layouts.is_empty() {
        r.rep.note(Kind::Approximate, "a deck without masters or layouts given the default ones");
        let mut fresh = Deck::new();
        fresh.size = r.deck.size;
        fresh.theme = r.deck.theme.clone();
        // Keep ids above what the fresh master took.
        let wm = fresh.id_watermark();
        r.deck.masters = fresh.masters;
        r.deck.layouts = fresh.layouts;
        while r.deck.id_watermark() < wm {
            r.deck.new_id();
        }
    }

    // Slides.
    r.on_slide = true;
    let slide_parts: Vec<String> = pres.child("p:sldIdLst").map(|l| l.children("p:sldId").filter_map(|s| s.attr("r:id")).filter_map(|id| prels.get(id).map(|t| t.1.clone())).collect()).unwrap_or_default();
    for sp in slide_parts {
        if r.deck.slide_count() >= MAX_SLIDES {
            r.rep.note(Kind::Dropped, "slides past the limit");
            break;
        }
        let Some(s) = part(&archive, &sp)? else {
            r.rep.note(Kind::Dropped, "slides the file names but doesn't hold");
            continue;
        };
        let srels = rels(&archive, &sp)?;
        let (layout, lmap) = match srels.values().find(|(k, _, _)| k == "slideLayout").and_then(|t| layout_ids.get(&t.1)) {
            Some((id, m)) => (*id, m.clone()),
            None => (r.deck.layouts[0].id, ClrMap::standard()),
        };
        let map = match s.child("p:clrMapOvr").and_then(|o| o.child("a:overrideClrMapping")) {
            Some(o) => ClrMap::read(o),
            None => lmap,
        };
        let id = r.deck.new_id();
        let mut slide = Slide::new(id, layout);
        let c_sld = s.child("p:cSld");
        let mut ctx = Ctx { rels: &srels, map: &map, ph_color: None, ids: HashMap::new() };
        slide.background = c_sld.and_then(|c| r.background(&ctx, c));
        slide.shapes = c_sld.and_then(|c| c.child("p:spTree")).map(|t| r.tree(&mut ctx, t, 0)).unwrap_or_default();
        slide.hidden = s.attr("show") == Some("0");
        slide.hide_layout_shapes = s.attr("showMasterSp") == Some("0");
        let tr = s.child("p:transition").or_else(|| s.child("mc:AlternateContent").and_then(|a| a.child("mc:Fallback").or_else(|| a.child("mc:Choice"))).and_then(|f| f.child("p:transition")));
        slide.transition = tr.and_then(|t| transition(&mut r.rep, t));
        if let Some(t) = s.child("p:timing") {
            slide.builds = builds(&mut r.rep, t, &ctx.ids);
        }
        // Notes: the body placeholder's text.
        if let Some(np) = srels.values().find(|(k, _, _)| k == "notesSlide").map(|t| t.1.clone()) {
            if let Some(n) = part(&archive, &np)? {
                let nrels = rels(&archive, &np)?;
                let nctx = Ctx { rels: &nrels, map: &map, ph_color: None, ids: HashMap::new() };
                let body = n.find("p:spTree").and_then(|t| t.children("p:sp").find(|sp| sp.find("p:ph").is_some_and(|p| p.attr("type") == Some("body"))));
                if let Some(tx) = body.and_then(|b| b.child("p:txBody")) {
                    let t = r.body(&nctx, tx);
                    if !t.text().trim().is_empty() {
                        slide.notes = Some(t);
                    }
                }
            }
        }
        if srels.values().any(|(k, _, _)| k == "comments" || k == "comment") {
            r.rep.note(Kind::Dropped, "slide comments");
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
    if archive.find("ppt/vbaProject.bin").is_some() {
        r.rep.note(Kind::Dropped, "macros");
    }
    Ok((r.deck, Loaded { report: r.rep, ..r.loaded }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_geometry_guides_follow_the_shape_size() {
        let x = br#"<a:custGeom><a:avLst><a:gd name="adj" fmla="val 25000"/></a:avLst><a:gdLst><a:gd name="x1" fmla="*/ w adj 100000"/><a:gd name="y1" fmla="+- b 0 hd4"/></a:gdLst><a:pathLst><a:path><a:moveTo><a:pt x="l" y="t"/></a:moveTo><a:lnTo><a:pt x="x1" y="y1"/></a:lnTo><a:arcTo wR="wd4" hR="hd4" stAng="0" swAng="cd4"/><a:close/></a:path><a:path w="10" h="10" fill="none"><a:moveTo><a:pt x="5" y="10"/></a:moveTo></a:path></a:pathLst></a:custGeom>"#;
        let c = xml::parse(x).unwrap();
        let p = guided_geom(&c).paths(400, 200, &[]);
        assert_eq!(p.len(), 2);
        assert_eq!(&p[0].cmds[..2], &[PathCmd::Move(0, 0), PathCmd::Line(100, 150)]);
        assert!(matches!(p[0].cmds[2], PathCmd::Cubic(.., 0, 200)));
        assert_eq!((p[1].cmds[0].clone(), p[1].filled), (PathCmd::Move(200, 200), false));
    }
}
