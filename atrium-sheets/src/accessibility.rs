//! Alt Text… for charts and Check Accessibility (docs/spec/officina.md
//! §16.159), as Excel's Review ▸ Check Accessibility: a list of what a
//! reader using a screen reader, or with low vision, may miss, stepped
//! through in a bar that goes to each place.

use officina_grid::charts::clean_alt;
use officina_grid::ops::GridOp;
use officina_grid::CellAddr;

use crate::doc::Doc;

/// Problems listed, at most; the rest are counted.
pub const MAX_ISSUES: usize = 1000;
/// Text cells looked at for contrast, a sheet.
const MAX_CELLS: usize = 200_000;
/// WCAG's least contrast for normal text, which Excel's check uses.
const MIN_CONTRAST: f64 = 4.5;

// ---------- Alt Text ----------

/// A chart or a picture.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Float {
    Chart(usize),
    Picture(usize),
}

/// What Alt Text describes at `cursor`: a chart over the cell, else a
/// picture over it, else a chart drawn from the cell.
fn float_at(d: &Doc, cursor: (u32, u32)) -> Option<Float> {
    let (row, col) = cursor;
    let over = |a: &officina_grid::charts::Anchor| a.sheet == d.sheet && (a.from.0..=a.to.0).contains(&row) && (a.from.1..=a.to.1).contains(&col);
    if let Some(i) = d.wb.charts().iter().rposition(|c| over(&c.anchor)) {
        return Some(Float::Chart(i));
    }
    if let Some(i) = d.wb.pictures().iter().rposition(|p| over(&p.anchor)) {
        return Some(Float::Picture(i));
    }
    crate::data::chart_at(&d.wb, d.sheet, row, col).map(Float::Chart)
}

/// Whether Alt Text has something to describe at `cursor`.
pub fn can_describe(d: &Doc, cursor: (u32, u32)) -> bool {
    float_at(d, cursor).is_some()
}

/// The Alt Text bar's text: the chart's or picture's alt text now.
pub fn alt_start(d: &Doc, cursor: (u32, u32)) -> String {
    let alt = match float_at(d, cursor) {
        Some(Float::Chart(i)) => d.wb.charts()[i].alt.clone(),
        Some(Float::Picture(i)) => d.wb.pictures()[i].alt.clone(),
        None => None,
    };
    alt.map(|a| a.replace('\n', " ")).unwrap_or_default()
}

/// Alt Text…: describes the chart or picture at the cursor (`none` clears
/// it). One step.
pub fn set_alt(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let at = float_at(d, cursor).ok_or("Put the cursor on a chart or picture, or on a chart's data, to describe it.")?;
    let t = typed.trim();
    let alt = if t.is_empty() || t.eq_ignore_ascii_case("none") { None } else { clean_alt(t) };
    let (what, op) = match at {
        Float::Chart(i) => {
            let mut charts = d.wb.charts().to_vec();
            if charts[i].alt == alt {
                return Ok("The alt text is already so.".into());
            }
            charts[i].alt = alt.clone();
            ("chart", GridOp::Charts(charts))
        }
        Float::Picture(i) => {
            let mut pictures = d.wb.pictures().to_vec();
            if pictures[i].alt == alt {
                return Ok("The alt text is already so.".into());
            }
            pictures[i].alt = alt.clone();
            ("picture", GridOp::Pictures(pictures))
        }
    };
    d.status.clear();
    if !d.run_action("Alt Text", vec![op]) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok(match alt {
        Some(a) => format!("The {what} is described: “{a}”."),
        None => format!("The {what} has no alt text now."),
    })
}

// ---------- Check Accessibility ----------

#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub sheet: u16,
    pub at: (u32, u32),
    pub say: String,
}

/// A check being stepped through.
#[derive(Debug, Clone)]
pub struct Run {
    pub issues: Vec<Issue>,
    /// More found than listed.
    pub more: usize,
    pub next: usize,
}

/// WCAG's relative luminance of 0xRRGGBB.
fn luminance(rgb: u32) -> f64 {
    let lin = |c: u32| {
        let s = f64::from(c & 0xFF) / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(rgb >> 16) + 0.7152 * lin(rgb >> 8) + 0.0722 * lin(rgb)
}

/// The contrast ratio of two colours, 1 to 21.
pub fn contrast(a: u32, b: u32) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

fn default_name(name: &str) -> bool {
    name.strip_prefix("Sheet").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// What the check finds, sheet by sheet: charts without alt text, tables
/// without a header row, merged cells, text hard to read against its fill,
/// and sheets with content still named SheetN.
pub fn issues(d: &Doc) -> (Vec<Issue>, usize) {
    let wb = &d.wb;
    let mut found = Vec::new();
    let mut more = 0usize;
    let mut add = |issue: Issue| if found.len() < MAX_ISSUES { found.push(issue) } else { more += 1 };
    for sheet in 0..wb.sheet_count() as u16 {
        let name = wb.sheet_name(sheet);
        let mut used = false;
        officina_grid::find::values(wb, sheet, &mut |_, _| { used = true; false });
        if used && default_name(&name) {
            add(Issue { sheet, at: (0, 0), say: format!("Default sheet name: “{name}” says nothing of what the sheet holds. Rename it.") });
        }
        for (n, c) in wb.charts().iter().enumerate().filter(|(_, c)| c.anchor.sheet == sheet) {
            if c.alt.is_none() {
                add(Issue { sheet, at: (c.anchor.from.0, c.anchor.from.1), say: format!("Missing alt text: chart {} has none. Use Alt Text… to describe it.", n + 1) });
            }
        }
        for (n, p) in wb.pictures().iter().enumerate().filter(|(_, p)| p.anchor.sheet == sheet) {
            if p.alt.is_none() {
                add(Issue { sheet, at: (p.anchor.from.0, p.anchor.from.1), say: format!("Missing alt text: picture {} has none. Use Alt Text… to describe it.", n + 1) });
            }
        }
        for t in wb.tables().iter().filter(|t| t.rect.sheet == sheet && !t.header) {
            add(Issue { sheet, at: (t.rect.r0, t.rect.c0), say: format!("No header row: table {} has none, so its columns can't be named to a reader.", t.name) });
        }
        for m in wb.merges(sheet) {
            add(Issue { sheet, at: (m.r0, m.c0), say: format!("Merged cells: {}:{} are merged, which a screen reader can misread.", officina_grid::addr::a1(m.r0, m.c0), officina_grid::addr::a1(m.r1, m.c1)) });
        }
        let mut seen = 0usize;
        officina_grid::find::values(wb, sheet, &mut |at: CellAddr, _| {
            seen += 1;
            let st = wb.style(at);
            let text = st.font.color.unwrap_or(0x000000);
            let fill = st.fill.unwrap_or(0xFFFFFF);
            let ratio = contrast(text, fill);
            if ratio < MIN_CONTRAST {
                add(Issue { sheet, at: (at.row, at.col), say: format!("Hard-to-read text contrast: {} is {ratio:.1}:1 against its fill; aim for {MIN_CONTRAST}:1.", officina_grid::addr::a1(at.row, at.col)) });
            }
            seen < MAX_CELLS
        });
    }
    (found, more)
}

/// The next issue, or the summary at the end.
pub enum Step {
    Show { sheet: u16, at: (u32, u32), say: String },
    Done(String),
}

/// Check Accessibility: runs the check and gives the first issue.
pub fn begin(d: &mut Doc) -> Step {
    let (issues, more) = issues(d);
    if issues.is_empty() {
        d.accessibility = None;
        return Step::Done("No accessibility issues found.".into());
    }
    d.accessibility = Some(Run { issues, more, next: 0 });
    step(d)
}

/// The next issue (the bar's Next).
pub fn step(d: &mut Doc) -> Step {
    let Some(run) = d.accessibility.as_mut() else { return Step::Done("The accessibility check is complete.".into()) };
    let total = run.issues.len() + run.more;
    match run.issues.get(run.next).cloned() {
        Some(issue) => {
            run.next += 1;
            let say = format!("{} ({} of {total}; {}!{})", issue.say, run.next, d.wb.sheet_name(issue.sheet), officina_grid::addr::a1(issue.at.0, issue.at.1));
            Step::Show { sheet: issue.sheet, at: issue.at, say }
        }
        None => {
            let more = run.more;
            d.accessibility = None;
            Step::Done(if more > 0 {
                format!("The accessibility check is complete: {total} issues, {more} past the first {MAX_ISSUES}.")
            } else {
                format!("The accessibility check is complete: {total} {}.", if total == 1 { "issue" } else { "issues" })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::Rect;
    use officina_grid::style::Style;

    #[test]
    fn alt_text_describes_the_chart_and_the_check_sees_it() {
        use officina_grid::charts::*;
        let mut d = Doc::from_csv("c.csv", b"1\n2\n3\n").unwrap().0;
        d.wb.add_chart(Chart {
            kind: ChartKind::Column, grouping: Grouping::Standard, title: None,
            series: vec![Series { name: None, cats: None, vals: Rect { sheet: 0, r0: 0, c0: 0, r1: 2, c1: 0 }, kind: None, look: Default::default() }],
            legend: None, x_title: None, y_title: None,
            anchor: Anchor { sheet: 0, from: (5, 2, 0.0, 0.0), to: (15, 8, 0.0, 0.0) }, alt: None,
        }).unwrap();
        assert!(issues(&d).0.iter().any(|i| i.say.starts_with("Missing alt text") && i.at == (5, 2)));
        assert!(set_alt(&mut d, (9, 9), "x").is_err());
        assert_eq!(set_alt(&mut d, (6, 3), "  Three rising\u{7} values ").unwrap(), "The chart is described: “Three rising values”.");
        assert_eq!(alt_start(&d, (0, 0)), "Three rising values");
        assert!(!issues(&d).0.iter().any(|i| i.say.starts_with("Missing alt text")));
        assert!(d.undo());
        assert_eq!(d.wb.charts()[0].alt, None);
        set_alt(&mut d, (0, 0), "x").unwrap();
        assert_eq!(set_alt(&mut d, (0, 0), "none").unwrap(), "The chart has no alt text now.");
        // A picture over the cell is described ahead of a chart drawn from it.
        d.wb.add_picture(Picture { anchor: Anchor { sheet: 0, from: (0, 0, 0.0, 0.0), to: (2, 1, 0.0, 0.0) }, bytes: b"\x89PNG\r\n\x1a\n".to_vec().into(), link: None, crop: [0; 4], rotation: 0, alt: None }).unwrap();
        assert!(issues(&d).0.iter().any(|i| i.say.starts_with("Missing alt text: picture 1")));
        assert_eq!(set_alt(&mut d, (1, 0), "Company logo").unwrap(), "The picture is described: “Company logo”.");
        assert_eq!(d.wb.pictures()[0].alt.as_deref(), Some("Company logo"));
        assert_eq!(alt_start(&d, (2, 1)), "Company logo");
        assert!(!issues(&d).0.iter().any(|i| i.say.contains("picture")));
    }

    #[test]
    fn contrast_matches_wcag() {
        assert!((contrast(0x000000, 0xFFFFFF) - 21.0).abs() < 1e-9);
        assert!((contrast(0x777777, 0xFFFFFF) - 4.48).abs() < 0.01);
        assert!(contrast(0xFFFF00, 0xFFFFFF) < 1.1);
    }

    #[test]
    fn check_finds_and_steps_through_issues() {
        let mut d = Doc::from_csv("a.csv", b"Name,Qty\nPen,3\n").unwrap().0;
        d.wb.set_style(&Rect::cell(CellAddr::new(0, 1, 1)), Style { font: officina_grid::style::Font { color: Some(0xBBBBBB), ..Default::default() }, ..Style::default() });
        d.run("Merge", vec![GridOp::Merges { sheet: 0, list: vec![Rect { sheet: 0, r0: 3, c0: 0, r1: 3, c1: 1 }] }]);
        let names: Vec<String> = issues(&d).0.iter().map(|i| i.say.split(':').next().unwrap_or("").to_string()).collect();
        assert_eq!(names, ["Default sheet name", "Merged cells", "Hard-to-read text contrast"], "{names:?}");
        let Step::Show { at, say, .. } = begin(&mut d) else { panic!("an issue") };
        assert_eq!(at, (0, 0));
        assert!(say.ends_with("(1 of 3; Sheet1!A1)"), "{say}");
        let Step::Show { at, .. } = step(&mut d) else { panic!() };
        assert_eq!(at, (3, 0));
        let Step::Show { at, .. } = step(&mut d) else { panic!() };
        assert_eq!(at, (1, 1));
        let Step::Done(say) = step(&mut d) else { panic!() };
        assert_eq!(say, "The accessibility check is complete: 3 issues.");
        assert!(d.accessibility.is_none());
    }
}
