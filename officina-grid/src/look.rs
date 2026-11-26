//! The workbook's look and its drawn objects (docs/spec/officina.md §16.165):
//! its theme (twelve colours and two fonts, as Excel's), custom table styles,
//! shapes and text boxes, and Camera pictures (a live picture of a range).
//! All of it changes only through [`GridOp::Look`], so every change undoes.
//!
//! Cell colours are kept as RGB, so a theme change is a restyle: colours that
//! were the old theme's become the new theme's ([`Workbook::theme_op`]).

use officina_core::wire::{Reader, WireError, Writer};
use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::charts::Anchor;
use crate::ops::GridOp;
use crate::style::Style;
use crate::tables::Table;
use crate::Workbook;

/// Custom table styles, shapes and cameras a workbook may have.
pub const MAX_TABLE_STYLES: usize = 64;
pub const MAX_SHAPES: usize = 1024;
pub const MAX_CAMERAS: usize = 64;
/// A shape's text, in bytes.
pub const MAX_SHAPE_TEXT: usize = 32 * 1024;
/// A name (theme, font, style, shape), in bytes.
pub const MAX_NAME: usize = 255;
/// Cells a camera may show.
pub const MAX_CAMERA_CELLS: u64 = 10_000;

/// A theme: Excel's colour scheme (dark 1, light 1, dark 2, light 2, accents 1
/// to 6, hyperlink, followed hyperlink) and its heading and body fonts.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: Arc<str>,
    pub colors: [u32; 12],
    pub heading: Arc<str>,
    pub body: Arc<str>,
}

/// The names of a theme's colours, in order.
pub const COLOR_NAMES: [&str; 12] = ["dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink"];

impl Default for Theme {
    fn default() -> Self {
        builtin_themes().swap_remove(0)
    }
}

/// The themes Themes… offers, Office first.
pub fn builtin_themes() -> Vec<Theme> {
    let t = |name: &str, colors: [u32; 12], heading: &str, body: &str| Theme { name: name.into(), colors, heading: heading.into(), body: body.into() };
    vec![
        t("Office", [0x000000, 0xFFFFFF, 0x44546A, 0xE7E6E6, 0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47, 0x0563C1, 0x954F72], "Calibri Light", "Calibri"),
        t("Office 2007", [0x000000, 0xFFFFFF, 0x1F497D, 0xEEECE1, 0x4F81BD, 0xC0504D, 0x9BBB59, 0x8064A2, 0x4BACC6, 0xF79646, 0x0000FF, 0x800080], "Cambria", "Calibri"),
        t("Grayscale", [0x000000, 0xFFFFFF, 0x000000, 0xF8F8F8, 0xDDDDDD, 0xB2B2B2, 0x969696, 0x808080, 0x5F5F5F, 0x4D4D4D, 0x5F5F5F, 0x919191], "Arial", "Arial"),
        t("Blue", [0x000000, 0xFFFFFF, 0x17406D, 0xDBEFF9, 0x0F6FC6, 0x009DD9, 0x0BD0D9, 0x10CF9B, 0x7CCA62, 0xA5C249, 0xF49100, 0x85DFD0], "Calibri", "Calibri"),
        t("Blue Green", [0x000000, 0xFFFFFF, 0x373545, 0xCEDBE6, 0x3494BA, 0x58B6C0, 0x75BDA7, 0x7A8C8E, 0x84ACB6, 0x2683C6, 0x6B9F25, 0x9F6715], "Calibri", "Calibri"),
        t("Green", [0x000000, 0xFFFFFF, 0x455F51, 0xE3DED1, 0x549E39, 0x8AB833, 0xC0CF3A, 0x029676, 0x4AB5C4, 0x0989B1, 0x6B9F25, 0xBA6906], "Calibri", "Calibri"),
        t("Yellow Orange", [0x000000, 0xFFFFFF, 0x4E3B30, 0xFBEEC9, 0xF0A22E, 0xA5644E, 0xB58B80, 0xC3986D, 0xA19574, 0xC17529, 0xAD1F1F, 0xFFC42F], "Georgia", "Georgia"),
        t("Orange Red", [0x000000, 0xFFFFFF, 0x696464, 0xE9E5DC, 0xD34817, 0x9B2D1F, 0xA28E6A, 0x956251, 0x918485, 0x855D5D, 0xCC9900, 0x96A9A9], "Tw Cen MT", "Tw Cen MT"),
        t("Red Violet", [0x000000, 0xFFFFFF, 0x454551, 0xD8D9DC, 0xE32D91, 0xC830CC, 0x4EA6DC, 0x4775E7, 0x8971E1, 0xD54773, 0x6B9F25, 0x8C8C8C], "Franklin Gothic Medium", "Franklin Gothic Book"),
        t("Violet", [0x000000, 0xFFFFFF, 0x373545, 0xDCD8DC, 0xAD84C6, 0x8784C7, 0x5D739A, 0x6997AF, 0x84ACB6, 0x6F8183, 0x69A020, 0x8C8C8C], "Century Gothic", "Century Gothic"),
        t("Median", [0x000000, 0xFFFFFF, 0x775F55, 0xEBDDC3, 0x94B6D2, 0xDD8047, 0xA5AB81, 0xD8B25C, 0x7BA79D, 0x968C8C, 0xF7B615, 0x704404], "Tw Cen MT", "Tw Cen MT"),
        t("Paper", [0x000000, 0xFFFFFF, 0x444D26, 0xFEFAC9, 0xA5B592, 0xF3A447, 0xE7BC29, 0xD092A7, 0x9C85C0, 0x809EC2, 0x8E58B6, 0x7F6F6F], "Constantia", "Constantia"),
    ]
}

/// The theme, colour set or font pair named `name` among the built-ins (any case).
pub fn builtin_theme(name: &str) -> Option<Theme> {
    builtin_themes().into_iter().find(|t| t.name.eq_ignore_ascii_case(name.trim()))
}

/// How one part of a table looks: fill, text colour, bold, and a border
/// colour (round the whole table for its whole-table part, under a header,
/// over a totals row).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TablePart {
    pub fill: Option<u32>,
    pub text: Option<u32>,
    pub bold: bool,
    pub border: Option<u32>,
}

/// A table style's parts, as Excel's `tableStyleElement` types.
pub const PART_NAMES: [&str; 7] = ["wholeTable", "headerRow", "totalRow", "firstRowStripe", "firstColumnStripe", "firstColumn", "lastColumn"];
pub const WHOLE: usize = 0;
pub const HEADER: usize = 1;
pub const TOTALS: usize = 2;
pub const ROW_STRIPE: usize = 3;
pub const COL_STRIPE: usize = 4;
pub const FIRST_COL: usize = 5;
pub const LAST_COL: usize = 6;

/// A custom table style: a name and its parts.
#[derive(Debug, Clone, PartialEq)]
pub struct TableStyleDef {
    pub name: Arc<str>,
    pub parts: [Option<TablePart>; 7],
}

/// A shape or text box on a sheet: an OOXML preset outline (`rect`,
/// `roundRect`, `ellipse`, `rightArrow`…), its fill and outline (`None` is
/// none), and its text.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
    pub name: Arc<str>,
    pub anchor: Anchor,
    pub geom: Arc<str>,
    pub fill: Option<u32>,
    pub line: Option<u32>,
    pub text: Arc<str>,
    pub text_color: Option<u32>,
    /// Text size in twentieths of a point (220 is 11 pt).
    pub text_size: u16,
    pub bold: bool,
    pub alt: Option<Arc<str>>,
}

impl Shape {
    /// Whether it is a text box: a rectangle with no fill or outline.
    pub fn is_text_box(&self) -> bool {
        &*self.geom == "rect" && self.fill.is_none() && self.line.is_none()
    }
}

/// A Camera picture: a live picture of `source`, drawn where `anchor` says.
#[derive(Debug, Clone, PartialEq)]
pub struct Camera {
    pub name: Arc<str>,
    pub anchor: Anchor,
    pub source: Rect,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Look {
    pub theme: Theme,
    pub table_styles: Vec<TableStyleDef>,
    pub shapes: Vec<Shape>,
    pub cameras: Vec<Camera>,
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_NAME && !s.chars().any(char::is_control)
}

fn rgb_ok(c: Option<u32>) -> bool {
    c.is_none_or(|c| c <= 0xFF_FFFF)
}

fn anchor_ok(a: &Anchor, sheets: usize) -> bool {
    (a.sheet as usize) < sheets && a.from.0 <= a.to.0 && a.from.1 <= a.to.1 && a.to.0 < MAX_ROWS && a.to.1 < MAX_COLS
        && [a.from, a.to].iter().all(|&(_, _, x, y)| (0.0..=1e6).contains(&x) && (0.0..=1e6).contains(&y))
}

impl Look {
    /// Whether every part is within bounds for a workbook of `sheets` sheets.
    pub fn valid(&self, sheets: usize) -> Result<(), &'static str> {
        let t = &self.theme;
        if !name_ok(&t.name) || !name_ok(&t.heading) || !name_ok(&t.body) || t.colors.iter().any(|&c| c > 0xFF_FFFF) {
            return Err("an invalid theme");
        }
        if self.table_styles.len() > MAX_TABLE_STYLES || self.shapes.len() > MAX_SHAPES || self.cameras.len() > MAX_CAMERAS {
            return Err("too many table styles, shapes or cameras");
        }
        for (i, s) in self.table_styles.iter().enumerate() {
            if !name_ok(&s.name) || is_builtin_table_style(&s.name) || self.table_styles[..i].iter().any(|o| o.name.eq_ignore_ascii_case(&s.name))
                || s.parts.iter().flatten().any(|p| !rgb_ok(p.fill) || !rgb_ok(p.text) || !rgb_ok(p.border))
            {
                return Err("an invalid table style");
            }
        }
        for s in &self.shapes {
            if !name_ok(&s.name) || officina_shapes::presets::preset(&s.geom).is_none() || !rgb_ok(s.fill) || !rgb_ok(s.line) || !rgb_ok(s.text_color)
                || s.text.len() > MAX_SHAPE_TEXT || !(20..=8000).contains(&s.text_size) || !anchor_ok(&s.anchor, sheets)
                || s.alt.as_ref().is_some_and(|a| a.is_empty() || a.len() > crate::charts::MAX_ALT)
            {
                return Err("an invalid shape");
            }
        }
        for c in &self.cameras {
            let r = c.source;
            if !name_ok(&c.name) || !anchor_ok(&c.anchor, sheets) || r.sheet as usize >= sheets || r.r0 > r.r1 || r.c0 > r.c1
                || r.r1 >= MAX_ROWS || r.c1 >= MAX_COLS || u64::from(r.r1 - r.r0 + 1) * u64::from(r.c1 - r.c0 + 1) > MAX_CAMERA_CELLS
            {
                return Err("an invalid camera picture");
            }
        }
        Ok(())
    }

    pub fn is_default(&self) -> bool {
        *self == Look::default()
    }

    pub fn encode(&self, w: &mut Writer) {
        let t = &self.theme;
        w.str(&t.name);
        for c in t.colors {
            w.u32(c);
        }
        w.str(&t.heading);
        w.str(&t.body);
        let color = |w: &mut Writer, c: Option<u32>| match c {
            Some(c) => {
                w.u8(1);
                w.u32(c);
            }
            None => w.u8(0),
        };
        w.var(self.table_styles.len() as u64);
        for s in &self.table_styles {
            w.str(&s.name);
            for p in &s.parts {
                match p {
                    Some(p) => {
                        w.u8(1 | u8::from(p.bold) << 1);
                        color(w, p.fill);
                        color(w, p.text);
                        color(w, p.border);
                    }
                    None => w.u8(0),
                }
            }
        }
        let anchor = |w: &mut Writer, a: &Anchor| {
            w.u16(a.sheet);
            for (r, c, x, y) in [a.from, a.to] {
                w.var(u64::from(r));
                w.var(u64::from(c));
                w.f64(f64::from(x));
                w.f64(f64::from(y));
            }
        };
        w.var(self.shapes.len() as u64);
        for s in &self.shapes {
            w.str(&s.name);
            anchor(w, &s.anchor);
            w.str(&s.geom);
            color(w, s.fill);
            color(w, s.line);
            w.str(&s.text);
            color(w, s.text_color);
            w.u16(s.text_size);
            w.u8(u8::from(s.bold));
            w.str(s.alt.as_deref().unwrap_or(""));
        }
        w.var(self.cameras.len() as u64);
        for c in &self.cameras {
            w.str(&c.name);
            anchor(w, &c.anchor);
            let r = c.source;
            w.u16(r.sheet);
            for v in [r.r0, r.c0, r.r1, r.c1] {
                w.var(u64::from(v));
            }
        }
    }

    /// Reads a look; [`Look::valid`] is the caller's to check.
    pub fn decode(r: &mut Reader) -> Result<Look, WireError> {
        let name = |r: &mut Reader, what: &'static str| -> Result<Arc<str>, WireError> { Ok(Arc::from(r.str(what, MAX_NAME as u64)?)) };
        let theme_name = name(r, "theme name")?;
        let mut colors = [0u32; 12];
        for c in &mut colors {
            *c = r.u32()?;
        }
        let theme = Theme { name: theme_name, colors, heading: name(r, "theme font")?, body: name(r, "theme font")? };
        let color = |r: &mut Reader| -> Result<Option<u32>, WireError> {
            match r.u8()? {
                0 => Ok(None),
                1 => Ok(Some(r.u32()?)),
                _ => Err(WireError::Invalid("look colour")),
            }
        };
        let n = r.count("table styles", MAX_TABLE_STYLES as u64, 8)?;
        let mut table_styles = Vec::with_capacity(n);
        for _ in 0..n {
            let style_name = name(r, "table style name")?;
            let mut parts = [None; 7];
            for p in &mut parts {
                let flags = r.u8()?;
                if flags > 3 {
                    return Err(WireError::Invalid("table style part"));
                }
                if flags & 1 != 0 {
                    *p = Some(TablePart { bold: flags & 2 != 0, fill: color(r)?, text: color(r)?, border: color(r)? });
                }
            }
            table_styles.push(TableStyleDef { name: style_name, parts });
        }
        let anchor = |r: &mut Reader| -> Result<Anchor, WireError> {
            let sheet = r.u16()?;
            let mut corner = || -> Result<(u32, u32, f32, f32), WireError> {
                let (row, col) = (r.var()?, r.var()?);
                let (x, y) = (r.f64()?, r.f64()?);
                if row >= u64::from(MAX_ROWS) || col >= u64::from(MAX_COLS) || !((0.0..=1e6).contains(&x) && (0.0..=1e6).contains(&y)) {
                    return Err(WireError::Invalid("object place"));
                }
                Ok((row as u32, col as u32, x as f32, y as f32))
            };
            Ok(Anchor { sheet, from: corner()?, to: corner()? })
        };
        let n = r.count("shapes", MAX_SHAPES as u64, 16)?;
        let mut shapes = Vec::with_capacity(n);
        for _ in 0..n {
            let shape_name = name(r, "shape name")?;
            let anchor = anchor(r)?;
            let geom = name(r, "shape outline")?;
            let (fill, line) = (color(r)?, color(r)?);
            let text = Arc::from(r.str("shape text", MAX_SHAPE_TEXT as u64)?);
            let text_color = color(r)?;
            let text_size = r.u16()?;
            let bold = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("shape text")),
            };
            let alt = r.str("alt text", crate::charts::MAX_ALT as u64)?;
            shapes.push(Shape { name: shape_name, anchor, geom, fill, line, text, text_color, text_size, bold, alt: (!alt.is_empty()).then(|| Arc::from(alt)) });
        }
        let n = r.count("cameras", MAX_CAMERAS as u64, 16)?;
        let mut cameras = Vec::with_capacity(n);
        for _ in 0..n {
            let camera_name = name(r, "camera name")?;
            let anchor = anchor(r)?;
            let sheet = r.u16()?;
            let mut v = [0u32; 4];
            for x in &mut v {
                *x = u32::try_from(r.var()?).map_err(|_| WireError::Invalid("camera range"))?;
            }
            cameras.push(Camera { name: camera_name, anchor, source: Rect { sheet, r0: v[0], c0: v[1], r1: v[2], c1: v[3] } });
        }
        Ok(Look { theme, table_styles, shapes, cameras })
    }
}

/// Mixes `c` toward white by `t` (0 none, 1 white), or toward black when
/// `t` is negative.
pub fn tint(c: u32, t: f64) -> u32 {
    let ch = |shift: u32| {
        let v = f64::from((c >> shift) & 0xFF);
        let out = if t >= 0.0 { v + (255.0 - v) * t } else { v * (1.0 + t) };
        (out.round().clamp(0.0, 255.0) as u32) << shift
    };
    ch(16) | ch(8) | ch(0)
}

/// Whether `name` is one of Excel's built-in table styles (`TableStyleLight1`
/// to 21, `TableStyleMedium1` to 28, `TableStyleDark1` to 11).
pub fn is_builtin_table_style(name: &str) -> bool {
    builtin_table_style(name).is_some()
}

fn builtin_table_style(name: &str) -> Option<(u8, u32)> {
    let rest = name.strip_prefix("TableStyle")?;
    for (family, prefix, max) in [(0, "Light", 21), (1, "Medium", 28), (2, "Dark", 11)] {
        if let Some(n) = rest.strip_prefix(prefix).and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=max).contains(n)) {
            return Some((family, n));
        }
    }
    None
}

/// The built-in table style names, as the gallery lists them.
pub fn builtin_table_styles() -> Vec<String> {
    (1..=21).map(|n| format!("TableStyleLight{n}"))
        .chain((1..=28).map(|n| format!("TableStyleMedium{n}")))
        .chain((1..=11).map(|n| format!("TableStyleDark{n}")))
        .collect()
}

/// A built-in style's parts, drawn from `theme` as Excel's are: within each
/// family of seven, the first uses dark 1 and the rest accents 1 to 6.
fn builtin_parts(family: u8, n: u32, theme: &Theme) -> [Option<TablePart>; 7] {
    let (dark, light) = (theme.colors[0], theme.colors[1]);
    let k = ((n - 1) % 7) as usize;
    let accent = if k == 0 { dark } else { theme.colors[3 + k] };
    let variant = (n - 1) / 7;
    let part = |fill: Option<u32>, text: Option<u32>, bold: bool, border: Option<u32>| Some(TablePart { fill, text, bold, border });
    let mut p: [Option<TablePart>; 7] = [None; 7];
    match (family, variant) {
        // Light 1–7: stripes, a line under the header and over the total.
        (0, 0) => {
            p[WHOLE] = part(None, None, false, Some(accent));
            p[HEADER] = part(None, None, true, Some(accent));
            p[TOTALS] = part(None, None, true, Some(accent));
            p[ROW_STRIPE] = part(Some(tint(accent, 0.8)), None, false, None);
            p[COL_STRIPE] = part(Some(tint(accent, 0.8)), None, false, None);
        }
        // Light 8–14: a filled header.
        (0, 1) => {
            p[WHOLE] = part(None, None, false, Some(accent));
            p[HEADER] = part(Some(accent), Some(light), true, None);
            p[TOTALS] = part(None, None, true, Some(accent));
            p[ROW_STRIPE] = part(None, None, false, Some(accent));
        }
        // Light 15–21: gridded.
        (0, _) => {
            p[WHOLE] = part(None, None, false, Some(accent));
            p[HEADER] = part(None, None, true, Some(accent));
            p[TOTALS] = part(None, None, true, Some(accent));
            p[ROW_STRIPE] = part(Some(tint(accent, 0.8)), None, false, None);
        }
        // Medium 1–7: light stripes, a dark header.
        (1, 0) => {
            p[WHOLE] = part(None, None, false, Some(tint(accent, 0.4)));
            p[HEADER] = part(Some(accent), Some(light), true, None);
            p[TOTALS] = part(None, None, true, Some(accent));
            p[ROW_STRIPE] = part(Some(tint(accent, 0.8)), None, false, None);
            p[COL_STRIPE] = part(Some(tint(accent, 0.8)), None, false, None);
        }
        // Medium 8–14: a tinted body.
        (1, 1) => {
            p[WHOLE] = part(Some(tint(accent, 0.8)), None, false, Some(light));
            p[HEADER] = part(Some(accent), Some(light), true, None);
            p[TOTALS] = part(Some(accent), Some(light), true, None);
            p[ROW_STRIPE] = part(Some(tint(accent, 0.6)), None, false, None);
            p[FIRST_COL] = part(Some(accent), Some(light), true, None);
            p[LAST_COL] = part(Some(accent), Some(light), true, None);
        }
        // Medium 15–21: a dark header on a plain body.
        (1, 2) => {
            p[WHOLE] = part(None, None, false, Some(dark));
            p[HEADER] = part(Some(accent), Some(light), true, Some(dark));
            p[TOTALS] = part(None, None, true, Some(dark));
            p[ROW_STRIPE] = part(Some(tint(dark, 0.85)), None, false, None);
        }
        // Medium 22–28: tinted body, accent stripes.
        (1, _) => {
            p[WHOLE] = part(Some(tint(accent, 0.8)), None, false, Some(tint(accent, 0.4)));
            p[HEADER] = part(None, None, true, Some(accent));
            p[TOTALS] = part(None, None, true, Some(accent));
            p[ROW_STRIPE] = part(Some(tint(accent, 0.6)), None, false, None);
        }
        // Dark 1–7: a dark body.
        (2, 0) => {
            let deep = tint(accent, -0.25);
            p[WHOLE] = part(Some(tint(accent, -0.1)), Some(light), false, None);
            p[HEADER] = part(Some(dark), Some(light), true, Some(light));
            p[TOTALS] = part(Some(deep), Some(light), true, Some(light));
            p[ROW_STRIPE] = part(Some(deep), Some(light), false, None);
            p[FIRST_COL] = part(Some(deep), Some(light), true, None);
            p[LAST_COL] = part(Some(deep), Some(light), true, None);
        }
        // Dark 8–11: a light body under a dark header.
        _ => {
            p[WHOLE] = part(Some(tint(dark, 0.85)), None, false, None);
            p[HEADER] = part(Some(dark), Some(light), true, None);
            p[TOTALS] = part(Some(tint(dark, 0.7)), None, true, Some(dark));
            p[ROW_STRIPE] = part(Some(tint(dark, 0.7)), None, false, None);
        }
    }
    p
}

impl Workbook {
    pub fn workbook_look(&self) -> &Look {
        &self.look
    }

    pub fn theme(&self) -> &Theme {
        &self.look.theme
    }

    /// The parts of the table style named `name`: a custom one, else a
    /// built-in drawn from the theme; `None` for a name neither knows.
    pub fn table_style_parts(&self, name: &str) -> Option<[Option<TablePart>; 7]> {
        if let Some(s) = self.look.table_styles.iter().find(|s| s.name.eq_ignore_ascii_case(name)) {
            return Some(s.parts);
        }
        let (family, n) = builtin_table_style(name)?;
        Some(builtin_parts(family, n, &self.look.theme))
    }

    /// How cell (`row`, `col`) of table `t` looks by its style and options:
    /// the parts that apply, later ones over earlier (whole table, column
    /// stripes, row stripes, first and last column, header, totals row).
    pub fn table_cell_look(&self, t: &Table, row: u32, col: u32) -> Option<TablePart> {
        let parts = self.table_style_parts(t.style.as_deref()?)?;
        let r = t.rect;
        if row < r.r0 || row > r.r1 || col < r.c0 || col > r.c1 {
            return None;
        }
        let header = t.header && row == r.r0;
        let totals = t.totals && row == r.r1;
        let body0 = r.r0 + u32::from(t.header);
        let mut on: Vec<usize> = vec![WHOLE];
        if !header && !totals {
            if t.bands.cols && (col - r.c0) % 2 == 0 {
                on.push(COL_STRIPE);
            }
            if t.stripes && row >= body0 && (row - body0) % 2 == 0 {
                on.push(ROW_STRIPE);
            }
        }
        if t.bands.first && col == r.c0 {
            on.push(FIRST_COL);
        }
        if t.bands.last && col == r.c1 {
            on.push(LAST_COL);
        }
        if header {
            on.push(HEADER);
        }
        if totals {
            on.push(TOTALS);
        }
        let mut out = TablePart::default();
        let mut any = false;
        for k in on {
            let Some(p) = parts[k] else { continue };
            any = true;
            out.fill = p.fill.or(out.fill);
            out.text = p.text.or(out.text);
            out.bold |= p.bold;
            // The whole table's border frames it; others mark their own edge.
            if k != WHOLE || row == r.r0 || row == r.r1 || col == r.c0 || col == r.c1 {
                out.border = p.border.or(out.border);
            }
        }
        any.then_some(out)
    }

    /// Changing the theme to `new`: cell, border and tab colours that were the
    /// old theme's become the new theme's, cells in the old fonts take the new
    /// ones, and the theme is set. One op, so one undo.
    pub fn theme_op(&self, new: Theme) -> GridOp {
        let old = self.look.theme.clone();
        let map_color = |c: Option<u32>| c.map(|c| old.colors.iter().position(|&o| o == c).map_or(c, |i| new.colors[i]));
        let map_font = |n: &Option<Arc<str>>| match n {
            Some(n) if **n == *old.heading => Some(new.heading.clone()),
            Some(n) if **n == *old.body => Some(new.body.clone()),
            n => n.clone(),
        };
        let restyle = |st: &Style| {
            let mut s = st.clone();
            s.font.color = map_color(s.font.color);
            s.font.name = map_font(&s.font.name);
            s.fill = map_color(s.fill);
            if let Some(p) = &mut s.pattern {
                p.foreground = map_color(p.foreground);
                p.background = map_color(p.background);
            }
            for e in s.borders.iter_mut().chain(s.diagonals.iter_mut()) {
                e.color = map_color(e.color);
            }
            s
        };
        let mut ops = Vec::new();
        for sheet in 0..self.sheet_count() as u16 {
            let sh = self.sheet(sheet);
            let mut spans = Vec::new();
            for (c, runs) in sh.styles.iter().enumerate() {
                for (a, b, id) in runs.spans(0, MAX_ROWS - 1) {
                    if id == 0 {
                        continue;
                    }
                    let st = self.styles.get(id);
                    if restyle(st) != *st {
                        spans.push((c as u32, a, b));
                    }
                }
            }
            if let Some((r0, c0, r1, c1)) = spans.iter().fold(None, |acc: Option<(u32, u32, u32, u32)>, &(c, a, b)| {
                Some(acc.map_or((a, c, b, c), |(r0, c0, r1, c1)| (r0.min(a), c0.min(c), r1.max(b), c1.max(c))))
            }) {
                ops.push(GridOp::restyle(self, Rect { sheet, r0, c0, r1, c1 }, restyle));
            }
            if let Some(tab) = sh.tab_color {
                let next = map_color(Some(tab));
                if next != Some(tab) {
                    ops.push(GridOp::TabColor { sheet, color: next });
                }
            }
        }
        ops.push(GridOp::Look(Box::new(Look { theme: new, ..self.look.clone() })));
        GridOp::Batch(ops)
    }

    /// The op that sets the look to `look` (shapes, cameras, table styles).
    pub fn look_op(&self, look: Look) -> GridOp {
        GridOp::Look(Box::new(look))
    }
}

/// Where a camera's source reads, for redrawing: its cells.
pub fn camera_cells(c: &Camera) -> impl Iterator<Item = CellAddr> + '_ {
    let r = c.source;
    (r.r0..=r.r1).flat_map(move |row| (r.c0..=r.c1).map(move |col| CellAddr::new(r.sheet, row, col)))
}
