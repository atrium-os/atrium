//! Conditional formats in `.ods`, as LibreOffice keeps them: `calcext:conditional-formats`
//! at the end of each `table:table`, each condition naming a cell style of
//! `office:styles` (OpenDocument 1.3 itself has only `style:map` on cell styles,
//! which can't say "top 10" or "colour scale").
//!
//! LibreOffice applies the first condition of a format that matches a cell and no
//! other; Excel applies every rule that matches until one says stop. Rules are
//! written in their order, and read back as stopping.

use std::fmt::Write as _;

use officina_grid::addr::{col_name, Rect};
use officina_grid::condfmt::{Cfvo, CmpOp, CondFormat, Dxf, Rule, RuleKind, TextOp};
use officina_grid::Workbook;

use super::formula::{sheet_a1, to_of};

/// The cell styles conditions apply, named `CF1`, `CF2`…: one per distinct format.
pub fn dxfs(wb: &Workbook) -> Vec<Dxf> {
    crate::xlsx::condfmt::dxfs(wb)
}

fn esc(s: &str) -> String {
    let mut o = String::new();
    super::write::esc(&mut o, s);
    o
}

fn hex(c: u32) -> String {
    format!("#{:06x}", c & 0xFF_FFFF)
}

/// The `office:styles` entries for the formats: their data styles, then the cell
/// styles.
pub fn styles_xml(list: &[Dxf]) -> String {
    let mut data = String::new();
    let mut cells = String::new();
    for (i, d) in list.iter().enumerate() {
        let n = i + 1;
        let ds = d.num_fmt.as_ref().and_then(|code| super::write::data_styles(code, &format!("CFN{n}")));
        let _ = write!(cells, r#"<style:style style:name="CF{n}" style:family="table-cell" style:parent-style-name="Default""#);
        if let Some(x) = ds {
            data.push_str(&x);
            let _ = write!(cells, r#" style:data-style-name="CFN{n}""#);
        }
        cells.push('>');
        if let Some(f) = d.fill {
            let _ = write!(cells, r#"<style:table-cell-properties fo:background-color="{}"/>"#, hex(f));
        }
        let mut tp = String::new();
        if let Some(b) = d.bold {
            let _ = write!(tp, r#" fo:font-weight="{}""#, if b { "bold" } else { "normal" });
        }
        if let Some(b) = d.italic {
            let _ = write!(tp, r#" fo:font-style="{}""#, if b { "italic" } else { "normal" });
        }
        if let Some(b) = d.underline {
            let _ = write!(tp, r#" style:text-underline-style="{}""#, if b { "solid" } else { "none" });
            if b {
                tp.push_str(r#" style:text-underline-width="auto" style:text-underline-color="font-color""#);
            }
        }
        if let Some(b) = d.strike {
            let _ = write!(tp, r#" style:text-line-through-style="{}""#, if b { "solid" } else { "none" });
        }
        if let Some(c) = d.color {
            let _ = write!(tp, r#" fo:color="{}""#, hex(c));
        }
        if !tp.is_empty() {
            let _ = write!(cells, "<style:text-properties{tp}/>");
        }
        cells.push_str("</style:style>");
    }
    data + &cells
}

fn cell_addr(sheet: &str, r: u32, c: u32) -> String {
    format!("{sheet}.{}{}", col_name(c), r + 1)
}

fn rect_addr(sheet: &str, r: &Rect) -> String {
    let a = cell_addr(sheet, r.r0, r.c0);
    if r.r0 == r.r1 && r.c0 == r.c1 {
        a
    } else {
        format!("{a}:{}", cell_addr(sheet, r.r1, r.c1))
    }
}

/// A rule formula as LibreOffice writes one in a condition: OpenFormula without
/// its namespace, relative to the base cell.
fn of(wb: &Workbook, e: &officina_grid::formula::Expr, origin: (u32, u32)) -> String {
    let t = to_of(&format!("={}", wb.cond_formula_text(e, origin)));
    t.strip_prefix("of:=").unwrap_or(&t).to_string()
}

fn text_lit(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

fn cfvo(wb: &Workbook, p: &Cfvo, origin: (u32, u32)) -> (&'static str, String) {
    match p {
        Cfvo::Min => ("minimum", "0".into()),
        Cfvo::Max => ("maximum", "0".into()),
        Cfvo::Num(n) => ("number", super::write::num(*n)),
        Cfvo::Percent(n) => ("percent", super::write::num(*n)),
        Cfvo::Percentile(n) => ("percentile", super::write::num(*n)),
        Cfvo::Formula(e) => ("formula", of(wb, e, origin)),
    }
}

/// The condition text for a rule that sets a style.
fn condition(wb: &Workbook, f: &CondFormat, rule: &Rule) -> String {
    let o = f.origin();
    let first = format!("[.{}{}]", col_name(o.1), o.0 + 1);
    // The format's ranges, absolute, for the statistics LibreOffice can't express.
    let all = || {
        let sh = sheet_a1(&wb.sheet_name(f.sheet));
        f.ranges.iter().map(|r| format!("[{sh}.${}${}:{sh}.${}${}]", col_name(r.c0), r.r0 + 1, col_name(r.c1), r.r1 + 1)).collect::<Vec<_>>().join(";")
    };
    match &rule.kind {
        RuleKind::CellIs { op, a, b } => {
            let x = of(wb, a, o);
            let y = b.as_ref().map(|b| of(wb, b, o)).unwrap_or_default();
            match op {
                CmpOp::Between => format!("between({x},{y})"),
                CmpOp::NotBetween => format!("not-between({x},{y})"),
                CmpOp::Equal => format!("={x}"),
                CmpOp::NotEqual => format!("!={x}"),
                CmpOp::Greater => format!(">{x}"),
                CmpOp::Less => format!("<{x}"),
                CmpOp::GreaterOrEqual => format!(">={x}"),
                CmpOp::LessOrEqual => format!("<={x}"),
            }
        }
        RuleKind::Expression(e) => format!("formula-is({})", of(wb, e, o)),
        RuleKind::Text { op, text } => {
            let t = text_lit(text);
            match op {
                TextOp::Contains => format!("contains-text({t})"),
                TextOp::NotContains => format!("not-contains-text({t})"),
                TextOp::BeginsWith => format!("begins-with({t})"),
                TextOp::EndsWith => format!("ends-with({t})"),
            }
        }
        // As Excel writes them in its own files.
        RuleKind::Blanks(true) => format!("formula-is(LEN(TRIM({first}))=0)"),
        RuleKind::Blanks(false) => format!("formula-is(LEN(TRIM({first}))>0)"),
        RuleKind::Errors(true) => "is-error".into(),
        RuleKind::Errors(false) => "is-no-error".into(),
        RuleKind::Top { n, percent, bottom } => format!("{}-{}({n})", if *bottom { "bottom" } else { "top" }, if *percent { "percent" } else { "elements" }),
        RuleKind::Average { below, equal, std_dev: 0 } => format!("{}{}-average", if *below { "below" } else { "above" }, if *equal { "-equal" } else { "" }),
        RuleKind::Average { below, equal, std_dev } => {
            let op = match (below, equal) {
                (false, false) => ">",
                (false, true) => ">=",
                (true, false) => "<",
                (true, true) => "<=",
            };
            let sign = if *below { '-' } else { '+' };
            let r = all();
            format!("formula-is(AND(ISNUMBER({first});{first}{op}AVERAGE({r}){sign}{std_dev}*STDEV({r})))")
        }
        RuleKind::Duplicates { unique: false } => "duplicate".into(),
        RuleKind::Duplicates { unique: true } => "unique".into(),
        RuleKind::ColorScale(_) | RuleKind::DataBar { .. } | RuleKind::IconSet { .. } => String::new(),
    }
}

/// A sheet's `calcext:conditional-formats`, or nothing.
pub fn sheet_xml(wb: &Workbook, s: u16, list: &[Dxf]) -> String {
    let mut out = String::new();
    let sh = sheet_a1(&wb.sheet_name(s));
    for f in wb.cond_formats().iter().filter(|f| f.sheet == s) {
        if out.is_empty() {
            out.push_str("<calcext:conditional-formats>");
        }
        let o = f.origin();
        let target = f.ranges.iter().map(|r| rect_addr(&sh, r)).collect::<Vec<_>>().join(" ");
        let _ = write!(out, r#"<calcext:conditional-format calcext:target-range-address="{}">"#, esc(&target));
        let base = esc(&cell_addr(&sh, o.0, o.1));
        for rule in &f.rules {
            match &rule.kind {
                RuleKind::ColorScale(points) => {
                    out.push_str("<calcext:color-scale>");
                    for (p, c) in points {
                        let (t, v) = cfvo(wb, p, o);
                        let _ = write!(out, r#"<calcext:color-scale-entry calcext:value="{}" calcext:type="{t}" calcext:color="{}"/>"#, esc(&v), hex(*c));
                    }
                    out.push_str("</calcext:color-scale>");
                }
                RuleKind::DataBar { min, max, color } => {
                    // Excel's bars start at a tenth of the cell.
                    let _ = write!(out, r##"<calcext:data-bar calcext:positive-color="{}" calcext:negative-color="#ff0000" calcext:gradient="true" calcext:axis-position="none" calcext:min-length="10" calcext:max-length="100">"##, hex(*color));
                    for p in [min, max] {
                        let (t, v) = cfvo(wb, p, o);
                        let _ = write!(out, r#"<calcext:formatting-entry calcext:value="{}" calcext:type="{t}"/>"#, esc(&v));
                    }
                    out.push_str("</calcext:data-bar>");
                }
                // LibreOffice reads show-value on the set and writes it on the first
                // entry: both carry it. ODF has no reverse or strict thresholds (they
                // are lost, as LibreOffice loses them).
                RuleKind::IconSet { set, points, show_value, .. } => {
                    let name = officina_grid::condfmt::ICON_SETS[*set as usize].0;
                    let hide = if *show_value { "" } else { r#" calcext:show-value="false""# };
                    let _ = write!(out, r#"<calcext:icon-set calcext:icon-set-type="{name}"{hide}>"#);
                    for (i, (p, _)) in points.iter().enumerate() {
                        let (t, v) = cfvo(wb, p, o);
                        let hide = if i == 0 && !*show_value { r#" calcext:show-value="false""# } else { "" };
                        let _ = write!(out, r#"<calcext:formatting-entry{hide} calcext:value="{}" calcext:type="{t}"/>"#, esc(&v));
                    }
                    out.push_str("</calcext:icon-set>");
                }
                _ => {
                    let style = list.iter().position(|d| *d == rule.dxf).map_or(0, |i| i + 1);
                    let _ = write!(out, r#"<calcext:condition calcext:apply-style-name="CF{style}" calcext:value="{}" calcext:base-cell-address="{base}"/>"#, esc(&condition(wb, f, rule)));
                }
            }
        }
        out.push_str("</calcext:conditional-format>");
    }
    if !out.is_empty() {
        out.push_str("</calcext:conditional-formats>");
    }
    out
}

// ---------------------------------------------------------------------------
// Reading.

/// One format as read: its target, base cell and entries.
#[derive(Default, Debug)]
pub struct FormatIn {
    pub target: String,
    pub entries: Vec<EntryIn>,
}

#[derive(Debug)]
pub enum EntryIn {
    Condition { style: String, value: String, base: String },
    Scale(Vec<(String, String, String)>),
    Bar { color: String, points: Vec<(String, String)> },
    /// An icon set: its name, whether values show, its points.
    Icons { set: String, show_value: bool, points: Vec<(String, String)> },
    /// Date conditions: not modelled.
    Other(String),
}

fn color_of(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix('#')?, 16).ok()
}

/// Splits `name(args)` into the name and its arguments at the top level (commas
/// outside strings, brackets and parentheses).
fn call(s: &str) -> Option<(&str, Vec<String>)> {
    let open = s.find('(')?;
    let inner = s[open + 1..].strip_suffix(')')?;
    let mut args = Vec::new();
    let (mut depth, mut quote, mut cur) = (0i32, false, String::new());
    for c in inner.chars() {
        match c {
            '"' => quote = !quote,
            '(' | '[' | '{' if !quote => depth += 1,
            ')' | ']' | '}' if !quote => depth -= 1,
            ',' if !quote && depth == 0 => {
                args.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    args.push(cur);
    Some((&s[..open], args))
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    Some(s.strip_prefix('"')?.strip_suffix('"')?.replace("\"\"", "\""))
}

#[cfg(feature = "read")]
/// The rules of a format read, or why not.
pub fn make(wb: &Workbook, f: &FormatIn, style: &dyn Fn(&str) -> Dxf) -> Result<CondFormat, String> {
    let mut ranges = Vec::new();
    let mut sheet = None;
    for part in f.target.split_whitespace() {
        let (s, r0, c0, r1, c1) = super::read::target_rect(wb, part).ok_or_else(|| format!("range {part}"))?;
        if sheet.is_some_and(|x| x != s) {
            return Err("ranges on two sheets".into());
        }
        sheet = Some(s);
        ranges.push(Rect { sheet: s, r0, c0, r1, c1 });
    }
    let sheet = sheet.ok_or("no range")?;
    let origin = (ranges[0].r0, ranges[0].c0);
    let formula = |src: &str, base: (u32, u32)| -> Result<std::sync::Arc<officina_grid::formula::Expr>, String> {
        let a1 = super::formula::from_of(&format!("of:={src}")).ok_or_else(|| format!("formula {src}"))?;
        // Relative to the base cell: a cell's references move by its offset from it.
        let e = wb.parse(&a1, sheet, base.0, base.1).map_err(|_| format!("formula {src}"))?;
        Ok(std::sync::Arc::new(e))
    };
    let point = |t: &str, v: &str| -> Result<Cfvo, String> {
        let n = || v.trim().parse::<f64>().map_err(|_| format!("value {v}"));
        Ok(match t {
            "minimum" | "auto-minimum" => Cfvo::Min,
            "maximum" | "auto-maximum" => Cfvo::Max,
            "number" => Cfvo::Num(n()?),
            "percent" => Cfvo::Percent(n()?),
            "percentile" => Cfvo::Percentile(n()?),
            "formula" => Cfvo::Formula(formula(v, origin)?),
            _ => return Err(format!("point {t}")),
        })
    };
    let mut rules = Vec::new();
    for e in &f.entries {
        match e {
            EntryIn::Scale(points) => {
                let pts = points.iter().map(|(t, v, c)| Ok((point(t, v)?, color_of(c).ok_or("colour")?))).collect::<Result<Vec<_>, String>>()?;
                rules.push(Rule { kind: RuleKind::ColorScale(pts), dxf: Dxf::default(), stop: false });
            }
            EntryIn::Bar { color, points } => {
                if points.len() != 2 {
                    return Err("data bar".into());
                }
                let kind = RuleKind::DataBar { min: point(&points[0].0, &points[0].1)?, max: point(&points[1].0, &points[1].1)?, color: color_of(color).ok_or("colour")? };
                rules.push(Rule { kind, dxf: Dxf::default(), stop: false });
            }
            EntryIn::Icons { set: name, show_value, points } => {
                let set = officina_grid::condfmt::icon_set(name).ok_or_else(|| format!("icon set {name}"))?;
                let n = officina_grid::condfmt::ICON_SETS[set as usize].1 as usize;
                let pts = points.iter().map(|(t, v)| Ok((point(t, v)?, true))).collect::<Result<Vec<_>, String>>()?;
                let pts = if pts.len() == n { pts } else { officina_grid::condfmt::icon_points(n as u8) };
                rules.push(Rule { kind: RuleKind::IconSet { set, points: pts, reverse: false, show_value: *show_value }, dxf: Dxf::default(), stop: false });
            }
            EntryIn::Other(what) => return Err(what.trim_start_matches("calcext:").into()),
            EntryIn::Condition { style: st, value, base } => {
                let base = super::read::target_rect(wb, base).map_or(origin, |b| (b.1, b.2));
                let v = value.trim();
                let f1 = |s: &str| formula(s, base);
                let kind = if let Some(x) = v.strip_prefix("formula-is(").and_then(|x| x.strip_suffix(')')) {
                    RuleKind::Expression(f1(x)?)
                } else if v.starts_with("between(") || v.starts_with("not-between(") {
                    let (n, args) = call(v).ok_or("between")?;
                    if args.len() != 2 {
                        return Err(format!("condition {v}"));
                    }
                    RuleKind::CellIs { op: if n == "between" { CmpOp::Between } else { CmpOp::NotBetween }, a: f1(&args[0])?, b: Some(f1(&args[1])?) }
                } else if let Some((n, args)) = call(v).filter(|(n, _)| n.chars().all(|c| c.is_ascii_lowercase() || c == '-')) {
                    let arg = args.first().map(String::as_str).unwrap_or("");
                    let num = || arg.trim().parse::<u32>().map_err(|_| format!("condition {v}"));
                    let text = || unquote(arg).map(|t| std::sync::Arc::from(t.as_str())).ok_or_else(|| format!("condition {v}"));
                    match n {
                        "top-elements" => RuleKind::Top { n: num()?, percent: false, bottom: false },
                        "bottom-elements" => RuleKind::Top { n: num()?, percent: false, bottom: true },
                        "top-percent" => RuleKind::Top { n: num()?, percent: true, bottom: false },
                        "bottom-percent" => RuleKind::Top { n: num()?, percent: true, bottom: true },
                        "contains-text" => RuleKind::Text { op: TextOp::Contains, text: text()? },
                        "not-contains-text" => RuleKind::Text { op: TextOp::NotContains, text: text()? },
                        "begins-with" => RuleKind::Text { op: TextOp::BeginsWith, text: text()? },
                        "ends-with" => RuleKind::Text { op: TextOp::EndsWith, text: text()? },
                        _ => return Err(format!("condition {v}")),
                    }
                } else {
                    match v {
                        "duplicate" => RuleKind::Duplicates { unique: false },
                        "unique" => RuleKind::Duplicates { unique: true },
                        "is-error" => RuleKind::Errors(true),
                        "is-no-error" => RuleKind::Errors(false),
                        "above-average" => RuleKind::Average { below: false, equal: false, std_dev: 0 },
                        "below-average" => RuleKind::Average { below: true, equal: false, std_dev: 0 },
                        "above-equal-average" => RuleKind::Average { below: false, equal: true, std_dev: 0 },
                        "below-equal-average" => RuleKind::Average { below: true, equal: true, std_dev: 0 },
                        _ => {
                            let ops = [("!=", CmpOp::NotEqual), (">=", CmpOp::GreaterOrEqual), ("<=", CmpOp::LessOrEqual), ("=", CmpOp::Equal), (">", CmpOp::Greater), ("<", CmpOp::Less)];
                            let (op, rest) = ops.iter().find_map(|(p, op)| v.strip_prefix(p).map(|r| (*op, r))).ok_or_else(|| format!("condition {v}"))?;
                            RuleKind::CellIs { op, a: f1(rest)?, b: None }
                        }
                    }
                };
                // LibreOffice stops at the first condition that matches.
                rules.push(Rule { kind, dxf: style(st), stop: true });
            }
        }
    }
    if rules.is_empty() {
        return Err("no rules".into());
    }
    Ok(CondFormat { sheet, ranges, rules })
}
