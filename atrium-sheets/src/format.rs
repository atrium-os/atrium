//! The Format menu: the selection's font (bold, italic, underline,
//! strikethrough, size, colour), fill, alignment, borders and number format
//! (docs/spec/officina.md §16.97). Each command is one undoable step.

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::ops::GridOp;
use officina_grid::style::{Edge, HAlign, Line, Style};
use pergola::widgets::{CellRange, MenuItem};

use crate::doc::Doc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatCmd {
    Bold,
    Italic,
    Underline,
    Strike,
    /// Left, centre, right, or as the value says (General).
    Align(HAlign),
    /// One of [`NUMBER_FORMATS`].
    Number(usize),
    MoreDecimals,
    FewerDecimals,
    Larger,
    Smaller,
    /// Opens the colour bar for the text (false) or the fill (true); the
    /// window runs it.
    Colors(bool),
    /// A colour from the bar, for the text or the fill; `None` takes it off.
    Color(bool, Option<u32>),
    Borders(BorderKind),
    /// Opens the bar for a format code of one's own; the window runs it.
    Custom,
    /// The text moved one step in (true) or out from the left.
    Indent(bool),
    /// Text turned by degrees (-90..=90, counter-clockwise), or stacked (255).
    Rotate(i16),
    /// Opens the bar for a rotation angle of one's own; the window runs it.
    Angle,
    /// The line the plain border commands draw (one of [`PEN_LINES`]).
    PenLine(usize),
    /// Their colour: one of the tab colours, or automatic.
    PenColor(Option<u32>),
    /// The text's face: monospace (`true`, as Courier New in files) or the
    /// workbook's own sans.
    Mono(bool),
    /// One of [`CELL_STYLES`]: its font, fill and borders on the selection.
    CellStyle(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderKind {
    All,
    Outside,
    Bottom,
    None,
    /// Thick lines around the selection.
    ThickOutside,
    ThickBottom,
    /// A double line under the selection (totals).
    DoubleBottom,
    /// Thin lines over the first row and under the last.
    TopAndBottom,
}

/// The number formats the menu offers: name and format code.
pub const NUMBER_FORMATS: [(&str, &str); 12] = [
    ("General", "General"),
    ("Number", "#,##0.00"),
    ("Currency", "$#,##0.00"),
    ("Percent", "0%"),
    ("Date", "yyyy-mm-dd"),
    ("Time", "hh:mm"),
    ("Scientific", "0.00E+00"),
    ("Text", "@"),
    ("Accounting", r#"_($* #,##0.00_);_($* (#,##0.00);_($* "-"??_);_(@_)"#),
    ("Comma Style", r#"_(* #,##0.00_);_(* (#,##0.00);_(* "-"??_);_(@_)"#),
    ("Long Date", "dddd, mmmm d, yyyy"),
    ("Fraction", "# ?/?"),
];

/// Light colours for fills, after the tab colours.
pub const LIGHT_COLORS: [(u32, &str); 6] = [
    (0xFFC7CE, "Light red"),
    (0xFFEB9C, "Light yellow"),
    (0xC6EFCE, "Light green"),
    (0xDDEBF7, "Light blue"),
    (0xE4DFEC, "Light purple"),
    (0xEDEDED, "Light grey"),
];

/// A cell style's look: what it sets, as Excel's built-in styles do. The
/// cells keep their number format and alignment.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub fill: Option<u32>,
    pub color: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    /// Points; None: the default size.
    pub size: Option<u16>,
    /// Left, right, top, bottom: a line and its colour.
    pub borders: [Option<(Line, u32)>; 4],
}

const PLAIN: Look = Look { fill: None, color: None, bold: false, italic: false, size: None, borders: [None; 4] };
const fn boxed(line: Line, c: u32) -> [Option<(Line, u32)>; 4] {
    [Some((line, c)); 4]
}
const fn under(line: Line, c: u32) -> [Option<(Line, u32)>; 4] {
    [None, None, None, Some((line, c))]
}
const HEAD: u32 = 0x44546A;
const ACCENT: u32 = 0x4472C4;

/// Excel's built-in cell styles (the Office theme's colours), by name.
pub const CELL_STYLES: [(&str, Look); 16] = [
    ("Normal", PLAIN),
    ("Good", Look { fill: Some(0xC6EFCE), color: Some(0x006100), ..PLAIN }),
    ("Bad", Look { fill: Some(0xFFC7CE), color: Some(0x9C0006), ..PLAIN }),
    ("Neutral", Look { fill: Some(0xFFEB9C), color: Some(0x9C5700), ..PLAIN }),
    ("Title", Look { color: Some(HEAD), size: Some(18), ..PLAIN }),
    ("Heading 1", Look { color: Some(HEAD), bold: true, size: Some(15), borders: under(Line::Thick, ACCENT), ..PLAIN }),
    ("Heading 2", Look { color: Some(HEAD), bold: true, size: Some(13), borders: under(Line::Thick, 0xA2B8E1), ..PLAIN }),
    ("Heading 3", Look { color: Some(HEAD), bold: true, borders: under(Line::Medium, 0x8EA9DB), ..PLAIN }),
    ("Heading 4", Look { color: Some(HEAD), bold: true, ..PLAIN }),
    ("Total", Look { bold: true, borders: [None, None, Some((Line::Thin, ACCENT)), Some((Line::Double, ACCENT))], ..PLAIN }),
    ("Input", Look { fill: Some(0xFFCC99), color: Some(0x3F3F76), borders: boxed(Line::Thin, 0x7F7F7F), ..PLAIN }),
    ("Output", Look { fill: Some(0xF2F2F2), color: Some(0x3F3F3F), bold: true, borders: boxed(Line::Thin, 0x3F3F3F), ..PLAIN }),
    ("Calculation", Look { fill: Some(0xF2F2F2), color: Some(0xFA7D00), bold: true, borders: boxed(Line::Thin, 0x7F7F7F), ..PLAIN }),
    ("Check Cell", Look { fill: Some(0xA5A5A5), color: Some(0xFFFFFF), bold: true, borders: boxed(Line::Double, 0x3F3F3F), ..PLAIN }),
    ("Note", Look { fill: Some(0xFFFFCC), borders: boxed(Line::Thin, 0xB2B2B2), ..PLAIN }),
    ("Warning Text", Look { color: Some(0xFF0000), ..PLAIN }),
];

/// `st` in cell style `look`: its font, fill and borders replaced; the
/// number format, alignment, wrapping, indent, rotation and protection kept.
pub fn styled(st: &Style, look: &Look) -> Style {
    let edge = |e: Option<(Line, u32)>| e.map_or(Edge::default(), |(line, c)| Edge { line, color: Some(c) });
    Style {
        font: officina_grid::style::Font { color: look.color, bold: look.bold, italic: look.italic, size: look.size.filter(|&p| p != 11).map(|p| p * 20), ..Default::default() },
        fill: look.fill,
        pattern: None,
        gradient: None,
        borders: look.borders.map(edge),
        diagonals: [Edge::default(); 2],
        ..st.clone()
    }
}

/// Font sizes Larger and Smaller step through, points.
/// The font name Font: Mono gives cells, as files keep it.
pub const MONO_FONT: &str = "Courier New";

pub const SIZES: [u16; 14] = [8, 9, 10, 11, 12, 14, 16, 18, 20, 24, 28, 36, 48, 72];

/// The menu, read from the selection's first cell.
pub fn menu(d: &Doc, sel: CellRange) -> Vec<(MenuItem, Option<FormatCmd>)> {
    let st = d.wb.style(CellAddr::new(d.sheet, sel.r0, sel.c0));
    let pt = st.font.size.unwrap_or(220) / 20;
    let mono = st.font.name.as_deref().is_some_and(crate::source::is_mono_font);
    let mut v = vec![
        (MenuItem::new("Bold").checked(st.font.bold).shortcut("Ctrl+B"), Some(FormatCmd::Bold)),
        (MenuItem::new("Italic").checked(st.font.italic).shortcut("Ctrl+I"), Some(FormatCmd::Italic)),
        (MenuItem::new("Underline").checked(st.font.underline).shortcut("Ctrl+U"), Some(FormatCmd::Underline)),
        (MenuItem::new("Strikethrough").checked(st.font.strike).shortcut("Ctrl+5"), Some(FormatCmd::Strike)),
        (MenuItem::new(format!("Larger Text ({pt} pt)")).separated(), Some(FormatCmd::Larger)),
        (MenuItem::new("Smaller Text"), Some(FormatCmd::Smaller)),
        (MenuItem::new("Font: Sans").radio(!mono), Some(FormatCmd::Mono(false))),
        (MenuItem::new("Font: Mono").radio(mono), Some(FormatCmd::Mono(true))),
        (MenuItem::new("Text Colour…"), Some(FormatCmd::Colors(false))),
        (MenuItem::new("Fill Colour…"), Some(FormatCmd::Colors(true))),
        (MenuItem::new("Align Left").checked(st.h_align == HAlign::Left).separated(), Some(FormatCmd::Align(HAlign::Left))),
        (MenuItem::new("Align Centre").checked(st.h_align == HAlign::Center), Some(FormatCmd::Align(HAlign::Center))),
        (MenuItem::new("Align Right").checked(st.h_align == HAlign::Right), Some(FormatCmd::Align(HAlign::Right))),
        (MenuItem::new("Align by Value").checked(st.h_align == HAlign::General), Some(FormatCmd::Align(HAlign::General))),
        (MenuItem::new("Increase Indent").enabled(st.indent < MAX_INDENT), Some(FormatCmd::Indent(true))),
        (MenuItem::new("Decrease Indent").enabled(st.indent > 0), Some(FormatCmd::Indent(false))),
    ];
    let keys = ["Ctrl+Shift+~", "Ctrl+Shift+!", "Ctrl+Shift+$", "Ctrl+Shift+%", "Ctrl+Shift+#", "Ctrl+Shift+@", "Ctrl+Shift+^", "", "", "", "", ""];
    for (i, (name, code)) in NUMBER_FORMATS.iter().enumerate() {
        let mut item = MenuItem::new(*name).radio(&*st.num_fmt == *code);
        if i == 0 {
            item = item.separated();
        }
        if !keys[i].is_empty() {
            item = item.shortcut(keys[i]);
        }
        v.push((item, Some(FormatCmd::Number(i))));
    }
    v.push((MenuItem::new("Custom Format…").shortcut("Ctrl+1"), Some(FormatCmd::Custom)));
    v.push((MenuItem::new("More Decimals"), Some(FormatCmd::MoreDecimals)));
    v.push((MenuItem::new("Fewer Decimals"), Some(FormatCmd::FewerDecimals)));
    v.push((MenuItem::new("All Borders").separated(), Some(FormatCmd::Borders(BorderKind::All))));
    v.push((MenuItem::new("Outside Borders").shortcut("Ctrl+Shift+&"), Some(FormatCmd::Borders(BorderKind::Outside))));
    v.push((MenuItem::new("Bottom Border"), Some(FormatCmd::Borders(BorderKind::Bottom))));
    v.push((MenuItem::new("Top and Bottom Border"), Some(FormatCmd::Borders(BorderKind::TopAndBottom))));
    v.push((MenuItem::new("Thick Outside Borders"), Some(FormatCmd::Borders(BorderKind::ThickOutside))));
    v.push((MenuItem::new("Thick Bottom Border"), Some(FormatCmd::Borders(BorderKind::ThickBottom))));
    v.push((MenuItem::new("Double Bottom Border"), Some(FormatCmd::Borders(BorderKind::DoubleBottom))));
    v.push((MenuItem::new("No Borders").shortcut("Ctrl+Shift+_"), Some(FormatCmd::Borders(BorderKind::None))));
    for (i, (name, line)) in PEN_LINES.iter().enumerate() {
        let item = MenuItem::new(format!("Border Line: {name}")).radio(d.border_pen.0 == *line);
        v.push((if i == 0 { item.separated() } else { item }, Some(FormatCmd::PenLine(i))));
    }
    v.push((MenuItem::new("Border Colour: Automatic").radio(d.border_pen.1.is_none()), Some(FormatCmd::PenColor(None))));
    for (rgb, name) in crate::tabs::COLORS {
        v.push((MenuItem::new(format!("Border Colour: {name}")).radio(d.border_pen.1 == Some(rgb)), Some(FormatCmd::PenColor(Some(rgb)))));
    }
    for (i, (name, deg)) in ROTATIONS.iter().enumerate() {
        let mut item = MenuItem::new(*name).radio(st.rotation == *deg);
        if i == 0 {
            item = item.separated();
        }
        v.push((item, Some(FormatCmd::Rotate(*deg))));
    }
    v.push((MenuItem::new("Rotation Angle…").radio(!ROTATIONS.iter().any(|r| r.1 == st.rotation)), Some(FormatCmd::Angle)));
    for (i, (name, look)) in CELL_STYLES.iter().enumerate() {
        let item = MenuItem::new(format!("Cell Style: {name}")).radio(styled(st, look) == *st);
        v.push((if i == 0 { item.separated() } else { item }, Some(FormatCmd::CellStyle(i))));
    }
    v
}

/// Whether F4 repeats `c`: commands that change the selected cells (not
/// those opening a bar or setting the border pen).
pub fn repeatable(c: FormatCmd) -> bool {
    !matches!(c, FormatCmd::Colors(_) | FormatCmd::Custom | FormatCmd::Angle | FormatCmd::PenLine(_) | FormatCmd::PenColor(_))
}

/// The command for a formatting key ([`pergola::widgets::GridAction::Format`]).
pub fn for_key(k: char) -> Option<FormatCmd> {
    Some(match k {
        'b' => FormatCmd::Bold,
        'i' => FormatCmd::Italic,
        'u' => FormatCmd::Underline,
        '5' => FormatCmd::Strike,
        '~' => FormatCmd::Number(0),
        '!' => FormatCmd::Number(1),
        '$' => FormatCmd::Number(2),
        '%' => FormatCmd::Number(3),
        '#' => FormatCmd::Number(4),
        '@' => FormatCmd::Number(5),
        '^' => FormatCmd::Number(6),
        '&' => FormatCmd::Borders(BorderKind::Outside),
        '_' => FormatCmd::Borders(BorderKind::None),
        '1' => FormatCmd::Custom,
        _ => return None,
    })
}

/// Runs `cmd` on the selection; the status says what happened.
pub fn run(d: &mut Doc, sel: CellRange, cmd: FormatCmd) {
    d.status.clear();
    let msg = match apply(d, sel, cmd) {
        Ok(m) | Err(m) => m,
    };
    if d.status != crate::sheet_protection::REFUSED {
        d.status = msg;
    }
}

fn apply(d: &mut Doc, sel: CellRange, cmd: FormatCmd) -> Result<String, String> {
    let s = d.sheet;
    let r = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    let first = d.wb.style(CellAddr::new(s, sel.r0, sel.c0)).clone();
    let one = |d: &mut Doc, label: &str, f: &dyn Fn(&Style) -> Style| {
        let op = GridOp::restyle(&d.wb, r, f);
        d.run(label, vec![op]);
    };
    // Toggles follow the first cell: on for all when it is off there.
    macro_rules! toggle {
        ($label:expr, $field:ident) => {{
            let on = !first.font.$field;
            one(d, $label, &|st| { let mut st = st.clone(); st.font.$field = on; st });
            Ok(format!("{} {}.", $label, if on { "on" } else { "off" }))
        }};
    }
    match cmd {
        FormatCmd::Custom | FormatCmd::Angle => Ok(String::new()),
        FormatCmd::PenLine(i) => {
            let (name, line) = *PEN_LINES.get(i).ok_or("Unknown line.")?;
            d.border_pen.0 = line;
            Ok(format!("Borders now draw {} lines.", name.to_lowercase()))
        }
        FormatCmd::PenColor(c) => {
            d.border_pen.1 = c;
            Ok(match c.and_then(|c| crate::tabs::COLORS.iter().find(|x| x.0 == c)) {
                Some((_, name)) => format!("Borders now draw in {}.", name.to_lowercase()),
                None => "Borders now draw in the automatic colour.".into(),
            })
        }
        FormatCmd::Rotate(deg) => {
            let (name, _) = ROTATIONS.iter().find(|r| r.1 == deg).ok_or("Unknown rotation.")?;
            one(d, "Text Rotation", &|st| Style { rotation: deg, ..st.clone() });
            Ok(if deg == 0 { "Rotation taken off.".into() } else { format!("{name}.") })
        }
        FormatCmd::Indent(more) => {
            if !more && first.indent == 0 {
                return Err("The text isn't indented.".into());
            }
            if more && first.indent >= MAX_INDENT {
                return Err(format!("Text is indented at most {MAX_INDENT} steps."));
            }
            let to = if more { first.indent + 1 } else { first.indent - 1 };
            // Indented text sits at the left, as Excel puts it.
            one(d, if more { "Increase Indent" } else { "Decrease Indent" }, &|st| Style {
                indent: to,
                h_align: if to > 0 && matches!(st.h_align, HAlign::General | HAlign::Center) { HAlign::Left } else { st.h_align },
                ..st.clone()
            });
            Ok(if to == 0 { "Indent taken off.".into() } else { format!("Indented {to} {}.", if to == 1 { "step" } else { "steps" }) })
        }
        FormatCmd::Bold => toggle!("Bold", bold),
        FormatCmd::Italic => toggle!("Italic", italic),
        FormatCmd::Underline => toggle!("Underline", underline),
        FormatCmd::Strike => toggle!("Strikethrough", strike),
        FormatCmd::Align(a) => {
            one(d, "Align", &|st| Style { h_align: a, ..st.clone() });
            Ok(match a {
                HAlign::Left => "Aligned left.",
                HAlign::Center => "Centred.",
                HAlign::Right => "Aligned right.",
                _ => "Aligned by value: text left, numbers right.",
            }.into())
        }
        FormatCmd::Number(i) => {
            let (name, code) = *NUMBER_FORMATS.get(i).ok_or("Unknown format.")?;
            one(d, "Number Format", &|st| Style { num_fmt: code.into(), ..st.clone() });
            Ok(format!("Formatted as {}.", name.to_lowercase()))
        }
        FormatCmd::MoreDecimals | FormatCmd::FewerDecimals => {
            let more = cmd == FormatCmd::MoreDecimals;
            let code = decimals(&first.num_fmt, more).ok_or(if more { "That format can't show more decimals." } else { "There are no decimals to take away." })?;
            one(d, if more { "More Decimals" } else { "Fewer Decimals" }, &|st| Style { num_fmt: code.as_str().into(), ..st.clone() });
            Ok(format!("Format is {code}."))
        }
        FormatCmd::Larger | FormatCmd::Smaller => {
            let pt = first.font.size.unwrap_or(220) / 20;
            let next = if cmd == FormatCmd::Larger { SIZES.iter().find(|&&x| x > pt) } else { SIZES.iter().rev().find(|&&x| x < pt) };
            let next = *next.ok_or(if cmd == FormatCmd::Larger { "That is the largest size." } else { "That is the smallest size." })?;
            one(d, "Font Size", &|st| { let mut st = st.clone(); st.font.size = (next != 11).then_some(next * 20); st });
            Ok(format!("Text is {next} pt."))
        }
        FormatCmd::Mono(mono) => {
            one(d, "Font", &|st| { let mut st = st.clone(); st.font.name = mono.then(|| MONO_FONT.into()); st });
            Ok(if mono { format!("Text is in a monospace font ({MONO_FONT}).") } else { "Text is in the workbook's own font.".into() })
        }
        FormatCmd::CellStyle(i) => {
            let (name, look) = *CELL_STYLES.get(i).ok_or("Unknown cell style.")?;
            one(d, "Cell Style", &|st| styled(st, &look));
            Ok(format!("Cell style {name}."))
        }
        // The window opens the bar.
        FormatCmd::Colors(_) => Ok(String::new()),
        FormatCmd::Color(fill, c) => {
            if fill {
                one(d, "Fill Colour", &|st| Style { fill: c, pattern: None, gradient: None, ..st.clone() });
            } else {
                one(d, "Text Colour", &|st| { let mut st = st.clone(); st.font.color = c; st });
            }
            Ok(match (fill, c) {
                (true, Some(_)) => "Fill set.",
                (true, None) => "Fill taken off.",
                (false, Some(_)) => "Text colour set.",
                (false, None) => "Text colour back to automatic.",
            }.into())
        }
        FormatCmd::Borders(kind) => {
            const L: usize = 0;
            const R: usize = 1;
            const T: usize = 2;
            const B: usize = 3;
            let (pen, color) = d.border_pen;
            let line = match kind {
                BorderKind::ThickOutside | BorderKind::ThickBottom => Line::Thick,
                BorderKind::DoubleBottom => Line::Double,
                _ => pen,
            };
            let thin = Edge { line, color };
            let set = |edges: &'static [usize]| move |st: &Style| {
                let mut st = st.clone();
                for &e in edges {
                    st.borders[e] = thin;
                }
                st
            };
            let ops = match kind {
                BorderKind::All => vec![GridOp::restyle(&d.wb, r, set(&[L, R, T, B]))],
                BorderKind::None => vec![GridOp::restyle(&d.wb, r, |st| Style { borders: [Edge::default(); 4], ..st.clone() })],
                BorderKind::Bottom | BorderKind::ThickBottom | BorderKind::DoubleBottom => vec![GridOp::restyle(&d.wb, Rect { r0: r.r1, ..r }, set(&[B]))],
                BorderKind::TopAndBottom if r.r0 == r.r1 => vec![GridOp::restyle(&d.wb, r, set(&[T, B]))],
                BorderKind::TopAndBottom => vec![GridOp::restyle(&d.wb, Rect { r1: r.r0, ..r }, set(&[T])), GridOp::restyle(&d.wb, Rect { r0: r.r1, ..r }, set(&[B]))],
                BorderKind::Outside | BorderKind::ThickOutside => {
                    // Disjoint parts, each built against the same styles:
                    // corners, edge strips (the inside keeps its own).
                    let mut ops = Vec::new();
                    let rows: Vec<(u32, u32, &'static [usize])> = if r.r0 == r.r1 { vec![(r.r0, r.r0, &[T, B])] } else {
                        let mut v: Vec<(u32, u32, &'static [usize])> = vec![(r.r0, r.r0, &[T]), (r.r1, r.r1, &[B])];
                        if r.r1 > r.r0 + 1 { v.push((r.r0 + 1, r.r1 - 1, &[])); }
                        v
                    };
                    let cols: Vec<(u32, u32, &'static [usize])> = if r.c0 == r.c1 { vec![(r.c0, r.c0, &[L, R])] } else {
                        let mut v: Vec<(u32, u32, &'static [usize])> = vec![(r.c0, r.c0, &[L]), (r.c1, r.c1, &[R])];
                        if r.c1 > r.c0 + 1 { v.push((r.c0 + 1, r.c1 - 1, &[])); }
                        v
                    };
                    for &(r0, r1, re) in &rows {
                        for &(c0, c1, ce) in &cols {
                            if re.is_empty() && ce.is_empty() {
                                continue;
                            }
                            let edges: Vec<usize> = re.iter().chain(ce).copied().collect();
                            ops.push(GridOp::restyle(&d.wb, Rect { sheet: s, r0, c0, r1, c1 }, move |st| {
                                let mut st = st.clone();
                                for &e in &edges {
                                    st.borders[e] = thin;
                                }
                                st
                            }));
                        }
                    }
                    ops
                }
            };
            d.run("Borders", ops);
            Ok(match kind {
                BorderKind::All => "Borders on every cell.",
                BorderKind::Outside => "Border around the selection.",
                BorderKind::Bottom => "Bottom border.",
                BorderKind::None => "Borders taken off.",
                BorderKind::ThickOutside => "Thick border around the selection.",
                BorderKind::ThickBottom => "Thick bottom border.",
                BorderKind::DoubleBottom => "Double bottom border.",
                BorderKind::TopAndBottom => "Top and bottom borders.",
            }.into())
        }
    }
}

/// `code` with one more (or one fewer) decimal place in each section.
/// General becomes `0.0` (more) and stays (fewer).
pub fn decimals(code: &str, more: bool) -> Option<String> {
    if code.eq_ignore_ascii_case("General") {
        return more.then(|| "0.0".to_string());
    }
    if code == "@" {
        return None;
    }
    let mut changed = false;
    let out: Vec<String> = code.split(';').map(|sec| {
        let b = sec.as_bytes();
        // The last run of 0 (or #) digits outside quotes and brackets.
        let (mut quoted, mut bracket, mut last) = (false, false, None);
        for (i, &c) in b.iter().enumerate() {
            match c {
                b'"' => quoted = !quoted,
                b'[' if !quoted => bracket = true,
                b']' if !quoted => bracket = false,
                b'0' | b'#' if !quoted && !bracket => last = Some(i),
                // The exponent's digits aren't decimals.
                b'E' | b'e' if !quoted && !bracket => break,
                _ => {}
            }
        }
        let Some(end) = last else { return sec.to_string() };
        // Is it after a decimal point?
        let mut start = end;
        while start > 0 && matches!(b[start - 1], b'0' | b'#') {
            start -= 1;
        }
        let after_point = start > 0 && b[start - 1] == b'.';
        changed = true;
        match (more, after_point) {
            (true, true) => format!("{}0{}", &sec[..=end], &sec[end + 1..]),
            (true, false) => format!("{}.0{}", &sec[..=end], &sec[end + 1..]),
            (false, true) if end > start => format!("{}{}", &sec[..end], &sec[end + 1..]),
            (false, true) => format!("{}{}", &sec[..start - 1], &sec[end + 1..]),
            (false, false) => {
                changed = false;
                sec.to_string()
            }
        }
    }).collect();
    changed.then(|| out.join(";"))
}

#[cfg(test)]
mod tests {
    use super::decimals;

    #[test]
    fn decimals_are_added_and_taken_in_every_section() {
        assert_eq!(decimals("General", true).as_deref(), Some("0.0"));
        assert_eq!(decimals("General", false), None);
        assert_eq!(decimals("#,##0.00", true).as_deref(), Some("#,##0.000"));
        assert_eq!(decimals("#,##0.00", false).as_deref(), Some("#,##0.0"));
        assert_eq!(decimals("0.0", false).as_deref(), Some("0"));
        assert_eq!(decimals("0", false), None);
        assert_eq!(decimals("0%", true).as_deref(), Some("0.0%"));
        assert_eq!(decimals("$#,##0.00;[Red]-$#,##0.00", true).as_deref(), Some("$#,##0.000;[Red]-$#,##0.000"));
        assert_eq!(decimals("0.00E+00", false).as_deref(), Some("0.0E+00"));
        assert_eq!(decimals("@", true), None);
    }
}

/// The Format menu's text rotations, as Excel's Orientation button offers
/// them: degrees counter-clockwise, 255 for stacked letters.
pub const ROTATIONS: [(&str, i16); 6] = [
    ("No Rotation", 0),
    ("Angle Counterclockwise", 45),
    ("Angle Clockwise", -45),
    ("Vertical Text", 255),
    ("Rotate Text Up", 90),
    ("Rotate Text Down", -90),
];

/// The lines the plain border commands can draw with.
pub const PEN_LINES: [(&str, Line); 5] = [
    ("Thin", Line::Thin),
    ("Medium", Line::Medium),
    ("Dashed", Line::Dashed),
    ("Dotted", Line::Dotted),
    ("Hair", Line::Hair),
];

/// The most indent steps a cell takes (Excel's indent buttons stop at 15).
pub const MAX_INDENT: u8 = 15;

/// The longest format code the Custom Format bar takes.
pub const MAX_CODE: usize = 255;

/// The selection given the number format `code` typed into the Custom
/// Format bar, as one step. Refused, with why, when Sheets doesn't
/// understand every part of it; the status shows how the first cell now
/// reads.
pub fn set_code(d: &mut Doc, sel: CellRange, code: &str) -> Result<String, String> {
    let code = code.trim();
    if code.is_empty() {
        return Err("Type a format code, such as 0.000, #,##0 \"kg\" or yyyy-mm-dd.".into());
    }
    if code.chars().count() > MAX_CODE {
        return Err(format!("A format code is at most {MAX_CODE} characters."));
    }
    if !officina_grid::numfmt::NumFmt::parse(code).is_exact() {
        return Err(format!("Sheets doesn't understand all of {code}; check its quotes and brackets."));
    }
    let s = d.sheet;
    let r = Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    d.status.clear();
    let op = GridOp::restyle(&d.wb, r, &|st: &Style| Style { num_fmt: code.into(), ..st.clone() });
    d.run("Number Format", vec![op]);
    if d.status == crate::sheet_protection::REFUSED {
        return Err(d.status.clone());
    }
    let at = CellAddr::new(s, sel.r0, sel.c0);
    let shown = d.wb.formatted(at).text;
    Ok(if shown.is_empty() {
        format!("Format is {code}.")
    } else {
        format!("Format is {code}: {} shows {shown}.", officina_grid::addr::a1(sel.r0, sel.c0))
    })
}

/// A colour typed as `#RRGGBB`, `RRGGBB` or `#RGB`.
pub fn parse_hex(text: &str) -> Option<u32> {
    let t = text.trim();
    let t = t.strip_prefix('#').unwrap_or(t);
    if !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match t.len() {
        6 => u32::from_str_radix(t, 16).ok(),
        3 => {
            let v = u32::from_str_radix(t, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF);
            Some((r * 17) << 16 | (g * 17) << 8 | (b * 17))
        }
        _ => None,
    }
}

/// The selection's text turned by `text` degrees (counter-clockwise, -90 to
/// 90, as Excel's Format Cells takes it), as one step.
pub fn set_angle(d: &mut Doc, sel: CellRange, text: &str) -> Result<String, String> {
    let t = text.trim().trim_end_matches('°').trim();
    let deg: i16 = t.parse().map_err(|_| format!("Type an angle from -90 to 90 degrees; {t:?} isn't one."))?;
    if !(-90..=90).contains(&deg) {
        return Err(format!("Text turns from -90 to 90 degrees; {deg} is outside."));
    }
    let r = Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    d.status.clear();
    let op = GridOp::restyle(&d.wb, r, &|st: &Style| Style { rotation: deg, ..st.clone() });
    d.run("Text Rotation", vec![op]);
    if d.status == crate::sheet_protection::REFUSED {
        return Err(d.status.clone());
    }
    Ok(if deg == 0 { "Rotation taken off.".into() } else { format!("Text turned {deg}°.") })
}
