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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderKind {
    All,
    Outside,
    Bottom,
    None,
}

/// The number formats the menu offers: name and format code.
pub const NUMBER_FORMATS: [(&str, &str); 8] = [
    ("General", "General"),
    ("Number", "#,##0.00"),
    ("Currency", "$#,##0.00"),
    ("Percent", "0%"),
    ("Date", "yyyy-mm-dd"),
    ("Time", "hh:mm"),
    ("Scientific", "0.00E+00"),
    ("Text", "@"),
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

/// Font sizes Larger and Smaller step through, points.
pub const SIZES: [u16; 14] = [8, 9, 10, 11, 12, 14, 16, 18, 20, 24, 28, 36, 48, 72];

/// The menu, read from the selection's first cell.
pub fn menu(d: &Doc, sel: CellRange) -> Vec<(MenuItem, Option<FormatCmd>)> {
    let st = d.wb.style(CellAddr::new(d.sheet, sel.r0, sel.c0));
    let pt = st.font.size.unwrap_or(220) / 20;
    let mut v = vec![
        (MenuItem::new("Bold").checked(st.font.bold).shortcut("Ctrl+B"), Some(FormatCmd::Bold)),
        (MenuItem::new("Italic").checked(st.font.italic).shortcut("Ctrl+I"), Some(FormatCmd::Italic)),
        (MenuItem::new("Underline").checked(st.font.underline).shortcut("Ctrl+U"), Some(FormatCmd::Underline)),
        (MenuItem::new("Strikethrough").checked(st.font.strike).shortcut("Ctrl+5"), Some(FormatCmd::Strike)),
        (MenuItem::new(format!("Larger Text ({pt} pt)")).separated(), Some(FormatCmd::Larger)),
        (MenuItem::new("Smaller Text"), Some(FormatCmd::Smaller)),
        (MenuItem::new("Text Colour…"), Some(FormatCmd::Colors(false))),
        (MenuItem::new("Fill Colour…"), Some(FormatCmd::Colors(true))),
        (MenuItem::new("Align Left").checked(st.h_align == HAlign::Left).separated(), Some(FormatCmd::Align(HAlign::Left))),
        (MenuItem::new("Align Centre").checked(st.h_align == HAlign::Center), Some(FormatCmd::Align(HAlign::Center))),
        (MenuItem::new("Align Right").checked(st.h_align == HAlign::Right), Some(FormatCmd::Align(HAlign::Right))),
        (MenuItem::new("Align by Value").checked(st.h_align == HAlign::General), Some(FormatCmd::Align(HAlign::General))),
    ];
    let keys = ["Ctrl+Shift+~", "Ctrl+Shift+!", "Ctrl+Shift+$", "Ctrl+Shift+%", "Ctrl+Shift+#", "Ctrl+Shift+@", "Ctrl+Shift+^", ""];
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
    v.push((MenuItem::new("More Decimals"), Some(FormatCmd::MoreDecimals)));
    v.push((MenuItem::new("Fewer Decimals"), Some(FormatCmd::FewerDecimals)));
    v.push((MenuItem::new("All Borders").separated(), Some(FormatCmd::Borders(BorderKind::All))));
    v.push((MenuItem::new("Outside Borders"), Some(FormatCmd::Borders(BorderKind::Outside))));
    v.push((MenuItem::new("Bottom Border"), Some(FormatCmd::Borders(BorderKind::Bottom))));
    v.push((MenuItem::new("No Borders"), Some(FormatCmd::Borders(BorderKind::None))));
    v
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
            let thin = Edge { line: Line::Thin, color: None };
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
                BorderKind::Bottom => vec![GridOp::restyle(&d.wb, Rect { r0: r.r1, ..r }, set(&[B]))],
                BorderKind::Outside => {
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
