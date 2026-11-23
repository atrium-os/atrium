//! The conditional-format dialog's model (docs/spec/officina.md §16.60): the rules
//! over the selection, a rule's settings as the dialog shows them, and the changes
//! it makes, each one undo step ([`GridOp::cond_formats`]).
//!
//! The panel itself is drawn by [`crate::app`]; everything here is plain data and
//! testable without a window.

use officina_grid::addr::{col_name, parse_a1, MAX_COLS, MAX_ROWS};
use officina_grid::condfmt::{Cfvo, CmpOp, CondFormat, Dxf, Rule, RuleKind, TextOp, ICON_SETS, MAX_RANGES};
use officina_grid::ops::GridOp;
use officina_grid::{Rect, Workbook};

use crate::doc::Doc;

/// The kinds of rule the dialog makes, in its Rule menu.
pub const KINDS: [&str; 11] = ["Cell Value", "Text", "Formula", "Top or Bottom", "Average", "Duplicates", "Blanks", "Errors", "Colour Scale", "Data Bar", "Icon Set"];
const K_VALUE: usize = 0;
const K_TEXT: usize = 1;
const K_FORMULA: usize = 2;
const K_TOP: usize = 3;
const K_AVERAGE: usize = 4;
const K_DUPS: usize = 5;
const K_BLANKS: usize = 6;
const K_ERRORS: usize = 7;
const K_SCALE: usize = 8;
const K_BAR: usize = 9;
const K_ICONS: usize = 10;

/// Excel's ready-made looks for rules that change a cell's text or fill.
pub const LOOKS: [(&str, Option<u32>, Option<u32>, bool); 6] = [
    ("Light Red Fill with Dark Red Text", Some(0xFFC7CE), Some(0x9C0006), false),
    ("Yellow Fill with Dark Yellow Text", Some(0xFFEB9C), Some(0x9C5700), false),
    ("Green Fill with Dark Green Text", Some(0xC6EFCE), Some(0x006100), false),
    ("Light Red Fill", Some(0xFFC7CE), None, false),
    ("Red Text", None, Some(0x9C0006), false),
    ("Bold Text", None, None, true),
];

/// Colour scales, named from the lowest value's colour to the highest's.
pub const SCALES: [(&str, &[u32]); 12] = [
    ("Red – Yellow – Green", &[0xF8696B, 0xFFEB84, 0x63BE7B]),
    ("Green – Yellow – Red", &[0x63BE7B, 0xFFEB84, 0xF8696B]),
    ("Red – White – Green", &[0xF8696B, 0xFCFCFF, 0x63BE7B]),
    ("Green – White – Red", &[0x63BE7B, 0xFCFCFF, 0xF8696B]),
    ("Red – White – Blue", &[0xF8696B, 0xFCFCFF, 0x5A8AC6]),
    ("Blue – White – Red", &[0x5A8AC6, 0xFCFCFF, 0xF8696B]),
    ("White – Red", &[0xFCFCFF, 0xF8696B]),
    ("Red – White", &[0xF8696B, 0xFCFCFF]),
    ("White – Green", &[0xFCFCFF, 0x63BE7B]),
    ("Green – White", &[0x63BE7B, 0xFCFCFF]),
    ("Yellow – Green", &[0xFFEF9C, 0x63BE7B]),
    ("Green – Yellow", &[0x63BE7B, 0xFFEF9C]),
];

/// Data bar colours.
pub const BARS: [(&str, u32); 6] = [("Blue", 0x638EC6), ("Green", 0x63C384), ("Red", 0xFF555A), ("Orange", 0xFFB628), ("Light Blue", 0x008AEF), ("Purple", 0xD6007B)];

/// The icon sets' names as the menu shows them, in [`ICON_SETS`] order.
pub const ICON_NAMES: [&str; 20] = [
    "3 Arrows",
    "3 Arrows (Grey)",
    "3 Flags",
    "3 Traffic Lights",
    "3 Traffic Lights (Rimmed)",
    "3 Signs",
    "3 Symbols (Circled)",
    "3 Symbols",
    "4 Arrows",
    "4 Arrows (Grey)",
    "Red to Black",
    "4 Ratings",
    "4 Traffic Lights",
    "5 Arrows",
    "5 Arrows (Grey)",
    "5 Ratings",
    "5 Quarters",
    "3 Stars",
    "3 Triangles",
    "5 Boxes",
];

const CMP_NAMES: [&str; 8] = ["between", "not between", "equal to", "not equal to", "greater than", "less than", "greater than or equal to", "less than or equal to"];
const TEXT_NAMES: [&str; 4] = ["contains", "does not contain", "begins with", "ends with"];
const TOP_NAMES: [&str; 4] = ["Top", "Bottom", "Top %", "Bottom %"];
const AVG_NAMES: [&str; 4] = ["Above average", "Below average", "At or above average", "At or below average"];

/// The second menu's items for a kind (the comparison, the scale, …); empty when
/// the kind has none.
pub fn choices(kind: usize) -> Vec<String> {
    let v: Vec<&str> = match kind {
        K_VALUE => CMP_NAMES.to_vec(),
        K_TEXT => TEXT_NAMES.to_vec(),
        K_TOP => TOP_NAMES.to_vec(),
        K_AVERAGE => AVG_NAMES.to_vec(),
        K_DUPS => vec!["Duplicate values", "Unique values"],
        K_BLANKS => vec!["Blank cells", "Cells that aren't blank"],
        K_ERRORS => vec!["Cells with errors", "Cells without errors"],
        K_SCALE => SCALES.iter().map(|s| s.0).collect(),
        K_BAR => BARS.iter().map(|s| s.0).collect(),
        K_ICONS => ICON_NAMES.to_vec(),
        _ => vec![],
    };
    v.into_iter().map(String::from).collect()
}

/// What the first field holds for a kind, as its placeholder; `None` when the
/// kind has no field.
pub fn field_a(kind: usize) -> Option<&'static str> {
    match kind {
        K_VALUE => Some("Value or =formula"),
        K_TEXT => Some("Text"),
        K_FORMULA => Some("Formula, e.g. =$B2>100"),
        K_TOP => Some("How many (e.g. 10)"),
        _ => None,
    }
}

/// Whether the kind changes the cell's text and fill (so the Look menu shows).
pub fn has_look(kind: usize) -> bool {
    kind < K_SCALE
}

/// A rule's settings in the dialog. Text fields live in the window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Editor {
    /// The (format, rule) being changed, or a new rule.
    pub editing: Option<(usize, usize)>,
    /// Into [`KINDS`].
    pub kind: usize,
    /// Into [`choices`] of the kind; one past them keeps the rule's own (a
    /// colour scale or bar not in the lists).
    pub choice: usize,
    /// Into [`LOOKS`]; `LOOKS.len()` keeps the rule's own look.
    pub look: usize,
    /// Icon sets: highest values get the first icon.
    pub reverse: bool,
    /// Icon sets: the icon without the value.
    pub icon_only: bool,
    pub stop: bool,
    /// The rule as it was, for what the dialog doesn't show (custom looks,
    /// thresholds, a standard-deviation line).
    pub orig: Option<Rule>,
}

/// The panel: the rules over the selection, or one rule being made or changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CondPanel {
    pub open: bool,
    /// The selection it opened on.
    pub sel: Option<Rect>,
    /// The picked row of [`entries`].
    pub picked: Option<usize>,
    pub editor: Option<Editor>,
    /// Why Save didn't save.
    pub error: String,
}

/// The rules of formats on `sheet` with a range meeting `sel`, as (format, rule),
/// first applied first.
pub fn entries(wb: &Workbook, sel: Rect) -> Vec<(usize, usize)> {
    let meets = |r: &Rect| r.sheet == sel.sheet && r.r0 <= sel.r1 && sel.r0 <= r.r1 && r.c0 <= sel.c1 && sel.c0 <= r.c1;
    let mut v = Vec::new();
    for (fi, f) in wb.cond_formats().iter().enumerate() {
        if f.ranges.iter().any(meets) {
            v.extend((0..f.rules.len()).map(|ri| (fi, ri)));
        }
    }
    v
}

/// A rectangle as A1 text.
pub fn rect_text(r: &Rect) -> String {
    let a = format!("{}{}", col_name(r.c0), r.r0 + 1);
    if r.r0 == r.r1 && r.c0 == r.c1 {
        return a;
    }
    format!("{a}:{}{}", col_name(r.c1), r.r1 + 1)
}

/// Ranges as the Apply To field shows them.
pub fn ranges_text(rs: &[Rect]) -> String {
    rs.iter().map(rect_text).collect::<Vec<_>>().join(", ")
}

/// Ranges typed in the Apply To field: `A1`, `B2:D20`, `C:C` or `3:5`, split by
/// commas or spaces.
pub fn parse_ranges(s: &str, sheet: u16) -> Result<Vec<Rect>, String> {
    let mut v = Vec::new();
    for part in s.split(|c: char| c == ',' || c == ';' || c.is_whitespace()).filter(|p| !p.is_empty()) {
        let p = part.replace('$', "").to_ascii_uppercase();
        let bad = || format!("“{part}” isn't a cell or range.");
        let (a, b) = p.split_once(':').unwrap_or((&p, &p));
        let end = |t: &str| -> Option<(Option<u32>, Option<u32>)> {
            if let Some(rc) = parse_a1(t) {
                return Some((Some(rc.0), Some(rc.1)));
            }
            if !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit()) {
                let r: u32 = t.parse().ok()?;
                return (1..=MAX_ROWS).contains(&r).then_some((Some(r - 1), None));
            }
            if !t.is_empty() && t.bytes().all(|c| c.is_ascii_uppercase()) {
                let (_, c) = parse_a1(&format!("{t}1"))?;
                return Some((None, Some(c)));
            }
            None
        };
        let (ra, ca) = end(a).ok_or_else(bad)?;
        let (rb, cb) = end(b).ok_or_else(bad)?;
        // Both ends whole columns, whole rows, or cells.
        if ra.is_some() != rb.is_some() || ca.is_some() != cb.is_some() {
            return Err(bad());
        }
        let (r0, r1) = match (ra, rb) {
            (Some(x), Some(y)) => (x.min(y), x.max(y)),
            _ => (0, MAX_ROWS - 1),
        };
        let (c0, c1) = match (ca, cb) {
            (Some(x), Some(y)) => (x.min(y), x.max(y)),
            _ => (0, MAX_COLS - 1),
        };
        v.push(Rect { sheet, r0, c0, r1, c1 });
    }
    if v.is_empty() {
        return Err("Say which cells the rule applies to.".into());
    }
    if v.len() > MAX_RANGES {
        return Err(format!("At most {MAX_RANGES} ranges."));
    }
    Ok(v)
}

fn look_of(i: usize) -> Dxf {
    let (_, fill, color, bold) = LOOKS[i.min(LOOKS.len() - 1)];
    Dxf { fill, color, bold: bold.then_some(true), ..Dxf::default() }
}

fn short(s: &str) -> String {
    let t: String = s.chars().take(40).collect();
    if t.len() < s.len() {
        format!("{t}…")
    } else {
        t
    }
}

/// A rule in a few words, for the list.
pub fn describe(wb: &Workbook, f: &CondFormat, rule: &Rule) -> String {
    let o = f.origin();
    let fx = |e: &officina_grid::formula::Expr| short(&wb.cond_formula_text(e, o));
    match &rule.kind {
        RuleKind::CellIs { op, a, b } => match b {
            Some(b) => format!("Cell value {} {} and {}", CMP_NAMES[*op as usize], fx(a), fx(b)),
            None => format!("Cell value {} {}", CMP_NAMES[*op as usize], fx(a)),
        },
        RuleKind::Text { op, text } => format!("Text {} “{}”", TEXT_NAMES[*op as usize], short(text)),
        RuleKind::Expression(e) => format!("Formula ={}", fx(e)),
        RuleKind::Top { n, percent, bottom } => format!("{} {n}{}", if *bottom { "Bottom" } else { "Top" }, if *percent { " %" } else { "" }),
        RuleKind::Average { below, equal, std_dev } => {
            let s = AVG_NAMES[*below as usize + 2 * *equal as usize];
            if *std_dev > 0 {
                format!("{s} by {std_dev} std dev")
            } else {
                s.into()
            }
        }
        RuleKind::Duplicates { unique } => if *unique { "Unique values" } else { "Duplicate values" }.into(),
        RuleKind::Blanks(b) => if *b { "Blank cells" } else { "Cells that aren't blank" }.into(),
        RuleKind::Errors(b) => if *b { "Cells with errors" } else { "Cells without errors" }.into(),
        RuleKind::ColorScale(ps) => match SCALES.iter().find(|s| s.1.len() == ps.len() && s.1.iter().zip(ps).all(|(c, p)| *c == p.1)) {
            Some(s) => format!("Colour scale {}", s.0),
            None => format!("{}-colour scale", ps.len()),
        },
        RuleKind::DataBar { color, .. } => match BARS.iter().find(|b| b.1 == *color) {
            Some(b) => format!("{} data bar", b.0),
            None => "Data bar".into(),
        },
        RuleKind::IconSet { set, reverse, show_value, .. } => {
            let mut s = format!("Icons: {}", ICON_NAMES.get(*set as usize).copied().unwrap_or("?"));
            if *reverse {
                s.push_str(", reversed");
            }
            if !*show_value {
                s.push_str(", icon only");
            }
            s
        }
    }
}

/// The dialog's settings and its two fields' texts for rule `ri` of format `fi`.
pub fn load(wb: &Workbook, fi: usize, ri: usize) -> Option<(Editor, String, String, String)> {
    let f = wb.cond_formats().get(fi)?;
    let rule = f.rules.get(ri)?;
    let o = f.origin();
    let fx = |e: &officina_grid::formula::Expr| {
        let t = wb.cond_formula_text(e, o);
        if t.parse::<f64>().is_ok() {
            t
        } else {
            format!("={t}")
        }
    };
    let mut e = Editor { editing: Some((fi, ri)), stop: rule.stop, orig: Some(rule.clone()), ..Editor::default() };
    e.look = LOOKS.len();
    if let Some(i) = (0..LOOKS.len()).find(|&i| look_of(i) == rule.dxf) {
        e.look = i;
    }
    let (mut a, mut b) = (String::new(), String::new());
    match &rule.kind {
        RuleKind::CellIs { op, a: x, b: y } => {
            e.kind = K_VALUE;
            e.choice = *op as usize;
            a = fx(x);
            b = y.as_ref().map(|y| fx(y)).unwrap_or_default();
        }
        RuleKind::Text { op, text } => {
            e.kind = K_TEXT;
            e.choice = *op as usize;
            a = text.to_string();
        }
        RuleKind::Expression(x) => {
            e.kind = K_FORMULA;
            a = format!("={}", wb.cond_formula_text(x, o));
        }
        RuleKind::Top { n, percent, bottom } => {
            e.kind = K_TOP;
            e.choice = *bottom as usize + 2 * *percent as usize;
            a = n.to_string();
        }
        RuleKind::Average { below, equal, .. } => {
            e.kind = K_AVERAGE;
            e.choice = *below as usize + 2 * *equal as usize;
        }
        RuleKind::Duplicates { unique } => {
            e.kind = K_DUPS;
            e.choice = *unique as usize;
        }
        RuleKind::Blanks(x) => {
            e.kind = K_BLANKS;
            e.choice = !*x as usize;
        }
        RuleKind::Errors(x) => {
            e.kind = K_ERRORS;
            e.choice = !*x as usize;
        }
        RuleKind::ColorScale(ps) => {
            e.kind = K_SCALE;
            e.choice = SCALES.iter().position(|s| s.1.len() == ps.len() && s.1.iter().zip(ps).all(|(c, p)| *c == p.1)).unwrap_or(SCALES.len());
        }
        RuleKind::DataBar { color, .. } => {
            e.kind = K_BAR;
            e.choice = BARS.iter().position(|b| b.1 == *color).unwrap_or(BARS.len());
        }
        RuleKind::IconSet { set, reverse, show_value, .. } => {
            e.kind = K_ICONS;
            e.choice = *set as usize;
            e.reverse = *reverse;
            e.icon_only = !*show_value;
        }
    }
    Some((e, ranges_text(&f.ranges), a, b))
}

/// A new rule's settings on the selection: the first look, the first kind.
pub fn fresh(sel: Rect) -> (Editor, String) {
    (Editor::default(), ranges_text(&[sel]))
}

/// A value typed for a comparison as a formula: `=…` as written, a number, or
/// anything else as text.
fn value_formula(t: &str) -> String {
    let t = t.trim();
    if let Some(f) = t.strip_prefix('=') {
        return f.to_string();
    }
    if t.parse::<f64>().is_ok_and(|x| x.is_finite()) {
        return t.to_string();
    }
    format!("\"{}\"", t.replace('"', "\"\""))
}

/// The rule the dialog's settings and fields make, with its ranges.
pub fn build(wb: &Workbook, sheet: u16, e: &Editor, range: &str, a: &str, b: &str) -> Result<(Vec<Rect>, Rule), String> {
    let ranges = parse_ranges(range, sheet)?;
    let o = (ranges[0].r0, ranges[0].c0);
    let formula = |t: &str, what: &str| wb.cond_formula(t, sheet, o).map_err(|_| format!("The {what} isn't a formula Sheets can read."));
    let orig = e.orig.as_ref().map(|r| &r.kind);
    let kind = match e.kind {
        K_VALUE => {
            let op = CmpOp::ALL[e.choice.min(7)];
            if a.trim().is_empty() {
                return Err("Type the value to compare with.".into());
            }
            let x = formula(&value_formula(a), "value")?;
            let y = if matches!(op, CmpOp::Between | CmpOp::NotBetween) {
                if b.trim().is_empty() {
                    return Err("Type the second value.".into());
                }
                Some(formula(&value_formula(b), "second value")?)
            } else {
                None
            };
            RuleKind::CellIs { op, a: x, b: y }
        }
        K_TEXT => {
            if a.is_empty() {
                return Err("Type the text to look for.".into());
            }
            RuleKind::Text { op: TextOp::ALL[e.choice.min(3)], text: a.chars().take(255).collect::<String>().into() }
        }
        K_FORMULA => {
            if a.trim().trim_start_matches('=').is_empty() {
                return Err("Type a formula that is true for the cells to change.".into());
            }
            RuleKind::Expression(formula(a.trim(), "formula")?)
        }
        K_TOP => {
            let percent = e.choice >= 2;
            let max = if percent { 100 } else { 1000 };
            let n: u32 = a.trim().parse().ok().filter(|n| (1..=max).contains(n)).ok_or_else(|| format!("Type how many, 1 to {max}."))?;
            RuleKind::Top { n, percent, bottom: e.choice % 2 == 1 }
        }
        K_AVERAGE => {
            let std_dev = match orig {
                Some(RuleKind::Average { std_dev, .. }) => *std_dev,
                _ => 0,
            };
            RuleKind::Average { below: e.choice % 2 == 1, equal: e.choice >= 2, std_dev }
        }
        K_DUPS => RuleKind::Duplicates { unique: e.choice == 1 },
        K_BLANKS => RuleKind::Blanks(e.choice == 0),
        K_ERRORS => RuleKind::Errors(e.choice == 0),
        K_SCALE => match (SCALES.get(e.choice), orig) {
            (Some((_, cs)), _) => {
                let pts = if cs.len() == 3 { vec![Cfvo::Min, Cfvo::Percentile(50.0), Cfvo::Max] } else { vec![Cfvo::Min, Cfvo::Max] };
                RuleKind::ColorScale(pts.into_iter().zip(cs.iter().copied()).collect())
            }
            (None, Some(k @ RuleKind::ColorScale(_))) => k.clone(),
            _ => return Err("Pick a colour scale.".into()),
        },
        K_BAR => match (BARS.get(e.choice), orig) {
            (Some((_, color)), Some(RuleKind::DataBar { min, max, .. })) => RuleKind::DataBar { min: min.clone(), max: max.clone(), color: *color },
            (Some((_, color)), _) => RuleKind::DataBar { min: Cfvo::Min, max: Cfvo::Max, color: *color },
            (None, Some(k @ RuleKind::DataBar { .. })) => k.clone(),
            _ => return Err("Pick a bar colour.".into()),
        },
        K_ICONS => {
            let set = e.choice.min(ICON_SETS.len() - 1) as u8;
            let points = match orig {
                // The rule's own thresholds, while it keeps its set.
                Some(RuleKind::IconSet { set: s, points, .. }) if *s == set => points.clone(),
                _ => officina_grid::condfmt::icon_points(ICON_SETS[set as usize].1),
            };
            let icons = match orig {
                Some(RuleKind::IconSet { set: old, icons, .. }) if *old == set => icons.clone(),
                _ => None,
            };
            RuleKind::IconSet { set, points, reverse: e.reverse && icons.is_none(), show_value: !e.icon_only, icons }
        }
        _ => return Err("Pick a kind of rule.".into()),
    };
    let dxf = if !has_look(e.kind) {
        Dxf::default()
    } else if e.look < LOOKS.len() {
        look_of(e.look)
    } else {
        e.orig.as_ref().map(|r| r.dxf.clone()).unwrap_or_else(|| look_of(0))
    };
    Ok((ranges, Rule { kind, dxf, stop: e.stop }))
}

fn commit(d: &mut Doc, label: &str, fs: Vec<CondFormat>) {
    let op = GridOp::cond_formats(&d.wb, &fs);
    d.run(label, vec![op]);
}

/// Saves the dialog's rule: a new one goes first (it wins over older ones, as in
/// Excel); a changed one keeps its place, its format taking the new ranges.
pub fn save(d: &mut Doc, e: &Editor, range: &str, a: &str, b: &str) -> Result<(), String> {
    let (ranges, rule) = build(&d.wb, d.sheet, e, range, a, b)?;
    let mut fs = d.wb.cond_formats().to_vec();
    match e.editing {
        Some((fi, ri)) if fi < fs.len() && ri < fs[fi].rules.len() => {
            fs[fi].ranges = ranges;
            fs[fi].rules[ri] = rule;
            commit(d, "Change Conditional Format", fs);
        }
        Some(_) => return Err("That rule is gone.".into()),
        None => {
            if fs.len() >= officina_grid::condfmt::MAX_FORMATS {
                return Err("This workbook has too many conditional formats.".into());
            }
            fs.insert(0, CondFormat { sheet: d.sheet, ranges, rules: vec![rule] });
            commit(d, "New Conditional Format", fs);
        }
    }
    Ok(())
}

/// Deletes rule `ri` of format `fi` (and the format, if it was its last rule).
pub fn delete(d: &mut Doc, fi: usize, ri: usize) {
    let mut fs = d.wb.cond_formats().to_vec();
    let Some(f) = fs.get_mut(fi) else { return };
    if ri >= f.rules.len() {
        return;
    }
    f.rules.remove(ri);
    if f.rules.is_empty() {
        fs.remove(fi);
    }
    commit(d, "Delete Conditional Format", fs);
}

/// Moves row `k` of the rules over `sel` ([`entries`]) one place earlier (`up`)
/// or later, so it applies before (or after) its neighbour; gives the row it is
/// now at. Rules of one format swap inside it; across formats the whole format
/// moves past the other.
pub fn shift(d: &mut Doc, sel: Rect, k: usize, up: bool) -> Option<usize> {
    let rows = entries(&d.wb, sel);
    let j = if up { k.checked_sub(1)? } else { k + 1 };
    let (&(fa, ra), &(fb, rb)) = (rows.get(k)?, rows.get(j)?);
    let mut fs = d.wb.cond_formats().to_vec();
    if fa == fb {
        fs[fa].rules.swap(ra, rb);
        commit(d, "Reorder Conditional Formats", fs);
        return Some(j);
    }
    // Rows go in format order, so format `fb` is just before (up) or after `fa`
    // among those shown; either way `fa` lands at `fb`'s index.
    let f = fs.remove(fa);
    fs.insert(fb, f);
    commit(d, "Reorder Conditional Formats", fs);
    entries(&d.wb, sel).iter().position(|&x| x == (fb, ra))
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::CellAddr;

    fn doc() -> Doc {
        let mut d = Doc::new("t");
        for i in 0..10u32 {
            d.type_into(i, 0, &(i + 1).to_string());
            d.type_into(i, 1, ["apple", "pear"][i as usize % 2]);
        }
        d
    }

    fn sel(s: &str) -> Rect {
        parse_ranges(s, 0).unwrap()[0]
    }

    #[test]
    fn ranges_read_and_written() {
        assert_eq!(rect_text(&sel("b2:$D$20")), "B2:D20");
        assert_eq!(rect_text(&sel("C5")), "C5");
        let col = sel("C:C");
        assert_eq!((col.r0, col.r1, col.c0, col.c1), (0, MAX_ROWS - 1, 2, 2));
        let rows = sel("3:5");
        assert_eq!((rows.r0, rows.r1, rows.c0, rows.c1), (2, 4, 0, MAX_COLS - 1));
        assert_eq!(parse_ranges("A1:B2, D4", 0).unwrap().len(), 2);
        assert!(parse_ranges("A1:3", 0).is_err() && parse_ranges("", 0).is_err() && parse_ranges("hello!", 0).is_err());
    }

    #[test]
    fn every_kind_and_choice_builds_and_loads_back() {
        let mut d = doc();
        for kind in 0..KINDS.len() {
            let n = choices(kind).len().max(1);
            for choice in 0..n {
                let e = Editor { kind, choice, look: 1, reverse: kind == K_ICONS, icon_only: kind == K_ICONS && choice % 2 == 0, ..Editor::default() };
                let (a, b) = match kind {
                    K_VALUE => ("5", "=$A$1+7"),
                    K_TEXT => ("app", ""),
                    K_FORMULA => ("=MOD(A1,2)=0", ""),
                    K_TOP => ("3", ""),
                    _ => ("", ""),
                };
                save(&mut d, &e, "A1:B10", a, b).unwrap_or_else(|err| panic!("{} {choice}: {err}", KINDS[kind]));
                let (back, range, a2, b2) = load(&d.wb, 0, 0).unwrap();
                assert_eq!((back.kind, back.choice, back.reverse, back.icon_only, range.as_str()), (kind, choice, e.reverse, e.icon_only, "A1:B10"), "{}", KINDS[kind]);
                if has_look(kind) {
                    assert_eq!(back.look, 1);
                }
                // Loaded back, it saves to the same rule.
                let before = d.wb.cond_formats()[0].clone();
                save(&mut d, &back, &range, &a2, &b2).unwrap();
                assert_eq!(d.wb.cond_formats()[0], before, "{} {choice}", KINDS[kind]);
                assert!(!describe(&d.wb, &before, &before.rules[0]).is_empty());
            }
        }
    }

    #[test]
    fn rules_are_made_changed_reordered_and_deleted_as_undo_steps() {
        let mut d = doc();
        let all = sel("A1:B10");
        // Greater than 5: light red.
        let e = Editor { kind: K_VALUE, choice: 4, ..Editor::default() };
        save(&mut d, &e, "A1:A10", "5", "").unwrap();
        assert_eq!(d.wb.look(CellAddr::new(0, 6, 0)).dxf.fill, Some(0xFFC7CE));
        assert!(d.wb.look(CellAddr::new(0, 3, 0)).is_empty());
        // Text "pear" in column B: green; new rules go first.
        save(&mut d, &Editor { kind: K_TEXT, look: 2, ..Editor::default() }, "B1:B10", "pear", "").unwrap();
        assert_eq!(entries(&d.wb, all), vec![(0, 0), (1, 0)]);
        assert!(describe(&d.wb, &d.wb.cond_formats()[0], &d.wb.cond_formats()[0].rules[0]).contains("pear"));
        assert_eq!(d.wb.look(CellAddr::new(0, 1, 1)).dxf.fill, Some(0xC6EFCE));
        // Only column A's rule meets A1.
        assert_eq!(entries(&d.wb, sel("A1")), vec![(1, 0)]);
        // Changing it keeps its place.
        let (mut e, r, _, b) = load(&d.wb, 1, 0).unwrap();
        e.choice = 5; // less than
        save(&mut d, &e, &r, "3", &b).unwrap();
        assert_eq!(d.wb.look(CellAddr::new(0, 0, 0)).dxf.fill, Some(0xFFC7CE));
        assert!(d.wb.look(CellAddr::new(0, 6, 0)).is_empty());
        // Down then up again.
        assert_eq!(shift(&mut d, all, 0, false), Some(1));
        assert!(matches!(d.wb.cond_formats()[0].rules[0].kind, RuleKind::CellIs { .. }));
        assert_eq!(shift(&mut d, all, 1, true), Some(0));
        assert!(matches!(d.wb.cond_formats()[0].rules[0].kind, RuleKind::Text { .. }));
        assert_eq!(shift(&mut d, all, 0, true), None);
        delete(&mut d, 0, 0);
        assert_eq!(d.wb.cond_formats().len(), 1);
        // Each was one undo step: delete, two moves, change, two new rules.
        for _ in 0..4 {
            assert!(d.undo());
        }
        assert_eq!(d.wb.cond_formats().len(), 2);
        assert_eq!(d.wb.look(CellAddr::new(0, 6, 0)).dxf.fill, Some(0xFFC7CE));
        assert!(d.undo() && d.undo());
        assert!(d.wb.cond_formats().is_empty());
    }

    #[test]
    fn bad_input_says_why() {
        let mut d = doc();
        let e = Editor { kind: K_VALUE, choice: 0, ..Editor::default() };
        assert!(save(&mut d, &e, "A1:A10", "1", "").unwrap_err().contains("second"));
        assert!(save(&mut d, &e, "nowhere", "1", "2").unwrap_err().contains("isn't a cell"));
        assert!(save(&mut d, &Editor { kind: K_FORMULA, ..Editor::default() }, "A1", "=SUM(", "").unwrap_err().contains("formula"));
        assert!(save(&mut d, &Editor { kind: K_TOP, choice: 2, ..Editor::default() }, "A1", "300", "").unwrap_err().contains("1 to 100"));
        assert!(d.wb.cond_formats().is_empty());
        // Text typed as a value compares as text.
        save(&mut d, &Editor { kind: K_VALUE, choice: 2, ..Editor::default() }, "B1:B10", "pear", "").unwrap();
        assert_eq!(d.wb.look(CellAddr::new(0, 1, 1)).dxf.fill, Some(0xFFC7CE));
        assert!(d.wb.look(CellAddr::new(0, 0, 1)).is_empty());
    }
}
