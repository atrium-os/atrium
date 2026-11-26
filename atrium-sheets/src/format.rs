//! The Format menu: the selection's font (bold, italic, underline,
//! strikethrough, size, colour), fill, alignment, borders and number format
//! (docs/spec/officina.md §16.97). Each command is one undoable step.

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::ops::GridOp;
use officina_grid::style::{Edge, HAlign, Line, Script, Style};
use pergola::widgets::{CellRange, MenuItem};

use crate::doc::Doc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatCmd {
    Bold,
    Italic,
    Underline,
    Strike,
    /// A double underline on (an underline doubled) or back to single.
    DoubleUnderline,
    /// An accounting underline (across the cell; single, or double with
    /// `true`) on, or off.
    Accounting(bool),
    /// Superscript or subscript on the whole cell, or back to normal.
    Script(officina_grid::style::Script),
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
    /// Opens the Borders bar (edges, line and colour typed); the window
    /// runs it: [`borders_typed`].
    BordersBar,
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
    /// One side of the selection.
    Top,
    Left,
    Right,
    /// Lines between the selection's cells, not round it.
    Inside,
    InsideHorizontal,
    InsideVertical,
    /// A line across each cell, from the top left (down) or bottom left (up).
    DiagonalDown,
    DiagonalUp,
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
        (MenuItem::new("Double Underline").checked(st.font.underline && st.font.double && !st.font.accounting), Some(FormatCmd::DoubleUnderline)),
        (MenuItem::new("Single Accounting Underline").checked(st.font.underline && st.font.accounting && !st.font.double), Some(FormatCmd::Accounting(false))),
        (MenuItem::new("Double Accounting Underline").checked(st.font.underline && st.font.accounting && st.font.double), Some(FormatCmd::Accounting(true))),
        (MenuItem::new("Superscript").checked(st.font.script == Script::Super), Some(FormatCmd::Script(Script::Super))),
        (MenuItem::new("Subscript").checked(st.font.script == Script::Sub), Some(FormatCmd::Script(Script::Sub))),
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
        (MenuItem::new("Fill the Cell").checked(st.h_align == HAlign::Fill), Some(FormatCmd::Align(HAlign::Fill))),
        (MenuItem::new("Justify").checked(st.h_align == HAlign::Justify), Some(FormatCmd::Align(HAlign::Justify))),
        (MenuItem::new("Distributed").checked(st.h_align == HAlign::Distributed), Some(FormatCmd::Align(HAlign::Distributed))),
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
    v.push((MenuItem::new("Top Border"), Some(FormatCmd::Borders(BorderKind::Top))));
    v.push((MenuItem::new("Left Border"), Some(FormatCmd::Borders(BorderKind::Left))));
    v.push((MenuItem::new("Right Border"), Some(FormatCmd::Borders(BorderKind::Right))));
    v.push((MenuItem::new("Top and Bottom Border"), Some(FormatCmd::Borders(BorderKind::TopAndBottom))));
    v.push((MenuItem::new("Inside Borders"), Some(FormatCmd::Borders(BorderKind::Inside))));
    v.push((MenuItem::new("Diagonal Down Border"), Some(FormatCmd::Borders(BorderKind::DiagonalDown))));
    v.push((MenuItem::new("Diagonal Up Border"), Some(FormatCmd::Borders(BorderKind::DiagonalUp))));
    v.push((MenuItem::new("Borders…"), Some(FormatCmd::BordersBar)));
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
    if !crate::sheet_protection::refused(&d.status) {
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
        FormatCmd::Underline => {
            let on = !first.font.underline;
            one(d, "Underline", &|st| { let mut st = st.clone(); st.font.underline = on; st.font.double = false; st.font.accounting = false; st });
            Ok(format!("Underline {}.", if on { "on" } else { "off" }))
        }
        FormatCmd::Strike => toggle!("Strikethrough", strike),
        FormatCmd::DoubleUnderline => {
            let on = !(first.font.underline && first.font.double && !first.font.accounting);
            one(d, "Double Underline", &|st| { let mut st = st.clone(); st.font.underline = on; st.font.double = on; st.font.accounting = false; st });
            Ok(if on { "Double underline on.".into() } else { "Double underline off.".into() })
        }
        FormatCmd::Accounting(double) => {
            let f = &first.font;
            let on = !(f.underline && f.accounting && f.double == double);
            one(d, if double { "Double Accounting Underline" } else { "Single Accounting Underline" }, &|st| { let mut st = st.clone(); st.font.underline = on; st.font.double = on && double; st.font.accounting = on; st });
            Ok(match (on, double) {
                (false, _) => "Accounting underline off.".into(),
                (true, false) => "Single accounting underline: across the cell.".into(),
                (true, true) => "Double accounting underline: across the cell.".into(),
            })
        }
        FormatCmd::Script(want) => {
            let to = if first.font.script == want { Script::Normal } else { want };
            one(d, if want == Script::Sub { "Subscript" } else { "Superscript" }, &|st| { let mut st = st.clone(); st.font.script = to; st });
            Ok(match to {
                Script::Super => "Superscript on.",
                Script::Sub => "Subscript on.",
                Script::Normal => "Back to normal text.",
            }.into())
        }
        FormatCmd::Align(a) => {
            one(d, "Align", &|st| Style { h_align: a, ..st.clone() });
            Ok(match a {
                HAlign::Left => "Aligned left.",
                HAlign::Center => "Centred.",
                HAlign::Right => "Aligned right.",
                HAlign::Fill => "The text is repeated to fill each cell.",
                HAlign::Justify => "Justified: lines wrap and stretch to both edges.",
                HAlign::Distributed => "Distributed: words spread evenly over each cell.",
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
            let (pen, color) = d.border_pen;
            let line = match kind {
                BorderKind::ThickOutside | BorderKind::ThickBottom => Line::Thick,
                BorderKind::DoubleBottom => Line::Double,
                _ => pen,
            };
            let ops = border_ops(&d.wb, r, kind, Edge { line, color });
            d.run("Borders", ops);
            Ok(border_done(kind).into())
        }
        FormatCmd::BordersBar => Ok(String::new()),
    }
}

const L: usize = 0;
const R: usize = 1;
const T: usize = 2;
const B: usize = 3;

/// What the status says once `kind` is drawn.
fn border_done(kind: BorderKind) -> &'static str {
    match kind {
        BorderKind::All => "Borders on every cell.",
        BorderKind::Outside => "Border around the selection.",
        BorderKind::Bottom => "Bottom border.",
        BorderKind::None => "Borders taken off.",
        BorderKind::ThickOutside => "Thick border around the selection.",
        BorderKind::ThickBottom => "Thick bottom border.",
        BorderKind::DoubleBottom => "Double bottom border.",
        BorderKind::TopAndBottom => "Top and bottom borders.",
        BorderKind::Top => "Top border.",
        BorderKind::Left => "Left border.",
        BorderKind::Right => "Right border.",
        BorderKind::Inside => "Borders between the cells.",
        BorderKind::InsideHorizontal => "Borders between the rows.",
        BorderKind::InsideVertical => "Borders between the columns.",
        BorderKind::DiagonalDown => "Diagonal down border.",
        BorderKind::DiagonalUp => "Diagonal up border.",
    }
}

/// The ops drawing `kind` on `r` with `edge`. Each op covers a part of
/// `r` no other one does, so all are built against the same styles.
pub fn border_ops(wb: &officina_grid::Workbook, r: Rect, kind: BorderKind, edge: Edge) -> Vec<GridOp> {
    type Span = (u32, u32, &'static [usize]);
    // Rows (columns) split into the first, the middle and the last, each
    // with the edges it takes.
    let split = |lo: u32, hi: u32, first: &'static [usize], last: &'static [usize], both: &'static [usize], mid: &'static [usize]| -> Vec<Span> {
        if lo == hi {
            return vec![(lo, lo, both)];
        }
        let mut v = vec![(lo, lo, first), (hi, hi, last)];
        if hi > lo + 1 {
            v.push((lo + 1, hi - 1, mid));
        }
        v
    };
    let grid = |rows: Vec<Span>, cols: Vec<Span>| -> Vec<GridOp> {
        let mut ops = Vec::new();
        for &(r0, r1, re) in &rows {
            for &(c0, c1, ce) in &cols {
                if re.is_empty() && ce.is_empty() {
                    continue;
                }
                let edges: Vec<usize> = re.iter().chain(ce).copied().collect();
                ops.push(GridOp::restyle(wb, Rect { sheet: r.sheet, r0, c0, r1, c1 }, move |st| {
                    let mut st = st.clone();
                    for &e in &edges {
                        st.borders[e] = edge;
                    }
                    st
                }));
            }
        }
        ops
    };
    let all_rows: Vec<Span> = vec![(r.r0, r.r1, &[])];
    let all_cols: Vec<Span> = vec![(r.c0, r.c1, &[])];
    let set = |edges: &'static [usize]| move |st: &Style| {
        let mut st = st.clone();
        for &e in edges {
            st.borders[e] = edge;
        }
        st
    };
    match kind {
        BorderKind::All => vec![GridOp::restyle(wb, r, set(&[L, R, T, B]))],
        BorderKind::None => vec![GridOp::restyle(wb, r, |st| Style { borders: [Edge::default(); 4], diagonals: [Edge::default(); 2], ..st.clone() })],
        BorderKind::Bottom | BorderKind::ThickBottom | BorderKind::DoubleBottom => vec![GridOp::restyle(wb, Rect { r0: r.r1, ..r }, set(&[B]))],
        BorderKind::Top => vec![GridOp::restyle(wb, Rect { r1: r.r0, ..r }, set(&[T]))],
        BorderKind::Left => vec![GridOp::restyle(wb, Rect { c1: r.c0, ..r }, set(&[L]))],
        BorderKind::Right => vec![GridOp::restyle(wb, Rect { c0: r.c1, ..r }, set(&[R]))],
        BorderKind::TopAndBottom if r.r0 == r.r1 => vec![GridOp::restyle(wb, r, set(&[T, B]))],
        BorderKind::TopAndBottom => vec![GridOp::restyle(wb, Rect { r1: r.r0, ..r }, set(&[T])), GridOp::restyle(wb, Rect { r0: r.r1, ..r }, set(&[B]))],
        BorderKind::Outside | BorderKind::ThickOutside => grid(split(r.r0, r.r1, &[T], &[B], &[T, B], &[]), split(r.c0, r.c1, &[L], &[R], &[L, R], &[])),
        // Between cells: each cell's inner sides, so either neighbour shows it.
        BorderKind::Inside => grid(split(r.r0, r.r1, &[B], &[T], &[], &[T, B]), split(r.c0, r.c1, &[R], &[L], &[], &[L, R])),
        BorderKind::InsideHorizontal => grid(split(r.r0, r.r1, &[B], &[T], &[], &[T, B]), all_cols),
        BorderKind::InsideVertical => grid(all_rows, split(r.c0, r.c1, &[R], &[L], &[], &[L, R])),
        BorderKind::DiagonalDown | BorderKind::DiagonalUp => {
            let k = usize::from(kind == BorderKind::DiagonalUp);
            vec![GridOp::restyle(wb, r, move |st: &Style| {
                let mut st = st.clone();
                st.diagonals[k] = edge;
                st
            })]
        }
    }
}

/// The longest text the Borders bar reads.
const MAX_BORDERS_TYPED: usize = 512;

/// The Borders bar's words for line styles, as Excel's Format Cells names
/// them (longest first, so `medium dashed` is not read as `medium`).
const LINE_WORDS: [(&str, Line); 13] = [
    ("medium dash dot dot", Line::MediumDashDotDot),
    ("medium dash dot", Line::MediumDashDot),
    ("slanted dash dot", Line::SlantDashDot),
    ("medium dashed", Line::MediumDashed),
    ("dash dot dot", Line::DashDotDot),
    ("dash dot", Line::DashDot),
    ("dashed", Line::Dashed),
    ("dotted", Line::Dotted),
    ("double", Line::Double),
    ("medium", Line::Medium),
    ("thick", Line::Thick),
    ("thin", Line::Thin),
    ("hair", Line::Hair),
];

const EDGE_WORDS: [(&str, BorderKind); 14] = [
    ("inside horizontal", BorderKind::InsideHorizontal),
    ("inside vertical", BorderKind::InsideVertical),
    ("diagonal down", BorderKind::DiagonalDown),
    ("diagonal up", BorderKind::DiagonalUp),
    ("top and bottom", BorderKind::TopAndBottom),
    ("outside", BorderKind::Outside),
    ("inside", BorderKind::Inside),
    ("bottom", BorderKind::Bottom),
    ("right", BorderKind::Right),
    ("left", BorderKind::Left),
    ("none", BorderKind::None),
    ("top", BorderKind::Top),
    ("all", BorderKind::All),
    ("box", BorderKind::Outside),
];

/// One clause of the Borders bar: `outside dashed red`, `inside thin
/// 1F4E79`, `diagonal up`, `none`.
fn border_clause(t: &str, pen: (Line, Option<u32>)) -> Result<(BorderKind, Edge), String> {
    let mut rest = t.trim().to_ascii_lowercase();
    let take = |rest: &mut String, word: &str| -> bool {
        if rest == word || rest.starts_with(&format!("{word} ")) {
            *rest = rest[word.len()..].trim_start().to_string();
            true
        } else {
            false
        }
    };
    let kind = EDGE_WORDS.iter().find(|(w, _)| take(&mut rest, w)).map(|e| e.1).ok_or_else(|| format!("Name the edges first: all, outside, inside, top, bottom, left, right, inside horizontal, inside vertical, diagonal down, diagonal up or none (not \"{}\").", t.trim()))?;
    let _ = take(&mut rest, "border") || take(&mut rest, "borders");
    let line = LINE_WORDS.iter().find(|(w, _)| take(&mut rest, w)).map_or(pen.0, |e| e.1);
    let color = match rest.trim() {
        "" => pen.1,
        "automatic" | "auto" | "black" => None,
        c => Some(crate::tabs::COLORS.iter().find(|(_, n)| n.eq_ignore_ascii_case(c)).map(|e| e.0).or_else(|| hex_color(c)).ok_or_else(|| format!("\"{c}\" isn't a colour: use a name such as red or blue, or six hex digits."))?),
    };
    Ok((kind, Edge { line, color }))
}

fn hex_color(c: &str) -> Option<u32> {
    let h = c.strip_prefix('#').unwrap_or(c);
    (h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit())).then(|| u32::from_str_radix(h, 16).ok()).flatten()
}

/// Borders…: clauses separated by `;`, each the edges, then a line
/// style and a colour if not the pen's (`outside thick; inside dotted
/// 808080`). All are one step, in the order typed.
pub fn borders_typed(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    if typed.len() > MAX_BORDERS_TYPED {
        return Err("That's too long.".into());
    }
    let clauses: Vec<(BorderKind, Edge)> = typed.split(';').filter(|c| !c.trim().is_empty()).map(|c| border_clause(c, d.border_pen)).collect::<Result<_, _>>()?;
    if clauses.is_empty() {
        return Err("Type the edges, for example outside thick; inside dotted.".into());
    }
    let r = Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    for (i, &(kind, edge)) in clauses.iter().enumerate() {
        let ops = border_ops(&d.wb, r, kind, edge);
        if i == 0 {
            if !d.run_action("Borders", ops) {
                return Err(std::mem::take(&mut d.status));
            }
        } else {
            d.extend(ops);
        }
    }
    Ok(clauses.iter().map(|c| border_done(c.0).trim_end_matches('.')).collect::<Vec<_>>().join("; ") + ".")
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
    fn side_inside_and_diagonal_borders_and_the_borders_bar() {
        use super::{borders_typed, run, BorderKind, FormatCmd};
        use officina_grid::style::Line;
        use officina_grid::CellAddr;
        use pergola::widgets::{CellRange, Dash, GridSource};
        let d = std::rc::Rc::new(std::cell::RefCell::new(crate::doc::Doc::from_csv("b.csv", b"1,2,3\n4,5,6\n7,8,9\n").unwrap().0));
        let src = crate::source::SheetSource::new(d.clone());
        let sel = CellRange { r0: 0, c0: 0, r1: 2, c1: 2 };
        let edges = |r: u32, c: u32| d.borrow().wb.style(CellAddr::new(0, r, c)).borders.map(|e| e.line != Line::None);
        // Inside: each cell's inner sides only (left, right, top, bottom).
        run(&mut d.borrow_mut(), sel, FormatCmd::Borders(BorderKind::Inside));
        assert_eq!(edges(0, 0), [false, true, false, true]);
        assert_eq!(edges(1, 1), [true, true, true, true]);
        assert_eq!(edges(2, 2), [true, false, true, false]);
        assert_eq!(edges(0, 2), [true, false, false, true]);
        run(&mut d.borrow_mut(), sel, FormatCmd::Borders(BorderKind::None));
        assert_eq!(edges(1, 1), [false; 4]);
        for (kind, cell, want) in [(BorderKind::Top, (0, 1), [false, false, true, false]), (BorderKind::Left, (1, 0), [true, false, false, false]), (BorderKind::Right, (2, 2), [false, true, false, false])] {
            run(&mut d.borrow_mut(), sel, FormatCmd::Borders(kind));
            assert_eq!(edges(cell.0, cell.1), want, "{kind:?}");
            assert_eq!(edges(1, 1), [false; 4], "{kind:?} leaves the middle");
        }
        run(&mut d.borrow_mut(), sel, FormatCmd::Borders(BorderKind::DiagonalUp));
        let st = d.borrow().wb.style(CellAddr::new(0, 1, 1)).clone();
        assert!(st.diagonals[1].line == Line::Thin && st.diagonals[0].line == Line::None);
        run(&mut d.borrow_mut(), sel, FormatCmd::Borders(BorderKind::None));
        assert_eq!(d.borrow().wb.style(CellAddr::new(0, 1, 1)).diagonals[1].line, Line::None, "No Borders takes diagonals off too");
        // The bar: several clauses, one step, each with its line and colour.
        let msg = borders_typed(&mut d.borrow_mut(), sel, "outside thick red; inside horizontal dashed #1F4E79").unwrap();
        assert_eq!(msg, "Border around the selection; Borders between the rows.");
        let st = |r: u32, c: u32| d.borrow().wb.style(CellAddr::new(0, r, c)).clone();
        assert_eq!((st(0, 0).borders[2].line, st(0, 0).borders[2].color), (Line::Thick, Some(0xC62828)));
        assert_eq!((st(1, 1).borders[3].line, st(1, 1).borders[3].color), (Line::Dashed, Some(0x1F4E79)));
        assert_eq!(st(1, 1).borders[0].line, Line::None);
        // Drawn dashed.
        let cv = src.cell(1, 1).unwrap();
        assert_eq!((cv.dashes[3], cv.dashes[2]), (Dash::Dashed, Dash::Dashed));
        assert_eq!(src.cell(0, 0).unwrap().dashes[2], Dash::Solid);
        d.borrow_mut().undo();
        assert_eq!(st(1, 1).borders[3].line, Line::None, "one undo takes both clauses");
        assert_eq!(st(0, 0).borders[2].line, Line::None);
        // Medium dashed is not read as medium; the pen gives what is not typed.
        borders_typed(&mut d.borrow_mut(), sel, "all medium dashed").unwrap();
        assert_eq!(st(2, 2).borders[0].line, Line::MediumDashed);
        borders_typed(&mut d.borrow_mut(), sel, "diagonal down hair").unwrap();
        assert_eq!(st(0, 0).diagonals[0].line, Line::Hair);
        // The pen offers the thick, double and dash-dot lines too.
        run(&mut d.borrow_mut(), sel, FormatCmd::PenLine(6));
        assert_eq!(d.borrow().status, "Borders now draw double lines.");
        run(&mut d.borrow_mut(), sel, FormatCmd::Borders(BorderKind::Bottom));
        assert_eq!(st(2, 1).borders[3].line, Line::Double);
        assert_eq!(src.cell(2, 1).unwrap().dashes[3], Dash::Double);
        for bad in ["", "sideways", "all wavy", "top thin 12345G", &"all ".repeat(200)] {
            assert!(borders_typed(&mut d.borrow_mut(), sel, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn font_effects_and_spread_alignments_toggle_and_reach_the_grid() {
        use super::{run, FormatCmd};
        use officina_grid::style::{HAlign, Script};
        use officina_grid::CellAddr;
        use pergola::inline::Emphasis;
        use pergola::widgets::{grid_view::Spread, CellRange, GridSource};
        let d = std::rc::Rc::new(std::cell::RefCell::new(crate::doc::Doc::from_csv("f.csv", b"H2O,x\n").unwrap().0));
        let src = crate::source::SheetSource::new(d.clone());
        let sel = CellRange { r0: 0, c0: 0, r1: 0, c1: 0 };
        let st = |d: &std::rc::Rc<std::cell::RefCell<crate::doc::Doc>>| d.borrow().wb.style(CellAddr::new(0, 0, 0)).clone();
        run(&mut d.borrow_mut(), sel, FormatCmd::Script(Script::Super));
        assert_eq!(st(&d).font.script, Script::Super);
        assert!(src.cell(0, 0).unwrap().marks & Emphasis::Super as u8 != 0);
        // Subscript replaces it; asked again, back to normal.
        run(&mut d.borrow_mut(), sel, FormatCmd::Script(Script::Sub));
        assert_eq!(st(&d).font.script, Script::Sub);
        run(&mut d.borrow_mut(), sel, FormatCmd::Script(Script::Sub));
        assert_eq!(st(&d).font.script, Script::Normal);
        run(&mut d.borrow_mut(), sel, FormatCmd::DoubleUnderline);
        assert!(st(&d).font.underline && st(&d).font.double);
        let m = src.cell(0, 0).unwrap().marks;
        assert!(m & Emphasis::Underline as u8 != 0 && m & Emphasis::Double as u8 != 0);
        // Plain Underline off takes the double away too.
        run(&mut d.borrow_mut(), sel, FormatCmd::Underline);
        assert!(!st(&d).font.underline && !st(&d).font.double);
        // Accounting: across the cell, single or double; asked again, off.
        run(&mut d.borrow_mut(), sel, FormatCmd::Accounting(true));
        assert!(st(&d).font.underline && st(&d).font.double && st(&d).font.accounting);
        assert!(src.cell(0, 0).unwrap().marks & Emphasis::Accounting as u8 != 0);
        run(&mut d.borrow_mut(), sel, FormatCmd::Accounting(false));
        assert!(st(&d).font.accounting && !st(&d).font.double);
        run(&mut d.borrow_mut(), sel, FormatCmd::Accounting(false));
        assert!(!st(&d).font.underline && !st(&d).font.accounting);
        for (a, spread) in [(HAlign::Fill, Spread::Fill), (HAlign::Justify, Spread::Justify), (HAlign::Distributed, Spread::Distributed), (HAlign::Left, Spread::None)] {
            run(&mut d.borrow_mut(), sel, FormatCmd::Align(a));
            assert_eq!(src.cell(0, 0).unwrap().spread, spread);
        }
        assert!(d.borrow_mut().undo());
        assert_eq!(st(&d).h_align, HAlign::Distributed);
    }

    #[test]
    fn next_comment_steps_in_reading_order_and_wraps() {
        use officina_grid::comments::Comment;
        let mut wb = officina_grid::Workbook::new();
        wb.add_sheet("S").unwrap();
        assert_eq!(crate::data::next_comment(&wb, 0, (0, 0), false), None);
        wb.set_comments(0, vec![(2, 1, Some(Comment::new("a", "x"))), (0, 3, Some(Comment::new("a", "y"))), (2, 0, Some(Comment::new("", "z")))]);
        let n = |at, back| crate::data::next_comment(&wb, 0, at, back);
        assert_eq!(n((0, 0), false), Some((0, 3)));
        assert_eq!(n((0, 3), false), Some((2, 0)));
        assert_eq!(n((2, 0), false), Some((2, 1)));
        assert_eq!(n((2, 1), false), Some((0, 3)), "wraps");
        assert_eq!(n((0, 3), true), Some((2, 1)), "back wraps");
        assert_eq!(n((2, 1), true), Some((2, 0)));
    }

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
pub const PEN_LINES: [(&str, Line); 9] = [
    ("Thin", Line::Thin),
    ("Medium", Line::Medium),
    ("Dashed", Line::Dashed),
    ("Dotted", Line::Dotted),
    ("Hair", Line::Hair),
    ("Thick", Line::Thick),
    ("Double", Line::Double),
    ("Medium Dashed", Line::MediumDashed),
    ("Dash Dot", Line::DashDot),
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
    if crate::sheet_protection::refused(&d.status) {
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
    if crate::sheet_protection::refused(&d.status) {
        return Err(d.status.clone());
    }
    Ok(if deg == 0 { "Rotation taken off.".into() } else { format!("Text turned {deg}°.") })
}
