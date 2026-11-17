//! The window's contents: toolbar, formula bar, the grid, a status line.
//!
//! The view is plain Pergola and owns no I/O: buttons queue a [`Cmd`] for the event loop
//! (the binary, which talks to the picker), and grid actions change the [`Doc`] directly.
//! The same view runs headless in `examples/scroll_cost.rs`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::grid_view::col_name;
use pergola::text_edit::TextEdit;
use pergola::widgets::{Button, CellRange, GridAction, GridState, GridView, MenuButton, MenuState, ScrollState, TextField};
use pergola::{clipboard, Color, Mutable, TextStyle};

use crate::doc::Doc;
use officina_grid::charts::{ChartKind, ChartText, Grouping, LegendPos};
use crate::source::SheetSource;

/// The slot the system picker's sheet goes into.
pub const PICKER_SLOT: u64 = 1;
pub const TOOLBAR_H: f32 = 48.0;
pub const FORMULA_H: f32 = 30.0;
pub const STATUS_H: f32 = 26.0;

/// What the toolbar asks the event loop for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    /// Set, change or remove the document's password.
    PasswordOpen,
    /// The password bar's field submitted (to set, or to open a file).
    PasswordDone,
    PasswordClose,
    Open,
    Save,
    SaveAs,
    Undo,
    Redo,
    /// Drop the file's macros (kept inert; never run).
    RemoveMacros,
    /// A Data menu command, on the selection.
    Data(crate::data::DataCmd),
    /// The picked chart: its kind ([`ChartKind::ALL`]).
    ChartKind(usize),
    /// Its legend: none, or at [`LEGENDS`].
    ChartLegend(usize),
    /// Side by side, stacked, stacked to 100 %.
    ChartGroup(usize),
    /// Its title from the chart bar's field.
    ChartTitle,
    ChartDelete,
    /// Lets the chart go.
    ChartDone,
}

/// Legend places the chart bar offers, after None.
pub const LEGENDS: [(&str, LegendPos); 4] = [("Right", LegendPos::Right), ("Bottom", LegendPos::Bottom), ("Top", LegendPos::Top), ("Left", LegendPos::Left)];

/// The chart bar's height (over the formula bar and the grid's top).
pub const CHART_BAR_H: f32 = 48.0;

fn kind_name(k: ChartKind) -> &'static str {
    match k {
        ChartKind::Column => "Column",
        ChartKind::Bar => "Bar",
        ChartKind::Line => "Line",
        ChartKind::Area => "Area",
        ChartKind::Scatter => "Scatter",
        ChartKind::Pie => "Pie",
        ChartKind::Doughnut => "Doughnut",
    }
}

/// The password bar: asking for a protected file's password, or for the
/// document's new one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PasswordBar {
    pub open: bool,
    pub prompt: String,
    /// Setting the document's password (else opening a file).
    pub setting: bool,
}

/// What the view shows besides the document.
#[derive(Debug, Clone, Default)]
pub struct Ui {
    /// The picker is up (its slot is placed over the window).
    pub asking: bool,
}

/// Cheap to clone: every field is shared.
#[derive(Clone)]
pub struct Sheets {
    pub doc: Rc<RefCell<Doc>>,
    pub source: Rc<SheetSource>,
    pub scroll: Mutable<ScrollState>,
    pub grid: Mutable<GridState>,
    pub ui: Mutable<Ui>,
    /// Bumped after every change to the document, so the view renders again.
    pub rev: Mutable<u64>,
    pub cmds: Arc<Mutex<Vec<Cmd>>>,
    /// The window's size in px.
    pub size: Rc<Cell<(f32, f32)>>,
    /// The Data menu's open state.
    pub data_menu: Mutable<MenuState>,
    pub password: Mutable<PasswordBar>,
    pub password_text: Mutable<TextEdit>,
    pub chart_title: Mutable<TextEdit>,
    /// The chart whose title the field holds.
    pub chart_loaded: Rc<Cell<Option<u64>>>,
    pub chart_kind_menu: Mutable<MenuState>,
    pub chart_legend_menu: Mutable<MenuState>,
    pub chart_group_menu: Mutable<MenuState>,
}

impl Sheets {
    pub fn new(doc: Doc, scroll: Mutable<ScrollState>, grid: Mutable<GridState>, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        let doc = Rc::new(RefCell::new(doc));
        let source = Rc::new(SheetSource::new(doc.clone()));
        Self { doc, source, scroll, grid, ui, rev, cmds: Arc::new(Mutex::new(Vec::new())), size: Rc::new(Cell::new(size)), data_menu: Mutable::new(MenuState::default()), password: Mutable::new(PasswordBar::default()), password_text: Mutable::new(TextEdit::new("").secret(true)), chart_title: Mutable::new(TextEdit::new("")), chart_loaded: Rc::new(Cell::new(None)), chart_kind_menu: Mutable::new(MenuState::default()), chart_legend_menu: Mutable::new(MenuState::default()), chart_group_menu: Mutable::new(MenuState::default()) }
    }

    /// The picked chart's index, if a chart is picked and still there.
    pub fn picked_chart(&self) -> Option<usize> {
        let k = self.grid.get_cloned().picked? as usize;
        (k < self.doc.borrow().wb.charts().len()).then_some(k)
    }

    /// Changes the picked chart through `f` (one undo step).
    fn change_chart(&self, label: &str, f: impl FnOnce(&mut officina_grid::charts::Chart)) {
        let Some(i) = self.picked_chart() else { return };
        let mut d = self.doc.borrow_mut();
        let mut all = d.wb.charts().to_vec();
        let before = all[i].clone();
        f(&mut all[i]);
        if all[i] != before {
            d.run(label, vec![officina_grid::ops::GridOp::Charts(all)]);
        }
    }

    /// Shows the password bar with `prompt`, its field empty.
    pub fn ask_password(&self, prompt: &str, setting: bool) {
        self.password_text.set(TextEdit::new("").secret(true));
        self.password.set(PasswordBar { open: true, prompt: prompt.into(), setting });
        self.changed();
    }

    /// The password bar's text, and the bar closed and emptied.
    pub fn take_password(&self) -> (PasswordBar, String) {
        let bar = self.password.get_cloned();
        let text = self.password_text.get_cloned().text().to_string();
        self.password.set(PasswordBar::default());
        self.password_text.set(TextEdit::new("").secret(true));
        self.changed();
        (bar, text)
    }

    /// The document changed outside the grid (opened, saved, recovered).
    pub fn changed(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// A new document on screen, at its top left.
    pub fn replace(&self, doc: Doc) -> Doc {
        let old = std::mem::replace(&mut *self.doc.borrow_mut(), doc);
        self.home();
        old
    }

    /// Back to the top left, nothing selected but A1.
    pub fn home(&self) {
        self.grid.set(GridState::default());
        let mut st = self.scroll.get();
        st.physics.set_position(0.0);
        st.physics_x.set_position(0.0);
        self.scroll.set(st);
        self.changed();
    }

    /// Where the grid sits in a window of `w` × `h`.
    pub fn grid_rect(w: f32, h: f32) -> Rect {
        let top = TOOLBAR_H + FORMULA_H;
        Rect::new(0.0, top, w, (h - top - STATUS_H).max(40.0))
    }

    fn act(doc: &Rc<RefCell<Doc>>, src: &SheetSource, rev: &Mutable<u64>, a: GridAction) {
        // A chart dragged: its anchor from where it was let go.
        let anchor = match &a {
            GridAction::PlaceFloat { at, .. } => Some(src.anchor_of(*at)),
            _ => None,
        };
        let mut d = doc.borrow_mut();
        match a {
            GridAction::PlaceFloat { key, .. } => {
                let mut all = d.wb.charts().to_vec();
                if let (Some(c), Some(an)) = (all.get_mut(key as usize), anchor) {
                    let moved = c.anchor.from.0 != an.from.0 || c.anchor.from.1 != an.from.1;
                    c.anchor = an;
                    d.run(if moved { "Move Chart" } else { "Size Chart" }, vec![officina_grid::ops::GridOp::Charts(all)]);
                }
            }
            GridAction::DeleteFloat(key) => {
                let mut all = d.wb.charts().to_vec();
                if (key as usize) < all.len() {
                    all.remove(key as usize);
                    d.run("Delete Chart", vec![officina_grid::ops::GridOp::Charts(all)]);
                    d.status = "Chart deleted.".into();
                }
            }
            GridAction::Commit { row, col, text } => {
                d.type_into(row, col, &text);
            }
            GridAction::Clear(r) => d.clear(r.r0, r.c0, r.r1, r.c1),
            GridAction::ResizeCol { col, width } => d.set_col_width(col, width),
            GridAction::Undo => {
                d.undo();
            }
            GridAction::Redo => {
                d.redo();
            }
            GridAction::Copy(r) | GridAction::Cut(r) if cells(r) > MAX_COPY_CELLS => {
                d.status = format!("That is {} cells; copying takes at most {MAX_COPY_CELLS}.", cells(r));
            }
            GridAction::Copy(r) => {
                copy(&mut d, r);
            }
            GridAction::Cut(r) => {
                if copy(&mut d, r) {
                    d.clear(r.r0, r.c0, r.r1, r.c1);
                }
            }
            GridAction::Paste { row, col } => match clipboard::paste_text() {
                Ok(Some(t)) => {
                    d.paste_tsv(row, col, &t);
                }
                Ok(None) => d.status = "The clipboard holds no text.".into(),
                Err(e) => d.status = format!("Couldn't paste: {e:?}"),
            },
        }
        drop(d);
        rev.set(rev.get().wrapping_add(1));
    }
}

/// The most cells one copy puts on the clipboard (as text, about 30 MB).
pub const MAX_COPY_CELLS: u64 = 2_000_000;

fn cells(r: CellRange) -> u64 {
    (r.r1 - r.r0) as u64 * (r.c1 - r.c0 + 1) as u64 + (r.c1 - r.c0 + 1) as u64
}

fn copy(d: &mut Doc, r: CellRange) -> bool {
    let text = d.copy_tsv(r.r0, r.c0, r.r1, r.c1);
    match clipboard::copy_text(&text, false) {
        Ok(()) => true,
        Err(e) => {
            d.status = format!("Couldn't copy: {e:?}");
            false
        }
    }
}

/// "B12", or "B2:D9" for a range.
pub fn range_name(r: CellRange) -> String {
    let one = |row: u32, col: u32| format!("{}{}", col_name(col), row as u64 + 1);
    if r.r0 == r.r1 && r.c0 == r.c1 {
        one(r.r0, r.c0)
    } else {
        format!("{}:{}", one(r.r0, r.c0), one(r.r1, r.c1))
    }
}

impl View for Sheets {
    fn render(&self, ctx: &mut Ctx) {
        let _ = self.rev.get();
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });

        // Toolbar.
        let mut x = 12.0;
        for (label, cmd, wd) in [("Open…", Cmd::Open, 76.0), ("Save", Cmd::Save, 64.0), ("Save As…", Cmd::SaveAs, 92.0), ("Undo", Cmd::Undo, 64.0), ("Redo", Cmd::Redo, 64.0)] {
            let cmds = self.cmds.clone();
            Button::secondary(label).at(x, 8.0).width(wd).on_click(move || cmds.lock().unwrap().push(cmd)).render(ctx);
            x += wd + 8.0;
        }
        // The Data menu, read from the document and the selection as they are now.
        {
            let g = self.grid.get_cloned();
            let entries = crate::data::menu(&self.doc.borrow(), g.selection(), g.cursor);
            let (items, cmds_of): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
            let cmds = self.cmds.clone();
            MenuButton::new("Data", self.data_menu.clone(), items, move |i| {
                if let Some(Some(c)) = cmds_of.get(i) {
                    cmds.lock().unwrap().push(Cmd::Data(*c));
                }
            })
            .at(x, 8.0)
            .width(76.0)
            .render(ctx);
            x += 84.0;
        }
        {
            let cmds = self.cmds.clone();
            Button::secondary("Password…").at(x, 8.0).width(100.0).on_click(move || cmds.lock().unwrap().push(Cmd::PasswordOpen)).render(ctx);
            x += 108.0;
        }
        let (name, dirty, status, macros) = {
            let d = self.doc.borrow();
            (d.name.clone(), d.dirty, d.status.clone(), d.wb.macros().is_some())
        };
        if macros {
            // The file has macros: say they don't run, and offer to drop them (§5.3).
            let cmds = self.cmds.clone();
            Button::secondary("Remove Macros").at(x, 8.0).width(132.0).on_click(move || cmds.lock().unwrap().push(Cmd::RemoveMacros)).render(ctx);
            x += 140.0;
        }
        let title = if dirty { format!("{name} (edited)") } else { name };
        let title = if macros { format!("{title} · has macros; Atrium doesn't run them") } else { title };
        ctx.add(Node::Text { rect: Rect::new(x + 12.0, 14.0, (w - x - 24.0).max(0.0), 22.0), content: title, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });

        // Formula bar: where the cursor is and what the cell holds.
        let g = self.grid.get_cloned();
        let (cr, cc) = g.cursor;
        let y = TOOLBAR_H;
        ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, 1.0), fill: t.border_default(), radius: 0.0 });
        ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, 96.0, 20.0), content: range_name(g.selection()), style: st(type_size::SM, Weight::Semibold, t.text_primary()) });
        ctx.add(Node::Rect { rect: Rect::new(112.0, y + 4.0, 1.0, FORMULA_H - 8.0), fill: t.border_default(), radius: 0.0 });
        let contents = match &g.edit {
            Some(e) => e.text().to_string(),
            None => pergola::widgets::GridSource::edit_text(&*self.source, cr, cc),
        };
        let shown: String = contents.chars().take(400).collect();
        ctx.add(Node::Text { rect: Rect::new(124.0, y + 6.0, (w - 136.0).max(0.0), 20.0), content: shown, style: TextStyle { family: font::MONO.into(), size: type_size::SM, weight: Weight::Regular, color: t.text_primary() } });

        // The grid.
        let doc = self.doc.clone();
        let rev = self.rev.clone();
        let src = self.source.clone();
        GridView::new(self.source.clone(), self.scroll.clone(), self.grid.clone(), Self::grid_rect(w, h)).on_action(move |a| Self::act(&doc, &src, &rev, a)).emit(ctx);

        // Status line.
        let sy = h - STATUS_H;
        ctx.add(Node::Rect { rect: Rect::new(0.0, sy, w, STATUS_H), fill: t.bg_surface(), radius: 0.0 });
        let line = if status.is_empty() {
            let d = self.doc.borrow();
            let (r, c) = d.wb.extent(d.sheet);
            format!("{r} rows × {c} columns used")
        } else {
            status
        };
        ctx.add(Node::Text { rect: Rect::new(12.0, sy + 4.0, (w - 24.0).max(0.0), 18.0), content: line, style: st(type_size::XS, Weight::Regular, t.text_secondary()) });

        // The password bar, over the formula bar and the grid's top.
        let pb = self.password.get_cloned();
        if pb.open {
            let (y, bh) = (TOOLBAR_H, 48.0);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, bh), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let lw = 300.0f32.min(w * 0.4);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, lw, 20.0), content: pb.prompt.clone(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let fw = (w - 24.0 - lw - 160.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.password_text.clone()).placeholder("Password").autofocus(true).at(lw + 20.0, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::PasswordDone)).render(ctx);
            Button::primary(if pb.setting { "Set" } else { "Open" }).at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::PasswordDone)).render(ctx);
            Button::secondary("Cancel").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::PasswordClose)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + bh - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        }

        // The chart bar: the picked chart's kind, title, legend and stacking.
        if let (Some(i), false) = (self.picked_chart(), pb.open) {
            let ch = self.doc.borrow().wb.charts()[i].clone();
            if self.chart_loaded.get() != Some(i as u64) {
                self.chart_loaded.set(Some(i as u64));
                let title = ch.title.as_ref().map(|t| officina_grid::charts::Chart::text(&self.doc.borrow().wb, t)).unwrap_or_default();
                self.chart_title.set(TextEdit::new(&title));
            }
            let (y, bh) = (TOOLBAR_H, CHART_BAR_H);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, bh), fill: t.bg_surface(), radius: 0.0 });
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, 60.0, 20.0), content: "Chart".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let mut x = 72.0;
            let kinds: Vec<pergola::widgets::MenuItem> = ChartKind::ALL.iter().map(|k| pergola::widgets::MenuItem::new(kind_name(*k)).radio(*k == ch.kind)).collect();
            let cmds = self.cmds.clone();
            MenuButton::new(kind_name(ch.kind).to_string(), self.chart_kind_menu.clone(), kinds, move |i| cmds.lock().unwrap().push(Cmd::ChartKind(i))).at(x, y + 8.0).width(110.0).render(ctx);
            x += 118.0;
            let fw = (w - x - 470.0).max(120.0);
            let cmds = self.cmds.clone();
            TextField::new(self.chart_title.clone()).placeholder("Title").at(x, y + 8.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::ChartTitle)).render(ctx);
            x += fw + 8.0;
            let mut legends = vec![pergola::widgets::MenuItem::new("No Legend").radio(ch.legend.is_none())];
            legends.extend(LEGENDS.iter().map(|(n, p)| pergola::widgets::MenuItem::new(format!("Legend {n}")).radio(ch.legend == Some(*p))));
            let now = ch.legend.and_then(|p| LEGENDS.iter().find(|l| l.1 == p)).map_or("No Legend".to_string(), |l| format!("Legend {}", l.0));
            let cmds = self.cmds.clone();
            MenuButton::new(now, self.chart_legend_menu.clone(), legends, move |i| cmds.lock().unwrap().push(Cmd::ChartLegend(i))).at(x, y + 8.0).width(140.0).render(ctx);
            x += 148.0;
            if !ch.kind.round() && ch.kind != ChartKind::Scatter {
                let groups = [("Side by Side", Grouping::Standard), ("Stacked", Grouping::Stacked), ("Stacked 100 %", Grouping::Percent)];
                let items: Vec<pergola::widgets::MenuItem> = groups.iter().map(|(n, g)| pergola::widgets::MenuItem::new(*n).radio(*g == ch.grouping)).collect();
                let now = groups.iter().find(|g| g.1 == ch.grouping).map_or("Side by Side", |g| g.0);
                let cmds = self.cmds.clone();
                MenuButton::new(now.to_string(), self.chart_group_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::ChartGroup(i))).at(x, y + 8.0).width(130.0).render(ctx);
            }
            Button::secondary("Delete").at(w - 148.0, y + 8.0).width(64.0).on_click(push(Cmd::ChartDelete)).render(ctx);
            Button::primary("Done").at(w - 76.0, y + 8.0).width(64.0).on_click(push(Cmd::ChartDone)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + bh - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
        } else {
            self.chart_loaded.set(None);
        }

        if self.ui.get_cloned().asking {
            // The system places the picker's sheet over the window.
            let id = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: Color::TRANSPARENT, radius: 0.0 });
            ctx.slot(id, PICKER_SLOT);
        }
        ctx.pop();
    }
}

impl Sheets {
    /// The toolbar's file commands since the last call; undo and redo are run here.
    pub fn take_cmds(&self) -> Vec<Cmd> {
        let cmds = std::mem::take(&mut *self.cmds.lock().unwrap());
        cmds.into_iter()
            .filter(|&c| match c {
                Cmd::Undo => {
                    Self::act(&self.doc, &self.source, &self.rev, GridAction::Undo);
                    false
                }
                Cmd::Redo => {
                    Self::act(&self.doc, &self.source, &self.rev, GridAction::Redo);
                    false
                }
                Cmd::Data(dc) => {
                    let g = self.grid.get_cloned();
                    crate::data::run(&mut self.doc.borrow_mut(), g.selection(), g.cursor, dc);
                    self.rev.set(self.rev.get().wrapping_add(1));
                    false
                }
                Cmd::ChartKind(k) => {
                    let k = ChartKind::ALL[k.min(ChartKind::ALL.len() - 1)];
                    self.change_chart("Chart Type", |c| c.kind = k);
                    self.changed();
                    false
                }
                Cmd::ChartLegend(i) => {
                    let p = i.checked_sub(1).map(|i| LEGENDS[i.min(LEGENDS.len() - 1)].1);
                    self.change_chart("Chart Legend", |c| c.legend = p);
                    self.changed();
                    false
                }
                Cmd::ChartGroup(i) => {
                    let g = [Grouping::Standard, Grouping::Stacked, Grouping::Percent][i.min(2)];
                    self.change_chart("Chart Stacking", |c| c.grouping = g);
                    self.changed();
                    false
                }
                Cmd::ChartTitle => {
                    let t = self.chart_title.get_cloned().text().trim().chars().take(officina_grid::charts::MAX_TITLE).collect::<String>();
                    self.change_chart("Chart Title", |c| c.title = (!t.is_empty()).then(|| ChartText::Lit(t.as_str().into())));
                    self.changed();
                    false
                }
                Cmd::ChartDelete => {
                    if let Some(i) = self.picked_chart() {
                        Self::act(&self.doc, &self.source, &self.rev, GridAction::DeleteFloat(i as u64));
                    }
                    let mut g = self.grid.get_cloned();
                    g.picked = None;
                    self.grid.set(g);
                    false
                }
                Cmd::ChartDone => {
                    let mut g = self.grid.get_cloned();
                    g.picked = None;
                    self.grid.set(g);
                    self.changed();
                    false
                }
                Cmd::PasswordOpen => {
                    let protected = self.doc.borrow().protection.is_some();
                    self.ask_password(if protected { "New password (empty removes it)" } else { "Password to protect this sheet" }, true);
                    false
                }
                Cmd::PasswordClose => {
                    self.password.set(PasswordBar::default());
                    self.password_text.set(TextEdit::new("").secret(true));
                    self.changed();
                    true
                }
                Cmd::RemoveMacros => {
                    let mut d = self.doc.borrow_mut();
                    d.remove_macros();
                    d.status = "Macros removed. The file keeps them until you save it.".into();
                    drop(d);
                    self.rev.set(self.rev.get().wrapping_add(1));
                    false
                }
                _ => true,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::app::App;
    use pergola::event::{Event, Key, KeyEventKind, Modifiers};
    use pergola::geom::Point;

    #[test]
    fn a_chart_is_picked_moved_sized_edited_and_deleted() {
        let (mut d, _) = Doc::from_csv("t.csv", b"Fruit,Qty\npear,3\napple,10\nfig,7\n").unwrap();
        d.wb.recalc();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::Chart(ChartKind::Column)));
        s.take_cmds();
        let mut app = App::new(s.clone());
        app.tick();
        let before = s.doc.borrow().wb.charts()[0].anchor;
        let gr = Sheets::grid_rect(1200.0, 800.0);
        // The chart's box on screen: from the source, less the scroll (none) and headers.
        let box_of = |s: &Sheets| {
            let a = s.doc.borrow().wb.charts()[0].anchor;
            let x = pergola::widgets::GridSource::col_left(&*s.source, a.from.1) + a.from.2 as f64 * 96.0 / 72.0;
            let y = pergola::widgets::GridSource::row_top(&*s.source, a.from.0) + a.from.3 as f64 * 96.0 / 72.0;
            (x, y)
        };
        let (x0, y0) = box_of(&s);
        // The body starts after the row and column headers.
        // A view of about 40 rows numbers its headers to row 64: two digits.
        let (hw, hh) = (pergola::widgets::GridView::header_width(64), pergola::widgets::grid_view::HEADER_H);
        let mid = |dx: f32, dy: f32| {
            let (bx, by) = (x0 as f32 + 120.0, y0 as f32 + 60.0);
            Point::new(gr.x() + hw + bx + dx, gr.y() + hh + by + dy)
        };
        let press = |app: &mut App<Sheets>, p: Point, to: Point| {
            app.handle_event(Event::PointerDown { at: p });
            app.handle_event(Event::PointerMove { at: Point::new((p.x + to.x) / 2.0, (p.y + to.y) / 2.0) });
            app.handle_event(Event::PointerMove { at: to });
            app.handle_event(Event::PointerUp { at: to });
            app.tick();
        };
        press(&mut app, mid(0.0, 0.0), mid(0.0, 0.0));
        assert_eq!(s.grid.get_cloned().picked, Some(0), "a click picks the chart");
        assert_eq!(s.picked_chart(), Some(0));
        // Dragged 150 px right and 100 px down: the anchor follows, one undo step.
        press(&mut app, mid(0.0, 0.0), mid(150.0, 100.0));
        let (x1, y1) = box_of(&s);
        assert!(((x1 - x0) - 150.0).abs() <= 1.5 && ((y1 - y0) - 100.0).abs() <= 1.5, "moved by {}, {}", x1 - x0, y1 - y0);
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(s.doc.borrow().wb.charts()[0].anchor, before);
        app.tick();
        // Its corner sizes it: the top-left stays.
        let a = before;
        let src = &*s.source;
        let (x2, y2) = (pergola::widgets::GridSource::col_left(src, a.to.1) + a.to.2 as f64 * 96.0 / 72.0, pergola::widgets::GridSource::row_top(src, a.to.0) + a.to.3 as f64 * 96.0 / 72.0);
        let corner = Point::new(mid(0.0, 0.0).x + (x2 - x0) as f32 - 120.0 - 4.0, mid(0.0, 0.0).y + (y2 - y0) as f32 - 60.0 - 4.0);
        press(&mut app, corner, Point::new(corner.x + 80.0, corner.y + 40.0));
        let after = s.doc.borrow().wb.charts()[0].anchor;
        assert_eq!((after.from.0, after.from.1), (before.from.0, before.from.1), "{after:?}");
        assert!(after.to.1 > before.to.1 || after.to.2 > before.to.2, "{after:?}");
        // The chart bar: kind, title, legend.
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let t = texts();
        for want in ["Chart", "Column", "Delete", "Done"] {
            assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
        }
        for c in [Cmd::ChartKind(2), Cmd::ChartLegend(2), Cmd::ChartGroup(1)] {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        }
        s.chart_title.set(TextEdit::new("Fruit sold"));
        s.cmds.lock().unwrap().push(Cmd::ChartTitle);
        s.take_cmds();
        {
            let d = s.doc.borrow();
            let c = &d.wb.charts()[0];
            assert_eq!((c.kind, c.legend, c.grouping, c.title.clone()), (ChartKind::Line, Some(LegendPos::Bottom), Grouping::Stacked, Some(ChartText::Lit("Fruit sold".into()))));
        }
        // Delete with it picked takes it away; undo brings it back.
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Delete, modifiers: Modifiers::default(), chars: String::new() });
        app.tick();
        assert!(s.doc.borrow().wb.charts().is_empty());
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(s.doc.borrow().wb.charts().len(), 1);
        // A click on a cell lets it go.
        press(&mut app, Point::new(gr.x() + 80.0, gr.y() + 60.0), Point::new(gr.x() + 80.0, gr.y() + 60.0));
        assert_eq!(s.grid.get_cloned().picked, None);
    }
}
