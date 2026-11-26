//! Data Analysis's tests and models (docs/spec/officina.md §16.162), as
//! the Analysis ToolPak lays them out: ANOVA (single factor, two-factor
//! with and without replication), the F-test for variances, the three
//! t-tests, the z-test, Regression and Fourier Analysis. All but Fourier
//! are written as formulas, so they follow the data.

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::book::Input;
use officina_grid::{Value, Workbook};

use crate::analysis::{abs_ref, at, num, numbers, option_num, series, text, unknown, Options, Table};
use crate::whatif::parse_area;

/// The most groups ANOVA compares, and predictors Regression fits.
const MAX_GROUPS: usize = 200;
const MAX_PREDICTORS: u32 = 16;
/// The most rows Regression lists residuals for.
const MAX_RESIDUALS: u32 = 100_000;
/// The longest series Fourier Analysis transforms (a power of two).
const MAX_FOURIER: usize = 4096;

/// Whether `tool` reads a second range after the first.
pub(crate) fn two_ranges(tool: &str) -> bool {
    tool.contains("test") || tool == "regression"
}

fn f(s: String) -> Input {
    Input::Formula(s)
}

fn alpha(o: &Options) -> Result<f64, String> {
    let a = option_num(o, "alpha")?.unwrap_or(0.05);
    if a > 0.0 && a < 1.0 { Ok(a) } else { Err("Alpha is between 0 and 1, such as alpha 0.05.".into()) }
}

/// Runs `tool` when it is one of this module's, else `None`.
pub(crate) fn run(wb: &Workbook, here: u16, tool: &str, parts: &[&str], o: &Options) -> Result<Option<Table>, String> {
    let first = || parse_area(wb, here, parts[0]).map_err(|e| if parts[0].trim().is_empty() { format!("Type the input range after {tool}.") } else { e });
    let second = || match parts.get(1).map(|p| p.trim()) {
        Some(p) if !p.is_empty() => parse_area(wb, here, p),
        _ => Err(if tool == "regression" { "Type the Y range, then the X range: regression B1:B20, C1:D20.".into() } else { format!("Type the two ranges to compare: {tool} A1:A20, B1:B20.") }),
    };
    Ok(Some(match tool {
        "anova" | "anova single factor" => anova1(wb, &first()?, o)?,
        "anova2" | "two-factor anova" => anova2(wb, &first()?, o)?,
        "ftest" | "f-test" => ftest(wb, &first()?, &second()?, o)?,
        "ttest paired" | "t-test paired" => ttest(wb, &first()?, &second()?, o, Kind::Paired)?,
        "ttest equal" | "t-test equal" => ttest(wb, &first()?, &second()?, o, Kind::Equal)?,
        "ttest unequal" | "t-test unequal" => ttest(wb, &first()?, &second()?, o, Kind::Unequal)?,
        "ztest" | "z-test" => ttest(wb, &first()?, &second()?, o, Kind::Z)?,
        "regression" => regression(wb, &first()?, &second()?, o)?,
        "fourier" => fourier(wb, &first()?, o)?,
        _ => return Ok(None),
    }))
}

/// The ANOVA table's rows from `top`: the header, then each source with
/// SS, df and MS, F, P and F crit for those in `tested`; the last source
/// (the error) is what each F divides by.
fn anova_rows(top: usize, sources: &[(&str, Input, Input)], tested: usize, alpha: f64, total: (Input, Input)) -> Vec<Vec<Input>> {
    let err = top + 1 + sources.len();
    let mut rows = vec![vec![text("ANOVA")], ["Source of Variation", "SS", "df", "MS", "F", "P-value", "F crit"].map(text).to_vec()];
    for (i, (name, ss, df)) in sources.iter().enumerate() {
        let r = top + 2 + i;
        let mut row = vec![text(name), ss.clone(), df.clone(), f(format!("={}/{}", at(r, 1), at(r, 2)))];
        if i < tested {
            row.extend([
                f(format!("={}/{}", at(r, 3), at(err, 3))),
                f(format!("=F.DIST.RT({},{},{})", at(r, 4), at(r, 2), at(err, 2))),
                f(format!("=F.INV.RT({alpha},{},{})", at(r, 2), at(err, 2))),
            ]);
        }
        rows.push(row);
    }
    rows.push(Vec::new());
    rows.push(vec![text("Total"), total.0, total.1]);
    rows
}

/// Anova: Single Factor, groups in columns.
fn anova1(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["alpha"])?;
    let a = alpha(o)?;
    let groups = series(wb, area, o.labels)?;
    if groups.len() < 2 || groups.len() > MAX_GROUPS {
        return Err(format!("ANOVA compares 2 to {MAX_GROUPS} groups, one a column."));
    }
    let refs: Vec<String> = groups.iter().map(|g| abs_ref(wb, &g.area)).collect();
    let all = refs.join(",");
    let k = groups.len();
    let mut rows = vec![vec![text("Anova: Single Factor")], Vec::new(), vec![text("SUMMARY")], ["Groups", "Count", "Sum", "Average", "Variance"].map(text).to_vec()];
    for (g, r) in groups.iter().zip(&refs) {
        rows.push(vec![text(&g.label), f(format!("=COUNT({r})")), f(format!("=SUM({r})")), f(format!("=AVERAGE({r})")), f(format!("=VAR.S({r})"))]);
    }
    rows.push(Vec::new());
    let top = rows.len();
    let total_row = top + 5;
    let within: Vec<String> = refs.iter().map(|r| format!("DEVSQ({r})")).collect();
    rows.extend(anova_rows(top, &[
        ("Between Groups", f(format!("={}-{}", at(total_row, 1), at(top + 3, 1))), num(k as f64 - 1.0)),
        ("Within Groups", f(format!("={}", within.join("+"))), f(format!("=COUNT({all})-{k}"))),
    ], 1, a, (f(format!("=DEVSQ({all})")), f(format!("=COUNT({all})-1")))));
    Ok(Table { name: "Anova Single Factor", rows, formats: Vec::new(), say: format!("Compared {k} groups") })
}

/// Anova: Two-Factor, rows and columns. `rows N` (N rows a sample) makes
/// it with replication; then the first column labels each sample.
fn anova2(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["alpha", "rows"])?;
    let a = alpha(o)?;
    let per = option_num(o, "rows")?;
    let lab = u32::from(o.labels || per.is_some());
    if area.r1 < area.r0 + lab + 1 || area.c1 < area.c0 + lab + 1 {
        return Err("Two-factor ANOVA needs at least two rows and two columns of data.".into());
    }
    let (r0, c0) = (area.r0 + lab, area.c0 + lab);
    let cols: Vec<u32> = (c0..=area.c1).collect();
    if cols.len() > MAX_GROUPS {
        return Err(format!("Compare at most {MAX_GROUPS} columns."));
    }
    let cell_text = |r: u32, c: u32| wb.formatted(CellAddr::new(area.sheet, r, c)).text;
    let col_label = |i: usize, c: u32| if lab == 1 && !cell_text(area.r0, c).is_empty() { cell_text(area.r0, c) } else { format!("Column {}", i + 1) };
    let rect = |ra: u32, ca: u32, rb: u32, cb: u32| abs_ref(wb, &Rect { sheet: area.sheet, r0: ra, c0: ca, r1: rb, c1: cb });
    let data = rect(r0, c0, area.r1, area.c1);
    let nc = cols.len();
    let Some(per) = per else {
        // Without replication: one value for each row and column.
        let nr = (area.r1 - r0 + 1) as usize;
        if nr > MAX_GROUPS {
            return Err(format!("Compare at most {MAX_GROUPS} rows."));
        }
        let mut rows = vec![vec![text("Anova: Two-Factor Without Replication")], Vec::new(), ["SUMMARY", "Count", "Sum", "Average", "Variance"].map(text).to_vec()];
        let summary = |label: String, r: String| vec![text(&label), f(format!("=COUNT({r})")), f(format!("=SUM({r})")), f(format!("=AVERAGE({r})")), f(format!("=VAR.S({r})"))];
        let row_top = rows.len();
        for (i, r) in (r0..=area.r1).enumerate() {
            let label = if lab == 1 && !cell_text(r, area.c0).is_empty() { cell_text(r, area.c0) } else { format!("Row {}", i + 1) };
            rows.push(summary(label, rect(r, c0, r, area.c1)));
        }
        rows.push(Vec::new());
        let col_top = rows.len();
        for (i, &c) in cols.iter().enumerate() {
            rows.push(summary(col_label(i, c), rect(r0, c, area.r1, c)));
        }
        rows.push(Vec::new());
        let top = rows.len();
        let total = top + 6;
        let avgs = |from: usize, n: usize| format!("{}:{}", at(from, 3), at(from + n - 1, 3));
        rows.extend(anova_rows(top, &[
            ("Rows", f(format!("={nc}*DEVSQ({})", avgs(row_top, nr))), num(nr as f64 - 1.0)),
            ("Columns", f(format!("={nr}*DEVSQ({})", avgs(col_top, nc))), num(nc as f64 - 1.0)),
            ("Error", f(format!("={}-{}-{}", at(total, 1), at(top + 2, 1), at(top + 3, 1))), num(((nr - 1) * (nc - 1)) as f64)),
        ], 2, a, (f(format!("=DEVSQ({data})")), num((nr * nc) as f64 - 1.0))));
        return Ok(Table { name: "Anova Two-Factor", rows, formats: Vec::new(), say: format!("Compared {nr} rows and {nc} columns") });
    };
    // With replication: samples of `per` rows, labelled in the first column.
    if per.fract() != 0.0 || per < 2.0 {
        return Err("Rows a sample is a whole number, 2 or more, such as rows 4.".into());
    }
    let per = per as u32;
    let height = area.r1 - r0 + 1;
    if height % per != 0 {
        return Err(format!("The {height} rows of data don't split into samples of {per}."));
    }
    let ns = (height / per) as usize;
    if ns < 2 || ns * nc > 1024 {
        return Err("With replication, use 2 or more samples and at most 1,024 sample-column cells.".into());
    }
    let mut head = vec![text("SUMMARY")];
    head.extend(cols.iter().enumerate().map(|(i, &c)| text(&col_label(i, c))));
    head.push(text("Total"));
    let mut rows = vec![vec![text("Anova: Two-Factor With Replication")], Vec::new(), head];
    let mut within = Vec::new();
    let sample_top = rows.len();
    for s in 0..ns {
        let (ra, rb) = (r0 + s as u32 * per, r0 + (s as u32 + 1) * per - 1);
        let label = cell_text(ra, area.c0);
        let label = if label.is_empty() { format!("Sample {}", s + 1) } else { label };
        let mut row = vec![text(&format!("{label} (Average)"))];
        for &c in &cols {
            let r = rect(ra, c, rb, c);
            row.push(f(format!("=AVERAGE({r})")));
            within.push(format!("DEVSQ({r})"));
        }
        row.push(f(format!("=AVERAGE({})", rect(ra, c0, rb, area.c1))));
        rows.push(row);
    }
    let mut total_row = vec![text("Total (Average)")];
    total_row.extend(cols.iter().map(|&c| f(format!("=AVERAGE({})", rect(r0, c, area.r1, c)))));
    total_row.push(f(format!("=AVERAGE({data})")));
    let col_avgs = format!("{}:{}", at(rows.len(), 1), at(rows.len(), nc));
    rows.push(total_row);
    rows.push(Vec::new());
    let sample_avgs = format!("{}:{}", at(sample_top, nc + 1), at(sample_top + ns - 1, nc + 1));
    let top = rows.len();
    let total = top + 7;
    let n = per as usize;
    rows.extend(anova_rows(top, &[
        ("Sample", f(format!("={}*DEVSQ({sample_avgs})", nc * n)), num(ns as f64 - 1.0)),
        ("Columns", f(format!("={}*DEVSQ({col_avgs})", ns * n)), num(nc as f64 - 1.0)),
        ("Interaction", f(format!("={}-{}-{}-{}", at(total, 1), at(top + 2, 1), at(top + 3, 1), at(top + 5, 1))), num(((ns - 1) * (nc - 1)) as f64)),
        ("Within", f(format!("={}", within.join("+"))), num((ns * nc * (n - 1)) as f64)),
    ], 3, a, (f(format!("=DEVSQ({data})")), num((ns * nc * n) as f64 - 1.0))));
    Ok(Table { name: "Anova Two-Factor", rows, formats: Vec::new(), say: format!("Compared {ns} samples of {n} rows in {nc} columns") })
}

/// The two variables of a two-sample tool: their labels and data.
fn pair(wb: &Workbook, a: &Rect, b: &Rect, o: &Options) -> Result<[(String, String, usize); 2], String> {
    let one = |r: &Rect, i: usize| -> Result<(String, String, usize), String> {
        if r.c0 != r.c1 && r.r0 != r.r1 {
            return Err("Each variable is one column or one row.".into());
        }
        let s = series(wb, &Rect { c1: r.c0, ..*r }, o.labels)?;
        let s = &s[0];
        let area = if r.c0 == r.c1 { s.area } else { Rect { r0: r.r0, r1: r.r0, c0: r.c0 + u32::from(o.labels), c1: r.c1, sheet: r.sheet } };
        let n = numbers(wb, &area).len();
        if n < 2 {
            return Err(format!("Variable {i} needs at least two numbers."));
        }
        let label = if o.labels { s.label.clone() } else { format!("Variable {i}") };
        Ok((label, abs_ref(wb, &area), n))
    };
    Ok([one(a, 1)?, one(b, 2)?])
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Paired,
    Equal,
    Unequal,
    Z,
}

/// F-Test Two-Sample for Variances.
fn ftest(wb: &Workbook, a: &Rect, b: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["alpha"])?;
    let al = alpha(o)?;
    let [(l1, r1, _), (l2, r2, _)] = pair(wb, a, b, o)?;
    let both = |name: &str, g: &dyn Fn(&str) -> String| vec![text(name), f(g(&r1)), f(g(&r2))];
    let fx = at(7, 1);
    let rows = vec![
        vec![text("F-Test Two-Sample for Variances")],
        Vec::new(),
        vec![Input::Empty, text(&l1), text(&l2)],
        both("Mean", &|r| format!("=AVERAGE({r})")),
        both("Variance", &|r| format!("=VAR.S({r})")),
        both("Observations", &|r| format!("=COUNT({r})")),
        both("df", &|r| format!("=COUNT({r})-1")),
        vec![text("F"), f(format!("={}/{}", at(4, 1), at(4, 2)))],
        vec![text("P(F<=f) one-tail"), f(format!("=IF({fx}>=1,F.DIST.RT({fx},{d1},{d2}),F.DIST({fx},{d1},{d2},TRUE))", d1 = at(6, 1), d2 = at(6, 2)))],
        vec![text("F Critical one-tail"), f(format!("=IF({fx}>=1,F.INV.RT({al},{d1},{d2}),F.INV({al},{d1},{d2}))", d1 = at(6, 1), d2 = at(6, 2)))],
    ];
    Ok(Table { name: "F-Test", rows, formats: Vec::new(), say: "Compared the two variances".into() })
}

/// t-Test (paired, equal or unequal variances) or z-Test (`variances A B`).
fn ttest(wb: &Workbook, a: &Rect, b: &Rect, o: &Options, kind: Kind) -> Result<Table, String> {
    unknown(o, &["alpha", "difference", "variances"])?;
    let al = alpha(o)?;
    let h = option_num(o, "difference")?.unwrap_or(0.0);
    let [(l1, r1, n1), (l2, r2, n2)] = pair(wb, a, b, o)?;
    if kind == Kind::Paired && n1 != n2 {
        return Err(format!("A paired test needs as many numbers in each: {n1} and {n2}."));
    }
    let known = if kind == Kind::Z {
        let v = o.rest.iter().find_map(|r| r.strip_prefix("variances")).ok_or("A z-test needs the known variances: variances 4 9.")?;
        let v: Vec<f64> = v.split_whitespace().map(str::parse).collect::<Result<_, _>>().map_err(|_| "Type the known variances as two numbers: variances 4 9.")?;
        match v[..] {
            [x, y] if x > 0.0 && y > 0.0 => Some((x, y)),
            _ => return Err("Type the known variances as two numbers over 0: variances 4 9.".into()),
        }
    } else {
        None
    };
    let both = |name: &str, g: &dyn Fn(&str) -> String| vec![text(name), f(g(&r1)), f(g(&r2))];
    let (m1, m2, v1, v2, c1, c2) = (at(3, 1), at(3, 2), at(4, 1), at(4, 2), at(5, 1), at(5, 2));
    let title = match kind {
        Kind::Paired => "t-Test: Paired Two Sample for Means",
        Kind::Equal => "t-Test: Two-Sample Assuming Equal Variances",
        Kind::Unequal => "t-Test: Two-Sample Assuming Unequal Variances",
        Kind::Z => "z-Test: Two Sample for Means",
    };
    let mut rows = vec![vec![text(title)], Vec::new(), vec![Input::Empty, text(&l1), text(&l2)], both("Mean", &|r| format!("=AVERAGE({r})"))];
    match known {
        Some((x, y)) => rows.push(vec![text("Known Variance"), num(x), num(y)]),
        None => rows.push(both("Variance", &|r| format!("=VAR.S({r})"))),
    }
    rows.push(both("Observations", &|r| format!("=COUNT({r})")));
    let extra = match kind {
        Kind::Paired => Some(vec![text("Pearson Correlation"), f(format!("=CORREL({r1},{r2})"))]),
        Kind::Equal => Some(vec![text("Pooled Variance"), f(format!("=(({c1}-1)*{v1}+({c2}-1)*{v2})/({c1}+{c2}-2)"))]),
        _ => None,
    };
    let pooled = extra.is_some().then(|| at(6, 1));
    rows.extend(extra);
    let hr = rows.len();
    rows.push(vec![text("Hypothesized Mean Difference"), num(h)]);
    let hc = at(hr, 1);
    let stat_row = if kind == Kind::Z { hr + 1 } else { hr + 2 };
    let (stat, df) = (at(stat_row, 1), at(hr + 1, 1));
    match kind {
        Kind::Z => rows.push(vec![text("z"), f(format!("=({m1}-{m2}-{hc})/SQRT({v1}/{c1}+{v2}/{c2})"))]),
        _ => {
            rows.push(vec![text("df"), match kind {
                Kind::Paired => f(format!("={c1}-1")),
                Kind::Equal => f(format!("={c1}+{c2}-2")),
                _ => f(format!("=ROUND(({v1}/{c1}+{v2}/{c2})^2/(({v1}/{c1})^2/({c1}-1)+({v2}/{c2})^2/({c2}-1)),0)")),
            }]);
            rows.push(vec![text("t Stat"), f(match kind {
                Kind::Paired => format!("=({m1}-{m2}-{hc})/SQRT(({v1}+{v2}-2*COVARIANCE.S({r1},{r2}))/{c1})"),
                Kind::Equal => format!("=({m1}-{m2}-{hc})/SQRT({}*(1/{c1}+1/{c2}))", pooled.as_deref().unwrap_or("0")),
                _ => format!("=({m1}-{m2}-{hc})/SQRT({v1}/{c1}+{v2}/{c2})"),
            })]);
        }
    }
    if kind == Kind::Z {
        rows.extend([
            vec![text("P(Z<=z) one-tail"), f(format!("=1-NORM.S.DIST(ABS({stat}),TRUE)"))],
            vec![text("z Critical one-tail"), f(format!("=NORM.S.INV(1-{al})"))],
            vec![text("P(Z<=z) two-tail"), f(format!("=2*(1-NORM.S.DIST(ABS({stat}),TRUE))"))],
            vec![text("z Critical two-tail"), f(format!("=NORM.S.INV(1-{al}/2)"))],
        ]);
    } else {
        rows.extend([
            vec![text("P(T<=t) one-tail"), f(format!("=T.DIST.RT(ABS({stat}),{df})"))],
            vec![text("t Critical one-tail"), f(format!("=T.INV(1-{al},{df})"))],
            vec![text("P(T<=t) two-tail"), f(format!("=T.DIST.2T(ABS({stat}),{df})"))],
            vec![text("t Critical two-tail"), f(format!("=T.INV.2T({al},{df})"))],
        ]);
    }
    Ok(Table { name: if kind == Kind::Z { "z-Test" } else { "t-Test" }, rows, formats: Vec::new(), say: "Compared the two means".into() })
}

/// Regression: `regression Y, X` (X up to 16 columns), `constant zero`,
/// `confidence 95%`, `residuals`. Every number is an INDEX into LINEST.
fn regression(wb: &Workbook, y: &Rect, x: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["constant zero", "confidence", "residuals"])?;
    let zero = o.rest.iter().any(|r| r == "constant zero");
    let residuals = o.rest.iter().any(|r| r == "residuals");
    let conf = option_num(o, "confidence")?.unwrap_or(0.95);
    if !(conf > 0.0 && conf < 1.0) {
        return Err("The confidence is between 0% and 100%, such as confidence 95%.".into());
    }
    if y.c0 != y.c1 {
        return Err("The Y range is one column.".into());
    }
    if x.r1 - x.r0 != y.r1 - y.r0 {
        return Err("The X range has as many rows as the Y range.".into());
    }
    let k = x.c1 - x.c0 + 1;
    if k > MAX_PREDICTORS {
        return Err(format!("Fit at most {MAX_PREDICTORS} X columns."));
    }
    let skip = u32::from(o.labels);
    let n = y.r1 - y.r0 + 1 - skip;
    if n < k + 2 {
        return Err("Regression needs more rows of data than X columns, and at least two more.".into());
    }
    let ys = Rect { r0: y.r0 + skip, ..*y };
    let xs = Rect { r0: x.r0 + skip, ..*x };
    let (yr, xr) = (abs_ref(wb, &ys), abs_ref(wb, &xs));
    let l = format!("LINEST({yr},{xr},{},TRUE)", if zero { "FALSE" } else { "TRUE" });
    let ix = |r: u32, c: u32| f(format!("=INDEX({l},{r},{c})"));
    let x_label = |j: u32| {
        let t = if o.labels { wb.formatted(CellAddr::new(x.sheet, x.r0, x.c0 + j)).text } else { String::new() };
        if t.trim().is_empty() { format!("X Variable {}", j + 1) } else { t }
    };
    let pct = (conf * 1000.0).round() / 10.0;
    let mut rows = vec![
        vec![text("SUMMARY OUTPUT")],
        Vec::new(),
        vec![text("Regression Statistics")],
        vec![text("Multiple R"), f(format!("=SQRT({})", at(4, 1)))],
        vec![text("R Square"), ix(3, 1)],
        vec![text("Adjusted R Square"), f(format!("=1-(1-{})*({}-{})/{}", at(4, 1), at(7, 1), u32::from(!zero), at(12, 1)))],
        vec![text("Standard Error"), ix(3, 2)],
        vec![text("Observations"), num(f64::from(n))],
        Vec::new(),
        vec![text("ANOVA")],
        vec![Input::Empty, text("df"), text("SS"), text("MS"), text("F"), text("Significance F")],
        vec![text("Regression"), num(f64::from(k)), ix(5, 1), f(format!("={}/{}", at(11, 2), at(11, 1))), ix(4, 1), f(format!("=F.DIST.RT({},{},{})", at(11, 4), at(11, 1), at(12, 1)))],
        vec![text("Residual"), ix(4, 2), ix(5, 2), f(format!("={}/{}", at(12, 2), at(12, 1)))],
        vec![text("Total"), f(format!("={}+{}", at(11, 1), at(12, 1))), f(format!("={}+{}", at(11, 2), at(12, 2)))],
        Vec::new(),
        [String::new(), "Coefficients".into(), "Standard Error".into(), "t Stat".into(), "P-value".into(), format!("Lower {pct}%"), format!("Upper {pct}%")].iter().map(|s| if s.is_empty() { Input::Empty } else { text(s) }).collect(),
    ];
    let crit = format!("T.INV.2T({},{})", ((1.0 - conf) * 1e12).round() / 1e12, at(12, 1));
    for j in 0..=k {
        let r = rows.len();
        // LINEST lists the last X first and the intercept last.
        let c = if j == 0 { k + 1 } else { k + 1 - j };
        let label = if j == 0 { "Intercept".to_string() } else { x_label(j - 1) };
        rows.push(vec![
            text(&label),
            ix(1, c),
            ix(2, c),
            f(format!("={}/{}", at(r, 1), at(r, 2))),
            f(format!("=T.DIST.2T(ABS({}),{})", at(r, 3), at(12, 1))),
            f(format!("={}-{crit}*{}", at(r, 1), at(r, 2))),
            f(format!("={}+{crit}*{}", at(r, 1), at(r, 2))),
        ]);
    }
    if residuals {
        if n > MAX_RESIDUALS {
            return Err(format!("List residuals for at most {MAX_RESIDUALS} rows."));
        }
        let coef = 16;
        rows.push(Vec::new());
        rows.push(vec![text("RESIDUAL OUTPUT")]);
        rows.push(vec![text("Observation"), text("Predicted Y"), text("Residuals")]);
        for i in 0..n {
            let r = rows.len();
            let mut pred = at(coef, 1);
            for j in 0..k {
                pred.push_str(&format!("+{}*{}", at(coef + 1 + j as usize, 1), abs_ref(wb, &Rect::cell(CellAddr::new(x.sheet, xs.r0 + i, x.c0 + j)))));
            }
            rows.push(vec![num(f64::from(i + 1)), f(format!("={pred}")), f(format!("={}-{}", abs_ref(wb, &Rect::cell(CellAddr::new(y.sheet, ys.r0 + i, y.c0))), at(r, 1)))]);
        }
    }
    Ok(Table { name: "Regression", rows, formats: Vec::new(), say: format!("Fitted {k} X {} to {n} rows", if k == 1 { "column" } else { "columns" }) })
}

/// A complex number as Excel's IM functions write it: `3`, `-2i`, `1.5-2i`.
fn complex_text(re: f64, im: f64) -> String {
    let g = |x: f64| -> String {
        let y: f64 = format!("{x:.14e}").parse().unwrap_or(x);
        if y == 0.0 { "0".into() } else { y.to_string() }
    };
    match (re == 0.0, im == 0.0) {
        (_, true) => g(re),
        (true, false) => format!("{}i", if im == 1.0 { String::new() } else if im == -1.0 { "-".into() } else { g(im) }),
        (false, false) => {
            let i = if im.abs() == 1.0 { String::new() } else { g(im.abs()) };
            format!("{}{}{i}i", g(re), if im < 0.0 { '-' } else { '+' })
        }
    }
}

/// A number, or complex text such as `1+2i`, `-3j` or `i`.
fn parse_complex(v: &Value) -> Option<(f64, f64)> {
    match v {
        Value::Num(x) => Some((*x, 0.0)),
        Value::Str(s) => {
            let s = s.trim();
            let Some(body) = s.strip_suffix(['i', 'j']) else { return s.parse().ok().map(|x| (x, 0.0)) };
            // The imaginary part starts at the last sign not after an exponent's E.
            let b = body.as_bytes();
            let split = (1..b.len()).rev().find(|&i| (b[i] == b'+' || b[i] == b'-') && !matches!(b[i - 1], b'e' | b'E'));
            let (re, im) = match split { Some(i) => (&body[..i], &body[i..]), None => ("", body) };
            let im = match im { "" | "+" => 1.0, "-" => -1.0, t => t.parse().ok()? };
            let re = if re.is_empty() { 0.0 } else { re.parse().ok()? };
            Some((re, im))
        }
        _ => None,
    }
}

/// Fourier Analysis: the discrete Fourier transform of a column or row of
/// 2, 4, … 4,096 numbers (or complex text), or with `inverse` the inverse.
/// Written as complex text, as the ToolPak does; rounding noise under a
/// 10⁻¹² of the largest part is shown as 0.
fn fourier(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["inverse"])?;
    let inverse = o.rest.iter().any(|r| r == "inverse");
    if area.c0 != area.c1 && area.r0 != area.r1 {
        return Err("Fourier Analysis reads one column or one row.".into());
    }
    let skip = u32::from(o.labels);
    let cells: Vec<CellAddr> = if area.c0 == area.c1 {
        (area.r0 + skip..=area.r1).map(|r| CellAddr::new(area.sheet, r, area.c0)).collect()
    } else {
        (area.c0 + skip..=area.c1).map(|c| CellAddr::new(area.sheet, area.r0, c)).collect()
    };
    let n = cells.len();
    if !n.is_power_of_two() || n < 2 || n > MAX_FOURIER {
        return Err(format!("Fourier Analysis takes 2, 4, 8 … up to {MAX_FOURIER} values; there are {n}."));
    }
    let xs: Vec<(f64, f64)> = cells.iter().map(|&a| parse_complex(&wb.get(a)).ok_or_else(|| format!("{} isn't a number or complex number.", officina_grid::addr::a1(a.row, a.col)))).collect::<Result<_, _>>()?;
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut out = Vec::with_capacity(n);
    for k in 0..n {
        let (mut re, mut im) = (0.0, 0.0);
        for (j, &(a, b)) in xs.iter().enumerate() {
            let t = sign * std::f64::consts::TAU * ((j * k) % n) as f64 / n as f64;
            let (s, c) = t.sin_cos();
            re += a * c - b * s;
            im += a * s + b * c;
        }
        if inverse {
            re /= n as f64;
            im /= n as f64;
        }
        out.push((re, im));
    }
    let big = out.iter().fold(0f64, |m, &(a, b)| m.max(a.abs()).max(b.abs()));
    let snap = |x: f64| if x.abs() <= big * 1e-12 { 0.0 } else { x };
    let label = if o.labels { wb.formatted(CellAddr::new(area.sheet, area.r0, area.c0)).text } else { String::new() };
    let head = if label.is_empty() { (if inverse { "Inverse Fourier" } else { "Fourier" }).to_string() } else { format!("Fourier ({label})") };
    let rows = std::iter::once(vec![text(&head)]).chain(out.into_iter().map(|(a, b)| vec![text(&complex_text(snap(a), snap(b)))])).collect();
    Ok(Table { name: "Fourier Analysis", rows, formats: Vec::new(), say: format!("Transformed {n} values") })
}

#[cfg(test)]
mod tests {
    use crate::analysis::run;
    use crate::doc::Doc;
    use officina_grid::{CellAddr, Value};

    fn v(d: &Doc, r: u32, c: u32) -> f64 {
        match d.wb.get(CellAddr::new(1, r, c)) {
            Value::Num(x) => x,
            other => panic!("({r}, {c}) is {other:?}"),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-6 * b.abs().max(1.0)
    }

    #[test]
    fn single_factor_anova_matches_worked_values() {
        // Groups 1,2,3 / 4,5,6 / 7,8,9: SSB 54, SSW 6, F 27, p 0.001.
        let mut d = Doc::from_csv("a.csv", b"A,B,C\n1,4,7\n2,5,8\n3,6,9\n").unwrap().0;
        run(&mut d, "anova A1:C4, labels").unwrap();
        d.wb.recalc();
        let top = 10; // the ANOVA table's Between Groups row
        assert_eq!(d.wb.get(CellAddr::new(1, top, 0)), Value::str("Between Groups"));
        assert!(close(v(&d, top, 1), 54.0) && close(v(&d, top + 1, 1), 6.0));
        assert_eq!((v(&d, top, 2), v(&d, top + 1, 2)), (2.0, 6.0));
        assert!(close(v(&d, top, 4), 27.0));
        assert!(close(v(&d, top, 5), 0.001), "{}", v(&d, top, 5));
        assert_eq!(d.wb.get(CellAddr::new(1, top + 3, 0)), Value::str("Total"));
        assert!(close(v(&d, top, 6), 5.143252849784718));
        assert!(close(v(&d, top + 3, 1), 60.0));
    }

    #[test]
    fn two_factor_anova_with_and_without_replication() {
        let mut d = Doc::from_csv("b.csv", b",X,Y\nr1,1,4\nr2,2,6\nr3,3,11\n").unwrap().0;
        run(&mut d, "anova2 A1:C4, labels").unwrap();
        d.wb.recalc();
        // Rows SS = 2*DEVSQ(2.5,4,7) = 21, Columns SS = 3*DEVSQ(2,7) = 37.5, total 52.833.
        let top = 12;
        assert_eq!(d.wb.get(CellAddr::new(1, top, 0)), Value::str("Rows"));
        assert!(close(v(&d, top, 1), 21.0) && close(v(&d, top + 1, 1), 37.5));
        assert!(close(v(&d, top + 2, 1), 7.0), "{}", v(&d, top + 2, 1));
        // With replication: two samples of two rows.
        let mut d = Doc::from_csv("c.csv", b",X,Y\ns1,1,5\n,3,7\ns2,2,10\n,4,12\n").unwrap().0;
        run(&mut d, "anova2 A1:C5, rows 2").unwrap();
        d.wb.recalc();
        let top = 9;
        assert_eq!(d.wb.get(CellAddr::new(1, top, 0)), Value::str("Sample"));
        // Sample means 4 and 7: 4*DEVSQ = 18; columns 2.5 and 8.5: 72; within 4*2 = 8; total 106.
        assert!(close(v(&d, top, 1), 18.0) && close(v(&d, top + 1, 1), 72.0) && close(v(&d, top + 3, 1), 8.0));
        assert!(close(v(&d, top + 2, 1), 8.0), "interaction {}", v(&d, top + 2, 1));
        assert_eq!(v(&d, top + 3, 2), 4.0);
        assert!(run(&mut d, "anova2 A1:C4, rows 2").unwrap_err().contains("don't split"));
    }

    #[test]
    fn the_f_t_and_z_tests() {
        let mut d = Doc::from_csv("t.csv", b"A,B\n1,2\n2,4\n3,5\n4,9\n5,10\n").unwrap().0;
        run(&mut d, "ttest paired A1:A6, B1:B6, labels").unwrap();
        d.wb.recalc();
        // Differences -1,-2,-2,-5,-5: mean -3, sd 1.8708, t -3.5857, df 4.
        assert_eq!(v(&d, 8, 1), 4.0);
        assert!(close(v(&d, 9, 1), -3.585685828003181), "{}", v(&d, 9, 1));
        assert!((v(&d, 12, 1) - 0.0231).abs() < 1e-3, "{}", v(&d, 12, 1));
        run(&mut d, "ttest equal A1:A6, B1:B6, labels").unwrap();
        d.wb.recalc();
        // Pooled variance (2.5 + 11.5)/2 = 7, t = -3/sqrt(2.8) = -1.7928.
        let s = 2;
        let g = |r, c| match d.wb.get(CellAddr::new(s, r, c)) { Value::Num(x) => x, o => panic!("{o:?}") };
        assert!(close(g(6, 1), 7.0) && g(8, 1) == 8.0 && close(g(9, 1), -1.7928429140015905));
        run(&mut d, "ttest unequal A1:A6, B1:B6, labels").unwrap();
        d.wb.recalc();
        let g = |r, c| match d.wb.get(CellAddr::new(3, r, c)) { Value::Num(x) => x, o => panic!("{o:?}") };
        assert_eq!(g(7, 1), 6.0, "Welch's df rounded");
        run(&mut d, "ftest A1:A6, B1:B6, labels").unwrap();
        run(&mut d, "ztest A1:A6, B1:B6, labels, variances 2.5 11.5").unwrap();
        d.wb.recalc();
        let g = |s, r, c| match d.wb.get(CellAddr::new(s, r, c)) { Value::Num(x) => x, o => panic!("{o:?}") };
        assert!(close(g(4, 7, 1), 2.5 / 11.5));
        assert!((g(4, 8, 1) - 0.084275).abs() < 1e-4, "{}", g(4, 8, 1));
        assert!(close(g(5, 7, 1), -3.0 / (2.8f64).sqrt()));
        assert!(close(g(5, 11, 1), 1.959963984540054));
        assert!(run(&mut d, "ztest A1:A6, B1:B6").unwrap_err().contains("known variances"));
        assert!(run(&mut d, "ttest paired A1:A6, B1:B5, labels").unwrap_err().contains("as many"));
    }

    #[test]
    fn regression_reads_linest_and_lists_residuals() {
        // y = 1 + 2x exactly, plus one point off by 1.
        let mut d = Doc::from_csv("r.csv", b"Y,X\n3,1\n5,2\n7,3\n10,4\n").unwrap().0;
        run(&mut d, "regression A1:A5, B1:B5, labels, residuals").unwrap();
        d.wb.recalc();
        // Fit: slope 2.3, intercept 0.5; SSreg 26.45 and SSres 0.3 of 26.75.
        assert_eq!(d.wb.get(CellAddr::new(1, 17, 0)), Value::str("X"));
        assert!(close(v(&d, 16, 1), 0.5) && close(v(&d, 17, 1), 2.3), "{} {}", v(&d, 16, 1), v(&d, 17, 1));
        assert!(close(v(&d, 11, 2), 26.45) && close(v(&d, 12, 2), 0.3));
        assert!(close(v(&d, 4, 1), 26.45 / 26.75));
        assert_eq!((v(&d, 11, 1), v(&d, 12, 1), v(&d, 7, 1)), (1.0, 2.0, 4.0));
        // Residuals: predicted 2.8, 5.1, 7.4, 9.7.
        assert!(close(v(&d, 21, 1), 2.8) && close(v(&d, 24, 2), 0.3), "{} {}", v(&d, 21, 1), v(&d, 24, 2));
    }

    #[test]
    fn fourier_transforms_and_inverts() {
        let mut d = Doc::from_csv("f.csv", b"1\n2\n3\n4\n").unwrap().0;
        run(&mut d, "fourier A1:A4, to C1").unwrap();
        let col: Vec<Value> = (1..5).map(|r| d.wb.get(CellAddr::new(0, r, 2))).collect();
        assert_eq!(col, ["10", "-2+2i", "-2", "-2-2i"].map(Value::str));
        run(&mut d, "fourier C2:C5, inverse, to D1").unwrap();
        let back: Vec<Value> = (1..5).map(|r| d.wb.get(CellAddr::new(0, r, 3))).collect();
        assert_eq!(back, ["1", "2", "3", "4"].map(Value::str));
        assert!(run(&mut d, "fourier A1:A3").unwrap_err().contains("2, 4, 8"));
    }
}
