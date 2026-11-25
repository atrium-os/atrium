//! View controls for searching cell inputs, split panes, outlines, and chart sources.
use super::*;
use officina_grid::addr::{a1, CellAddr, Rect as Range, MAX_COLS, MAX_ROWS};
use officina_grid::book::{Input, Workbook};
use officina_grid::charts::{Chart, MAX_SERIES, MAX_TITLE};
use officina_grid::find::{self, Search};
use officina_grid::formula::ast::{Expr, RangeKind};
use officina_grid::ops::GridOp;
use officina_grid::outline::{Group, Outline};
use officina_grid::shift::Axis;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Split {
    #[default]
    None,
    Rows,
    Columns,
    Both,
}

impl Split {
    fn panes(self, rect: Rect) -> (usize, usize, f32, f32) {
        let cols = if matches!(self, Self::Columns | Self::Both) && rect.w() >= 320.0 { 2 } else { 1 };
        let rows = if matches!(self, Self::Rows | Self::Both) && rect.h() >= 200.0 { 2 } else { 1 };
        (rows, cols, (rect.w() - 4.0 * (cols - 1) as f32) / cols as f32,
            (rect.h() - 4.0 * (rows - 1) as f32) / rows as f32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineCmd { Group, Ungroup, ToggleSelection, Toggle(u32, u32) }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorCmd {
    FindOpen,
    FindClose,
    Case,
    Whole,
    Workbook,
    Find(bool),
    Replace(bool),
    Split(Split),
    Outline(Axis, OutlineCmd),
    ChartOpen,
    ChartClose,
    ChartSeries(usize),
    ChartAdd,
    ChartRemove,
    ChartType(Option<ChartKind>),
    ChartApply,
}

const VALUE: usize = 0;
const CATEGORY: usize = 1;
const NAME: usize = 2;
const X_TITLE: usize = 3;
const Y_TITLE: usize = 4;

#[derive(Clone)]
struct Draft {
    index: usize,
    original: Chart,
    chart: Chart,
    series: usize,
}

#[derive(Clone)]
pub(super) struct Authoring {
    find_open: Mutable<bool>,
    query: Mutable<TextEdit>,
    replacement: Mutable<TextEdit>,
    case: Mutable<bool>,
    whole: Mutable<bool>,
    all_sheets: Mutable<bool>,
    split: Mutable<Split>,
    extra_scrolls: [Mutable<ScrollState>; 3],
    active: Mutable<usize>,
    chart: Mutable<Option<Draft>>,
    fields: [Mutable<TextEdit>; 5],
    kind_menu: Mutable<MenuState>,
    chart_error: Mutable<String>,
}

impl Authoring {
    pub(super) fn new(rev: &Mutable<u64>) -> Self {
        Self {
            find_open: rev.sibling(false),
            query: rev.sibling(TextEdit::new("")),
            replacement: rev.sibling(TextEdit::new("")),
            case: rev.sibling(false),
            whole: rev.sibling(false),
            all_sheets: rev.sibling(false),
            split: rev.sibling(Split::None),
            extra_scrolls: std::array::from_fn(|_| rev.sibling(ScrollState::default())),
            active: rev.sibling(0),
            chart: rev.sibling(None),
            fields: std::array::from_fn(|_| rev.sibling(TextEdit::new(""))),
            kind_menu: rev.sibling(MenuState::default()),
            chart_error: rev.sibling(String::new()),
        }
    }
    fn scroll(&self, s: &Sheets, pane: usize) -> Mutable<ScrollState> {
        match pane {
            0 => s.scroll.clone(),
            _ => self.extra_scrolls[pane - 1].clone(),
        }
    }

    pub(super) fn panes(&self, s: &Sheets, rect: Rect) -> impl Iterator<Item = (usize, Rect, Mutable<ScrollState>)> {
        let (rows, cols, w, h) = self.split.get().panes(rect);
        let scrolls = [s.scroll.clone(), self.extra_scrolls[0].clone(), self.extra_scrolls[1].clone(), self.extra_scrolls[2].clone()];
        scrolls.into_iter().take(rows * cols).enumerate().map(move |(i, scroll)| (i, Rect::new(
            rect.x() + (i % cols) as f32 * (w + 4.0),
            rect.y() + (i / cols) as f32 * (h + 4.0),
            w, h,
        ), scroll))
    }

    pub(super) fn active(&self, rect: Rect) -> usize {
        let (rows, cols, _, _) = self.split.get().panes(rect);
        let i = self.active.get();
        if i < rows * cols { i } else { 0 }
    }

    /// Every pane's scroll, split or not.
    pub(super) fn all_scrolls(&self, s: &Sheets) -> [Mutable<ScrollState>; 4] {
        [s.scroll.clone(), self.extra_scrolls[0].clone(), self.extra_scrolls[1].clone(), self.extra_scrolls[2].clone()]
    }

    pub(super) fn activation(&self) -> Mutable<usize> { self.active.clone() }

    pub(super) fn active_scroll(&self, s: &Sheets) -> Mutable<ScrollState> {
        let (w, h) = s.size.get();
        self.scroll(s, self.active(Sheets::grid_rect(w, h)))
    }

    pub(super) fn reset_scrolls(&self, s: &Sheets) {
        for scroll in std::iter::once(&s.scroll).chain(self.extra_scrolls.iter()) {
            let mut state = scroll.get();
            state.physics.stop();
            state.physics_x.stop();
            state.physics.set_position(0.0);
            state.physics_x.set_position(0.0);
            scroll.set(state);
        }
    }

    pub(super) fn escape(&self) -> Option<AuthorCmd> {
        if self.find_open.get() { Some(AuthorCmd::FindClose) }
        else if self.chart.get_cloned().is_some() { Some(AuthorCmd::ChartClose) }
        else { None }
    }

    pub(super) fn reset(&self) {
        self.find_open.set(false);
        self.chart.set(None);
        self.split.set(Split::None);
        self.active.set(0);
    }

    fn search(&self, s: &Sheets, forward: bool, replace: Option<bool>) -> Result<(), String> {
        let search = Search::new(self.query.get_cloned().text(), self.case.get(), self.whole.get())?;
        let replacement = self.replacement.get_cloned();
        if replacement.text().len() > 16_384 { return Err("The replacement text is too long.".into()); }
        let mut d = s.doc.borrow_mut();
        let (row, col) = s.grid.get_cloned().cursor;
        let cursor = CellAddr::new(d.sheet, row, col);
        let sheets = if self.all_sheets.get() { 0..d.wb.sheet_count() as u16 } else { d.sheet..d.sheet + 1 };

        if let Some(all) = replace {
            let mut ops = Vec::new();
            let mut bytes = 0;
            let mut error = None;
            for sheet in sheets {
                find::inputs(&d.wb, sheet, &mut |at, input, text| {
                    if !all && at != cursor { return true; }
                    let next = match search.replacement(&input, &text, replacement.text()) {
                        Ok(Some(next)) => next,
                        Ok(None) => return true,
                        Err(message) => { error = Some(message.into()); return false; }
                    };
                    let size = match &next {
                        Input::Formula(code) => {
                            if d.wb.parse(code, at.sheet, at.row, at.col).is_err() {
                                error = Some(format!("The replacement would make {}!{} an invalid formula.", d.wb.sheet_name(at.sheet), a1(at.row, at.col)));
                                return false;
                            }
                            code.len()
                        }
                        Input::Value(officina_grid::Value::Str(text)) => text.len(),
                        Input::Value(_) | Input::Empty => 0,
                    };
                    bytes += size + text.len();
                    if bytes > 8 << 20 || ops.len() == 10_000 {
                        error = Some("The replacement is too large. Narrow the search or shorten the replacement.".into());
                        return false;
                    }
                    ops.push(GridOp::set(at, next));
                    all
                });
                if error.is_some() || (!all && !ops.is_empty()) { break; }
            }
            if let Some(error) = error { return Err(error); }
            if ops.is_empty() {
                return Err(if all { "No matching inputs to replace." } else { "The selected cell has no text to replace. Try Find Next." }.into());
            }
            let count = ops.len();
            if d.run_validated(if all { "Replace All" } else { "Replace" }, ops) && d.status.is_empty() {
                d.status = format!("Replaced text in {count} {}.", if count == 1 { "cell" } else { "cells" });
            }
            return Ok(());
        }

        let mut wrapped = None;
        let mut next = None;
        let better = |a, b| if forward { a < b } else { a > b };
        for sheet in sheets {
            find::inputs(&d.wb, sheet, &mut |at, _, text| {
                if search.matches(&text) {
                    if wrapped.is_none_or(|old| better(at, old)) { wrapped = Some(at); }
                    if better(cursor, at) && next.is_none_or(|old| better(at, old)) { next = Some(at); }
                }
                true
            });
        }
        let at = next.or(wrapped).ok_or("No matching cell inputs.")?;
        let message = format!("Found in {}!{}.", d.wb.sheet_name(at.sheet), a1(at.row, at.col));
        drop(d);
        s.show_sheet(at.sheet);
        s.show_cell(at.row, at.col);
        s.doc.borrow_mut().status = message;
        Ok(())
    }

    fn outline(&self, s: &Sheets, axis: Axis, cmd: OutlineCmd) -> Result<(), String> {
        let selection = s.grid.get_cloned().selection();
        let (start, end) = if axis == Axis::Rows { (selection.r0, selection.r1) } else { (selection.c0, selection.c1) };
        let mut d = s.doc.borrow_mut();
        let sheet = d.sheet;
        let mut groups = d.wb.sheet(sheet).outline(axis).groups().to_vec();
        match cmd {
            OutlineCmd::Group => {
                groups.push(Group { start, end, collapsed: false });
            }
            OutlineCmd::Ungroup => {
                let i = groups.iter().enumerate().filter(|(_, g)| g.start <= start && end <= g.end)
                    .min_by_key(|(_, g)| g.end - g.start).map(|(i, _)| i).ok_or("Select cells within an outline group to ungroup.")?;
                groups.remove(i);
            }
            OutlineCmd::ToggleSelection => {
                let g = groups.iter_mut().filter(|g| g.start <= start && end <= g.end)
                    .min_by_key(|g| g.end - g.start).ok_or("Select cells within an outline group to collapse or expand.")?;
                g.collapsed = !g.collapsed;
            }
            OutlineCmd::Toggle(start, end) => {
                let g = groups.iter_mut().find(|g| (g.start, g.end) == (start, end)).ok_or("The outline group changed.")?;
                g.collapsed = !g.collapsed;
            }
        }
        let outline = Outline::new(groups, axis).ok_or("Outline groups must be distinct, nested or separate, and within the worksheet limits.")?;
        let (row, col) = s.grid.get_cloned().cursor;
        let cursor = if axis == Axis::Rows { row } else { col };
        let hidden = outline.hidden().iter().find(|&&(a, b)| a <= cursor && cursor <= b).copied();
        if !d.run_validated("Outline", vec![GridOp::Outline { sheet, axis, outline }]) { return Ok(()); }
        drop(d);
        if let Some((a, b)) = hidden {
            let limit = if axis == Axis::Rows { MAX_ROWS } else { MAX_COLS };
            let visible = if b + 1 < limit { b + 1 } else { a.saturating_sub(1) };
            let mut grid = s.grid.get_cloned();
            if axis == Axis::Rows { grid.select(visible, col); } else { grid.select(row, visible); }
            s.grid.set(grid);
        }
        Ok(())
    }

    pub(super) fn run(&self, s: &Sheets, command: AuthorCmd) {
        if s.launcher || s.password.get_cloned().open || s.closing.get() { return; }
        let result = match command {
            AuthorCmd::FindOpen => {
                let mut grid = s.grid.get_cloned();
                if let Some(edit) = grid.edit.take() {
                    Sheets::act(&s.doc, &s.source, &s.rev, GridAction::Commit {
                        row: grid.cursor.0, col: grid.cursor.1, text: edit.text().to_owned(),
                    });
                    s.grid.set(grid);
                }
                self.chart.set(None);
                self.find_open.set(true);
                Ok(())
            }
            AuthorCmd::FindClose => { self.find_open.set(false); Ok(()) }
            AuthorCmd::Case => { self.case.set(!self.case.get()); Ok(()) }
            AuthorCmd::Whole => { self.whole.set(!self.whole.get()); Ok(()) }
            AuthorCmd::Workbook => { self.all_sheets.set(!self.all_sheets.get()); Ok(()) }
            AuthorCmd::Find(forward) => self.search(s, forward, None),
            AuthorCmd::Replace(all) => self.search(s, true, Some(all)),
            AuthorCmd::Split(mode) => {
                if self.split.get() != mode {
                    let (w, h) = s.size.get();
                    let current = self.scroll(s, self.active(Sheets::grid_rect(w, h))).get();
                    for scroll in std::iter::once(&s.scroll).chain(self.extra_scrolls.iter()) {
                        let mut state = current;
                        state.extent = None;
                        scroll.set(state);
                    }
                    self.active.set(0);
                    self.split.set(mode);
                }
                Ok(())
            }
            AuthorCmd::Outline(axis, c) => self.outline(s, axis, c),
            other => {
                self.chart_error.set(self.chart_command(s, other).err().unwrap_or_default());
                Ok(())
            }
        };
        if let Err(error) = result { s.doc.borrow_mut().status = error; }
        s.changed();
    }

    fn load_series(&self, wb: &Workbook, draft: &Draft) {
        let host = draft.chart.anchor.sheet;
        let series = &draft.chart.series[draft.series];
        self.fields[VALUE].set(TextEdit::new(&range_text(wb, host, series.vals)));
        self.fields[CATEGORY].set(TextEdit::new(&series.cats.map(|r| range_text(wb, host, r)).unwrap_or_default()));
        self.fields[NAME].set(TextEdit::new(&label_text(wb, host, series.name.as_ref())));
    }

    fn read_series(&self, wb: &Workbook, draft: &mut Draft) -> Result<(), String> {
        let host = draft.chart.anchor.sheet;
        let values = self.fields[VALUE].get_cloned();
        let cats = self.fields[CATEGORY].get_cloned();
        let vals = chart_range(wb, host, values.text()).ok_or("Enter one row or column of values, for example B2:B8.")?;
        let cats = if cats.text().trim().is_empty() { None } else {
            let cats = chart_range(wb, host, cats.text()).ok_or("Enter one row or column for the category labels or X values.")?;
            if cats.cells() != vals.cells() { return Err("The label range and value range must have the same number of cells.".into()); }
            Some(cats)
        };
        let series = &mut draft.chart.series[draft.series];
        series.vals = vals;
        series.cats = cats;
        series.name = chart_label(wb, host, self.fields[NAME].get_cloned().text())?;
        Ok(())
    }

    fn chart_command(&self, s: &Sheets, command: AuthorCmd) -> Result<(), String> {
        if command == AuthorCmd::ChartClose { self.chart.set(None); return Ok(()); }
        let mut d = s.doc.borrow_mut();
        if command == AuthorCmd::ChartOpen {
            let i = s.grid.get_cloned().picked.ok_or("Select a chart first.")? as usize;
            let chart = d.wb.charts().get(i).filter(|c| !c.series.is_empty()).ok_or("Select a chart with a data series.")?.clone();
            self.fields[X_TITLE].set(TextEdit::new(chart.x_title.as_deref().unwrap_or("")));
            self.fields[Y_TITLE].set(TextEdit::new(chart.y_title.as_deref().unwrap_or("")));
            let draft = Draft { index: i, original: chart.clone(), chart, series: 0 };
            self.load_series(&d.wb, &draft);
            self.find_open.set(false);
            self.chart.set(Some(draft));
            return Ok(());
        }
        let Some(mut draft) = self.chart.get_cloned() else { return Ok(()) };
        if d.wb.charts().get(draft.index) != Some(&draft.original) {
            self.chart.set(None);
            d.status = "The chart changed. Open the chart editor again.".into();
            return Ok(());
        }
        match command {
            AuthorCmd::ChartType(kind) => {
                draft.chart.series[draft.series].kind = kind;
            }
            AuthorCmd::ChartRemove if draft.chart.series.len() > 1 => {
                draft.chart.series.remove(draft.series);
                draft.series = draft.series.min(draft.chart.series.len() - 1);
                self.load_series(&d.wb, &draft);
            }
            AuthorCmd::ChartSeries(i) if i < draft.chart.series.len() => {
                self.read_series(&d.wb, &mut draft)?;
                draft.series = i;
                self.load_series(&d.wb, &draft);
            }
            AuthorCmd::ChartAdd if draft.chart.series.len() < MAX_SERIES && !draft.chart.kind.round() => {
                self.read_series(&d.wb, &mut draft)?;
                let mut series = draft.chart.series[draft.series].clone();
                series.name = None;
                draft.series = draft.chart.series.len();
                draft.chart.series.push(series);
                self.load_series(&d.wb, &draft);
            }
            AuthorCmd::ChartApply => {
                self.read_series(&d.wb, &mut draft)?;
                draft.chart.x_title = axis_title(self.fields[X_TITLE].get_cloned().text())?;
                draft.chart.y_title = axis_title(self.fields[Y_TITLE].get_cloned().text())?;
                if draft.chart.series.iter().any(|s| s.kind.is_some_and(|kind| kind != draft.chart.kind)) {
                    if !combo_kind(draft.chart.kind) || draft.chart.series.iter().any(|s| s.kind.is_some_and(|kind| !combo_kind(kind))) {
                        return Err("Combo charts can mix columns, lines, and areas on the same axes.".into());
                    }
                    draft.chart.grouping = Grouping::Standard;
                }
                if draft.chart != draft.original {
                    let mut charts = d.wb.charts().to_vec();
                    charts[draft.index] = draft.chart;
                    if !d.run_validated("Edit Chart", vec![GridOp::Charts(charts)]) {
                        return Err(d.status.clone());
                    }
                }
                self.chart.set(None);
                return Ok(());
            }
            _ => return Ok(()),
        }
        self.chart.set(Some(draft));
        Ok(())
    }

    pub(super) fn render(&self, s: &Sheets, ctx: &mut Ctx, w: f32, h: f32) {
        use pergola::widgets::Checkbox;
        use pergola::widgets::Check;
        let push = |cmd| {
            let cmds = s.cmds.clone();
            move || cmds.lock().unwrap().push(Cmd::Author(cmd))
        };
        let theme = ctx.theme;
        let text = |content: &str, rect, bold| Node::Text {
            rect, content: content.into(),
            style: TextStyle { family: font::SANS.into(), size: type_size::SM, weight: if bold { Weight::Semibold } else { Weight::Regular }, color: theme.text_primary() },
        };
        if self.find_open.get() {
            let width = (w - 12.0).clamp(0.0, 470.0);
            let x = w - width - 6.0;
            let y = TOOLBAR_H + FORMULA_H + 6.0;
            let back = ctx.push(Node::Rect { rect: Rect::new(x, y, width, 166.0), fill: theme.bg_surface(), radius: 5.0 });
            ctx.on_click(back, || {});
            let field_width = (width - 16.0).max(0.0);
            let submit = push(AuthorCmd::Find(true));
            TextField::new(self.query.clone()).key(0x4649_4e44).placeholder("Find text or formula").autofocus(true)
                .at(x + 8.0, y + 8.0).width(field_width).on_submit(move |_| submit()).render(ctx);
            let replace = push(AuthorCmd::Replace(false));
            TextField::new(self.replacement.clone()).placeholder("Replace with").at(x + 8.0, y + 44.0).width(field_width)
                .on_submit(move |_| replace()).render(ctx);
            let options = [("Match case", self.case.get(), AuthorCmd::Case),
                ("Whole cell", self.whole.get(), AuthorCmd::Whole),
                ("All sheets", self.all_sheets.get(), AuthorCmd::Workbook)];
            for (i, (label, on, action)) in options.into_iter().enumerate() {
                let state = Mutable::new(if on { Check::On } else { Check::Off });
                let click = push(action);
                Checkbox::new(state, label).at(x + 8.0 + i as f32 * field_width / 3.0, y + 82.0)
                    .on_change(move |_| click()).render(ctx);
            }
            let commands = [("Previous", AuthorCmd::Find(false)), ("Next", AuthorCmd::Find(true)),
                ("Replace", AuthorCmd::Replace(false)), ("Replace All", AuthorCmd::Replace(true)), ("Close", AuthorCmd::FindClose)];
            let bw = (field_width - 4.0 * 4.0).max(0.0) / commands.len() as f32;
            for (i, (label, action)) in commands.into_iter().enumerate() {
                Button::secondary(label).at(x + 8.0 + i as f32 * (bw + 4.0), y + 120.0).width(bw).retain_focus_on_press().on_click(push(action)).render(ctx);
            }
            ctx.pop();
        }

        if let Some(draft) = self.chart.get_cloned() {
            let width = (w - 12.0).clamp(0.0, 540.0);
            let x = (w - width) / 2.0;
            let y = ((h - 360.0) / 2.0).max(2.0);
            let back = ctx.push(Node::Rect { rect: Rect::new(x, y, width, 348.0), fill: theme.bg_surface(), radius: 5.0 });
            ctx.on_click(back, || {});
            ctx.add(text(&format!("Edit Chart · Series {} of {}", draft.series + 1, draft.chart.series.len()),
                Rect::new(x + 10.0, y + 10.0, width - 20.0, 24.0), true));
            let labels = ["Values", "Categories / X", "Series label", "Category / X title", "Value / Y title"];
            for (i, label) in labels.into_iter().enumerate() {
                let iy = y + 38.0 + i as f32 * 34.0;
                ctx.add(text(label, Rect::new(x + 10.0, iy + 8.0, 120.0, 22.0), false));
                TextField::new(self.fields[i].clone()).key(0x4348_4544 + i as u64)
                    .placeholder(match i {
                        VALUE | CATEGORY => "A2:A8 or 'Sheet Name'!A2:A8",
                        NAME => "Text or =A1",
                        _ => "Axis title",
                    })
                    .at(x + 132.0, iy).width((width - 142.0).max(0.0)).render(ctx);
            }
            if combo_kind(draft.chart.kind) {
                let kinds = [None, Some(ChartKind::Column), Some(ChartKind::Line), Some(ChartKind::Area)];
                let selected = draft.chart.series[draft.series].kind;
                let title = selected.map_or("Chart default", kind_name);
                let items = kinds.map(|kind| pergola::widgets::MenuItem::new(kind.map_or("Chart default", kind_name)).radio(kind == selected));
                let cmds = s.cmds.clone();
                ctx.add(text("Series type", Rect::new(x + 10.0, y + 218.0, 120.0, 22.0), false));
                MenuButton::new(title.to_string(), self.kind_menu.clone(), items.to_vec(), move |i| {
                    if let Some(kind) = kinds.get(i) { cmds.lock().unwrap().push(Cmd::Author(AuthorCmd::ChartType(*kind))); }
                }).at(x + 132.0, y + 210.0).width(140.0).render(ctx);
            }
            let buttons = [
                ("Previous", AuthorCmd::ChartSeries(draft.series.saturating_sub(1)), draft.series > 0),
                ("Next", AuthorCmd::ChartSeries(draft.series + 1), draft.series + 1 < draft.chart.series.len()),
                ("Add Series", AuthorCmd::ChartAdd, draft.chart.series.len() < MAX_SERIES && !draft.chart.kind.round()),
                ("Remove", AuthorCmd::ChartRemove, draft.chart.series.len() > 1),
            ];
            let bw = ((width - 32.0) / 4.0).min(110.0);
            for (i, (label, command, enabled)) in buttons.into_iter().enumerate() {
                let button = Button::secondary(label).at(x + 10.0 + i as f32 * (bw + 4.0), y + 246.0).width(bw);
                if enabled { button.on_click(push(command)) } else { button }.render(ctx);
            }
            ctx.add(text(&self.chart_error.get_cloned(), Rect::new(x + 10.0, y + 281.0, width - 20.0, 25.0), false));
            Button::secondary("Cancel").at(x + width - 164.0, y + 309.0).width(72.0).on_click(push(AuthorCmd::ChartClose)).render(ctx);
            Button::primary("Apply").at(x + width - 84.0, y + 309.0).width(74.0).on_click(push(AuthorCmd::ChartApply)).render(ctx);
            ctx.pop();
        }
    }
}

pub(super) fn combo_kind(kind: ChartKind) -> bool {
    matches!(kind, ChartKind::Column | ChartKind::Line | ChartKind::Area)
}

fn chart_range(wb: &Workbook, host: u16, text: &str) -> Option<Range> {
    let code = format!("={}", text.trim().trim_start_matches('='));
    let (sheet, r0, c0, r1, c1) = match wb.parse(&code, host, 0, 0).ok()? {
        Expr::Ref(r) => (r.sheet, r.row, r.col, r.row, r.col),
        Expr::Range(r) if r.kind == RangeKind::Cells => (r.sheet, r.r0, r.c0, r.r1, r.c1),
        _ => return None,
    };
    let (r0, r1) = (r0.resolve(0, MAX_ROWS)?, r1.resolve(0, MAX_ROWS)?);
    let (c0, c1) = (c0.resolve(0, MAX_COLS)?, c1.resolve(0, MAX_COLS)?);
    if r0 != r1 && c0 != c1 { return None; }
    Some(Range { sheet: sheet.unwrap_or(host), r0: r0.min(r1), c0: c0.min(c1), r1: r1.max(r0), c1: c1.max(c0) })
}

fn axis_title(text: &str) -> Result<Option<Arc<str>>, String> {
    if text.len() > MAX_TITLE { return Err("Axis titles are limited to 1,024 bytes.".into()); }
    Ok((!text.is_empty()).then(|| text.into()))
}

fn chart_label(wb: &Workbook, host: u16, text: &str) -> Result<Option<ChartText>, String> {
    if text.is_empty() { return Ok(None); }
    if text.len() > MAX_TITLE { return Err("Chart titles and series labels are limited to 1,024 bytes.".into()); }
    if text.starts_with('=') {
        let r = chart_range(wb, host, text).filter(|r| r.r0 == r.r1 && r.c0 == r.c1).ok_or("A linked label needs a single cell reference.")?;
        return Ok(Some(ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0))));
    }
    Ok(Some(ChartText::Lit(text.strip_prefix("'=").map_or(text, |_| &text[1..]).into())))
}

fn range_text(wb: &Workbook, host: u16, r: Range) -> String {
    let prefix = if r.sheet == host { String::new() } else {
        format!("'{}'!", wb.sheet_name(r.sheet).replace('\'', "''"))
    };
    let first = a1(r.r0, r.c0);
    if r.r0 == r.r1 && r.c0 == r.c1 { format!("{prefix}{first}") }
    else { format!("{prefix}{first}:{}", a1(r.r1, r.c1)) }
}

fn label_text(wb: &Workbook, host: u16, label: Option<&ChartText>) -> String {
    match label {
        Some(ChartText::Cell(addr)) => format!("={}", range_text(wb, host, Range::cell(*addr))),
        Some(ChartText::Lit(text)) if text.starts_with('=') => format!("'{text}"),
        Some(ChartText::Lit(text)) => text.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::event::{Event, Key, KeyEventKind, Modifiers, ScrollPhase};
    use pergola::geom::Point;
    use pergola::widgets::GridSource;
    use pergola::App;

    fn sheets(csv: &[u8]) -> Sheets {
        let (doc, _) = Doc::from_csv("authoring.csv", csv).unwrap();
        Sheets::new(doc, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()),
            Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0))
    }

    fn run(s: &Sheets, command: AuthorCmd) {
        s.cmds.lock().unwrap().push(Cmd::Author(command));
        assert!(s.take_cmds().is_empty());
    }

    fn click(app: &mut App<Sheets>, s: &Sheets, at: Point) {
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        s.take_cmds();
        app.tick();
    }

    fn input(s: &Sheets, index: usize, value: &str) {
        s.authoring.fields[index].set(TextEdit::new(value));
    }

    fn reopen(s: &Sheets) -> Doc {
        Doc::from_native("authoring.osheet", &s.doc.borrow().stored_native().unwrap()).unwrap()
    }

    #[test]
    fn authoring_find_replace_searches_inputs_and_undoes_as_one_step() {
        let s = sheets(b"Apple,appleton\napple,APPLE\n");
        let mut d = s.doc.borrow_mut();
        d.wb.add_sheet("Other").unwrap();
        d.wb.set_input(CellAddr::new(1, 0, 0), "aPpLe").unwrap();
        d.wb.set_input(CellAddr::new(1, 1, 0), "=LEN(\"apple\")").unwrap();
        d.wb.recalc();
        drop(d);
        run(&s, AuthorCmd::FindOpen);
        s.authoring.query.set(TextEdit::new("apple"));
        run(&s, AuthorCmd::Whole);
        run(&s, AuthorCmd::Find(true));
        assert_eq!(s.grid.get_cloned().cursor, (1, 0));
        run(&s, AuthorCmd::Find(false));
        assert_eq!(s.grid.get_cloned().cursor, (0, 0));
        run(&s, AuthorCmd::Find(false));
        assert_eq!(s.grid.get_cloned().cursor, (1, 1));
        run(&s, AuthorCmd::Case);
        run(&s, AuthorCmd::Find(true));
        assert_eq!(s.grid.get_cloned().cursor, (1, 0));
        s.authoring.replacement.set(TextEdit::new("007"));
        run(&s, AuthorCmd::Replace(false));
        assert_eq!(s.doc.borrow().wb.value(0, 1, 0), officina_grid::Value::str("007"));
        assert!(s.doc.borrow_mut().undo());

        run(&s, AuthorCmd::Case);
        run(&s, AuthorCmd::Whole);
        run(&s, AuthorCmd::Workbook);
        s.authoring.replacement.set(TextEdit::new("pear"));
        run(&s, AuthorCmd::Replace(true));
        {
            let d = s.doc.borrow();
            assert_eq!(d.wb.value(0, 0, 1), officina_grid::Value::str("pearton"));
            assert_eq!(d.wb.value(1, 0, 0), officina_grid::Value::str("pear"));
            assert_eq!(d.wb.value(1, 1, 0), officina_grid::Value::Num(4.0));
            assert_eq!(d.status, "Replaced text in 6 cells.");
        }
        let restored = reopen(&s);
        assert_eq!(restored.wb.input(CellAddr::new(1, 1, 0)), Input::Formula("=LEN(\"pear\")".into()));
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(s.doc.borrow().wb.value(1, 0, 0), officina_grid::Value::str("aPpLe"));
        assert_eq!(s.doc.borrow().wb.value(1, 1, 0), officina_grid::Value::Num(5.0));

        s.doc.borrow_mut().wb.set_input(CellAddr::new(0, 4, 0), "Call LEN( here").unwrap();
        s.authoring.query.set(TextEdit::new("LEN("));
        s.authoring.replacement.set(TextEdit::new("LEN)"));
        run(&s, AuthorCmd::Replace(true));
        assert!(s.doc.borrow().status.contains("invalid formula"));
        assert_eq!(s.doc.borrow().wb.value(1, 1, 0), officina_grid::Value::Num(5.0));
        assert_eq!(s.doc.borrow().wb.value(0, 4, 0), officina_grid::Value::str("Call LEN( here"));

        s.authoring.query.set(TextEdit::new("aPpLe"));
        run(&s, AuthorCmd::Case);
        run(&s, AuthorCmd::Find(true));
        assert_eq!(s.doc.borrow().sheet, 1);
        assert_eq!(s.grid.get_cloned().cursor, (0, 0));
    }

    #[test]
    fn authoring_split_panes_scroll_independently_and_share_edits() {
        let s = sheets(b"Original\n");
        run(&s, AuthorCmd::Split(Split::Both));
        let mut app = App::new(s.clone());
        app.tick();
        let rect = Sheets::grid_rect(1200.0, 800.0);
        let panes: Vec<_> = s.authoring.panes(&s, rect).collect();
        assert_eq!(panes.len(), 4);
        let bounds = panes[3].1;
        let point = Point::new(bounds.x() + 70.0, bounds.y() + 45.0);
        // The bottom-right body gets wheel and pointer input at its own origin.
        app.handle_event(Event::Scroll { at: Some(point), dx: 64.0, dy: 92.0, modifiers: Modifiers::default(), phase: ScrollPhase::None });
        app.set_time(250.0);
        app.frame();
        app.tick();
        let sc = panes[3].2.get();
        assert!(sc.physics.y > 0.0 && sc.physics_x.y > 0.0, "{sc:?}");
        for (_, _, sibling) in &panes[..3] {
            assert_eq!((sibling.get().physics.y, sibling.get().physics_x.y), (0.0, 0.0));
        }
        click(&mut app, &s, point);
        assert_eq!(s.authoring.active(rect), 3);
        let at = s.grid.get_cloned().cursor;
        assert!(at.0 > 0 && at.1 > 0);
        for (key, chars) in [(Key::Char, "From pane 4"), (Key::Enter, "")] {
            app.handle_event(Event::Key { kind: KeyEventKind::Down, key, modifiers: Modifiers::default(), chars: chars.into() });
            app.tick();
        }
        run(&s, AuthorCmd::Split(Split::None));
        assert_eq!(s.authoring.panes(&s, rect).count(), 1);
        assert!(s.scroll.get().physics.y > 0.0);
        assert_eq!(s.doc.borrow().wb.value(0, at.0, at.1), officina_grid::Value::str("From pane 4"));
    }

    #[test]
    fn authoring_outline_buttons_hide_and_restore_rows_and_columns() {
        use pergola::widgets::grid_view::HEADER_H;
        let s = sheets(b"A,B,C,D,E\n1,2,3,4,5\n6,7,8,9,10\n");
        let mut g = s.grid.get_cloned();
        g.anchor = (1, 1);
        g.cursor = (2, 2);
        s.grid.set(g);
        for axis in [Axis::Rows, Axis::Cols] { run(&s, AuthorCmd::Outline(axis, OutlineCmd::Group)); }
        let mut app = App::new(s.clone());
        app.tick();
        let y = Sheets::grid_rect(1200.0, 800.0).y();
        let hw = GridView::header_width(64);
        let row_button = Point::new(6.0, y + HEADER_H + 20.0 + 6.0);
        let col_button = Point::new(hw + 64.0 + 6.0, y + 8.0);
        click(&mut app, &s, row_button);
        click(&mut app, &s, col_button);
        assert_eq!(s.source.row_top(1), s.source.row_top(3));
        assert_eq!(s.source.col_left(1), s.source.col_left(3));
        assert_eq!(s.source.outline_control(true, 3), Some((1, 2, true)));
        assert_eq!(s.source.outline_control(false, 3), Some((1, 2, true)));

        let doc = reopen(&s);
        assert_eq!(doc.wb.sheet(0).view_row_sizes().size(1), 0.0);
        assert_eq!(doc.wb.sheet(0).view_col_sizes().size(2), 0.0);
        assert_eq!(doc.wb.value(0, 2, 2), officina_grid::Value::Num(8.0));

        // The adjacent visible header has moved into the same physical slot.
        click(&mut app, &s, row_button);
        click(&mut app, &s, col_button);
        assert_eq!(s.source.row_top(3) - s.source.row_top(1), 40.0);
        assert_eq!(s.source.col_left(3) - s.source.col_left(1), 128.0);
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(s.source.col_left(1), s.source.col_left(3));
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(s.source.row_top(1), s.source.row_top(3));
    }

    #[test]
    fn authoring_chart_edits_ranges_labels_axes_and_combo_and_reopens() {
        let s = sheets(b"Day,Units\nMon,5\nTue,8\n");
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::Chart(ChartKind::Column)));
        s.take_cmds();
        let mut g = s.grid.get_cloned();
        g.picked = Some(0);
        s.grid.set(g);
        let original = {
            let mut d = s.doc.borrow_mut();
            let sheet = d.wb.add_sheet("Sales Team").unwrap();
            for (row, text) in ["Margin", "10", "20"].into_iter().enumerate() {
                d.wb.set_input(CellAddr::new(sheet, row as u32, 2), text).unwrap();
            }
            d.wb.charts()[0].clone()
        };
        let mut app = App::new(s.clone());
        app.tick();
        click(&mut app, &s, Point::new(980.0, TOOLBAR_H + 22.0));
        assert!(s.authoring.chart.get_cloned().is_some());
        input(&s, VALUE, "B2:B3");
        input(&s, CATEGORY, "A2:A3");
        input(&s, NAME, "=B1");
        input(&s, X_TITLE, "Weekday");
        input(&s, Y_TITLE, "Totals");
        run(&s, AuthorCmd::ChartAdd);
        input(&s, VALUE, "A1:B2");
        run(&s, AuthorCmd::ChartApply);
        assert!(s.authoring.chart_error.get_cloned().contains("one row or column"));
        assert_eq!(s.doc.borrow().wb.charts(), &[original.clone()]);
        input(&s, VALUE, "'Sales Team'!$C$2:$C$3");
        input(&s, NAME, "='Sales Team'!C1");
        run(&s, AuthorCmd::ChartType(Some(ChartKind::Line)));
        run(&s, AuthorCmd::ChartAdd);
        input(&s, NAME, "=B1");
        input(&s, VALUE, "B2:B3");
        run(&s, AuthorCmd::ChartType(None));
        run(&s, AuthorCmd::ChartApply);
        assert!(s.authoring.chart.get_cloned().is_none(), "{}", s.authoring.chart_error.get_cloned());
        let chart = s.doc.borrow().wb.charts()[0].clone();
        let reopened = reopen(&s);
        assert_eq!(reopened.wb.charts(), &[chart.clone()]);
        assert_eq!(chart.x_title.as_deref(), Some("Weekday"));
        assert_eq!(chart.y_title.as_deref(), Some("Totals"));
        let data = chart.data(&reopened.wb, 100);
        assert_eq!(data[0].cats, ["Mon", "Tue"]);
        assert_eq!(data[0].name, "Units");
        assert_eq!(data[1].name, "Margin");
        assert_eq!(data[1].kind, ChartKind::Line);
        assert_eq!(data[1].vals, [Some(10.0), Some(20.0)]);
        assert_eq!(data[2].kind, ChartKind::Column);
        for (bytes, read) in [
            (officina_convert::xlsx::write(&reopened.wb, Vec::new()).unwrap(), true),
            (officina_convert::ods::write(&reopened.wb, Vec::new()).unwrap(), false),
        ] {
            let imported = if read { officina_convert::xlsx::read(&bytes).unwrap().0 }
                else { officina_convert::ods::read(&bytes).unwrap().0 };
            let got = &imported.charts()[0];
            assert_eq!(got.x_title, chart.x_title);
            assert_eq!(got.y_title, chart.y_title);
            assert_eq!(got.data(&imported, 100), data, "{}", if read { "XLSX" } else { "ODS" });
        }
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(s.doc.borrow().wb.charts(), &[original]);
        assert!(s.doc.borrow_mut().redo());
        assert_eq!(s.doc.borrow().wb.charts(), &[chart]);
    }
}
