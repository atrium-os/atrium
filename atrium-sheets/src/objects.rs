//! Look and objects (docs/spec/officina.md §16.165), each a typed bar as
//! Excel's ribbon groups: Themes… (Page Layout ▸ Themes, Colours, Fonts),
//! Table Design… (the table style gallery, its options, custom styles),
//! Insert Shape… (shapes and text boxes), Camera… (a live picture of a
//! range) and Quick Analysis… (formatting, charts, totals, tables and
//! sparklines for the selection). Each line is one undo step.

use std::sync::Arc;

use officina_grid::addr::{col_name, parse_a1, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::charts::{Anchor, ChartKind};
use officina_grid::condfmt::{Cfvo, CondFormat, Dxf, Rule, RuleKind};
use officina_grid::look::{builtin_table_styles, builtin_theme, builtin_themes, is_builtin_table_style, Camera, Look, Shape, TablePart, TableStyleDef, Theme, MAX_CAMERAS, MAX_SHAPES, MAX_SHAPE_TEXT, MAX_TABLE_STYLES};
use officina_grid::ops::GridOp;
use officina_grid::Workbook;
use pergola::widgets::CellRange;

use crate::doc::Doc;

/// What a bar may be given, in bytes.
const MAX_TYPED: usize = 4096;

fn too_long(typed: &str) -> Result<(), String> {
    if typed.len() > MAX_TYPED {
        return Err("That's too long.".into());
    }
    Ok(())
}

fn cell_text(r: u32, c: u32) -> String {
    format!("{}{}", col_name(c), r + 1)
}

fn rect_text(r: &Rect) -> String {
    if r.r0 == r.r1 && r.c0 == r.c1 {
        cell_text(r.r0, r.c0)
    } else {
        format!("{}:{}", cell_text(r.r0, r.c0), cell_text(r.r1, r.c1))
    }
}

fn parse_cell(t: &str) -> Result<(u32, u32), String> {
    parse_a1(&t.trim().replace('$', "").to_ascii_uppercase()).ok_or_else(|| format!("“{}” isn't a cell.", t.trim()))
}

fn parse_range(t: &str, sheet: u16) -> Result<Rect, String> {
    let t = t.trim().replace('$', "").to_ascii_uppercase();
    let (a, b) = t.split_once(':').unwrap_or((&t, &t));
    let (a, b) = (parse_cell(a)?, parse_cell(b)?);
    Ok(Rect { sheet, r0: a.0.min(b.0), c0: a.1.min(b.1), r1: a.0.max(b.0), c1: a.1.max(b.1) })
}

/// A colour: `none`, a theme colour (`accent1`…`accent6`, `dark1`, `light1`,
/// `dark2`, `light2`), a name (white, black, red…) or six hex digits.
fn parse_color(wb: &Workbook, t: &str) -> Result<Option<u32>, String> {
    let t = t.trim().to_ascii_lowercase();
    let theme = &wb.theme().colors;
    let named = match t.as_str() {
        "none" | "no" => return Ok(None),
        "dark1" | "dark 1" => theme[0],
        "light1" | "light 1" => theme[1],
        "dark2" | "dark 2" => theme[2],
        "light2" | "light 2" => theme[3],
        "white" => 0xFFFFFF,
        "black" => 0x000000,
        "red" => 0xC00000,
        "orange" => 0xED7D31,
        "yellow" => 0xFFC000,
        "green" => 0x70AD47,
        "blue" => 0x4472C4,
        "purple" => 0x7030A0,
        "gray" | "grey" => 0x808080,
        _ => {
            if let Some(n) = t.strip_prefix("accent").and_then(|n| n.trim().parse::<usize>().ok()).filter(|n| (1..=6).contains(n)) {
                theme[3 + n]
            } else {
                let h = t.trim_start_matches('#');
                match (h.len(), u32::from_str_radix(h, 16)) {
                    (6, Ok(v)) => v,
                    _ => return Err(format!("“{t}” isn't a colour: none, accent1 to accent6, a name such as blue, or six hex digits.")),
                }
            }
        }
    };
    Ok(Some(named))
}

fn color_text(wb: &Workbook, c: Option<u32>) -> String {
    match c {
        None => "none".into(),
        Some(c) => match wb.theme().colors[4..10].iter().position(|&a| a == c) {
            Some(i) => format!("accent{}", i + 1),
            None => format!("{c:06X}"),
        },
    }
}

/// Words, with a "quoted text" kept whole (its quotes gone).
fn words(typed: &str) -> Result<Vec<(String, bool)>, String> {
    let mut out = Vec::new();
    let mut rest = typed.trim();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('"').or_else(|| rest.strip_prefix('“')) {
            let end = r.find(['"', '”']).ok_or("A quoted text has no closing quote.")?;
            out.push((r[..end].to_string(), true));
            rest = r[end + r[end..].chars().next().map_or(1, char::len_utf8)..].trim_start();
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push((rest[..end].to_string(), false));
            rest = rest[end..].trim_start();
        }
    }
    Ok(out)
}

fn apply_look(d: &mut Doc, label: &str, look: Look) -> Result<(), String> {
    look.valid(d.wb.sheet_count()).map_err(|e| format!("Couldn't: {e}."))?;
    if !d.run_action(label, vec![GridOp::Look(Box::new(look))]) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Themes…

pub fn themes_start(d: &Doc) -> String {
    format!("theme {}", d.wb.theme().name)
}

/// `theme NAME`, `colors NAME`, `fonts NAME` or `fonts HEADING / BODY`.
pub fn themes_run(d: &mut Doc, typed: &str) -> Result<String, String> {
    too_long(typed)?;
    let t = typed.trim();
    let low = t.to_ascii_lowercase();
    let names = || builtin_themes().iter().map(|t| t.name.to_string()).collect::<Vec<_>>().join(", ");
    let find = |n: &str| builtin_theme(n).ok_or_else(|| format!("No theme “{}”: {}.", n.trim(), names()));
    let now = d.wb.theme().clone();
    let (new, said) = if let Some(n) = low.strip_prefix("theme ").map(|_| &t[6..]) {
        let th = find(n)?;
        let said = format!("Theme {}.", th.name);
        (th, said)
    } else if let Some(n) = low.strip_prefix("colors ").or_else(|| low.strip_prefix("colours ")).map(|r| &t[t.len() - r.len()..]) {
        let th = find(n)?;
        let said = format!("Colours of {}.", th.name);
        (Theme { colors: th.colors, name: format!("{} ({} colours)", now.name.split(" (").next().unwrap_or(&now.name), th.name).chars().take(255).collect::<String>().into(), ..now.clone() }, said)
    } else if let Some(n) = low.strip_prefix("fonts ").map(|r| &t[t.len() - r.len()..]) {
        let (heading, body) = match n.split_once('/') {
            Some((h, b)) => (h.trim().to_string(), b.trim().to_string()),
            None => {
                let th = find(n)?;
                (th.heading.to_string(), th.body.to_string())
            }
        };
        if heading.is_empty() || body.is_empty() || heading.len() > 255 || body.len() > 255 {
            return Err("Type fonts HEADING / BODY, or a theme's name.".into());
        }
        let said = format!("Fonts {heading} and {body}.");
        (Theme { heading: heading.into(), body: body.into(), ..now.clone() }, said)
    } else if low == "list" {
        return Ok(format!("Themes: {}.", names()));
    } else {
        return Err(format!("Type theme NAME, colors NAME or fonts NAME (or fonts HEADING / BODY). Themes: {}.", names()));
    };
    if new == now {
        return Ok("Nothing to change.".into());
    }
    let op = d.wb.theme_op(new);
    if !d.run_action("Theme", vec![op]) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok(said)
}

// ---------------------------------------------------------------------------
// Table Design…

fn table_here(d: &Doc, cursor: (u32, u32)) -> Option<usize> {
    d.wb.tables().iter().position(|t| t.rect.sheet == d.sheet && (t.rect.r0..=t.rect.r1).contains(&cursor.0) && (t.rect.c0..=t.rect.c1).contains(&cursor.1))
}

pub fn can_design(d: &Doc, cursor: (u32, u32)) -> bool {
    table_here(d, cursor).is_some()
}

/// A style name as typed: `Medium 9`, `medium9`, `TableStyleMedium9`, a
/// custom style's name, or `none`.
fn style_name(wb: &Workbook, t: &str) -> Result<Option<Arc<str>>, String> {
    let t = t.trim();
    if t.eq_ignore_ascii_case("none") {
        return Ok(None);
    }
    if let Some(s) = wb.workbook_look().table_styles.iter().find(|s| s.name.eq_ignore_ascii_case(t)) {
        return Ok(Some(s.name.clone()));
    }
    let squeezed: String = t.split_whitespace().collect();
    let low = squeezed.to_ascii_lowercase();
    let low = low.strip_prefix("tablestyle").unwrap_or(&low);
    for family in ["Light", "Medium", "Dark"] {
        if let Some(n) = low.strip_prefix(&family.to_ascii_lowercase()) {
            let name = format!("TableStyle{family}{n}");
            if is_builtin_table_style(&name) {
                return Ok(Some(name.into()));
            }
        }
    }
    Err(format!("No table style “{t}”: Light 1–21, Medium 1–28, Dark 1–11, a custom style, or none."))
}

fn short_style(name: &str) -> String {
    for family in ["Light", "Medium", "Dark"] {
        if let Some(n) = name.strip_prefix("TableStyle").and_then(|r| r.strip_prefix(family)) {
            return format!("{family} {n}");
        }
    }
    name.to_string()
}

pub fn design_start(d: &Doc, cursor: (u32, u32)) -> String {
    let Some(i) = table_here(d, cursor) else { return String::new() };
    let t = &d.wb.tables()[i];
    let on = |b: bool, what: &str| format!("{}{what}", if b { "" } else { "no " });
    format!("style {}; {}; {}; {}; {}", t.style.as_deref().map_or("none".into(), short_style), on(t.stripes, "banded rows"), on(t.bands.cols, "banded columns"), on(t.bands.first, "first column"), on(t.bands.last, "last column"))
}

/// One custom style from `NAME with PART SETTINGS, PART SETTINGS…`: parts
/// whole, header, totals, stripe (rows), column stripe, first column, last
/// column; settings a fill colour, `text COLOUR`, `bold`, `border COLOUR`.
fn custom_style(wb: &Workbook, spec: &str) -> Result<TableStyleDef, String> {
    let low = spec.to_ascii_lowercase();
    let k = low.find(" with ").ok_or("Type new style NAME with header 1F4E78 text white bold, stripe DDEBF7, ….")?;
    let name = spec[..k].trim();
    if name.is_empty() || name.len() > 255 || is_builtin_table_style(name) {
        return Err("Give the style a name of its own.".into());
    }
    let mut parts: [Option<TablePart>; 7] = [None; 7];
    for item in spec[k + 6..].split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let il = item.to_ascii_lowercase();
        let (part, rest) = [("column stripe", 4), ("row stripe", 3), ("first column", 5), ("last column", 6), ("whole", 0), ("table", 0), ("header", 1), ("totals", 2), ("total", 2), ("stripe", 3)]
            .iter()
            .find(|(p, _)| il.starts_with(p))
            .map(|(p, i)| (*i, &item[p.len()..]))
            .ok_or_else(|| format!("“{item}”: start with whole, header, totals, stripe, column stripe, first column or last column."))?;
        let mut tp = TablePart::default();
        let w: Vec<&str> = rest.split_whitespace().collect();
        let mut j = 0;
        while j < w.len() {
            match w[j].to_ascii_lowercase().as_str() {
                "bold" => tp.bold = true,
                "text" | "border" | "fill" => {
                    let c = parse_color(wb, w.get(j + 1).ok_or_else(|| format!("A colour after {}.", w[j]))?)?;
                    match w[j].to_ascii_lowercase().as_str() {
                        "text" => tp.text = c,
                        "border" => tp.border = c,
                        _ => tp.fill = c,
                    }
                    j += 1;
                }
                other => tp.fill = parse_color(wb, other)?,
            }
            j += 1;
        }
        parts[part] = Some(tp);
    }
    if parts.iter().all(Option::is_none) {
        return Err("Say how at least one part looks.".into());
    }
    Ok(TableStyleDef { name: name.into(), parts })
}

/// Table Design: `style NAME`, `banded rows`, `banded columns`, `first
/// column`, `last column` (each with `no` to turn off), `new style …` and
/// `delete style NAME`, split by `;`.
pub fn design_run(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    too_long(typed)?;
    let i = table_here(d, cursor).ok_or("Put the cursor in a table.")?;
    let mut tables = d.wb.tables().to_vec();
    let mut look = d.wb.workbook_look().clone();
    let mut said = Vec::new();
    for part in typed.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let low = part.to_ascii_lowercase();
        if let Some(rest) = low.strip_prefix("new style ").map(|r| &part[part.len() - r.len()..]) {
            let def = custom_style(&d.wb, rest)?;
            look.table_styles.retain(|s| !s.name.eq_ignore_ascii_case(&def.name));
            if look.table_styles.len() >= MAX_TABLE_STYLES {
                return Err(format!("A workbook has at most {MAX_TABLE_STYLES} custom table styles."));
            }
            said.push(format!("style {} made", def.name));
            look.table_styles.push(def);
        } else if let Some(rest) = low.strip_prefix("delete style ").map(|r| &part[part.len() - r.len()..]) {
            let n = look.table_styles.len();
            look.table_styles.retain(|s| !s.name.eq_ignore_ascii_case(rest.trim()));
            if look.table_styles.len() == n {
                return Err(format!("No custom style “{}”.", rest.trim()));
            }
            for t in &mut tables {
                if t.style.as_deref().is_some_and(|s| s.eq_ignore_ascii_case(rest.trim())) {
                    t.style = Some("TableStyleMedium2".into());
                }
            }
            said.push(format!("style {} deleted", rest.trim()));
        } else if let Some(rest) = low.strip_prefix("style ").map(|r| &part[part.len() - r.len()..]) {
            // A style made in this same line counts.
            let probe = {
                let mut wb = Workbook::new();
                let _ = wb.add_sheet("x");
                officina_core::history::Op::apply(GridOp::Look(Box::new(Look { shapes: Vec::new(), cameras: Vec::new(), ..look.clone() })), &mut wb);
                wb
            };
            let name = style_name(&probe, rest)?;
            said.push(format!("style {}", name.as_deref().map_or("none".into(), short_style)));
            tables[i].style = name;
        } else {
            let (off, what) = match low.strip_prefix("no ") {
                Some(w) => (true, w.trim()),
                None => (false, low.as_str()),
            };
            let t = &mut tables[i];
            match what {
                "banded rows" => t.stripes = !off,
                "banded columns" => t.bands.cols = !off,
                "first column" => t.bands.first = !off,
                "last column" => t.bands.last = !off,
                _ => return Err(format!("“{part}”: type style NAME, banded rows, banded columns, first column, last column (no … to turn off), new style NAME with …, or delete style NAME.")),
            }
            said.push(part.to_string());
        }
    }
    if said.is_empty() {
        return Ok("Nothing to change.".into());
    }
    let mut ops = Vec::new();
    if look != *d.wb.workbook_look() {
        look.valid(d.wb.sheet_count()).map_err(|e| format!("Couldn't: {e}."))?;
        ops.push(GridOp::Look(Box::new(look)));
    }
    if tables != d.wb.tables() {
        ops.push(GridOp::Tables(tables));
    }
    if !d.run_action("Table Design", ops) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok(format!("{}: {}.", d.wb.tables()[i].name, said.join(", ")))
}

/// The gallery's names, for the bar's hint.
pub fn gallery(wb: &Workbook) -> Vec<String> {
    builtin_table_styles().iter().map(|n| short_style(n)).chain(wb.workbook_look().table_styles.iter().map(|s| s.name.to_string())).collect()
}

// ---------------------------------------------------------------------------
// Insert Shape…

/// Friendly names for presets; any preset's own name works too.
const SHAPES: [(&str, &str); 22] = [
    ("rectangle", "rect"), ("rect", "rect"), ("rounded", "roundRect"), ("rounded rectangle", "roundRect"), ("oval", "ellipse"), ("circle", "ellipse"),
    ("ellipse", "ellipse"), ("triangle", "triangle"), ("right arrow", "rightArrow"), ("left arrow", "leftArrow"), ("up arrow", "upArrow"),
    ("down arrow", "downArrow"), ("arrow", "rightArrow"), ("star", "star5"), ("diamond", "diamond"), ("hexagon", "hexagon"), ("pentagon", "pentagon"),
    ("heart", "heart"), ("cloud", "cloud"), ("callout", "wedgeRectCallout"), ("smiley", "smileyFace"), ("plus", "mathPlus"),
];

fn shape_here(d: &Doc, cursor: (u32, u32)) -> Option<usize> {
    d.wb.workbook_look().shapes.iter().rposition(|s| s.anchor.sheet == d.sheet && (s.anchor.from.0..=s.anchor.to.0).contains(&cursor.0) && (s.anchor.from.1..=s.anchor.to.1).contains(&cursor.1))
}

pub fn shape_start(d: &Doc, cursor: (u32, u32)) -> String {
    match shape_here(d, cursor) {
        Some(i) => {
            let s = &d.wb.workbook_look().shapes[i];
            let kind = if s.is_text_box() { "text box".to_string() } else { SHAPES.iter().find(|(_, p)| *p == &*s.geom).map_or(s.geom.to_string(), |(n, _)| n.to_string()) };
            let a = &s.anchor;
            format!("{kind} \"{}\" at {} size {}x{} fill {} line {}", s.text, cell_text(a.from.0, a.from.1), a.to.0 - a.from.0, a.to.1 - a.from.1, color_text(&d.wb, s.fill), color_text(&d.wb, s.line))
        }
        None => format!("rounded \"\" at {}", cell_text(cursor.0, cursor.1)),
    }
}

/// Insert Shape: `KIND "TEXT" at CELL size ROWSxCOLS fill C line C text C
/// bold size PT`; `text box "TEXT" …`. On a shape: the same changes it, and
/// `delete`, `front`, `back`.
pub fn shape_run(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    too_long(typed)?;
    let mut look = d.wb.workbook_look().clone();
    let here = shape_here(d, cursor);
    let low = typed.trim().to_ascii_lowercase();
    if let Some(i) = here {
        let name = look.shapes[i].name.clone();
        match low.as_str() {
            "delete" => {
                look.shapes.remove(i);
                apply_look(d, "Delete Shape", look)?;
                return Ok(format!("{name} deleted."));
            }
            "front" | "bring to front" => {
                let s = look.shapes.remove(i);
                look.shapes.push(s);
                apply_look(d, "Bring to Front", look)?;
                return Ok(format!("{name} in front."));
            }
            "back" | "send to back" => {
                let s = look.shapes.remove(i);
                look.shapes.insert(0, s);
                apply_look(d, "Send to Back", look)?;
                return Ok(format!("{name} at the back."));
            }
            _ => {}
        }
    }
    let w = words(typed)?;
    // The kind: the words before the first quoted text or setting.
    let setting = |s: &str| matches!(s, "at" | "size" | "fill" | "line" | "text" | "bold" | "pt");
    let lw = |i: usize| w.get(i).filter(|(_, q)| !*q).map(|(t, _)| t.to_ascii_lowercase());
    let skip = if lw(0).as_deref() == Some("text") && lw(1).as_deref() == Some("box") { 2 } else { 0 };
    let k = skip + w[skip..].iter().position(|(t, q)| *q || setting(&t.to_ascii_lowercase())).unwrap_or(w.len() - skip);
    let kind = w[..k].iter().map(|(t, _)| t.to_ascii_lowercase()).collect::<Vec<_>>().join(" ");
    let mut s = match here {
        Some(i) => look.shapes[i].clone(),
        None => {
            let n = (1..).find(|n| !look.shapes.iter().any(|s| *s.name == *format!("Shape {n}"))).unwrap_or(1);
            Shape { name: format!("Shape {n}").into(), anchor: Anchor { sheet: d.sheet, from: (cursor.0, cursor.1, 0.0, 0.0), to: (cursor.0 + 4, cursor.1 + 3, 0.0, 0.0) },
                geom: "roundRect".into(), fill: Some(d.wb.theme().colors[4]), line: Some(officina_grid::look::tint(d.wb.theme().colors[4], -0.25)), text: "".into(),
                text_color: Some(0xFFFFFF), text_size: 220, bold: false, alt: None }
        }
    };
    if !kind.is_empty() {
        if kind == "text box" || kind == "textbox" {
            s.geom = "rect".into();
            s.fill = None;
            s.line = None;
            s.text_color = None;
        } else if let Some((_, p)) = SHAPES.iter().find(|(n, _)| *n == kind) {
            s.geom = (*p).into();
        } else if let Some(p) = officina_shapes::presets::names().find(|p| p.eq_ignore_ascii_case(&kind.replace(' ', ""))) {
            s.geom = p.into();
        } else {
            return Err(format!("No shape “{kind}”: rectangle, rounded, oval, triangle, arrow, star, diamond, hexagon, heart, cloud, callout, text box, or a preset's name such as flowChartProcess."));
        }
    }
    let (rows, cols) = (s.anchor.to.0 - s.anchor.from.0, s.anchor.to.1 - s.anchor.from.1);
    let mut size = (rows.max(1), cols.max(1));
    let mut at = (s.anchor.from.0, s.anchor.from.1);
    let mut j = k;
    while j < w.len() {
        let (t, quoted) = &w[j];
        let next = || w.get(j + 1).map(|x| x.0.as_str()).ok_or_else(|| format!("Something after {t}."));
        if *quoted {
            if t.len() > MAX_SHAPE_TEXT {
                return Err("That text is too long.".into());
            }
            s.text = t.replace("\\n", "\n").into();
            j += 1;
            continue;
        }
        match t.to_ascii_lowercase().as_str() {
            "at" => {
                at = parse_cell(next()?)?;
                j += 1;
            }
            "size" => {
                let v = next()?.to_ascii_lowercase();
                let (r, c) = v.split_once('x').ok_or("Type size ROWSxCOLUMNS, such as 4x3.")?;
                let (r, c) = (r.parse::<u32>().ok(), c.parse::<u32>().ok());
                size = match (r, c) {
                    (Some(r), Some(c)) if (1..=1000).contains(&r) && (1..=500).contains(&c) => (r, c),
                    _ => return Err("A size is 1 to 1000 rows by 1 to 500 columns.".into()),
                };
                j += 1;
            }
            "fill" => {
                s.fill = parse_color(&d.wb, next()?)?;
                j += 1;
            }
            "line" => {
                s.line = parse_color(&d.wb, next()?)?;
                j += 1;
            }
            "text" => {
                s.text_color = parse_color(&d.wb, next()?)?;
                j += 1;
            }
            "bold" => s.bold = true,
            "pt" => {
                let pt = next()?.parse::<f64>().ok().filter(|p| (1.0..=400.0).contains(p)).ok_or("A text size is 1 to 400 points.")?;
                s.text_size = (pt * 20.0).round() as u16;
                j += 1;
            }
            other => return Err(format!("“{other}”: type the shape, \"text\", at CELL, size 4x3, fill, line, text COLOUR, bold, pt N.")),
        }
        j += 1;
    }
    let to = (at.0.saturating_add(size.0).min(MAX_ROWS - 1), at.1.saturating_add(size.1).min(MAX_COLS - 1));
    s.anchor = Anchor { sheet: d.sheet, from: (at.0, at.1, 0.0, 0.0), to: (to.0, to.1, 0.0, 0.0) };
    let name = s.name.clone();
    let label = match here {
        Some(i) => {
            if look.shapes[i] == s {
                return Ok("Nothing to change.".into());
            }
            look.shapes[i] = s;
            "Format Shape"
        }
        None => {
            if look.shapes.len() >= MAX_SHAPES {
                return Err(format!("A workbook has at most {MAX_SHAPES} shapes."));
            }
            look.shapes.push(s);
            "Insert Shape"
        }
    };
    apply_look(d, label, look)?;
    Ok(format!("{name} {}.", if here.is_some() { "changed" } else { "added" }))
}

// ---------------------------------------------------------------------------
// Camera…

fn camera_here(d: &Doc, cursor: (u32, u32)) -> Option<usize> {
    d.wb.workbook_look().cameras.iter().rposition(|c| c.anchor.sheet == d.sheet && (c.anchor.from.0..=c.anchor.to.0).contains(&cursor.0) && (c.anchor.from.1..=c.anchor.to.1).contains(&cursor.1))
}

pub fn camera_start(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> String {
    match camera_here(d, cursor) {
        Some(i) => {
            let c = &d.wb.workbook_look().cameras[i];
            let sheet = if c.source.sheet == d.sheet { String::new() } else { format!("{}!", d.wb.sheet_name(c.source.sheet)) };
            format!("source {sheet}{}", rect_text(&c.source))
        }
        None => {
            let r = Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
            format!("{} at {}", rect_text(&r), cell_text(sel.r0, (sel.c1 + 2).min(MAX_COLS - 1)))
        }
    }
}

/// `RANGE at CELL` (`Sheet2!A1:C9` reads another sheet); on a camera,
/// `source RANGE` or `delete`.
pub fn camera_run(d: &mut Doc, sel: CellRange, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    too_long(typed)?;
    let _ = sel;
    let mut look = d.wb.workbook_look().clone();
    let t = typed.trim();
    let low = t.to_ascii_lowercase();
    let range_of = |d: &Doc, text: &str| -> Result<Rect, String> {
        let (sheet, cells) = match text.rsplit_once('!') {
            Some((name, cells)) => {
                let name = name.trim().trim_matches('\'');
                (d.wb.sheet_id(name).ok_or_else(|| format!("No sheet “{name}”."))?, cells)
            }
            None => (d.sheet, text),
        };
        let r = parse_range(cells, sheet)?;
        if u64::from(r.r1 - r.r0 + 1) * u64::from(r.c1 - r.c0 + 1) > officina_grid::look::MAX_CAMERA_CELLS {
            return Err(format!("A camera shows at most {} cells.", officina_grid::look::MAX_CAMERA_CELLS));
        }
        Ok(r)
    };
    if let Some(i) = camera_here(d, cursor) {
        let name = look.cameras[i].name.clone();
        if low == "delete" {
            look.cameras.remove(i);
            apply_look(d, "Delete Picture", look)?;
            return Ok(format!("{name} deleted."));
        }
        let rest = low.strip_prefix("source ").map(|r| &t[t.len() - r.len()..]).ok_or("Type source RANGE, or delete.")?;
        look.cameras[i].source = range_of(d, rest)?;
        apply_look(d, "Camera", look)?;
        return Ok(format!("{name} shows {}.", rest.trim()));
    }
    let k = low.rfind(" at ").ok_or("Type RANGE at CELL, such as A1:C9 at F2.")?;
    let source = range_of(d, &t[..k])?;
    let at = parse_cell(&t[k + 4..])?;
    if look.cameras.len() >= MAX_CAMERAS {
        return Err(format!("A workbook has at most {MAX_CAMERAS} camera pictures."));
    }
    let (rows, cols) = (source.r1 - source.r0 + 1, source.c1 - source.c0 + 1);
    let to = (at.0.saturating_add(rows).min(MAX_ROWS - 1), at.1.saturating_add(cols).min(MAX_COLS - 1));
    let n = (1..).find(|n| !look.cameras.iter().any(|c| *c.name == *format!("Picture {n}"))).unwrap_or(1);
    let name: Arc<str> = format!("Picture {n}").into();
    look.cameras.push(Camera { name: name.clone(), anchor: Anchor { sheet: d.sheet, from: (at.0, at.1, 0.0, 0.0), to: (to.0, to.1, 0.0, 0.0) }, source });
    apply_look(d, "Camera", look)?;
    Ok(format!("{name}: a live picture of {} at {}.", rect_text(&source), cell_text(at.0, at.1)))
}

// ---------------------------------------------------------------------------
// Quick Analysis…

pub fn quick_start() -> String {
    "data bars".into()
}

/// Quick Analysis on the selection: formatting (`data bars [COLOUR]`,
/// `color scale`, `icon set`, `greater than N`, `top N[%]`, `clear format`),
/// `chart KIND`, `totals …` (as Quick Totals), `table`, `sparklines KIND`.
pub fn quick_run(d: &mut Doc, sel: CellRange, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    too_long(typed)?;
    let t = typed.trim();
    let low = t.to_ascii_lowercase();
    let s = d.sheet;
    let rect = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    let add_rule = |d: &mut Doc, kind: RuleKind, dxf: Dxf, what: &str| -> Result<String, String> {
        let mut all = d.wb.cond_formats().to_vec();
        all.push(CondFormat { sheet: s, ranges: vec![rect], rules: vec![Rule { kind, dxf, stop: false }] });
        if !d.run_action("Quick Analysis", vec![GridOp::cond_formats(&d.wb, &all)]) {
            return Err(std::mem::take(&mut d.status));
        }
        Ok(format!("{what} on {}.", rect_text(&rect)))
    };
    let red = Dxf { color: Some(0x9C0006), fill: Some(0xFFC7CE), ..Default::default() };
    if let Some(rest) = low.strip_prefix("data bars") {
        let color = if rest.trim().is_empty() { d.wb.theme().colors[4] } else { parse_color(&d.wb, rest)?.ok_or("Data bars need a colour.")? };
        return add_rule(d, RuleKind::DataBar { min: Cfvo::Min, max: Cfvo::Max, color }, Dxf::default(), "Data bars");
    }
    if low == "color scale" || low == "colour scale" {
        return add_rule(d, RuleKind::ColorScale(vec![(Cfvo::Min, 0xF8696B), (Cfvo::Percentile(50.0), 0xFFEB84), (Cfvo::Max, 0x63BE7B)]), Dxf::default(), "A colour scale");
    }
    if low == "icon set" || low == "icons" {
        let points = vec![(Cfvo::Percent(0.0), true), (Cfvo::Percent(33.0), true), (Cfvo::Percent(67.0), true)];
        return add_rule(d, RuleKind::IconSet { set: 0, points, reverse: false, show_value: true, icons: None }, Dxf::default(), "Icons");
    }
    if let Some(rest) = low.strip_prefix("greater than ") {
        let n: f64 = rest.trim().parse().ok().filter(|v: &f64| v.is_finite()).ok_or("Type greater than NUMBER.")?;
        let expr = officina_grid::formula::ast::Expr::Num(n);
        return add_rule(d, RuleKind::CellIs { op: officina_grid::condfmt::CmpOp::Greater, a: Arc::new(expr), b: None }, red, &format!("Greater than {n}"));
    }
    if let Some(rest) = low.strip_prefix("top ") {
        let (n, percent) = match rest.trim().strip_suffix('%') {
            Some(p) => (p.trim(), true),
            None => (rest.trim(), false),
        };
        let n: u32 = n.parse().ok().filter(|n| (1..=1000).contains(n)).ok_or("Type top N or top N%.")?;
        return add_rule(d, RuleKind::Top { n, percent, bottom: false }, red, &format!("Top {n}{}", if percent { "%" } else { "" }));
    }
    if low == "clear format" || low == "clear" {
        let all: Vec<CondFormat> = d.wb.cond_formats().iter().filter(|f| !(f.sheet == s && f.ranges.iter().all(|r| rect.contains_rect(r)))).cloned().collect();
        let n = d.wb.cond_formats().len() - all.len();
        if n == 0 {
            return Err("No conditional formats in the selection.".into());
        }
        if !d.run_action("Clear Rules", vec![GridOp::cond_formats(&d.wb, &all)]) {
            return Err(std::mem::take(&mut d.status));
        }
        return Ok(format!("{n} rule{} cleared.", if n == 1 { "" } else { "s" }));
    }
    if let Some(rest) = low.strip_prefix("chart") {
        let kind = match rest.trim() {
            "" | "column" => ChartKind::Column,
            "bar" => ChartKind::Bar,
            "line" => ChartKind::Line,
            "pie" => ChartKind::Pie,
            "scatter" => ChartKind::Scatter,
            "area" => ChartKind::Area,
            other => return Err(format!("No quick chart “{other}”: column, bar, line, pie, scatter or area.")),
        };
        return crate::data::apply(d, sel, cursor, crate::data::DataCmd::Chart(kind));
    }
    if let Some(rest) = low.strip_prefix("totals") {
        return crate::dataform::quick_totals(d, sel, if rest.trim().is_empty() { "sum below" } else { rest.trim() });
    }
    if low == "table" {
        return crate::data::apply(d, sel, cursor, crate::data::DataCmd::FormatAsTable);
    }
    if let Some(rest) = low.strip_prefix("sparklines") {
        let kind = match rest.trim() {
            "" | "line" => "line",
            "column" => "column",
            "win loss" | "win/loss" | "winloss" => "winloss",
            other => return Err(format!("No sparkline “{other}”: line, column or win loss.")),
        };
        // One per row, in the column right of the selection.
        let at = sel.c1 + 1;
        if at >= MAX_COLS {
            return Err("There is no column right of the selection.".into());
        }
        let place = CellRange { r0: sel.r0, c0: at, r1: sel.r1, c1: at };
        return crate::sparkbar::run(d, place, &format!("{kind} {}", rect_text(&rect)));
    }
    Err("Type data bars, color scale, icon set, greater than N, top N, clear format; chart column|line|pie…; totals sum below|right…; table; or sparklines line|column|win loss.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::{CellAddr, Value};

    /// Region, Sales over five rows, made a table at A1:B6.
    fn doc() -> Doc {
        let mut d = Doc::new("t");
        d.wb.set_value(CellAddr::new(0, 0, 0), Value::str("Region"));
        d.wb.set_value(CellAddr::new(0, 0, 1), Value::str("Sales"));
        for r in 1..=5u32 {
            d.wb.set_value(CellAddr::new(0, r, 0), Value::str(&format!("R{r}")));
            d.wb.set_value(CellAddr::new(0, r, 1), Value::Num(f64::from(r * 10)));
        }
        d.wb.recalc();
        d
    }

    fn sel(r0: u32, c0: u32, r1: u32, c1: u32) -> CellRange {
        CellRange { r0, c0, r1, c1 }
    }

    #[test]
    fn themes_recolour_and_change_fonts() {
        let mut d = doc();
        assert_eq!(themes_start(&d), "theme Office");
        assert_eq!(themes_run(&mut d, "theme office 2007").unwrap(), "Theme Office 2007.");
        assert_eq!(d.wb.theme().colors[4], 0x4F81BD);
        themes_run(&mut d, "colors Green").unwrap();
        assert_eq!(d.wb.theme().colors[4], 0x549E39);
        assert_eq!(&*d.wb.theme().heading, "Cambria");
        themes_run(&mut d, "fonts Georgia / Verdana").unwrap();
        assert_eq!((&*d.wb.theme().heading, &*d.wb.theme().body), ("Georgia", "Verdana"));
        assert!(themes_run(&mut d, "theme Nope").unwrap_err().contains("Office 2007"));
        d.undo();
        assert_eq!(&*d.wb.theme().body, "Calibri");
    }

    #[test]
    fn table_design_sets_styles_options_and_custom_styles() {
        let mut d = doc();
        crate::data::apply(&mut d, sel(0, 0, 5, 1), (0, 0), crate::data::DataCmd::FormatAsTable).unwrap();
        assert!(can_design(&d, (2, 1)) && !can_design(&d, (9, 9)));
        assert_eq!(design_start(&d, (2, 1)), "style Medium 2; banded rows; no banded columns; no first column; no last column");
        design_run(&mut d, (2, 1), "style dark 3; banded columns; no banded rows").unwrap();
        let t = d.wb.tables()[0].clone();
        assert_eq!((t.style.as_deref(), t.stripes, t.bands.cols), (Some("TableStyleDark3"), false, true));
        let m = design_run(&mut d, (2, 1), "new style Brand with header 1F4E78 text white bold, stripe DDEBF7, whole border 1F4E78; style brand").unwrap();
        assert!(m.contains("style Brand made"), "{m}");
        let t = d.wb.tables()[0].clone();
        assert_eq!(t.style.as_deref(), Some("Brand"));
        assert_eq!(d.wb.table_cell_look(&t, 0, 0).unwrap().fill, Some(0x1F4E78));
        assert!(design_run(&mut d, (2, 1), "style Medium 99").is_err());
        design_run(&mut d, (2, 1), "delete style Brand").unwrap();
        assert_eq!(d.wb.tables()[0].style.as_deref(), Some("TableStyleMedium2"));
        assert!(d.wb.workbook_look().table_styles.is_empty());
    }

    #[test]
    fn shapes_text_boxes_and_cameras() {
        let mut d = doc();
        assert_eq!(shape_run(&mut d, (0, 4), "oval \"Up 12%\" at E2 size 3x4 fill accent2 bold").unwrap(), "Shape 1 added.");
        let s = d.wb.workbook_look().shapes[0].clone();
        assert_eq!((&*s.geom, &*s.text, s.fill, s.bold), ("ellipse", "Up 12%", Some(0xED7D31), true));
        assert_eq!(s.anchor.from, (1, 4, 0.0, 0.0));
        assert_eq!(s.anchor.to, (4, 8, 0.0, 0.0));
        // On the shape, the bar shows it and changes it.
        assert!(shape_start(&d, (2, 5)).starts_with("oval \"Up 12%\" at E2 size 3x4 fill accent2"), "{}", shape_start(&d, (2, 5)));
        shape_run(&mut d, (2, 5), "star line none").unwrap();
        assert_eq!((&*d.wb.workbook_look().shapes[0].geom, d.wb.workbook_look().shapes[0].line), ("star5", None));
        shape_run(&mut d, (10, 0), "text box \"Note\" at A11").unwrap();
        assert!(d.wb.workbook_look().shapes[1].is_text_box());
        assert!(shape_run(&mut d, (20, 0), "blob").unwrap_err().contains("No shape"));
        shape_run(&mut d, (2, 5), "delete").unwrap();
        assert_eq!(d.wb.workbook_look().shapes.len(), 1);

        assert_eq!(camera_start(&d, sel(0, 0, 5, 1), (0, 0)), "A1:B6 at D1");
        let m = camera_run(&mut d, sel(0, 0, 5, 1), (0, 0), "A1:B6 at D1").unwrap();
        assert!(m.contains("live picture of A1:B6"), "{m}");
        assert_eq!(d.wb.workbook_look().cameras[0].source, Rect { sheet: 0, r0: 0, c0: 0, r1: 5, c1: 1 });
        camera_run(&mut d, sel(1, 4, 1, 4), (1, 4), "source A1:A3").unwrap();
        assert_eq!(d.wb.workbook_look().cameras[0].source.r1, 2);
        assert!(camera_run(&mut d, sel(20, 0, 20, 0), (20, 0), "A1:Z1000 at H1").unwrap_err().contains("at most"));
        camera_run(&mut d, sel(1, 4, 1, 4), (1, 4), "delete").unwrap();
        assert!(d.wb.workbook_look().cameras.is_empty());
    }

    #[test]
    fn quick_analysis_formats_charts_totals_and_sparklines() {
        let mut d = doc();
        let data = sel(1, 1, 5, 1);
        quick_run(&mut d, data, (1, 1), "data bars").unwrap();
        quick_run(&mut d, data, (1, 1), "greater than 25").unwrap();
        quick_run(&mut d, data, (1, 1), "top 2").unwrap();
        assert_eq!(d.wb.cond_formats().len(), 3);
        assert!(matches!(d.wb.cond_formats()[0].rules[0].kind, RuleKind::DataBar { .. }));
        assert_eq!(quick_run(&mut d, data, (1, 1), "clear format").unwrap(), "3 rules cleared.");
        quick_run(&mut d, sel(0, 0, 5, 1), (0, 0), "chart line").unwrap();
        assert_eq!(d.wb.charts()[0].kind, ChartKind::Line);
        quick_run(&mut d, data, (1, 1), "totals sum below").unwrap();
        assert_eq!(d.wb.formula_text(CellAddr::new(0, 6, 1)).as_deref(), Some("=SUM(B2:B6)"));
        quick_run(&mut d, sel(1, 0, 5, 1), (1, 0), "sparklines column").unwrap();
        assert_eq!(d.wb.sheet(0).sparklines().len(), 5);
        assert!(quick_run(&mut d, data, (1, 1), "sideways").is_err());
    }
}
