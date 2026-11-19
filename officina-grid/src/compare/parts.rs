//! Changes to static styles and workbook objects in two-way review.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::charts::{Chart, ChartText, MAX_CHARTS};
use crate::names::{NameDef, MAX_NAMES};
use crate::ops::GridOp;
use crate::style::Style;
use crate::Workbook;

use super::{Change, MAX_CHANGES};

#[derive(Debug, Clone, PartialEq)]
pub struct ChartChange {
    pub(crate) sheet: Option<u16>,
    before: Option<(usize, Box<Chart>)>,
    after: Option<Box<Chart>>,
}

impl ChartChange {
    pub(crate) fn anchor(&self) -> Option<(u16, u32, u32)> {
        let chart = self.before.as_ref().map(|(_, c)| c).or(self.after.as_ref())?;
        Some((self.sheet?, chart.anchor.from.0, chart.anchor.from.1))
    }

    pub(crate) fn same_review_item(&self, other: &Self) -> bool {
        self.sheet == other.sheet && self.after == other.after
            && self.before.as_ref().map(|(_, c)| c) == other.before.as_ref().map(|(_, c)| c)
    }

    pub(crate) fn describe(&self) -> String {
        let action = action(self.before.is_some(), self.after.is_some());
        let chart = self.before.as_ref().map(|(_, c)| c).or(self.after.as_ref()).unwrap();
        let address = crate::addr::a1(chart.anchor.from.0, chart.anchor.from.1);
        match &chart.title {
            Some(ChartText::Lit(title)) if !title.is_empty() => format!("Chart {address} {action}: {title}"),
            _ => format!("Chart {address} {action}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NameChange {
    pub(crate) sheet: Option<u16>,
    scope: Option<String>,
    key: String,
    before: Option<(String, String)>,
    after: Option<(String, String)>,
}

impl NameChange {
    pub(crate) fn describe(&self) -> String {
        let action = action(self.before.is_some(), self.after.is_some());
        let (name, formula) = self.after.as_ref().or(self.before.as_ref()).unwrap();
        format!("Name {name} {action}: {formula}")
    }
}

fn action(before: bool, after: bool) -> &'static str {
    match (before, after) {
        (false, true) => "added",
        (true, false) => "removed",
        _ => "changed",
    }
}

type RowRun = (u32, u32, u32);
type StyleRun = (u32, u32, u32);

pub(super) fn pair(runs: &mut Vec<RowRun>, ours: u32, theirs: u32, len: u32) {
    if len == 0 { return; }
    if let Some((_, _, n)) = runs.last_mut().filter(|(a, b, n)| *a + *n == ours && *b + *n == theirs) {
        *n += len;
    } else {
        runs.push((ours, theirs, len));
    }
}

/// Project sparse style runs onto the paired local rows.
fn project(styles: &[StyleRun], pairs: &[RowRun], remote: bool) -> Vec<StyleRun> {
    let mut out = Vec::new();
    let offset = |a, b| if remote { b } else { a };
    for &(lo, hi, id) in styles {
        let first = pairs.partition_point(|&(a, b, n)| offset(a, b) + n <= lo);
        for &(a, b, n) in &pairs[first..] {
            let start = offset(a, b);
            if start > hi { break; }
            out.push((a + lo.max(start) - start, a + hi.min(start + n - 1) - start, id));
        }
    }
    out
}

pub(super) fn styles(ours: &Workbook, s: u16, theirs: &Workbook, t: u16, pairs: &[RowRun], out: &mut Vec<Change>) -> bool {
    let (left, right) = (&ours.sheet(s).styles, &theirs.sheet(t).styles);
    for col in 0..left.len().max(right.len()) {
        let a = project(&left.get(col).map_or_else(Vec::new, |r| r.styled(MAX_ROWS - 1)), pairs, false);
        let b = project(&right.get(col).map_or_else(Vec::new, |r| r.styled(MAX_ROWS - 1)), pairs, true);
        let mut i = 0;
        let mut j = 0;
        let mut at = 0;
        let mut pending: Option<(Rect, &Style, &Style)> = None;
        loop {
            let (x, nx) = segment(&a, &mut i, at);
            let (y, ny) = segment(&b, &mut j, at);
            let next = nx.min(ny);
            if at == MAX_ROWS { break; }
            let mine = ours.styles.get(x);
            let other = theirs.styles.get(y);
            if mine != other {
                if let Some((r, _, _)) = pending.as_mut().filter(|(r, x, y)| r.r1 + 1 == at && *x == mine && *y == other) {
                    r.r1 = next - 1;
                } else {
                    if emit_style(out, pending.take()) { return true; }
                    pending = Some((Rect { sheet: s, r0: at, r1: next - 1, c0: col as u32, c1: col as u32 }, mine, other));
                }
            } else if emit_style(out, pending.take()) {
                return true;
            }
            at = next;
        }
        if emit_style(out, pending) { return true; }
    }
    false
}

fn segment(runs: &[StyleRun], index: &mut usize, at: u32) -> (u32, u32) {
    while runs.get(*index).is_some_and(|&(_, hi, _)| hi < at) { *index += 1; }
    match runs.get(*index) {
        Some(&(lo, hi, id)) if lo <= at => (id, hi + 1),
        Some(&(lo, _, _)) => (0, lo),
        None => (0, MAX_ROWS),
    }
}

fn emit_style(out: &mut Vec<Change>, span: Option<(Rect, &Style, &Style)>) -> bool {
    if let Some((rect, ours, theirs)) = span {
        out.push(Change::Style { rect, ours: Box::new(ours.clone()), theirs: Box::new(theirs.clone()) });
    }
    out.len() >= MAX_CHANGES
}

pub(super) fn chart_matches(ours: &Workbook, theirs: &Workbook) -> impl Iterator<Item = Change> {
    type Key = (String, u32, u32);
    type Groups = BTreeMap<Key, (Vec<usize>, Vec<usize>)>;
    let mut groups = Groups::new();
    for (wb, remote) in [(ours, false), (theirs, true)] {
        for (index, chart) in wb.charts().iter().enumerate() {
            let k = (wb.sheet_name(chart.anchor.sheet).to_ascii_lowercase(), chart.anchor.from.0, chart.anchor.from.1);
            let (left, right) = groups.entry(k).or_default();
            if remote { right.push(index); } else { left.push(index); }
        }
    }
    let mut changes = Vec::new();
    for ((sheet, _, _), (left, right)) in groups {
        let mapped: Vec<_> = right.iter().map(|&i| remap_chart(&theirs.charts()[i], theirs, ours)).collect();
        let mut used = BTreeSet::new();
        let left: Vec<_> = left.into_iter().filter(|&i| {
            if let Some(j) = mapped.iter().enumerate().position(|(j, c)| !used.contains(&j) && c.as_ref() == Some(&ours.charts()[i])) {
                used.insert(j);
                false
            } else {
                true
            }
        }).collect();
        let right: Vec<_> = right.into_iter().enumerate().filter_map(|(j, i)| (!used.contains(&j)).then_some(i)).collect();
        for i in 0..left.len().max(right.len()) {
            changes.push(Change::Chart(ChartChange {
                sheet: ours.sheet_id(&sheet),
                before: left.get(i).map(|&j| (j, Box::new(ours.charts()[j].clone()))),
                after: right.get(i).map(|&j| Box::new(theirs.charts()[j].clone())),
            }));
        }
    }
    changes.into_iter()
}

fn remap_chart(chart: &Chart, src: &Workbook, dst: &Workbook) -> Option<Chart> {
    let mut chart = chart.clone();
    let sheet = |s: u16| (usize::from(s) < src.sheet_count()).then(|| dst.sheet_id(&src.sheet_name(s))).flatten();
    let text = |t: &mut Option<ChartText>| {
        if let Some(ChartText::Cell(addr)) = t { addr.sheet = sheet(addr.sheet)?; }
        Some(())
    };
    chart.anchor.sheet = sheet(chart.anchor.sheet)?;
    text(&mut chart.title)?;
    for series in &mut chart.series {
        text(&mut series.name)?;
        series.vals.sheet = sheet(series.vals.sheet)?;
        if let Some(cats) = &mut series.cats { cats.sheet = sheet(cats.sheet)?; }
    }
    Some(chart)
}

pub(super) fn name_matches<'a>(ours: &'a Workbook, theirs: &Workbook) -> impl Iterator<Item = Change> + use<'a> {
    type Key = (Option<String>, String);
    type Versions = (Option<(String, String)>, Option<(String, String)>);
    let mut names = BTreeMap::<Key, Versions>::new();
    for (wb, remote) in [(ours, false), (theirs, true)] {
        for def in wb.names().iter() {
            let k = (def.scope.map(|s| wb.sheet_name(s).to_ascii_lowercase()), def.name.to_uppercase());
            let (before, after) = names.entry(k).or_default();
            let formula = crate::formula::to_a1(&def.expr, 0, 0, &|sheet| {
                let name = wb.sheet_name(sheet);
                if remote {
                    if let Some(s) = ours.sheet_id(&name) { return ours.sheet_name(s); }
                }
                name
            });
            let value = Some((def.name.to_string(), formula));
            if remote { *after = value; } else { *before = value; }
        }
    }
    names.into_iter().filter_map(move |((scope, key), (before, after))| {
        let sheet = scope.as_deref().and_then(|name| ours.sheet_id(name));
        (before != after).then_some(Change::Name(NameChange { sheet, scope, key, before, after }))
    })
}

/// Take chart and name changes together so multiple changes compose into a
/// single update of each collection without overwriting kept objects.
pub(super) fn take_objects(ours: &Workbook, theirs: &Workbook, changes: &[Change]) -> (Vec<GridOp>, usize) {
    let mut charts = None;
    let mut names = None;
    let mut chart_count = 0;
    let mut name_count = 0;
    let mut next_name = ours.names().len();
    for change in changes {
        match change {
            Change::Chart(c) => {
                let after = match &c.after {
                    Some(chart) => match remap_chart(chart, theirs, ours) {
                        Some(chart) => Some(chart),
                        None => continue,
                    },
                    None => None,
                };
                let slots = charts.get_or_insert_with(|| ours.charts().iter().cloned().map(Some).collect::<Vec<_>>());
                if let Some((i, original)) = &c.before {
                    let Some(slot) = slots.get_mut(*i) else { continue; };
                    if slot.as_ref() != Some(original.as_ref()) { continue; }
                    *slot = after;
                } else if let Some(chart) = after {
                    slots.push(Some(chart));
                }
                chart_count += 1;
            }
            Change::Name(c) => {
                let scope = match &c.scope {
                    Some(name) => match ours.sheet_id(name) {
                        Some(s) => Some(s),
                        None => continue,
                    },
                    None => None,
                };
                let defs = names.get_or_insert_with(|| ours.names().iter().enumerate().map(|(i, d)| ((d.scope, d.name.to_uppercase()), (i, d.clone()))).collect::<BTreeMap<_, _>>());
                let key = (scope, c.key.clone());
                if defs.get(&key).map(|(_, d)| (d.name.to_string(), ours.name_text(d))) != c.before { continue; }
                if let Some((name, formula)) = &c.after {
                    let Ok(expr) = ours.parse_stored(formula, 0, 0) else { continue; };
                    let def = NameDef { name: name.as_str().into(), scope, expr: Arc::new(expr) };
                    // Parsing against a different set of sheets may turn a reference into #REF!.
                    if ours.name_text(&def) != *formula { continue; }
                    let order = defs.get(&key).map_or_else(|| {
                        let order = next_name;
                        next_name += 1;
                        order
                    }, |(order, _)| *order);
                    defs.insert(key, (order, def));
                } else {
                    defs.remove(&key);
                }
                name_count += 1;
            }
            _ => {}
        }
    }
    let mut ops = Vec::new();
    let charts = charts.map(|slots| slots.into_iter().flatten().collect::<Vec<_>>());
    if let Some(charts) = charts.filter(|c| chart_count != 0 && c.len() <= MAX_CHARTS) {
        ops.push(GridOp::Charts(charts));
    } else {
        chart_count = 0;
    }
    if let Some(names) = names.filter(|n| name_count != 0 && n.len() <= MAX_NAMES) {
        let mut defs: Vec<_> = names.into_values().collect();
        defs.sort_unstable_by_key(|(order, _)| *order);
        ops.push(GridOp::names(ours, &defs.into_iter().map(|(_, d)| d).collect::<Vec<_>>()));
    } else {
        name_count = 0;
    }
    (ops, chart_count + name_count)
}

pub(super) fn style_spans(rect: Rect, style: &Style) -> Vec<(u32, u32, u32, Style)> {
    if *style == Style::default() { Vec::new() }
    else { vec![(rect.c0, rect.r0, rect.r1, style.clone())] }
}

pub(super) fn row_styles(theirs: &Workbook, source: Rect, target: Rect) -> Option<GridOp> {
    let mut styles = theirs.style_snapshot(&Rect { c1: MAX_COLS - 1, ..source });
    if styles.spans.is_empty() { return None; }
    styles.rect = Rect { c1: styles.spans.last()?.0, ..target };
    for (_, a, b, _) in &mut styles.spans {
        *a = target.r0 + (*a - source.r0);
        *b = target.r0 + (*b - source.r0);
    }
    Some(GridOp::Restyle(styles))
}
