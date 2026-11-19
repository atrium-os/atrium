//! Changes to static styles and workbook objects in two-way review.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::charts::{Anchor, Chart, ChartText, Picture, MAX_CHARTS};
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
pub struct PictureChange {
    pub(crate) sheet: Option<u16>,
    before: Option<(usize, Box<Picture>)>,
    after: Option<Box<Picture>>,
}

impl PictureChange {
    pub(crate) fn anchor(&self) -> Option<(u16, u32, u32)> {
        let picture = self.before.as_ref().map(|(_, picture)| picture).or(self.after.as_ref())?;
        Some((self.sheet?, picture.anchor.from.0, picture.anchor.from.1))
    }

    pub(crate) fn same_review_item(&self, other: &Self) -> bool {
        self.sheet == other.sheet && self.after == other.after
            && self.before.as_ref().map(|(_, picture)| picture) == other.before.as_ref().map(|(_, picture)| picture)
    }

    pub(crate) fn describe(&self) -> String {
        let picture = self.before.as_ref().map(|(_, picture)| picture).or(self.after.as_ref()).unwrap();
        let address = crate::addr::a1(picture.anchor.from.0, picture.anchor.from.1);
        let action = action(self.before.is_some(), self.after.is_some());
        format!("Picture {address} {action}")
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
    pub(super) fn formulas(&self) -> (&str, &str) {
        (self.before.as_ref().map_or("(none)", |(_, formula)| formula),
         self.after.as_ref().map_or("(none)", |(_, formula)| formula))
    }

    pub(crate) fn describe(&self) -> String {
        let action = action(self.before.is_some(), self.after.is_some());
        let (name, formula) = self.after.as_ref().or(self.before.as_ref()).unwrap();
        format!("Name {name} {action}: {formula}")
    }
}

pub(super) fn action(before: bool, after: bool) -> &'static str {
    match (before, after) {
        (false, true) => "added",
        (true, false) => "removed",
        _ => "changed",
    }
}

pub(super) type RowRun = (u32, u32, u32);
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
    drawing_pairs(ours, theirs, ours.charts(), theirs.charts(), |chart| chart.anchor, remap_chart)
        .map(|(sheet, before, after)| Change::Chart(ChartChange {
            sheet,
            before: before.map(|i| (i, Box::new(ours.charts()[i].clone()))),
            after: after.map(|i| Box::new(theirs.charts()[i].clone())),
        })).collect::<Vec<_>>().into_iter()
}

pub(super) fn picture_matches(ours: &Workbook, theirs: &Workbook) -> impl Iterator<Item = Change> {
    drawing_pairs(ours, theirs, ours.pictures(), theirs.pictures(), |picture| picture.anchor, remap_picture)
        .map(|(sheet, before, after)| Change::Picture(PictureChange {
            sheet,
            before: before.map(|i| (i, Box::new(ours.pictures()[i].clone()))),
            after: after.map(|i| Box::new(theirs.pictures()[i].clone())),
        })).collect::<Vec<_>>().into_iter()
}

/// Pair drawings by host sheet and top-left cell, matching identical
/// drawings first so another drawing at the same anchor can be reviewed.
fn drawing_pairs<T: PartialEq>(
    ours: &Workbook, theirs: &Workbook, left: &[T], right: &[T],
    anchor: impl Fn(&T) -> Anchor, remap: impl Fn(&T, &Workbook, &Workbook) -> Option<T>,
) -> impl Iterator<Item = (Option<u16>, Option<usize>, Option<usize>)> {
    type Key = (String, u32, u32);
    type Groups = BTreeMap<Key, (Vec<usize>, Vec<usize>)>;
    let mut groups = Groups::new();
    for (wb, drawings, remote) in [(ours, left, false), (theirs, right, true)] {
        for (index, drawing) in drawings.iter().enumerate() {
            let anchor = anchor(drawing);
            let key = (wb.sheet_name(anchor.sheet).to_ascii_lowercase(), anchor.from.0, anchor.from.1);
            let (a, b) = groups.entry(key).or_default();
            if remote { b.push(index); } else { a.push(index); }
        }
    }
    let mut pairs = Vec::new();
    for ((sheet, _, _), (a, b)) in groups {
        let mapped: Vec<_> = b.iter().map(|&i| remap(&right[i], theirs, ours)).collect();
        let mut used = BTreeSet::new();
        let a: Vec<_> = a.into_iter().filter(|&i| {
            if let Some(j) = mapped.iter().enumerate().position(|(j, drawing)| !used.contains(&j) && drawing.as_ref() == Some(&left[i])) {
                used.insert(j);
                false
            } else {
                true
            }
        }).collect();
        let b: Vec<_> = b.into_iter().enumerate().filter_map(|(j, i)| (!used.contains(&j)).then_some(i)).collect();
        pairs.extend((0..a.len().max(b.len())).map(|i| (ours.sheet_id(&sheet), a.get(i).copied(), b.get(i).copied())));
    }
    pairs.into_iter()
}

fn remap_picture(picture: &Picture, src: &Workbook, dst: &Workbook) -> Option<Picture> {
    let mut picture = picture.clone();
    picture.anchor.sheet = dst.sheet_id(&src.sheet_name(picture.anchor.sheet))?;
    Some(picture)
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

/// Combine chart, picture and name changes into one update per collection.
/// Entries outside the selection keep their local values.
pub(super) fn take_objects(ours: &Workbook, theirs: &Workbook, changes: &[Change]) -> (Vec<GridOp>, usize) {
    let mut charts = None;
    let mut names = None;
    let mut pictures = None;
    let mut picture_count = 0;
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
            Change::Picture(c) => {
                let after = match &c.after {
                    Some(picture) => match remap_picture(picture, theirs, ours) {
                        Some(picture) => Some(picture),
                        None => continue,
                    },
                    None => None,
                };
                let slots = pictures.get_or_insert_with(|| ours.pictures().iter().cloned().map(Some).collect::<Vec<_>>());
                if let Some((i, original)) = &c.before {
                    let Some(slot) = slots.get_mut(*i) else { continue };
                    if slot.as_ref() != Some(original.as_ref()) { continue; }
                    *slot = after;
                } else if let Some(picture) = after {
                    slots.push(Some(picture));
                }
                picture_count += 1;
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
                    if ours.table(name).is_some() { continue; }
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
    let pictures = pictures.map(|slots| slots.into_iter().flatten().collect::<Vec<_>>());
    if let Some(pictures) = pictures.filter(|p| picture_count != 0 && Picture::collection_fits(p)) {
        ops.push(GridOp::Pictures(pictures));
    } else {
        picture_count = 0;
    }
    (ops, chart_count + name_count + picture_count)
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
