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
use pergola::{Color, Mutable, TextStyle};

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
    /// The fill options button pressed: its menu of ways to fill.
    FillOptions,
    /// Set, change or remove the document's password.
    PasswordOpen,
    /// The password bar's field submitted (to set, or to open a file).
    PasswordDone,
    PasswordClose,
    /// Closing with unsaved changes: save first (then close).
    CloseSave,
    /// Closing with unsaved changes: close without them.
    CloseDiscard,
    /// Closing with unsaved changes: keep the window open.
    CloseCancel,
    Open,
    Save,
    SaveAs,
    /// Print the sheet through the print sheet (Ctrl+P too).
    Print,
    Undo,
    Redo,
    /// Drop the file's macros (kept inert; never run).
    RemoveMacros,
    /// A Data menu command, on the selection.
    Data(crate::data::DataCmd),
    /// A press in the Compare with… pane.
    Review(crate::review::ReviewCmd),
    /// A press on the sheet tabs or their rename bar.
    Tab(crate::tabs::TabCmd),
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
    /// The value list of filter column `col`, opened.
    FilterOpen(u32),
    /// The value list: value `i` of the column chosen or not.
    FilterToggle(usize),
    /// Every value the search shows chosen (`true`) or none of them.
    FilterAll(bool),
    FilterApply,
    FilterCancel,
    /// The data validation panel.
    Valid(ValidCmd),
    /// The open rule panel's Apply To field takes the grid's selection.
    UseSelection,
    /// The validation list: item `i` (of those the search shows) typed into
    /// the cell; closed.
    ListPick(usize),
    ListCancel,
    /// The custom lists panel: forget list `.0`; closed.
    ListsForget(usize),
    ListsClose,
    /// The conditional-format panel.
    Cond(CondCmd),
    /// The pivot field list.
    Pivot(PivotCmd),
}

/// What the pivot field list asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PivotCmd {
    /// Source field `f` picked.
    Pick(u32),
    /// The picked field to an area, or out of the pivot.
    To(Option<crate::pivotpane::Area>),
    Earlier,
    Later,
    /// Value field `k` summed by [`Agg::ALL`](officina_grid::pivot::Agg::ALL)`[i]`.
    Agg(usize, usize),
    Totals(crate::pivotpane::Totals),
    Close,
}

/// The pivot field list's width, at the window's right over the grid.
pub const PIVOT_W: f32 = 340.0;
/// A field's row in it.
pub const PIVOT_ROW_H: f32 = 28.0;
/// Value fields whose summary the pane offers to change.
pub const PIVOT_AGG_ROWS: usize = 4;
const AGG_NAMES: [&str; 11] = ["Sum", "Count", "Average", "Max", "Min", "Product", "Count Numbers", "StdDev", "StdDevp", "Var", "Varp"];

/// What the conditional-format panel asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CondCmd {
    /// Row `i` of the rules over the selection picked.
    Pick(usize),
    New,
    Edit,
    Delete,
    Up,
    Down,
    Close,
    /// The rule's kind ([`crate::condfmt::KINDS`]).
    Kind(usize),
    /// Its comparison, scale, colour or set ([`crate::condfmt::choices`]).
    Choice(usize),
    /// Its look ([`crate::condfmt::LOOKS`]).
    Look(usize),
    Reverse,
    IconOnly,
    Stop,
    Save,
    /// Back to the list without saving.
    Back,
}

/// The data validation panel's controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidCmd {
    /// What the cells allow ([`crate::validation::ALLOWS`]), the comparison
    /// ([`crate::validation::OPS`]), the error style ([`crate::validation::STYLES`]).
    Allow(usize),
    Op(usize),
    Style(usize),
    Blank,
    Dropdown,
    Prompt,
    Error,
    Save,
    /// The rule being changed, deleted.
    Remove,
    Close,
}

/// The conditional-format panel's width, at the window's right over the grid.
pub const COND_W: f32 = 360.0;
/// A rule's row in its list.
pub const COND_ROW_H: f32 = 44.0;

/// The value list's width, at the window's right over the grid.
pub const FILTER_W: f32 = 300.0;
/// A value's row in the list.
pub const FILTER_ROW_H: f32 = 28.0;

/// A validation rule's list, to pick the cell's value from (§16.83).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListPanel {
    pub open: bool,
    pub sheet: u16,
    pub row: u32,
    pub col: u32,
    pub items: Vec<String>,
}

impl ListPanel {
    /// The items the search `q` shows (ignoring case), by index.
    pub fn shown(&self, q: &str) -> Vec<usize> {
        let q = q.trim().to_lowercase();
        (0..self.items.len()).filter(|&i| q.is_empty() || self.items[i].to_lowercase().contains(&q)).collect()
    }
}

/// The value list of a filter column: what Apply keeps.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FilterPanel {
    pub open: bool,
    /// The column, of the sheet.
    pub col: u32,
    /// The header's text, for the panel's title.
    pub title: String,
    /// Each distinct shown text and how many rows show it.
    pub values: Vec<(String, u32)>,
    /// Chosen, by value.
    pub on: Vec<bool>,
}

impl FilterPanel {
    /// The values the search `q` shows (ignoring case), by index.
    pub fn shown(&self, q: &str) -> Vec<usize> {
        let q = q.trim().to_lowercase();
        (0..self.values.len()).filter(|&i| q.is_empty() || self.values[i].0.to_lowercase().contains(&q)).collect()
    }
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
    /// The fill options button's menu.
    pub fill_menu: Mutable<MenuState>,
    /// The custom lists panel is open.
    pub lists_open: Mutable<bool>,
    pub password: Mutable<PasswordBar>,
    pub password_text: Mutable<TextEdit>,
    pub chart_title: Mutable<TextEdit>,
    /// The chart whose title the field holds.
    pub chart_loaded: Rc<Cell<Option<u64>>>,
    pub chart_kind_menu: Mutable<MenuState>,
    pub chart_legend_menu: Mutable<MenuState>,
    pub chart_group_menu: Mutable<MenuState>,
    pub filter: Mutable<FilterPanel>,
    pub filter_search: Mutable<TextEdit>,
    pub filter_scroll: Mutable<ScrollState>,
    /// Asking whether to save before the window closes.
    pub closing: Mutable<bool>,
    pub cond: Mutable<crate::condfmt::CondPanel>,
    /// Compare with…: the pane, and the other copy it compares with.
    pub review: Mutable<crate::review::ReviewPane>,
    pub theirs: Rc<RefCell<Option<officina_grid::Workbook>>>,
    pub review_scroll: Mutable<ScrollState>,
    /// Its Apply To field and the rule's two value fields.
    pub cond_range: Mutable<TextEdit>,
    pub cond_a: Mutable<TextEdit>,
    pub cond_b: Mutable<TextEdit>,
    pub cond_kind_menu: Mutable<MenuState>,
    pub cond_choice_menu: Mutable<MenuState>,
    pub cond_look_menu: Mutable<MenuState>,
    pub pivot_pane: Mutable<crate::pivotpane::PivotPane>,
    pub pivot_scroll: Mutable<ScrollState>,
    pub pivot_agg_menus: Rc<Vec<Mutable<MenuState>>>,
    /// The first sheet tab drawn, and the rename bar (open, and its field).
    pub tab_first: Mutable<u16>,
    pub renaming: Mutable<bool>,
    /// The tab colour bar is showing.
    pub coloring: Mutable<bool>,
    pub rename_text: Mutable<TextEdit>,
    /// The comment bar: the cell it writes the comment of (sheet, row,
    /// column), and its field (lines shown joined by ¶).
    pub commenting: Mutable<Option<(u16, u32, u32)>>,
    thread_edit: Mutable<Option<crate::comment::ThreadEdit>>,
    pub comment_text: Mutable<TextEdit>,
    /// The link bar: the cell it writes the link of, and its field.
    pub linking: Mutable<Option<(u16, u32, u32)>>,
    pub link_text: Mutable<TextEdit>,
    /// The exact-size bar: sheet, rows (else columns), first and last.
    pub size_bar: Mutable<Option<(u16, bool, u32, u32)>>,
    /// The Paste Special bar, open with its choices.
    pub paste_bar: Mutable<Option<officina_grid::clip::Special>>,
    pub size_text: Mutable<TextEdit>,
    pub list: Mutable<ListPanel>,
    pub valid: Mutable<crate::validation::Panel>,
    pub valid_text: Rc<Vec<Mutable<TextEdit>>>,
    pub valid_menus: Rc<Vec<Mutable<MenuState>>>,
    pub list_search: Mutable<TextEdit>,
    pub list_scroll: Mutable<ScrollState>,
    /// The activation of the press on Open Link, for the link opener.
    pub link_press: Rc<Cell<Option<fresco_protocol::activation::Token>>>,
    /// The link opener's answer, from its thread.
    pub opened: Arc<Mutex<Option<Result<(), String>>>>,
}

impl Sheets {
    pub fn new(doc: Doc, scroll: Mutable<ScrollState>, grid: Mutable<GridState>, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        let doc = Rc::new(RefCell::new(doc));
        let source = Rc::new(SheetSource::new(doc.clone()));
        let comment_text = rev.sibling(TextEdit::new(""));
        Self { doc, source, scroll, grid, ui, rev, cmds: Arc::new(Mutex::new(Vec::new())), size: Rc::new(Cell::new(size)), data_menu: Mutable::new(MenuState::default()), fill_menu: Mutable::new(MenuState::default()), lists_open: Mutable::new(false), password: Mutable::new(PasswordBar::default()), password_text: Mutable::new(TextEdit::new("").secret(true)), chart_title: Mutable::new(TextEdit::new("")), chart_loaded: Rc::new(Cell::new(None)), chart_kind_menu: Mutable::new(MenuState::default()), chart_legend_menu: Mutable::new(MenuState::default()), chart_group_menu: Mutable::new(MenuState::default()), filter: Mutable::new(FilterPanel::default()), filter_search: Mutable::new(TextEdit::new("")), filter_scroll: Mutable::new(ScrollState::default()), closing: Mutable::new(false), cond: Mutable::new(crate::condfmt::CondPanel::default()), review: Mutable::new(crate::review::ReviewPane::default()), theirs: Rc::new(RefCell::new(None)), review_scroll: Mutable::new(ScrollState::default()), cond_range: Mutable::new(TextEdit::new("")), cond_a: Mutable::new(TextEdit::new("")), cond_b: Mutable::new(TextEdit::new("")), cond_kind_menu: Mutable::new(MenuState::default()), cond_choice_menu: Mutable::new(MenuState::default()), cond_look_menu: Mutable::new(MenuState::default()), pivot_pane: Mutable::new(crate::pivotpane::PivotPane::default()), pivot_scroll: Mutable::new(ScrollState::default()), pivot_agg_menus: Rc::new((0..PIVOT_AGG_ROWS).map(|_| Mutable::new(MenuState::default())).collect()), tab_first: Mutable::new(0), renaming: Mutable::new(false), coloring: Mutable::new(false), rename_text: Mutable::new(TextEdit::new("")), commenting: Mutable::new(None), thread_edit: Mutable::new(None), comment_text, linking: Mutable::new(None), link_text: Mutable::new(TextEdit::new("")), size_bar: Mutable::new(None), paste_bar: Mutable::new(None), size_text: Mutable::new(TextEdit::new("")), link_press: Rc::new(Cell::new(None)), opened: Arc::new(Mutex::new(None)), list: Mutable::new(ListPanel::default()), valid: Mutable::new(crate::validation::Panel::default()), valid_text: Rc::new((0..crate::validation::FIELDS).map(|_| Mutable::new(TextEdit::new(""))).collect()), valid_menus: Rc::new((0..3).map(|_| Mutable::new(MenuState::default())).collect()), list_search: Mutable::new(TextEdit::new("")), list_scroll: Mutable::new(ScrollState::default()) }
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

    /// Opens the value list of filter column `col`, or says why not.
    pub fn choose_values(&self, col: u32) {
        let got = crate::data::column_values(&self.doc.borrow(), col);
        match got {
            Ok(v) => {
                let d = self.doc.borrow();
                let r0 = d.wb.sheet(d.sheet).filter.as_ref().map_or(0, |f| f.rect.r0);
                let head = d.wb.formatted(officina_grid::addr::CellAddr::new(d.sheet, r0, col)).text;
                drop(d);
                let title = if head.trim().is_empty() { format!("Column {}", col_name(col)) } else { format!("{} · {}", col_name(col), head.trim()) };
                let on = v.iter().map(|x| x.2).collect();
                self.filter.set(FilterPanel { open: true, col, title, values: v.into_iter().map(|x| (x.0, x.1)).collect(), on });
                self.filter_search.set(TextEdit::new(""));
                self.filter_scroll.set(ScrollState::default());
            }
            Err(e) => self.doc.borrow_mut().status = e,
        }
        self.changed();
    }

    /// The value list's choice as the filter, and the list closed.
    pub fn apply_values(&self) {
        let p = self.filter.get_cloned();
        if !p.open {
            return;
        }
        if !p.on.iter().any(|&o| o) {
            self.doc.borrow_mut().status = "Choose at least one value to show.".into();
            self.changed();
            return;
        }
        let keep: Vec<String> = p.values.iter().zip(&p.on).filter(|(_, &o)| o).map(|(v, _)| v.0.clone()).collect();
        let every = keep.len() == p.values.len();
        crate::data::keep_values(&mut self.doc.borrow_mut(), p.col, keep, every);
        self.filter.set(FilterPanel::default());
        self.changed();
    }

    /// Opens the field list of the pivot at the cursor, or says why not.
    pub fn open_pivot_pane(&self) {
        let (r, c) = self.grid.get_cloned().cursor;
        let here = {
            let d = self.doc.borrow();
            crate::pivotpane::pivot_here(&d.wb, d.sheet, r, c)
        };
        match here {
            Some(i) => {
                self.filter.set(FilterPanel::default());
                self.cond.set(crate::condfmt::CondPanel::default());
                self.pivot_scroll.set(ScrollState::default());
                self.pivot_pane.set(crate::pivotpane::PivotPane { open: true, pivot: i, picked: None, error: String::new() });
            }
            None => self.doc.borrow_mut().status = "Put the cursor in a pivot table or its source data first.".into(),
        }
        self.changed();
    }

    /// Runs a press in the pivot field list.
    pub fn pivot_cmd(&self, c: PivotCmd) {
        use crate::pivotpane as pp;
        let mut p = self.pivot_pane.get_cloned();
        if !p.open {
            return;
        }
        let i = p.pivot;
        let r = {
            let mut d = self.doc.borrow_mut();
            match c {
                PivotCmd::Pick(f) => {
                    p.picked = Some(f);
                    Ok(())
                }
                PivotCmd::To(a) => p.picked.map_or(Ok(()), |f| pp::place(&mut d, i, f, a)),
                PivotCmd::Earlier | PivotCmd::Later => p.picked.map_or(Ok(()), |f| pp::shift(&mut d, i, f, c == PivotCmd::Earlier)),
                PivotCmd::Agg(k, a) => pp::set_agg(&mut d, i, k, officina_grid::pivot::Agg::ALL[a.min(10)]),
                PivotCmd::Totals(t) => pp::toggle(&mut d, i, t),
                PivotCmd::Close => {
                    p = pp::PivotPane::default();
                    Ok(())
                }
            }
        };
        p.error = r.err().unwrap_or_default();
        self.pivot_pane.set(p);
        self.changed();
    }

    /// Compares this workbook with `theirs` (named `name`) and lists what
    /// differs in the Compare pane.
    pub fn compare_with(&self, name: &str, theirs: officina_grid::Workbook) {
        let mut p = crate::review::ReviewPane { open: true, name: name.to_string(), ..Default::default() };
        p.refresh(&self.doc.borrow().wb, &theirs);
        p.picked = (!p.changes.is_empty()).then_some(0);
        *self.theirs.borrow_mut() = Some(theirs);
        self.cond.set(crate::condfmt::CondPanel::default());
        self.pivot_pane.set(crate::pivotpane::PivotPane::default());
        self.filter.set(FilterPanel::default());
        self.doc.borrow_mut().status = p.summary() + ".";
        self.review.set(p);
        self.review_cmd(crate::review::ReviewCmd::Pick(0));
    }

    /// Selects change `ch`'s cell and scrolls it into view.
    fn show_change(&self, ch: &officina_grid::compare::Change) {
        use pergola::widgets::GridSource;
        let Some((sheet, r, c)) = ch.at() else { return };
        self.doc.borrow_mut().sheet = sheet;
        let mut g = self.grid.get_cloned();
        g.select(r, c);
        self.grid.set(g);
        let mut st = self.scroll.get();
        st.physics.stop();
        st.physics_x.stop();
        st.physics.set_position((self.source.row_top(r) - 3.0 * (self.source.row_top(r + 1) - self.source.row_top(r))).max(0.0));
        st.physics_x.set_position((self.source.col_left(c) - 60.0).max(0.0));
        self.scroll.set(st);
    }

    /// Runs a press in the Compare pane.
    pub fn review_cmd(&self, c: crate::review::ReviewCmd) {
        use crate::review::{self as rv, ReviewCmd};
        let mut p = self.review.get_cloned();
        let theirs = self.theirs.clone();
        let theirs = theirs.borrow();
        let Some(other) = theirs.as_ref() else { return };
        match c {
            ReviewCmd::Pick(i) => {
                p.picked = (i < p.changes.len()).then_some(i);
                if let Some(ch) = p.picked.map(|i| p.changes[i].clone()) {
                    self.show_change(&ch);
                }
            }
            ReviewCmd::Take | ReviewCmd::Keep => {
                if let Some(ch) = p.picked.and_then(|i| p.changes.get(i)).cloned() {
                    if c == ReviewCmd::Keep {
                        p.kept.push(ch);
                    } else if !rv::take(&mut self.doc.borrow_mut(), other, &ch) {
                        self.doc.borrow_mut().status = "Sheets added or removed can't be taken here; add or delete the sheet itself.".into();
                        p.kept.push(ch);
                    }
                    p.refresh(&self.doc.borrow().wb, other);
                    if let Some(ch) = p.picked.map(|i| p.changes[i].clone()) {
                        self.show_change(&ch);
                    }
                }
            }
            ReviewCmd::TakeAll => {
                let n = rv::take_all(&mut self.doc.borrow_mut(), other, &p.changes);
                p.refresh(&self.doc.borrow().wb, other);
                self.doc.borrow_mut().status = format!("Took {n} change{} from “{}”.", if n == 1 { "" } else { "s" }, p.name);
            }
            ReviewCmd::Close => {
                p = rv::ReviewPane::default();
                drop(theirs);
                *self.theirs.borrow_mut() = None;
            }
        }
        self.review.set(p);
        self.changed();
    }

    /// Opens the conditional-format panel on the selection's rules.
    pub fn open_cond(&self) {
        let sel = self.grid.get_cloned().selection();
        let sheet = self.doc.borrow().sheet;
        let sel = officina_grid::Rect { sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
        let any = !crate::condfmt::entries(&self.doc.borrow().wb, sel).is_empty();
        self.filter.set(FilterPanel::default());
        self.pivot_pane.set(crate::pivotpane::PivotPane::default());
        self.cond.set(crate::condfmt::CondPanel { open: true, sel: Some(sel), picked: any.then_some(0), editor: None, error: String::new() });
        if !any {
            // Nothing here yet: straight to a new rule.
            self.cond_cmd(CondCmd::New);
        }
        self.changed();
    }

    /// Opens the data validation panel on the cursor's rule, or a new rule
    /// for the selection.
    pub fn open_valid(&self) {
        let g = self.grid.get_cloned();
        let sel = g.selection();
        let sheet = self.doc.borrow().sheet;
        let sel = officina_grid::Rect { sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
        let (p, f) = crate::validation::open(&self.doc.borrow().wb, sel, g.cursor);
        for (m, t) in self.valid_text.iter().zip(f.iter()) {
            m.set(TextEdit::new(t));
        }
        self.filter.set(FilterPanel::default());
        self.cond.set(crate::condfmt::CondPanel::default());
        self.list.set(ListPanel::default());
        self.valid.set(p);
        self.changed();
    }

    pub fn valid_cmd(&self, c: ValidCmd) {
        use crate::validation as va;
        let mut p = self.valid.get_cloned();
        if !p.open {
            return;
        }
        p.error.clear();
        match c {
            ValidCmd::Allow(i) => p.allow = i.min(va::ALLOWS.len() - 1),
            ValidCmd::Op(i) => p.op = i.min(va::OPS.len() - 1),
            ValidCmd::Style(i) => p.style = i.min(va::STYLES.len() - 1),
            ValidCmd::Blank => p.blank = !p.blank,
            ValidCmd::Dropdown => p.dropdown = !p.dropdown,
            ValidCmd::Prompt => p.show_prompt = !p.show_prompt,
            ValidCmd::Error => p.show_error = !p.show_error,
            ValidCmd::Close => p = va::Panel::default(),
            ValidCmd::Save => {
                let f: [String; va::FIELDS] = std::array::from_fn(|i| self.valid_text[i].get_cloned().text().to_string());
                let saved = va::save(&mut self.doc.borrow_mut(), &p, &f);
                match saved {
                    Ok(()) => {
                        self.doc.borrow_mut().status = "Validation saved.".into();
                        p = va::Panel::default();
                    }
                    Err(e) => p.error = e,
                }
            }
            ValidCmd::Remove => {
                if let Some(i) = p.editing {
                    let mut d = self.doc.borrow_mut();
                    let mut vs = d.wb.validations().to_vec();
                    if i < vs.len() {
                        vs.remove(i);
                        let op = officina_grid::ops::GridOp::validations(&d.wb, &vs);
                        d.run("Delete Validation", vec![op]);
                        d.status = "Validation rule deleted.".into();
                    }
                }
                p = va::Panel::default();
            }
        }
        self.valid.set(p);
        self.changed();
    }

    fn cond_fields(&self, range: &str, a: &str, b: &str) {
        self.cond_range.set(TextEdit::new(range));
        self.cond_a.set(TextEdit::new(a));
        self.cond_b.set(TextEdit::new(b));
    }

    /// Runs a press in the conditional-format panel.
    pub fn cond_cmd(&self, c: CondCmd) {
        use crate::condfmt as cf;
        let mut p = self.cond.get_cloned();
        let Some(sel) = p.sel else { return };
        let rows = cf::entries(&self.doc.borrow().wb, sel);
        p.error.clear();
        match c {
            CondCmd::Pick(i) => p.picked = (i < rows.len()).then_some(i),
            CondCmd::New => {
                let (e, range) = cf::fresh(sel);
                self.cond_fields(&range, "", "");
                p.editor = Some(e);
            }
            CondCmd::Edit => {
                if let Some(&(fi, ri)) = p.picked.and_then(|i| rows.get(i)) {
                    if let Some((e, range, a, b)) = cf::load(&self.doc.borrow().wb, fi, ri) {
                        self.cond_fields(&range, &a, &b);
                        p.editor = Some(e);
                    }
                }
            }
            CondCmd::Delete => {
                if let Some(&(fi, ri)) = p.picked.and_then(|i| rows.get(i)) {
                    cf::delete(&mut self.doc.borrow_mut(), fi, ri);
                    let n = cf::entries(&self.doc.borrow().wb, sel).len();
                    p.picked = p.picked.map(|i| i.min(n.saturating_sub(1))).filter(|_| n > 0);
                }
            }
            CondCmd::Up | CondCmd::Down => {
                if let Some(i) = p.picked {
                    if let Some(j) = cf::shift(&mut self.doc.borrow_mut(), sel, i, c == CondCmd::Up) {
                        p.picked = Some(j);
                    }
                }
            }
            CondCmd::Close => p = cf::CondPanel::default(),
            CondCmd::Back => p.editor = None,
            CondCmd::Kind(k) => {
                if let Some(e) = p.editor.as_mut() {
                    if e.kind != k.min(cf::KINDS.len() - 1) {
                        e.kind = k.min(cf::KINDS.len() - 1);
                        e.choice = 0;
                        e.orig = None;
                        if e.look >= cf::LOOKS.len() {
                            e.look = 0;
                        }
                    }
                }
            }
            CondCmd::Choice(i) => {
                if let Some(e) = p.editor.as_mut() {
                    e.choice = i;
                }
            }
            CondCmd::Look(i) => {
                if let Some(e) = p.editor.as_mut() {
                    e.look = i.min(cf::LOOKS.len());
                }
            }
            CondCmd::Reverse | CondCmd::IconOnly | CondCmd::Stop => {
                if let Some(e) = p.editor.as_mut() {
                    let f = match c {
                        CondCmd::Reverse => &mut e.reverse,
                        CondCmd::IconOnly => &mut e.icon_only,
                        _ => &mut e.stop,
                    };
                    *f = !*f;
                }
            }
            CondCmd::Save => {
                if let Some(e) = p.editor.clone() {
                    let (range, a, b) = (self.cond_range.get_cloned().text().to_string(), self.cond_a.get_cloned().text().to_string(), self.cond_b.get_cloned().text().to_string());
                    let r = cf::save(&mut self.doc.borrow_mut(), &e, &range, &a, &b);
                    match r {
                        Ok(()) => {
                            p.editor = None;
                            let rows = cf::entries(&self.doc.borrow().wb, sel);
                            p.picked = match e.editing {
                                Some(x) => rows.iter().position(|&y| y == x),
                                None => rows.iter().position(|&y| y == (0, 0)),
                            };
                        }
                        Err(m) => p.error = m,
                    }
                }
            }
        }
        self.cond.set(p);
        self.changed();
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
    /// Puts `s` on the status line.
    pub fn set_status(&self, s: impl Into<String>) {
        self.doc.borrow_mut().status = s.into();
        self.changed();
    }

    pub fn changed(&self) {
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// A new document on screen, at its top left.
    pub fn replace(&self, doc: Doc) -> Doc {
        let old = std::mem::replace(&mut *self.doc.borrow_mut(), doc);
        self.close_comment_bar();
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

    /// Follows the cursor's link: a place in the workbook is shown and
    /// selected; an address goes to the link opener (Monstrator) with the
    /// press that asked, from a thread, as it may ask the person first.
    pub fn open_link(&self, press: Option<fresco_protocol::activation::Token>) {
        let (r, c) = self.grid.get_cloned().cursor;
        let (link, place) = {
            let d = self.doc.borrow();
            let l = d.wb.link_at(officina_grid::addr::CellAddr::new(d.sheet, r, c));
            let p = l.as_ref().and_then(|l| l.place(&d.wb, d.sheet));
            (l, p)
        };
        let Some(link) = link else {
            // A Ctrl+click on a plain cell just selects it.
            if press.is_none() {
                self.set_status("There's no link in this cell.");
            }
            return;
        };
        if !link.is_external() {
            match place {
                Some(a) => {
                    self.show_sheet(a.sheet);
                    self.show_cell(a.row, a.col);
                    self.set_status(format!("Went to {}.", link.target.trim_start_matches('#')));
                }
                None => self.set_status(format!("There's no {} in this workbook.", link.target.trim_start_matches('#'))),
            }
            return;
        }
        let Some(token) = press else {
            self.set_status("Ctrl+click the cell, or choose Open Link in the Data menu, to open this link.");
            return;
        };
        let (url, out) = (link.target.to_string(), self.opened.clone());
        *out.lock().unwrap() = None;
        std::thread::spawn(move || {
            let r = match monstrator::open_link(&monstrator::socket(), &url, token, std::time::Duration::from_secs(120)) {
                Ok(r) => r,
                Err(e) => Err(format!("the link opener isn't reachable ({e})")),
            };
            *out.lock().unwrap() = Some(r);
        });
        self.set_status(format!("Opening {}…", short_url(&link.target)));
    }

    /// The link opener answered: the status says so, or why not.
    fn poll_link(&self) {
        let Some(r) = self.opened.lock().unwrap().take() else { return };
        self.set_status(match r {
            Ok(()) => "Link opened.".to_string(),
            Err(why) => format!("The link didn't open: {why}."),
        });
    }

    /// Selects cell (`r`, `c`) of the sheet on screen and scrolls it into view.
    fn show_cell(&self, r: u32, c: u32) {
        use pergola::widgets::GridSource;
        let mut g = self.grid.get_cloned();
        g.select(r, c);
        self.grid.set(g);
        let mut st = self.scroll.get();
        st.physics.stop();
        st.physics_x.stop();
        st.physics.set_position((self.source.row_top(r) - 3.0 * (self.source.row_top(r + 1) - self.source.row_top(r))).max(0.0));
        st.physics_x.set_position((self.source.col_left(c) - 60.0).max(0.0));
        self.scroll.set(st);
    }

    /// Shows sheet `s`: at its top left, the strip moved to show its tab,
    /// panels about the last sheet closed.
    pub fn show_sheet(&self, s: u16) {
        {
            let mut d = self.doc.borrow_mut();
            if s as usize >= d.wb.sheet_count() || d.sheet == s {
                return;
            }
            d.sheet = s;
        }
        self.filter.set(FilterPanel::default());
        self.cond.set(Default::default());
        self.renaming.set(false);
        self.coloring.set(false);
        self.reveal_tab();
        self.home();
    }

    /// Moves the tab strip, if need be, so the sheet on screen has its tab showing.
    fn reveal_tab(&self) {
        let room = (self.size.get().0 - crate::tabs::TABS_X - 8.0).max(0.0);
        let d = self.doc.borrow();
        let first = self.tab_first.get().min(d.wb.sheet_count().saturating_sub(1) as u16);
        self.tab_first.set(crate::tabs::reveal(&d.wb, first, d.sheet, room));
    }

    /// The exact-size bar for the selection's rows (or columns), holding
    /// the cursor's row height (column width).
    pub fn open_size_bar(&self, rows: bool) {
        let g = self.grid.get_cloned();
        let sel = g.selection();
        let (lo, hi) = if rows { (sel.r0, sel.r1) } else { (sel.c0, sel.c1) };
        let mut d = self.doc.borrow_mut();
        if hi - lo >= MAX_SIZED {
            d.status = format!("Size at most {MAX_SIZED} at once.");
            drop(d);
            self.changed();
            return;
        }
        let sh = d.wb.sheet(d.sheet);
        let now = if rows { sh.row_sizes.size(g.cursor.0) } else { sh.col_sizes.size(g.cursor.1) };
        let s = d.sheet;
        d.status = format!("{} px is {} in Excel. Type px, or a size with pt{}.", now.round(), excel_size(now.round(), rows), if rows { "" } else { " or ch (characters)" });
        drop(d);
        self.renaming.set(false);
        self.coloring.set(false);
        self.close_comment_bar();
        self.linking.set(None);
        let mut ed = TextEdit::new(&format!("{}", now.round()));
        ed.select_all();
        self.size_text.set(ed);
        self.size_bar.set(Some((s, rows, lo, hi)));
        self.changed();
    }

    fn close_comment_bar(&self) {
        self.commenting.set(None);
        self.thread_edit.set(None);
        self.comment_text.set(TextEdit::new(""));
    }

    pub fn tab_cmd(&self, c: crate::tabs::TabCmd) {
        use crate::tabs::TabCmd;
        match c {
            TabCmd::Go(s) => self.show_sheet(s),
            TabCmd::Add => {
                let s = {
                    let mut d = self.doc.borrow_mut();
                    let n = d.wb.sheet_count();
                    if n >= officina_grid::book::MAX_SHEETS {
                        d.status = format!("A workbook holds at most {} sheets.", officina_grid::book::MAX_SHEETS);
                        None
                    } else {
                        let name = crate::tabs::new_name(&d.wb);
                        d.run("Add Sheet", vec![officina_grid::ops::GridOp::AddSheet(name)]);
                        (d.wb.sheet_count() > n).then_some(n as u16)
                    }
                };
                match s {
                    Some(s) => self.show_sheet(s),
                    None => self.changed(),
                }
            }
            TabCmd::Delete => {
                let shown = {
                    let mut d = self.doc.borrow_mut();
                    let s = d.sheet;
                    match officina_grid::sheetops::delete_sheet_op(&d.wb, s) {
                        Some(op) => {
                            let name = d.wb.sheet_name(s);
                            let n = d.wb.sheet_count();
                            d.run("Delete Sheet", vec![op]);
                            if d.wb.sheet_count() < n {
                                d.status = format!("Deleted “{name}”; Undo brings it back.");
                            }
                            // The sheet that took its place, or the one before the last.
                            let to = s.min(d.wb.sheet_count().saturating_sub(1) as u16);
                            d.sheet = u16::MAX;
                            Some(to)
                        }
                        None => {
                            d.status = "A workbook keeps at least one sheet.".into();
                            None
                        }
                    }
                };
                match shown {
                    Some(s) => self.show_sheet(s),
                    None => self.changed(),
                }
            }
            TabCmd::MoveLeft | TabCmd::MoveRight => {
                let to = {
                    let mut d = self.doc.borrow_mut();
                    let s = d.sheet;
                    let n = d.wb.sheet_count() as u16;
                    let to = if c == TabCmd::MoveLeft { s.checked_sub(1) } else { (s + 1 < n).then_some(s + 1) };
                    if let Some(to) = to {
                        d.run("Move Sheet", vec![officina_grid::ops::GridOp::MoveSheet { from: s, to }]);
                        d.sheet = to;
                    }
                    to
                };
                if to.is_some() {
                    self.reveal_tab();
                }
                self.changed();
            }
            TabCmd::Earlier => {
                self.tab_first.set(self.tab_first.get().saturating_sub(1));
                self.changed();
            }
            TabCmd::Later => {
                let n = self.doc.borrow().wb.sheet_count() as u16;
                self.tab_first.set((self.tab_first.get() + 1).min(n.saturating_sub(1)));
                self.changed();
            }
            TabCmd::Rename => {
                let name = {
                    let d = self.doc.borrow();
                    d.wb.sheet_name(d.sheet)
                };
                self.rename_text.set(TextEdit::new(&name));
                self.renaming.set(true);
                self.changed();
            }
            TabCmd::RenameDone => {
                let name = self.rename_text.get_cloned().text().trim().to_string();
                let mut d = self.doc.borrow_mut();
                let s = d.sheet;
                if name == d.wb.sheet_name(s) {
                    drop(d);
                } else {
                    match d.wb.sheet_name_ok(&name, Some(s)) {
                        Ok(()) => d.run("Rename Sheet", vec![officina_grid::ops::GridOp::RenameSheet { sheet: s, name }]),
                        Err(why) => {
                            d.status = match why {
                                "bad sheet name" => format!("A sheet name is 1 to {} characters, without [ ] : * ? / or \\.", officina_grid::book::MAX_SHEET_NAME),
                                w => format!("{}{}.", w[..1].to_uppercase(), &w[1..]),
                            };
                            drop(d);
                            self.changed();
                            return;
                        }
                    }
                    drop(d);
                }
                self.renaming.set(false);
                self.reveal_tab();
                self.changed();
            }
            TabCmd::RenameCancel => {
                self.renaming.set(false);
                self.changed();
            }
            TabCmd::Coloring => {
                self.renaming.set(false);
                self.coloring.set(true);
                self.changed();
            }
            TabCmd::Color(color) => {
                let mut d = self.doc.borrow_mut();
                let s = d.sheet;
                if d.wb.sheet(s).tab_color != color {
                    d.run("Tab Colour", vec![officina_grid::ops::GridOp::TabColor { sheet: s, color }]);
                }
                drop(d);
                self.coloring.set(false);
                self.changed();
            }
            TabCmd::ColorCancel => {
                self.coloring.set(false);
                self.changed();
            }
            TabCmd::Commenting | TabCmd::NewThread => {
                let new_thread = matches!(c, TabCmd::NewThread);
                let (r, c) = self.grid.get_cloned().cursor;
                let doc = self.doc.borrow();
                let s = doc.sheet;
                let comment = doc.wb.comment(s, r, c);
                if new_thread && comment.is_some() { return; }
                let thread_edit = (new_thread
                    || comment.is_some_and(|c| !c.thread().is_empty()))
                    .then(|| crate::comment::ThreadEdit { original: comment.cloned(), index: 0 });
                let edit = thread_edit.as_ref().map_or_else(|| crate::comment::edit(comment), |t| t.open());
                drop(doc);
                self.thread_edit.set(thread_edit);
                self.renaming.set(false);
                self.coloring.set(false);
                self.comment_text.set(edit);
                self.commenting.set(Some((s, r, c)));
                self.changed();
            }
            TabCmd::ThreadMessage(index) => {
                let Some(mut thread) = self.thread_edit.get_cloned() else { return };
                if self.commenting.get().is_none() || index > thread.messages().len()
                    || !thread.unchanged(&self.comment_text.get_cloned())
                {
                    return;
                }
                thread.index = index;
                self.comment_text.set(thread.open());
                self.thread_edit.set(Some(thread));
                self.changed();
            }
            TabCmd::CommentDone => {
                let Some((s, r, c)) = self.commenting.get() else { return };
                let edit = self.comment_text.get_cloned();
                let mut d = self.doc.borrow_mut();
                let old = d.wb.comment(s, r, c).cloned();
                let (label, new) = if let Some(thread) = self.thread_edit.get_cloned() {
                    if thread.original != old {
                        d.status = "The comment changed. Reopen the thread to edit it.".into();
                        drop(d);
                        self.close_comment_bar();
                        self.changed();
                        return;
                    }
                    if thread.index < thread.messages().len() && thread.unchanged(&edit) {
                        ("Edit Thread Message", old.clone())
                    } else {
                        let Some(new) = thread.save(&edit) else {
                            d.status = "Enter a message within the comment limits.".into();
                            drop(d);
                            self.changed();
                            return;
                        };
                        let label = if old.is_none() { "New Comment Thread" }
                            else if thread.index == thread.messages().len() { "Reply to Comment" }
                            else { "Edit Thread Message" };
                        (label, Some(new))
                    }
                } else {
                    let author = old.as_ref().map_or_else(comment_author, |k| k.author.to_string());
                    let new = crate::comment::save(&author, &edit);
                    let label = match (&old, &new) {
                        (None, _) => "New Comment",
                        (_, None) => "Delete Comment",
                        _ => "Edit Comment",
                    };
                    (label, new)
                };
                if new != old {
                    d.run(label, vec![officina_grid::ops::GridOp::Comments { sheet: s, set: vec![(r, c, new)] }]);
                }
                drop(d);
                self.close_comment_bar();
                self.changed();
            }
            TabCmd::CommentFormat(mark) => {
                if self.commenting.get().is_some() && self.thread_edit.get_cloned().is_none() {
                    let mut edit = self.comment_text.get_cloned();
                    if edit.toggle_emphasis(mark) {
                        self.comment_text.set(edit);
                        self.changed();
                    }
                }
            }
            TabCmd::CommentCancel => {
                self.close_comment_bar();
                self.changed();
            }
            TabCmd::Linking => {
                let (r, c) = self.grid.get_cloned().cursor;
                let (s, text) = {
                    let d = self.doc.borrow();
                    (d.sheet, d.wb.link(d.sheet, r, c).map(|k| k.target.to_string()).unwrap_or_default())
                };
                self.renaming.set(false);
                self.coloring.set(false);
                self.close_comment_bar();
                let mut ed = TextEdit::new(&text);
                ed.set_caret(ed.text().len(), false);
                self.link_text.set(ed);
                self.linking.set(Some((s, r, c)));
                self.changed();
            }
            TabCmd::LinkDone => {
                let Some((s, r, c)) = self.linking.get() else { return };
                let typed = self.link_text.get_cloned().text().to_string();
                let mut d = self.doc.borrow_mut();
                let new = match link_from_bar(&d.wb, s, &typed) {
                    Ok(l) => l,
                    Err(why) => {
                        d.status = why;
                        drop(d);
                        self.changed();
                        return;
                    }
                };
                let old = d.wb.link(s, r, c).cloned();
                if new != old {
                    let label = match (&old, &new) {
                        (None, _) => "Insert Link",
                        (_, None) => "Remove Link",
                        _ => "Edit Link",
                    };
                    let mut ops = vec![officina_grid::ops::GridOp::Links { sheet: s, set: vec![(r, c, new.clone())] }];
                    // An empty cell shows where it goes.
                    if let Some(l) = new.filter(|_| d.wb.input(officina_grid::addr::CellAddr::new(s, r, c)) == officina_grid::Input::Empty) {
                        let shown = l.target.strip_prefix('#').unwrap_or(&l.target);
                        ops.push(officina_grid::ops::GridOp::set(officina_grid::addr::CellAddr::new(s, r, c), officina_grid::Input::Value(officina_grid::Value::Str(shown.into()))));
                    }
                    d.run(label, ops);
                }
                drop(d);
                self.linking.set(None);
                self.changed();
            }
            TabCmd::LinkCancel => {
                self.linking.set(None);
                self.changed();
            }
            TabCmd::SizeDone => {
                let Some((s, rows, lo, hi)) = self.size_bar.get() else { return };
                let typed = self.size_text.get_cloned().text().trim().to_string();
                let most = if rows { MAX_ROW_PX } else { MAX_COL_PX };
                let mut d = self.doc.borrow_mut();
                match typed_px(&typed, rows).ok_or(()) {
                    // A row 0 high is a hidden row, as Hide Rows makes it.
                    Ok(px) if rows && px == 0.0 && s == d.sheet => {
                        crate::data::run(&mut d, pergola::widgets::CellRange { r0: lo, c0: 0, r1: hi, c1: 0 }, (lo, 0), crate::data::DataCmd::HideRows);
                    }
                    Ok(px) if px.is_finite() && (0.0..=most).contains(&px) && s == d.sheet => {
                        d.size_preview(rows, lo, hi, px.round());
                        d.size_done();
                    }
                    _ => {
                        d.status = format!("Type a size from 0 to {most} px (0 hides), or with pt{}.", if rows { "" } else { " or ch" });
                        drop(d);
                        self.changed();
                        return;
                    }
                }
                drop(d);
                self.size_bar.set(None);
                self.changed();
            }
            TabCmd::PasteWhat(_) | TabCmd::PasteOp(_) | TabCmd::PasteSkip | TabCmd::PasteTurn => {
                use officina_grid::clip::{Arith, What};
                let Some(mut sp) = self.paste_bar.get() else { return };
                match c {
                    TabCmd::PasteWhat(i) => sp.what = [What::All, What::Values, What::Formulas, What::Formats, What::Comments][(i as usize).min(4)],
                    TabCmd::PasteOp(i) => sp.arith = [None, Some(Arith::Add), Some(Arith::Sub), Some(Arith::Mul), Some(Arith::Div)][(i as usize).min(4)],
                    TabCmd::PasteSkip => sp.skip_blanks = !sp.skip_blanks,
                    _ => sp.transpose = !sp.transpose,
                }
                self.paste_bar.set(Some(sp));
                self.changed();
            }
            TabCmd::PasteDone => {
                let Some(sp) = self.paste_bar.get() else { return };
                let sel = self.grid.get_cloned().selection();
                let mut d = self.doc.borrow_mut();
                match crate::data::paste_with(&mut d, sel, sp, "Paste Special") {
                    Ok(s) => {
                        d.status = s;
                        drop(d);
                        self.paste_bar.set(None);
                    }
                    // The bar stays open to change a choice.
                    Err(e) => d.status = e,
                }
                self.rev.set(self.rev.get().wrapping_add(1));
                self.changed();
            }
            TabCmd::PasteCancel => {
                self.paste_bar.set(None);
                self.changed();
            }
            TabCmd::SizeCancel => {
                self.size_bar.set(None);
                self.changed();
            }
            TabCmd::WarnKeep => {
                self.doc.borrow_mut().keep_warned();
                self.changed();
            }
            TabCmd::WarnUndo => {
                self.doc.borrow_mut().undo_warned();
                self.changed();
            }
        }
    }

    /// Opens the cursor cell's validation list, or says why not.
    pub fn choose_from_list(&self) {
        let (r, c) = self.grid.get_cloned().cursor;
        let mut d = self.doc.borrow_mut();
        let a = officina_grid::addr::CellAddr::new(d.sheet, r, c);
        let items: Vec<String> = d.wb.list_items(a).iter().map(|v| v.display()).filter(|t| !t.is_empty()).collect();
        if items.is_empty() {
            d.status = "This cell has no list to choose from.".into();
            return;
        }
        let sheet = d.sheet;
        drop(d);
        self.list.set(ListPanel { open: true, sheet, row: r, col: c, items });
        self.list_search.set(TextEdit::new(""));
        self.list_scroll.set(ScrollState::default());
    }

    /// Types item `k` of those the list's search shows into its cell.
    pub fn list_pick(&self, k: usize) {
        let p = self.list.get_cloned();
        let shown = p.shown(self.list_search.get_cloned().text());
        self.list.set(ListPanel::default());
        let Some(text) = shown.get(k).and_then(|&i| p.items.get(i)) else { return };
        let mut d = self.doc.borrow_mut();
        if d.sheet != p.sheet {
            return;
        }
        // A text that would read as a formula or a number stays the item's text.
        d.type_into(p.row, p.col, text);
        drop(d);
        self.rev.set(self.rev.get().wrapping_add(1));
    }

    /// The tab strip at `y`: add, earlier, later, then the tabs that fit; or
    /// the rename bar in its place.
    fn render_tabs(&self, ctx: &mut Ctx, w: f32, y: f32) {
        use crate::tabs::{TabCmd, TABS_H, TABS_X};
        let t = ctx.theme;
        let push = |c: TabCmd| {
            let cmds = self.cmds.clone();
            move || cmds.lock().unwrap().push(Cmd::Tab(c))
        };
        ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, TABS_H), fill: t.bg_surface(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, 1.0), fill: t.border_default(), radius: 0.0 });
        if self.renaming.get() {
            let st = crate::tabs::style(t.text_primary(), true);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, 100.0, 18.0), content: "Rename sheet".into(), style: st });
            let fw = (w - 120.0 - 180.0).clamp(80.0, 320.0);
            let cmds = self.cmds.clone();
            TextField::new(self.rename_text.clone()).autofocus(true).at(116.0, y + 2.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Tab(TabCmd::RenameDone))).render(ctx);
            Button::primary("Rename").at(124.0 + fw, y + 2.0).width(80.0).on_click(push(TabCmd::RenameDone)).render(ctx);
            Button::secondary("Cancel").at(212.0 + fw, y + 2.0).width(72.0).on_click(push(TabCmd::RenameCancel)).render(ctx);
            return;
        }
        let warned = self.doc.borrow().warned.clone();
        if let Some((a, msg)) = warned {
            let st = crate::tabs::style(t.text_primary(), true);
            let label = format!("{}: {msg} Keep it?", officina_grid::addr::a1(a.row, a.col));
            let lw = (w - 180.0).max(80.0);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, lw, 18.0), content: label, style: st });
            Button::primary("Keep").at(w - 160.0, y + 2.0).width(72.0).on_click(push(TabCmd::WarnKeep)).render(ctx);
            Button::secondary("Undo").at(w - 80.0, y + 2.0).width(72.0).on_click(push(TabCmd::WarnUndo)).render(ctx);
            return;
        }
        if let (Some((_, r, c)), Some(thread)) = (self.commenting.get(), self.thread_edit.get_cloned()) {
            let here = officina_grid::addr::a1(r, c);
            let messages = thread.messages();
            let current = messages.get(thread.index);
            let label = match current {
                Some(m) => format!("{} · {}/{} · {}", here, thread.index + 1, messages.len(), m.author.name),
                None if messages.is_empty() => format!("New thread on {here}"),
                None => format!("Reply on {here}"),
            };
            let st = crate::tabs::style(t.text_primary(), true);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, 186.0, 18.0), content: label, style: st });
            let fw = (w - 452.0).max(88.0);
            let cmds = self.cmds.clone();
            TextField::new(self.comment_text.clone()).autofocus(true).at(200.0, y + 2.0).width(fw)
                .on_submit(move |_| cmds.lock().unwrap().push(Cmd::Tab(TabCmd::CommentDone))).render(ctx);
            let x = 208.0 + fw;
            if thread.unchanged(&self.comment_text.get_cloned()) {
                if thread.index > 0 {
                    Button::secondary("‹").at(x, y + 2.0).width(28.0).tooltip("Previous message")
                        .on_click(push(TabCmd::ThreadMessage(thread.index - 1))).render(ctx);
                }
                if thread.index + 1 < messages.len() {
                    Button::secondary("›").at(x + 32.0, y + 2.0).width(28.0).tooltip("Next message")
                        .on_click(push(TabCmd::ThreadMessage(thread.index + 1))).render(ctx);
                }
                if current.is_some() && messages.len() < officina_grid::comments::MAX_THREAD_MESSAGES {
                    Button::secondary("Reply").at(x + 64.0, y + 2.0).width(56.0)
                        .on_click(push(TabCmd::ThreadMessage(messages.len()))).render(ctx);
                }
            }
            Button::primary(if current.is_some() { "Save" } else { "Post" })
                .at(x + 124.0, y + 2.0).width(56.0).on_click(push(TabCmd::CommentDone)).render(ctx);
            Button::secondary("Cancel").at(x + 184.0, y + 2.0).width(64.0).on_click(push(TabCmd::CommentCancel)).render(ctx);
            return;
        }
        if let Some((_, r, c)) = self.commenting.get() {
            let st = crate::tabs::style(t.text_primary(), true);
            let label = format!("Comment on {}", officina_grid::addr::a1(r, c));
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, 130.0, 18.0), content: label, style: st });
            use pergola::inline::Emphasis;
            let fw = (w - 444.0).clamp(88.0, 640.0);
            let cmds = self.cmds.clone();
            TextField::new(self.comment_text.clone()).autofocus(true).at(146.0, y + 2.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Tab(TabCmd::CommentDone))).render(ctx);
            let edit = self.comment_text.get_cloned();
            let mut x = 154.0 + fw;
            for (label, tip, mark) in [
                ("B", "Bold (Ctrl+B)", Emphasis::Bold),
                ("I", "Italic (Ctrl+I)", Emphasis::Italic),
                ("U", "Underline (Ctrl+U)", Emphasis::Underline),
                ("S̶", "Strikethrough (Ctrl+Shift+U)", Emphasis::Strike),
            ] {
                let button = if edit.emphasis_active(mark) { Button::primary(label) } else { Button::secondary(label) };
                button.at(x, y + 2.0).width(32.0).tooltip(tip).retain_focus_on_press().on_click(push(TabCmd::CommentFormat(mark))).render(ctx);
                x += 36.0;
            }
            Button::primary("Save").at(x, y + 2.0).width(64.0).on_click(push(TabCmd::CommentDone)).render(ctx);
            Button::secondary("Cancel").at(x + 68.0, y + 2.0).width(64.0).on_click(push(TabCmd::CommentCancel)).render(ctx);
            return;
        }
        if let Some((_, r, c)) = self.linking.get() {
            let st = crate::tabs::style(t.text_primary(), true);
            let label = format!("Link from {}", officina_grid::addr::a1(r, c));
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, 130.0, 18.0), content: label, style: st });
            let fw = (w - 150.0 - 180.0).clamp(120.0, 640.0);
            let cmds = self.cmds.clone();
            TextField::new(self.link_text.clone()).autofocus(true).placeholder("https://… or Sheet2!B4").at(146.0, y + 2.0).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Tab(TabCmd::LinkDone))).render(ctx);
            Button::primary("Save").at(154.0 + fw, y + 2.0).width(72.0).on_click(push(TabCmd::LinkDone)).render(ctx);
            Button::secondary("Cancel").at(234.0 + fw, y + 2.0).width(72.0).on_click(push(TabCmd::LinkCancel)).render(ctx);
            return;
        }
        if let Some(sp) = self.paste_bar.get() {
            use officina_grid::clip::{Arith, What};
            let st = crate::tabs::style(t.text_primary(), true);
            let mut x = 12.0;
            ctx.add(Node::Text { rect: Rect::new(x, y + 6.0, 44.0, 18.0), content: "Paste".into(), style: st.clone() });
            x += 46.0;
            let chip = |ctx: &mut Ctx, label: &str, on: bool, x: f32, w: f32, c: TabCmd| {
                let b = if on { Button::primary(label) } else { Button::secondary(label) };
                b.at(x, y + 2.0).width(w).on_click(push(c)).render(ctx);
            };
            for (i, (label, what)) in [("All", What::All), ("Values", What::Values), ("Formulas", What::Formulas), ("Formats", What::Formats), ("Comments", What::Comments)].into_iter().enumerate() {
                let w = 12.0 + 7.0 * label.len() as f32;
                chip(ctx, label, sp.what == what, x, w, TabCmd::PasteWhat(i as u8));
                x += w + 4.0;
            }
            x += 10.0;
            for (i, (label, op)) in [("None", None), ("+", Some(Arith::Add)), ("−", Some(Arith::Sub)), ("×", Some(Arith::Mul)), ("÷", Some(Arith::Div))].into_iter().enumerate() {
                let w = if i == 0 { 48.0 } else { 28.0 };
                chip(ctx, label, sp.arith == op, x, w, TabCmd::PasteOp(i as u8));
                x += w + 4.0;
            }
            x += 10.0;
            chip(ctx, "Skip blanks", sp.skip_blanks, x, 92.0, TabCmd::PasteSkip);
            x += 96.0;
            chip(ctx, "Transpose", sp.transpose, x, 84.0, TabCmd::PasteTurn);
            x += 98.0;
            Button::primary("Paste").at(x, y + 2.0).width(64.0).on_click(push(TabCmd::PasteDone)).render(ctx);
            Button::secondary("Cancel").at(x + 70.0, y + 2.0).width(64.0).on_click(push(TabCmd::PasteCancel)).render(ctx);
            return;
        }
        if let Some((_, rows, lo, hi)) = self.size_bar.get() {
            let st = crate::tabs::style(t.text_primary(), true);
            let name = |i: u32| if rows { (i + 1).to_string() } else { officina_grid::addr::col_name(i) };
            let span = if lo == hi { name(lo) } else { format!("{}–{}", name(lo), name(hi)) };
            let label = if rows { format!("Height of row {span}") } else { format!("Width of column {span}") };
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, 200.0, 18.0), content: label, style: st });
            let cmds = self.cmds.clone();
            TextField::new(self.size_text.clone()).autofocus(true).at(216.0, y + 2.0).width(90.0).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Tab(TabCmd::SizeDone))).render(ctx);
            Button::primary("Set").at(314.0, y + 2.0).width(72.0).on_click(push(TabCmd::SizeDone)).render(ctx);
            Button::secondary("Cancel").at(394.0, y + 2.0).width(72.0).on_click(push(TabCmd::SizeCancel)).render(ctx);
            return;
        }
        if self.coloring.get() {
            let st = crate::tabs::style(t.text_primary(), true);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 6.0, 100.0, 18.0), content: "Tab colour".into(), style: st });
            let mut x = 100.0;
            for (rgb, _) in crate::tabs::COLORS {
                let sw = ctx.add(Node::Rect { rect: Rect::new(x, y + 5.0, 18.0, 18.0), fill: crate::tabs::color(rgb), radius: 4.0 });
                let cmds = self.cmds.clone();
                ctx.on_click(sw, move || cmds.lock().unwrap().push(Cmd::Tab(TabCmd::Color(Some(rgb)))));
                x += 24.0;
            }
            Button::secondary("None").at(x + 8.0, y + 2.0).width(64.0).on_click(push(TabCmd::Color(None))).render(ctx);
            Button::secondary("Cancel").at(x + 80.0, y + 2.0).width(72.0).on_click(push(TabCmd::ColorCancel)).render(ctx);
            return;
        }
        Button::secondary("+").at(4.0, y + 2.0).width(28.0).on_click(push(TabCmd::Add)).render(ctx);
        Button::secondary("‹").at(34.0, y + 2.0).width(26.0).on_click(push(TabCmd::Earlier)).render(ctx);
        Button::secondary("›").at(62.0, y + 2.0).width(26.0).on_click(push(TabCmd::Later)).render(ctx);
        let d = self.doc.borrow();
        let first = self.tab_first.get().min(d.wb.sheet_count().saturating_sub(1) as u16);
        for (s, x, tw) in crate::tabs::visible(&d.wb, first, (w - TABS_X - 8.0).max(0.0)) {
            let on = s == d.sheet;
            let r = Rect::new(TABS_X + x, y + 1.0, tw - 2.0, TABS_H - 2.0);
            let hit = ctx.add(Node::Rect { rect: r, fill: if on { t.bg_canvas() } else { Color::TRANSPARENT }, radius: 0.0 });
            // Its colour as a bar along the bottom (thicker on screen), or
            // the accent under the tab on screen.
            match d.wb.sheet(s).tab_color {
                Some(rgb) => {
                    let h = if on { 4.0 } else { 3.0 };
                    ctx.add(Node::Rect { rect: Rect::new(TABS_X + x, y + TABS_H - 1.0 - h, tw - 2.0, h), fill: crate::tabs::color(rgb), radius: 0.0 });
                }
                None if on => {
                    ctx.add(Node::Rect { rect: Rect::new(TABS_X + x, y + TABS_H - 3.0, tw - 2.0, 2.0), fill: t.accent_bg(), radius: 0.0 });
                }
                None => {}
            }
            let st = crate::tabs::style(if on { t.text_primary() } else { t.text_secondary() }, on);
            let name = d.wb.sheet_name(s);
            ctx.add(Node::Text { rect: Rect::new(TABS_X + x + 12.0, y + 6.0, tw - 24.0, 18.0), content: name, style: st });
            let cmds = self.cmds.clone();
            ctx.on_click(hit, move || cmds.lock().unwrap().push(Cmd::Tab(TabCmd::Go(s))));
        }
    }

    /// The fill options menu, hanging under its button: fill again by
    /// copying, as a series, only the formats, or without them.
    fn render_fill_menu(&self, ctx: &mut Ctx, w: f32, h: f32) {
        use crate::data::DataCmd;
        let at = {
            let d = self.doc.borrow();
            d.last_fill.filter(|_| d.can_refill()).and_then(|f| self.source.fill_button_at(f.1))
        };
        let Some(at) = at else {
            self.fill_menu.set(MenuState::default());
            return;
        };
        // Content px to window px, as the grid places its body.
        use pergola::widgets::grid_view::{HEADER_H, TILE_ROWS};
        let r = Self::grid_rect(w, h);
        let st = self.scroll.get();
        let (tiles_r, _) = GridView::tiles(&*self.source, st.physics_x.y, st.physics.y, r.w() as f64, r.h() as f64);
        let hw = GridView::header_width((tiles_r.1 + 1) * TILE_ROWS);
        let x = r.x() + hw + (at[0] - st.physics_x.y) as f32;
        let y = r.y() + HEADER_H + (at[1] + at[3] - st.physics.y) as f32;
        let cmds = self.cmds.clone();
        let picks = [DataCmd::FillCopy, DataCmd::FillSeries, DataCmd::FillFormats, DataCmd::FillNoFormats];
        use pergola::widgets::{Menu, MenuItem};
        let items = vec![MenuItem::new("Copy Cells"), MenuItem::new("Fill Series"), MenuItem::new("Fill Formats Only"), MenuItem::new("Fill Without Formatting")];
        let menu = Menu::new(self.fill_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Data(picks[i.min(3)]))).width(200.0);
        let (mw, mh) = (200.0, menu.height());
        let (x, y) = (x.clamp(0.0, (w - mw).max(0.0)), y.min((h - mh).max(0.0)));
        menu.at(x, y).anchored(Rect::new(x, y - at[3] as f32, at[2] as f32, at[3] as f32), pergola::popup::Side::Below, pergola::popup::Align::Start).render(ctx);
    }

    /// Where the grid sits in a window of `w` × `h`.
    pub fn grid_rect(w: f32, h: f32) -> Rect {
        let top = TOOLBAR_H + FORMULA_H;
        Rect::new(0.0, top, w, (h - top - STATUS_H - crate::tabs::TABS_H).max(40.0))
    }

    fn act(doc: &Rc<RefCell<Doc>>, src: &SheetSource, rev: &Mutable<u64>, a: GridAction) {
        // A chart dragged: its anchor from where it was let go.
        let anchor = match &a {
            GridAction::PlaceFloat { at, .. } => Some(src.anchor_of(*at)),
            _ => None,
        };
        let mut d = doc.borrow_mut();
        // A row typed into that may need to grow (several lines).
        let mut grow = None;
        match a {
            GridAction::PlaceFloat { key, .. } => {
                let mut all = d.wb.charts().to_vec();
                if let (Some(c), Some(an)) = (all.get_mut(key as usize), anchor) {
                    let moved = c.anchor.from.0 != an.from.0 || c.anchor.from.1 != an.from.1;
                    c.anchor = an;
                    d.run(if moved { "Move Chart" } else { "Size Chart" }, vec![officina_grid::ops::GridOp::Charts(all)]);
                }
            }
            GridAction::Follow { .. } | GridAction::PressFloat(_) | GridAction::Choose { .. } | GridAction::FillToData(_) | GridAction::FlashFill { .. } | GridAction::FitCol(_) | GridAction::FitRow(_) | GridAction::ResizeCol { .. } | GridAction::ResizeRow { .. } => {}
            GridAction::DeleteFloat(key) => {
                let mut all = d.wb.charts().to_vec();
                if (key as usize) < all.len() {
                    all.remove(key as usize);
                    d.run("Delete Chart", vec![officina_grid::ops::GridOp::Charts(all)]);
                    d.status = "Chart deleted.".into();
                }
            }
            GridAction::Commit { row, col, text } => {
                if d.type_into(row, col, &text) {
                    grow = Some(row);
                }
            }
            GridAction::CommitAll { sel, text } => {
                let area = officina_grid::addr::Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
                if d.type_all(area, &text) && sel.r0 == sel.r1 {
                    grow = Some(sel.r0);
                }
            }
            GridAction::Clear(r) => {
                d.clear(r.r0, r.c0, r.r1, r.c1);
                // A row grown for lines now gone shrinks back.
                let s = d.sheet;
                if let Some(&(_, row)) = d.grown_rows.iter().find(|&&(t, row)| t == s && (r.r0..=r.r1).contains(&row)) {
                    grow = Some(row);
                }
            }
            GridAction::Fill { from, to, copy, flip } => {
                use officina_grid::autofill::Mode;
                let rect = |r: CellRange| officina_grid::addr::Rect { sheet: d.sheet, r0: r.r0, c0: r.c0, r1: r.r1, c1: r.c1 };
                let (from, to) = (rect(from), rect(to));
                d.fill_as(from, to, if copy { Mode::Copy } else if flip { Mode::Flip } else { Mode::Series });
            }
            GridAction::ResizeEnd => d.size_done(),
            GridAction::Undo => {
                d.undo();
            }
            GridAction::Redo => {
                d.redo();
            }
            GridAction::Copy(r) | GridAction::Cut(r) if cells(r) > MAX_COPY_CELLS => {
                d.status = format!("That is {} cells; copying takes at most {MAX_COPY_CELLS}.", cells(r));
            }
            GridAction::Copy(r) | GridAction::Cut(r) => {
                let cut = matches!(a, GridAction::Cut(_));
                let rect = officina_grid::addr::Rect { sheet: d.sheet, r0: r.r0, c0: r.c0, r1: r.r1, c1: r.c1 };
                crate::clipboard::copy(&mut d, rect, cut);
            }
            GridAction::Paste { row, col, sel, plain } => {
                let area = officina_grid::addr::Rect { sheet: d.sheet, r0: row, c0: col, r1: sel.r1.max(row), c1: sel.c1.max(col) };
                crate::clipboard::paste(&mut d, area, plain);
            }
        }
        drop(d);
        // Rows nobody sized grow to fit wrapped or broken lines typed into
        // them, as part of the typing (Excel fits such rows).
        if let Some(row) = grow {
            let h = src.fit_height(row);
            doc.borrow_mut().grow_row(row, h);
        }
        rev.set(rev.get().wrapping_add(1));
    }
}

/// The most rows or columns one drag sizes together.
pub const MAX_SIZED: u32 = 16_384;
/// The tallest row and widest column the size bar sets, in px (Excel's
/// 409 pt and 255 characters, roughly).
pub const MAX_ROW_PX: f64 = 545.0;
pub const MAX_COL_PX: f64 = 1790.0;

/// A size typed in the size bar, in px: plain or `px`, `pt` (points, as
/// Excel gives row heights: 0.75 pt a px), or for columns `ch` (Excel's
/// characters of the default font: 7 px each and 5 px of padding).
pub fn typed_px(text: &str, rows: bool) -> Option<f64> {
    let t = text.trim().to_ascii_lowercase();
    let (num, unit) = match t.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (t[..i].trim(), t[i..].trim()),
        None => (t.as_str(), ""),
    };
    let n: f64 = num.parse().ok().filter(|n: &f64| n.is_finite() && *n >= 0.0)?;
    match unit {
        "" | "px" => Some(n),
        "pt" => Some(n / 0.75),
        "ch" if !rows => Some(if n == 0.0 { 0.0 } else { n * 7.0 + 5.0 }),
        _ => None,
    }
}

/// `px` as Excel would give it: a row's height in points, a column's
/// width in characters.
pub fn excel_size(px: f64, rows: bool) -> String {
    let v = if rows { px * 0.75 } else { ((px - 5.0) / 7.0).max(0.0) };
    let s = format!("{:.2}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if rows { format!("{s} pt") } else { format!("{s} ch") }
}

/// The columns (`rows` false) or rows an edge of `i` sizes: the selected
/// whole columns (rows) when `i` is one of them, else `i` alone.
pub fn sized_span(sel: &CellRange, rows: bool, i: u32, src: &dyn pergola::widgets::GridSource) -> (u32, u32) {
    let (whole, lo, hi) = if rows { (sel.c0 == 0 && sel.c1 + 1 >= src.cols(), sel.r0, sel.r1) } else { (sel.r0 == 0 && sel.r1 + 1 >= src.rows(), sel.c0, sel.c1) };
    if whole && (lo..=hi).contains(&i) && hi - lo < MAX_SIZED {
        (lo, hi)
    } else {
        (i, i)
    }
}

pub use officina_grid::clip::MAX_SYSTEM_COPY_CELLS as MAX_COPY_CELLS;

fn cells(r: CellRange) -> u64 {
    (r.r1 - r.r0) as u64 * (r.c1 - r.c0 + 1) as u64 + (r.c1 - r.c0 + 1) as u64
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
        for (label, cmd, wd) in [("Open…", Cmd::Open, 76.0), ("Save", Cmd::Save, 64.0), ("Save As…", Cmd::SaveAs, 92.0), ("Print…", Cmd::Print, 64.0), ("Undo", Cmd::Undo, 64.0), ("Redo", Cmd::Redo, 64.0)] {
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
            let press = self.link_press.clone();
            MenuButton::new("Data", self.data_menu.clone(), items, move |i| {
                if let Some(Some(c)) = cmds_of.get(i) {
                    if *c == crate::data::DataCmd::OpenLink {
                        press.set(pergola::clipboard::current_activation());
                    }
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

        // The grid, with the cursor cell's comment beside it while not editing.
        self.source.note_at.set((g.edit.is_none() && self.commenting.get().is_none() && self.linking.get().is_none()).then_some(g.cursor));
        let doc = self.doc.clone();
        let rev = self.rev.clone();
        let src = self.source.clone();
        let (cmds, press) = (self.cmds.clone(), self.link_press.clone());
        let grid = self.grid.clone();
        GridView::new(self.source.clone(), self.scroll.clone(), self.grid.clone(), Self::grid_rect(w, h))
            .on_action(move |a| match a {
                // Ctrl+click: the cell is selected; follow its link with this click.
                GridAction::Follow { .. } => {
                    press.set(pergola::clipboard::current_activation());
                    cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::OpenLink));
                }
                // The cursor cell's list button, or Alt+Down: its list.
                GridAction::PressFloat(crate::source::KEY_FILL) => cmds.lock().unwrap().push(Cmd::FillOptions),
                GridAction::PressFloat(crate::source::KEY_LIST) | GridAction::Choose { .. } => cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::ChooseFromList)),
                // A filter column's button: its value list.
                GridAction::PressFloat(k) if k & crate::source::KEY_FILTER != 0 => cmds.lock().unwrap().push(Cmd::FilterOpen((k & 0xFFFF_FFFF) as u32)),
                // A double-click on a column's edge: as wide as its widest entry.
                // With whole columns selected, every one of them.
                // On the double line of hidden columns: show them.
                // A row's edge: show hidden rows there, or fit the row (all
                // the selected whole rows) to its tallest cell.
                GridAction::FitRow(row) if crate::data::unhide_rows_at(&mut doc.borrow_mut(), row) => {
                    rev.set(rev.get().wrapping_add(1));
                }
                GridAction::FitRow(row) => {
                    let (lo, hi) = sized_span(&grid.get_cloned().selection(), true, row, &*src);
                    let heights: Vec<(u32, f64)> = (lo..=hi).filter(|&r| !doc.borrow().wb.sheet(doc.borrow().sheet).row_hidden(r)).map(|r| (r, src.fit_height(r))).collect();
                    let mut d = doc.borrow_mut();
                    for (r, h) in heights {
                        d.size_preview(true, r, r, h);
                    }
                    d.size_done();
                    drop(d);
                    rev.set(rev.get().wrapping_add(1));
                }
                GridAction::FitCol(col) if crate::data::unhide_cols_at(&mut doc.borrow_mut(), col) => {
                    rev.set(rev.get().wrapping_add(1));
                }
                GridAction::FitCol(col) => {
                    let (lo, hi) = sized_span(&grid.get_cloned().selection(), false, col, &*src);
                    let widths: Vec<(u32, f64)> = (lo..=hi).filter_map(|c| src.fit_width(c).map(|w| (c, w))).collect();
                    let mut d = doc.borrow_mut();
                    if widths.is_empty() {
                        d.status = if lo == hi { "That column is empty.".into() } else { "Those columns are empty.".into() };
                    }
                    for (c, w) in widths {
                        d.size_preview(false, c, c, w);
                    }
                    d.size_done();
                    drop(d);
                    rev.set(rev.get().wrapping_add(1));
                }
                // An edge dragged: with whole columns (rows) selected and the
                // edge one of theirs, all of them take the size.
                GridAction::ResizeCol { col, width } => {
                    let (lo, hi) = sized_span(&grid.get_cloned().selection(), false, col, &*src);
                    doc.borrow_mut().size_preview(false, lo, hi, width);
                    rev.set(rev.get().wrapping_add(1));
                }
                GridAction::ResizeRow { row, height } => {
                    let (lo, hi) = sized_span(&grid.get_cloned().selection(), true, row, &*src);
                    doc.borrow_mut().size_preview(true, lo, hi, height);
                    rev.set(rev.get().wrapping_add(1));
                }
                // A double-click on the fill handle: down along the data beside it.
                // Ctrl+E: the cursor's column by its examples.
                GridAction::FlashFill { .. } => cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::FlashFill)),
                GridAction::FillToData(sel) => {
                    let mut d = doc.borrow_mut();
                    let from = officina_grid::addr::Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
                    match officina_grid::autofill::to_data_end(&d.wb, from) {
                        Some(to) => {
                            if d.fill(from, to, false) {
                                let mut g = grid.get_cloned();
                                g.anchor = (to.r0, to.c0);
                                g.cursor = (to.r1, to.c1);
                                grid.set(g);
                            }
                        }
                        None => d.status = "There's no data beside the selection to fill down along.".into(),
                    }
                    drop(d);
                    rev.set(rev.get().wrapping_add(1));
                }
                a => Self::act(&doc, &src, &rev, a),
            })
            .emit(ctx);

        self.render_tabs(ctx, w, h - STATUS_H - crate::tabs::TABS_H);

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
        // Whether the editor is entering or editing, at the right, as Excel's
        // status bar shows it (F2 switches).
        let mode = self.grid.get_cloned().edit_mode().map(|m| match m {
            pergola::widgets::EditMode::Enter => "Enter",
            pergola::widgets::EditMode::Edit => "Edit",
        });
        let room = if mode.is_some() { 72.0 } else { 0.0 };
        ctx.add(Node::Text { rect: Rect::new(12.0, sy + 4.0, (w - 24.0 - room).max(0.0), 18.0), content: line, style: st(type_size::XS, Weight::Regular, t.text_secondary()) });
        if let Some(m) = mode {
            ctx.add(Node::Text { rect: Rect::new((w - 12.0 - 60.0).max(0.0), sy + 4.0, 60.0, 18.0), content: m.into(), style: st(type_size::XS, Weight::Semibold, t.text_primary()) });
        }

        // The fill options button's menu, under the button.
        if self.fill_menu.get().open {
            self.render_fill_menu(ctx, w, h);
        }

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

        // The value list of a filter column, at the right over the grid.
        let fp = self.filter.get_cloned();
        if fp.open && !pb.open {
            let (x0, y0) = ((w - FILTER_W).max(0.0), TOOLBAR_H + FORMULA_H);
            let ph = (h - y0 - STATUS_H).max(160.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, FILTER_W, ph), fill: t.bg_surface(), radius: 0.0 });
            // Presses on the panel stay off the grid under it.
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, FILTER_W - 24.0, 20.0), content: fp.title.clone(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            TextField::new(self.filter_search.clone()).placeholder("Search values").autofocus(true).at(x0 + 12.0, y0 + 38.0).width(FILTER_W - 24.0).render(ctx);
            let shown = fp.shown(self.filter_search.get_cloned().text());
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            Button::secondary("Select All").at(x0 + 12.0, y0 + 78.0).width(100.0).on_click(push(Cmd::FilterAll(true))).render(ctx);
            Button::secondary("Clear").at(x0 + 120.0, y0 + 78.0).width(72.0).on_click(push(Cmd::FilterAll(false))).render(ctx);
            let chosen = fp.on.iter().filter(|&&o| o).count();
            ctx.add(Node::Text { rect: Rect::new(x0 + 200.0, y0 + 86.0, FILTER_W - 212.0, 18.0), content: format!("{chosen} of {}", fp.values.len()), style: st(type_size::XS, Weight::Regular, t.text_secondary()) });
            let ly = y0 + 120.0;
            let lh = (ph - 120.0 - 56.0).max(FILTER_ROW_H);
            let (vals, on, cmds) = (Rc::new(fp.values.clone()), Rc::new(fp.on.clone()), self.cmds.clone());
            let rows = Rc::new(shown);
            let n = rows.len();
            pergola::widgets::VirtualList::new(self.filter_scroll.clone(), Rect::new(x0 + 1.0, ly, FILTER_W - 1.0, lh), n, FILTER_ROW_H, move |k, ctx| {
                let t = ctx.theme;
                // One node a row; what is in it placed as given.
                let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, FILTER_W - 1.0, FILTER_ROW_H), 0.0));
                ctx.set_flex(row, pergola::layout::FlexStyle { free: true, ..pergola::layout::FlexStyle::default() });
                let i = rows[k];
                let (text, count) = &vals[i];
                let label = if text.is_empty() { "(Empty)".to_string() } else { text.chars().take(60).collect() };
                let cmds = cmds.clone();
                let v = Mutable::new(if on[i] { pergola::widgets::Check::On } else { pergola::widgets::Check::Off });
                pergola::widgets::Checkbox::new(v, label).at(12.0, 4.0).on_change(move |_| cmds.lock().unwrap().push(Cmd::FilterToggle(i))).render(ctx);
                let c = count.to_string();
                ctx.add(Node::Text { rect: Rect::new(FILTER_W - 72.0, 6.0, 56.0, 16.0), content: c, style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_tertiary() } });
                ctx.pop();
            })
            .backdrop(t.bg_surface())
            .key(0xF117)
            .emit(ctx);
            if n == 0 {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, ly + 8.0, FILTER_W - 24.0, 18.0), content: "No value matches.".into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            }
            let by = y0 + ph - 48.0;
            ctx.add(Node::Rect { rect: Rect::new(x0, by - 8.0, FILTER_W, 1.0), fill: t.border_default(), radius: 0.0 });
            Button::secondary("Cancel").at(x0 + FILTER_W - 164.0, by).width(72.0).on_click(push(Cmd::FilterCancel)).render(ctx);
            Button::primary("Apply").at(x0 + FILTER_W - 84.0, by).width(72.0).on_click(push(Cmd::FilterApply)).render(ctx);
        }

        // The custom lists, at the right over the grid: each with Forget.
        if self.lists_open.get() && !pb.open {
            let (x0, y0) = ((w - FILTER_W).max(0.0), TOOLBAR_H + FORMULA_H);
            let ph = (h - y0 - STATUS_H).max(160.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, FILTER_W, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, FILTER_W - 24.0, 20.0), content: "Custom lists".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let lists = officina_grid::autofill::custom_lists();
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            if lists.is_empty() {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 40.0, FILTER_W - 24.0, 40.0), content: "None yet. Select a row or column of entries, then Data ▸ Fill: Keep Selection as a List.".into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            }
            let room = ((ph - 40.0 - 56.0) / FILTER_ROW_H).max(0.0) as usize;
            for (i, l) in lists.iter().enumerate().take(room) {
                let y = y0 + 40.0 + i as f32 * FILTER_ROW_H;
                let mut text: String = l.join(", ").chars().take(80).collect();
                if text.chars().count() >= 80 {
                    text.push('…');
                }
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y + 5.0, FILTER_W - 104.0, 18.0), content: text, style: st(type_size::SM, Weight::Regular, t.text_primary()) });
                Button::secondary("Forget").at(x0 + FILTER_W - 84.0, y + 2.0).width(72.0).on_click(push(Cmd::ListsForget(i))).render(ctx);
            }
            let by = y0 + ph - 44.0;
            if !lists.is_empty() {
                Button::secondary("Forget all").at(x0 + 12.0, by).width(96.0).on_click(push(Cmd::Data(crate::data::DataCmd::ClearCustomLists))).render(ctx);
            }
            Button::primary("Close").at(x0 + FILTER_W - 84.0, by).width(72.0).on_click(push(Cmd::ListsClose)).render(ctx);
        }

        // A validation list, at the right over the grid: a press types an item.
        let lp = self.list.get_cloned();
        if lp.open && !pb.open {
            let (x0, y0) = ((w - FILTER_W).max(0.0), TOOLBAR_H + FORMULA_H);
            let ph = (h - y0 - STATUS_H).max(160.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, FILTER_W, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            let title = format!("Choose for {}", officina_grid::addr::a1(lp.row, lp.col));
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, FILTER_W - 24.0, 20.0), content: title, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let cmds = self.cmds.clone();
            TextField::new(self.list_search.clone()).placeholder("Search the list").autofocus(true).at(x0 + 12.0, y0 + 38.0).width(FILTER_W - 24.0).on_submit(move |_| cmds.lock().unwrap().push(Cmd::ListPick(0))).render(ctx);
            let shown = lp.shown(self.list_search.get_cloned().text());
            let ly = y0 + 80.0;
            let lh = (ph - 80.0 - 56.0).max(FILTER_ROW_H);
            let (items, cmds) = (Rc::new(lp.items.clone()), self.cmds.clone());
            let rows = Rc::new(shown);
            let n = rows.len();
            pergola::widgets::VirtualList::new(self.list_scroll.clone(), Rect::new(x0 + 1.0, ly, FILTER_W - 1.0, lh), n, FILTER_ROW_H, move |k, ctx| {
                let t = ctx.theme;
                let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, FILTER_W - 1.0, FILTER_ROW_H), 0.0));
                ctx.set_flex(row, pergola::layout::FlexStyle { free: true, ..pergola::layout::FlexStyle::default() });
                let label: String = items[rows[k]].chars().take(60).collect();
                let hit = ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, FILTER_W - 1.0, FILTER_ROW_H), fill: Color::TRANSPARENT, radius: 0.0 });
                let cmds = cmds.clone();
                ctx.on_click(hit, move || cmds.lock().unwrap().push(Cmd::ListPick(k)));
                ctx.add(Node::Text { rect: Rect::new(12.0, 6.0, FILTER_W - 24.0, 16.0), content: label, style: TextStyle { family: font::SANS.into(), size: type_size::SM, weight: Weight::Regular, color: t.text_primary() } });
                ctx.pop();
            })
            .backdrop(t.bg_surface())
            .key(0xF118)
            .emit(ctx);
            if n == 0 {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, ly + 8.0, FILTER_W - 24.0, 18.0), content: "No item matches.".into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
            }
            let by = y0 + ph - 48.0;
            ctx.add(Node::Rect { rect: Rect::new(x0, by - 8.0, FILTER_W, 1.0), fill: t.border_default(), radius: 0.0 });
            let cmds = self.cmds.clone();
            Button::secondary("Cancel").at(x0 + FILTER_W - 84.0, by).width(72.0).on_click(move || cmds.lock().unwrap().push(Cmd::ListCancel)).render(ctx);
        }

        // The conditional-format panel, at the right over the grid.
        let cp = self.cond.get_cloned();
        if let (true, false, Some(sel)) = (cp.open, pb.open, cp.sel) {
            use crate::condfmt as cf;
            let (x0, y0) = ((w - COND_W).max(0.0), TOOLBAR_H + FORMULA_H);
            let ph = (h - y0 - STATUS_H).max(200.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, COND_W, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            let push = |c: CondCmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(Cmd::Cond(c))
            };
            let label = |ctx: &mut Ctx, y: f32, s: &str| {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y, COND_W - 24.0, 16.0), content: s.into(), style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Semibold, color: t.text_secondary() } });
            };
            let fw = COND_W - 24.0;
            let by = y0 + ph - 48.0;
            match &cp.editor {
                None => {
                    ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, fw, 20.0), content: "Conditional Formatting".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
                    ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 32.0, fw, 16.0), content: format!("Rules over {}, first applied first", cf::rect_text(&sel)), style: st(type_size::XS, Weight::Regular, t.text_secondary()) });
                    let d = self.doc.borrow();
                    let rows = cf::entries(&d.wb, sel);
                    let ly = y0 + 58.0;
                    let room = ((by - 96.0 - ly) / COND_ROW_H).floor().max(1.0) as usize;
                    // Keep the picked row in view.
                    let first = cp.picked.map_or(0, |p| (p + 1).saturating_sub(room));
                    for (k, &(fi, ri)) in rows.iter().enumerate().skip(first).take(room) {
                        let f = &d.wb.cond_formats()[fi];
                        let y = ly + (k - first) as f32 * COND_ROW_H;
                        let on = cp.picked == Some(k);
                        let hit = ctx.add(Node::Rect { rect: Rect::new(x0 + 6.0, y + 1.0, COND_W - 12.0, COND_ROW_H - 2.0), fill: if on { t.accent_bg() } else { Color::TRANSPARENT }, radius: 6.0 });
                        ctx.on_click(hit, push(CondCmd::Pick(k)));
                        ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(8.0, 4.0, COND_W - 28.0, 18.0), content: cf::describe(&d.wb, f, &f.rules[ri]), style: st(type_size::SM, Weight::Regular, t.text_primary()) });
                        ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(8.0, 23.0, COND_W - 28.0, 16.0), content: cf::ranges_text(&f.ranges).chars().take(60).collect(), style: st(type_size::XS, Weight::Regular, t.text_tertiary()) });
                    }
                    if rows.is_empty() {
                        ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, ly + 8.0, fw, 18.0), content: "No rules here yet.".into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                    } else if rows.len() > room {
                        ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, by - 100.0, fw, 16.0), content: format!("{} rules; move through them with Up and Down.", rows.len()), style: st(type_size::XS, Weight::Regular, t.text_tertiary()) });
                    }
                    drop(d);
                    let picked = cp.picked.is_some();
                    let ry = by - 80.0;
                    ctx.add(Node::Rect { rect: Rect::new(x0, ry - 8.0, COND_W, 1.0), fill: t.border_default(), radius: 0.0 });
                    Button::secondary("New Rule…").at(x0 + 12.0, ry).width(104.0).on_click(push(CondCmd::New)).render(ctx);
                    if picked {
                        Button::secondary("Edit…").at(x0 + 124.0, ry).width(72.0).on_click(push(CondCmd::Edit)).render(ctx);
                        Button::secondary("Delete").at(x0 + 204.0, ry).width(72.0).on_click(push(CondCmd::Delete)).render(ctx);
                        Button::secondary("Up").at(x0 + 12.0, ry + 40.0).width(56.0).on_click(push(CondCmd::Up)).render(ctx);
                        Button::secondary("Down").at(x0 + 76.0, ry + 40.0).width(64.0).on_click(push(CondCmd::Down)).render(ctx);
                    }
                    Button::primary("Done").at(x0 + COND_W - 84.0, by).width(72.0).on_click(push(CondCmd::Close)).render(ctx);
                }
                Some(e) => {
                    let title = if e.editing.is_some() { "Change Rule" } else { "New Rule" };
                    ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, fw, 20.0), content: title.into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
                    let mut y = y0 + 40.0;
                    label(ctx, y, "Apply to");
                    TextField::new(self.cond_range.clone()).placeholder("e.g. B2:D20").at(x0 + 12.0, y + 18.0).width(fw - 104.0).render(ctx);
                    let cmds = self.cmds.clone();
                    Button::secondary("Selection").at(x0 + 12.0 + fw - 96.0, y + 18.0).width(96.0).on_click(move || cmds.lock().unwrap().push(Cmd::UseSelection)).render(ctx);
                    y += 60.0;
                    label(ctx, y, "Format cells by");
                    let kinds: Vec<pergola::widgets::MenuItem> = cf::KINDS.iter().enumerate().map(|(i, k)| pergola::widgets::MenuItem::new(*k).radio(i == e.kind)).collect();
                    let cmds = self.cmds.clone();
                    MenuButton::new(cf::KINDS[e.kind].to_string(), self.cond_kind_menu.clone(), kinds, move |i| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::Kind(i)))).at(x0 + 12.0, y + 18.0).width(fw).render(ctx);
                    y += 60.0;
                    let ch = cf::choices(e.kind);
                    if !ch.is_empty() {
                        let mut items: Vec<pergola::widgets::MenuItem> = ch.iter().enumerate().map(|(i, c)| pergola::widgets::MenuItem::new(c.clone()).radio(i == e.choice)).collect();
                        if e.choice >= ch.len() {
                            items.push(pergola::widgets::MenuItem::new("As it is").radio(true));
                        }
                        let now = ch.get(e.choice).cloned().unwrap_or_else(|| "As it is".into());
                        let cmds = self.cmds.clone();
                        MenuButton::new(now, self.cond_choice_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::Choice(i)))).at(x0 + 12.0, y).width(fw).render(ctx);
                        y += 42.0;
                    }
                    if let Some(ph_a) = cf::field_a(e.kind) {
                        let between = e.kind == 0 && e.choice < 2;
                        let aw = if between { (fw - 40.0) / 2.0 } else { fw };
                        let cmds = self.cmds.clone();
                        TextField::new(self.cond_a.clone()).placeholder(ph_a).at(x0 + 12.0, y).width(aw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::Save))).render(ctx);
                        if between {
                            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0 + aw + 8.0, y + 8.0, 24.0, 18.0), content: "and".into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                            let cmds = self.cmds.clone();
                            TextField::new(self.cond_b.clone()).placeholder("Value").at(x0 + 12.0 + aw + 40.0, y).width(aw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::Save))).render(ctx);
                        }
                        y += 42.0;
                    }
                    if e.kind == 10 {
                        let cmds = self.cmds.clone();
                        pergola::widgets::Checkbox::new(Mutable::new(if e.reverse { pergola::widgets::Check::On } else { pergola::widgets::Check::Off }), "Reverse icon order").at(x0 + 12.0, y).on_change(move |_| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::Reverse))).render(ctx);
                        let cmds = self.cmds.clone();
                        pergola::widgets::Checkbox::new(Mutable::new(if e.icon_only { pergola::widgets::Check::On } else { pergola::widgets::Check::Off }), "Show icon only").at(x0 + 12.0, y + 28.0).on_change(move |_| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::IconOnly))).render(ctx);
                        y += 60.0;
                    }
                    if cf::has_look(e.kind) {
                        label(ctx, y, "Look");
                        let mut items: Vec<pergola::widgets::MenuItem> = cf::LOOKS.iter().enumerate().map(|(i, l)| pergola::widgets::MenuItem::new(l.0).radio(i == e.look)).collect();
                        if e.look >= cf::LOOKS.len() {
                            items.push(pergola::widgets::MenuItem::new("As it is").radio(true));
                        }
                        let now = cf::LOOKS.get(e.look).map_or("As it is", |l| l.0).to_string();
                        let cmds = self.cmds.clone();
                        MenuButton::new(now, self.cond_look_menu.clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::Look(i)))).at(x0 + 12.0, y + 18.0).width(fw).render(ctx);
                        if let Some(&(_, fill, color, bold)) = cf::LOOKS.get(e.look) {
                            // A sample of the look.
                            let sy = y + 62.0;
                            let bg = fill.map_or(t.bg_canvas(), crate::source::rgb);
                            ctx.add(Node::Rect { rect: Rect::new(x0 + 12.0, sy, 120.0, 24.0), fill: bg, radius: 2.0 });
                            let fg = color.map_or(t.text_primary(), crate::source::rgb);
                            ctx.add(Node::Text { rect: Rect::new(x0 + 20.0, sy + 4.0, 104.0, 16.0), content: "AaBbCc 123".into(), style: st(type_size::SM, if bold { Weight::Bold } else { Weight::Regular }, fg) });
                            y += 34.0;
                        }
                        y += 60.0;
                    }
                    let cmds = self.cmds.clone();
                    pergola::widgets::Checkbox::new(Mutable::new(if e.stop { pergola::widgets::Check::On } else { pergola::widgets::Check::Off }), "Stop if true (later rules skip these cells)").at(x0 + 12.0, y).on_change(move |_| cmds.lock().unwrap().push(Cmd::Cond(CondCmd::Stop))).render(ctx);
                    y += 34.0;
                    if !cp.error.is_empty() {
                        ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y, fw, 36.0), content: cp.error.clone(), style: st(type_size::SM, Weight::Regular, t.danger_fg()) });
                    }
                    ctx.add(Node::Rect { rect: Rect::new(x0, by - 8.0, COND_W, 1.0), fill: t.border_default(), radius: 0.0 });
                    Button::secondary("Cancel").at(x0 + COND_W - 164.0, by).width(72.0).on_click(push(CondCmd::Back)).render(ctx);
                    Button::primary("Save").at(x0 + COND_W - 84.0, by).width(72.0).on_click(push(CondCmd::Save)).render(ctx);
                }
            }
        }

        // The data validation panel, at the right over the grid.
        let vp = self.valid.get_cloned();
        if vp.open && !pb.open {
            use crate::validation as va;
            let (x0, y0) = ((w - COND_W).max(0.0), TOOLBAR_H + FORMULA_H);
            let ph = (h - y0 - STATUS_H).max(200.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, COND_W, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            let push = |c: ValidCmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(Cmd::Valid(c))
            };
            let label = |ctx: &mut Ctx, y: f32, s: &str| {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y, COND_W - 24.0, 16.0), content: s.into(), style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Semibold, color: t.text_secondary() } });
            };
            let check = |ctx: &mut Ctx, x: f32, y: f32, on: bool, text: &str, c: ValidCmd| {
                let cmds = self.cmds.clone();
                pergola::widgets::Checkbox::new(Mutable::new(if on { pergola::widgets::Check::On } else { pergola::widgets::Check::Off }), text).at(x, y).on_change(move |_| cmds.lock().unwrap().push(Cmd::Valid(c))).render(ctx);
            };
            let field = |ctx: &mut Ctx, i: usize, x: f32, y: f32, fw: f32, ph: &str| {
                let cmds = self.cmds.clone();
                TextField::new(self.valid_text[i].clone()).placeholder(ph).at(x, y).width(fw).on_submit(move |_| cmds.lock().unwrap().push(Cmd::Valid(ValidCmd::Save))).render(ctx);
            };
            let menu = |ctx: &mut Ctx, k: usize, names: &[&str], now: usize, y: f32, mk: fn(usize) -> ValidCmd| {
                let items: Vec<pergola::widgets::MenuItem> = names.iter().enumerate().map(|(i, n)| pergola::widgets::MenuItem::new(*n).radio(i == now)).collect();
                let cmds = self.cmds.clone();
                MenuButton::new(names[now.min(names.len() - 1)].to_string(), self.valid_menus[k].clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Valid(mk(i)))).at(x0 + 12.0, y).width(COND_W - 24.0).render(ctx);
            };
            let fw = COND_W - 24.0;
            let title = if vp.editing.is_some() { "Change Validation" } else { "New Validation" };
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, fw, 20.0), content: title.into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            let mut y = y0 + 40.0;
            label(ctx, y, "Apply to");
            field(ctx, va::RANGE, x0 + 12.0, y + 18.0, fw - 104.0, "e.g. B2:B100");
            {
                let cmds = self.cmds.clone();
                Button::secondary("Selection").at(x0 + 12.0 + fw - 96.0, y + 18.0).width(96.0).on_click(move || cmds.lock().unwrap().push(Cmd::UseSelection)).render(ctx);
            }
            y += 60.0;
            label(ctx, y, "Allow");
            menu(ctx, 0, &va::ALLOWS, vp.allow, y + 18.0, ValidCmd::Allow);
            y += 60.0;
            if va::compares(vp.allow) {
                menu(ctx, 1, &va::OPS, vp.op, y, ValidCmd::Op);
                y += 42.0;
            }
            if let Some(pa) = va::field_a(vp.allow) {
                let both = va::compares(vp.allow) && va::two(vp.op);
                let aw = if both { (fw - 40.0) / 2.0 } else { fw };
                field(ctx, va::A, x0 + 12.0, y, aw, pa);
                if both {
                    ctx.add(Node::Text { rect: Rect::new(x0 + 12.0 + aw + 8.0, y + 8.0, 24.0, 18.0), content: "and".into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
                    field(ctx, va::B, x0 + 12.0 + aw + 40.0, y, aw, "Highest");
                }
                y += 42.0;
            }
            check(ctx, x0 + 12.0, y, vp.blank, "Allow empty cells", ValidCmd::Blank);
            if vp.allow == officina_grid::validation::Allow::List as usize {
                check(ctx, x0 + 180.0, y, vp.dropdown, "Offer the list", ValidCmd::Dropdown);
            }
            y += 34.0;
            check(ctx, x0 + 12.0, y, vp.show_prompt, "Show a prompt when the cell is chosen", ValidCmd::Prompt);
            y += 28.0;
            field(ctx, va::PROMPT_TITLE, x0 + 12.0, y, fw, "Prompt title");
            field(ctx, va::PROMPT_TEXT, x0 + 12.0, y + 38.0, fw, "Prompt");
            y += 84.0;
            check(ctx, x0 + 12.0, y, vp.show_error, "Check values typed in", ValidCmd::Error);
            y += 28.0;
            menu(ctx, 2, &va::STYLES, vp.style, y, ValidCmd::Style);
            field(ctx, va::ERROR_TITLE, x0 + 12.0, y + 42.0, fw, "Error title");
            field(ctx, va::ERROR_TEXT, x0 + 12.0, y + 80.0, fw, "Error message");
            y += 126.0;
            if !vp.error.is_empty() {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y, fw, 36.0), content: vp.error.clone(), style: st(type_size::SM, Weight::Regular, t.danger_fg()) });
            }
            let by = y0 + ph - 48.0;
            ctx.add(Node::Rect { rect: Rect::new(x0, by - 8.0, COND_W, 1.0), fill: t.border_default(), radius: 0.0 });
            if vp.editing.is_some() {
                Button::secondary("Delete Rule").at(x0 + 12.0, by).width(104.0).on_click(push(ValidCmd::Remove)).render(ctx);
            }
            Button::secondary("Cancel").at(x0 + COND_W - 164.0, by).width(72.0).on_click(push(ValidCmd::Close)).render(ctx);
            Button::primary("Save").at(x0 + COND_W - 84.0, by).width(72.0).on_click(push(ValidCmd::Save)).render(ctx);
        }

        // Compare with…: the other copy's differences, at the right over the grid.
        let rv = self.review.get_cloned();
        if rv.open {
            use crate::review::{ReviewCmd, REVIEW_ROW_H, REVIEW_W};
            let (x0, y0) = ((w - REVIEW_W).max(0.0), TOOLBAR_H + FORMULA_H);
            let ph = (h - y0 - STATUS_H).max(200.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, REVIEW_W, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            let push = |c: ReviewCmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(Cmd::Review(c))
            };
            let fw = REVIEW_W - 24.0;
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, fw - 70.0, 20.0), content: "Compare".into(), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            Button::secondary("Close").at(x0 + REVIEW_W - 76.0, y0 + 6.0).width(64.0).on_click(push(ReviewCmd::Close)).render(ctx);
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 36.0, fw, 16.0), content: rv.summary().chars().take(60).collect(), style: st(type_size::XS, Weight::Regular, t.text_secondary()) });
            let (lines, sheets): (Vec<String>, Vec<String>) = {
                let d = self.doc.borrow();
                let theirs = self.theirs.borrow();
                match theirs.as_ref() {
                    Some(o) => rv.changes.iter().map(|c| (c.describe(&d.wb, o), c.sheet().filter(|_| d.wb.sheet_count() > 1).map(|s| d.wb.sheet_name(s)).unwrap_or_default())).unzip(),
                    None => (Vec::new(), Vec::new()),
                }
            };
            let ly = y0 + 60.0;
            let lh = (ph - 60.0 - 56.0).max(REVIEW_ROW_H);
            let n = lines.len();
            let (lines, sheets, cmds, picked) = (Rc::new(lines), Rc::new(sheets), self.cmds.clone(), rv.picked);
            pergola::widgets::VirtualList::new(self.review_scroll.clone(), Rect::new(x0 + 1.0, ly, REVIEW_W - 1.0, lh), n, REVIEW_ROW_H, move |k, ctx| {
                let t = ctx.theme;
                let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, REVIEW_W - 1.0, REVIEW_ROW_H), 0.0));
                ctx.set_flex(row, pergola::layout::FlexStyle { free: true, ..pergola::layout::FlexStyle::default() });
                let hit = ctx.add(Node::Rect { rect: Rect::new(6.0, 1.0, REVIEW_W - 13.0, REVIEW_ROW_H - 2.0), fill: if picked == Some(k) { t.accent_bg() } else { Color::TRANSPARENT }, radius: 6.0 });
                let cmds = cmds.clone();
                ctx.on_click(hit, move || cmds.lock().unwrap().push(Cmd::Review(ReviewCmd::Pick(k))));
                let sheet = &sheets[k];
                let tw = if sheet.is_empty() { REVIEW_W - 28.0 } else { REVIEW_W - 110.0 };
                ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(8.0, 6.0, tw, 18.0), content: lines[k].chars().take(56).collect(), style: TextStyle { family: font::SANS.into(), size: type_size::SM, weight: Weight::Regular, color: t.text_primary() } });
                if !sheet.is_empty() {
                    ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(REVIEW_W - 96.0, 7.0, 80.0, 16.0), content: sheet.chars().take(14).collect(), style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_tertiary() } });
                }
                ctx.pop();
            })
            .backdrop(t.bg_surface())
            .key(0xF119)
            .emit(ctx);
            let by = ly + lh + 12.0;
            ctx.add(Node::Rect { rect: Rect::new(x0, by - 6.0, REVIEW_W, 1.0), fill: t.border_default(), radius: 0.0 });
            Button::primary("Take").at(x0 + 12.0, by).width(100.0).on_click(push(ReviewCmd::Take)).render(ctx);
            Button::secondary("Keep Mine").at(x0 + 120.0, by).width(96.0).on_click(push(ReviewCmd::Keep)).render(ctx);
            Button::secondary("Take All").at(x0 + REVIEW_W - 100.0, by).width(88.0).on_click(push(ReviewCmd::TakeAll)).render(ctx);
        }

        // The pivot field list, at the right over the grid.
        let pv = self.pivot_pane.get_cloned();
        let pivot = pv.open.then(|| self.doc.borrow().wb.pivots().get(pv.pivot).cloned()).flatten();
        if let (Some(piv), false) = (pivot, pb.open) {
            use crate::pivotpane as pp;
            let (x0, y0) = ((w - PIVOT_W).max(0.0), TOOLBAR_H + FORMULA_H);
            let ph = (h - y0 - STATUS_H).max(200.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(x0, y0, PIVOT_W, ph), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            ctx.add(Node::Rect { rect: Rect::new(x0, y0, 1.0, ph), fill: t.border_default(), radius: 0.0 });
            let push = |c: PivotCmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(Cmd::Pivot(c))
            };
            let fw = PIVOT_W - 24.0;
            let (fields, lines, captions) = {
                let d = self.doc.borrow();
                let lines: Vec<String> = pp::Area::ALL.iter().map(|a| pp::area_line(&d.wb, &piv, *a)).collect();
                let caps: Vec<String> = piv.data.iter().map(|x| d.wb.data_caption(&piv, x)).collect();
                (pp::fields(&d.wb, &piv), lines, caps)
            };
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 10.0, fw, 20.0), content: format!("“{}” Fields", piv.name), style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y0 + 32.0, fw, 16.0), content: "Pick a field, then where it goes.".into(), style: st(type_size::XS, Weight::Regular, t.text_secondary()) });
            let ly = y0 + 56.0;
            let n = fields.len();
            let lh = (n as f32 * PIVOT_ROW_H).min((ph * 0.35).max(PIVOT_ROW_H * 3.0)).max(PIVOT_ROW_H);
            let (rows, cmds, picked) = (Rc::new(fields), self.cmds.clone(), pv.picked);
            pergola::widgets::VirtualList::new(self.pivot_scroll.clone(), Rect::new(x0 + 1.0, ly, PIVOT_W - 1.0, lh), n, PIVOT_ROW_H, move |k, ctx| {
                let t = ctx.theme;
                let row = ctx.push(Node::stack(pergola::geom::Axis::Vertical, Rect::new(0.0, 0.0, PIVOT_W - 1.0, PIVOT_ROW_H), 0.0));
                ctx.set_flex(row, pergola::layout::FlexStyle { free: true, ..pergola::layout::FlexStyle::default() });
                let (name, area) = &rows[k];
                let on = picked == Some(k as u32);
                let hit = ctx.add(Node::Rect { rect: Rect::new(6.0, 1.0, PIVOT_W - 13.0, PIVOT_ROW_H - 2.0), fill: if on { t.accent_bg() } else { Color::TRANSPARENT }, radius: 6.0 });
                let cmds = cmds.clone();
                let f = k as u32;
                ctx.on_click(hit, move || cmds.lock().unwrap().push(Cmd::Pivot(PivotCmd::Pick(f))));
                let tx = TextStyle { family: font::SANS.into(), size: type_size::SM, weight: if area.is_some() { Weight::Semibold } else { Weight::Regular }, color: t.text_primary() };
                ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(8.0, 5.0, PIVOT_W - 110.0, 18.0), content: name.chars().take(40).collect(), style: tx });
                if let Some(a) = area {
                    ctx.tree.insert(Some(hit), Node::Text { rect: Rect::new(PIVOT_W - 96.0, 6.0, 80.0, 16.0), content: a.name().into(), style: TextStyle { family: font::SANS.into(), size: type_size::XS, weight: Weight::Regular, color: t.text_tertiary() } });
                }
                ctx.pop();
            })
            .backdrop(t.bg_surface())
            .key(0xF118)
            .emit(ctx);
            let mut y = ly + lh + 10.0;
            ctx.add(Node::Rect { rect: Rect::new(x0, y - 5.0, PIVOT_W, 1.0), fill: t.border_default(), radius: 0.0 });
            if pv.picked.is_some() {
                let mut x = x0 + 12.0;
                for a in pp::Area::ALL {
                    Button::secondary(a.name()).at(x, y).width(74.0).on_click(push(PivotCmd::To(Some(a)))).render(ctx);
                    x += 80.0;
                }
                Button::secondary("Remove").at(x0 + 12.0, y + 38.0).width(84.0).on_click(push(PivotCmd::To(None))).render(ctx);
                Button::secondary("Earlier").at(x0 + 104.0, y + 38.0).width(76.0).on_click(push(PivotCmd::Earlier)).render(ctx);
                Button::secondary("Later").at(x0 + 188.0, y + 38.0).width(68.0).on_click(push(PivotCmd::Later)).render(ctx);
            } else {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y + 8.0, fw, 18.0), content: "No field picked.".into(), style: st(type_size::SM, Weight::Regular, t.text_tertiary()) });
            }
            y += 84.0;
            for l in &lines {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y, fw, 16.0), content: l.chars().take(70).collect(), style: st(type_size::XS, Weight::Regular, t.text_secondary()) });
                y += 18.0;
            }
            y += 6.0;
            for (k, cap) in captions.iter().enumerate().take(PIVOT_AGG_ROWS) {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y + 8.0, fw - 132.0, 18.0), content: cap.chars().take(30).collect(), style: st(type_size::SM, Weight::Regular, t.text_primary()) });
                let agg = piv.data[k].agg;
                let items: Vec<pergola::widgets::MenuItem> = officina_grid::pivot::Agg::ALL.iter().zip(AGG_NAMES).map(|(a, n)| pergola::widgets::MenuItem::new(n).radio(*a == agg)).collect();
                let now = officina_grid::pivot::Agg::ALL.iter().position(|a| *a == agg).map_or("Sum", |i| AGG_NAMES[i]).to_string();
                let cmds = self.cmds.clone();
                MenuButton::new(now, self.pivot_agg_menus[k].clone(), items, move |i| cmds.lock().unwrap().push(Cmd::Pivot(PivotCmd::Agg(k, i)))).at(x0 + PIVOT_W - 132.0, y).width(120.0).render(ctx);
                y += 38.0;
            }
            for (label, on, which) in [("Grand total row", piv.grand_row, pp::Totals::GrandRow), ("Grand total column", piv.grand_col, pp::Totals::GrandCol), ("Subtotals", piv.subtotals, pp::Totals::Subtotals), ("Compact form", piv.compact, pp::Totals::Compact)] {
                let cmds = self.cmds.clone();
                pergola::widgets::Checkbox::new(Mutable::new(if on { pergola::widgets::Check::On } else { pergola::widgets::Check::Off }), label).at(x0 + 12.0, y).on_change(move |_| cmds.lock().unwrap().push(Cmd::Pivot(PivotCmd::Totals(which)))).render(ctx);
                y += 28.0;
            }
            if !pv.error.is_empty() {
                ctx.add(Node::Text { rect: Rect::new(x0 + 12.0, y + 4.0, fw, 36.0), content: pv.error.clone(), style: st(type_size::SM, Weight::Regular, t.danger_fg()) });
            }
            let by = y0 + ph - 48.0;
            ctx.add(Node::Rect { rect: Rect::new(x0, by - 8.0, PIVOT_W, 1.0), fill: t.border_default(), radius: 0.0 });
            Button::primary("Done").at(x0 + PIVOT_W - 84.0, by).width(72.0).on_click(push(PivotCmd::Close)).render(ctx);
        }

        // Closing with unsaved changes: save them, drop them, or stay.
        if self.closing.get() {
            let (y, bh) = (TOOLBAR_H, 48.0);
            let back = ctx.add(Node::Rect { rect: Rect::new(0.0, y, w, bh), fill: t.bg_surface(), radius: 0.0 });
            ctx.on_click(back, || {});
            let push = |c: Cmd| {
                let cmds = self.cmds.clone();
                move || cmds.lock().unwrap().push(c)
            };
            let ask = format!("Save the changes to “{}” before closing?", self.doc.borrow().name);
            ctx.add(Node::Text { rect: Rect::new(12.0, y + 14.0, (w - 360.0).max(120.0), 20.0), content: ask, style: st(type_size::MD, Weight::Semibold, t.text_primary()) });
            Button::secondary("Don't Save").at(w - 324.0, y + 8.0).width(110.0).on_click(push(Cmd::CloseDiscard)).render(ctx);
            Button::secondary("Cancel").at(w - 206.0, y + 8.0).width(90.0).on_click(push(Cmd::CloseCancel)).render(ctx);
            Button::primary("Save").at(w - 108.0, y + 8.0).width(96.0).on_click(push(Cmd::CloseSave)).render(ctx);
            ctx.add(Node::Rect { rect: Rect::new(0.0, y + bh - 1.0, w, 1.0), fill: t.border_default(), radius: 0.0 });
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
        self.poll_link();
        if crate::clipboard::poll_html(&mut self.doc.borrow_mut()) {
            self.changed();
        }
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
                Cmd::Data(crate::data::DataCmd::CompareWith) => true,
                Cmd::Tab(t) => {
                    self.tab_cmd(t);
                    false
                }
                Cmd::Data(crate::data::DataCmd::AddSheet) => {
                    self.tab_cmd(crate::tabs::TabCmd::Add);
                    false
                }
                Cmd::Data(crate::data::DataCmd::RenameSheet) => {
                    self.tab_cmd(crate::tabs::TabCmd::Rename);
                    false
                }
                Cmd::Data(crate::data::DataCmd::TabColor) => {
                    self.tab_cmd(crate::tabs::TabCmd::Coloring);
                    false
                }
                Cmd::Data(crate::data::DataCmd::Comment) => {
                    self.tab_cmd(crate::tabs::TabCmd::Commenting);
                    false
                }
                Cmd::Data(crate::data::DataCmd::NewThread) => {
                    self.tab_cmd(crate::tabs::TabCmd::NewThread);
                    false
                }
                Cmd::Data(crate::data::DataCmd::Link) => {
                    self.tab_cmd(crate::tabs::TabCmd::Linking);
                    false
                }
                Cmd::Data(crate::data::DataCmd::PasteSpecial) => {
                    self.size_bar.set(None);
                    self.renaming.set(false);
                    self.coloring.set(false);
                    self.close_comment_bar();
                    self.linking.set(None);
                    self.paste_bar.set(Some(officina_grid::clip::Special::of(officina_grid::clip::What::All)));
                    self.changed();
                    false
                }
                Cmd::Data(c @ (crate::data::DataCmd::RowHeight | crate::data::DataCmd::ColWidth)) => {
                    self.open_size_bar(c == crate::data::DataCmd::RowHeight);
                    false
                }
                Cmd::Data(crate::data::DataCmd::OpenLink) => {
                    let press = self.link_press.take();
                    self.open_link(press);
                    false
                }
                Cmd::Data(crate::data::DataCmd::DeleteSheet) => {
                    self.tab_cmd(crate::tabs::TabCmd::Delete);
                    false
                }
                Cmd::Data(crate::data::DataCmd::MoveSheetLeft) => {
                    self.tab_cmd(crate::tabs::TabCmd::MoveLeft);
                    false
                }
                Cmd::Data(crate::data::DataCmd::MoveSheetRight) => {
                    self.tab_cmd(crate::tabs::TabCmd::MoveRight);
                    false
                }
                Cmd::Review(c) => {
                    self.review_cmd(c);
                    false
                }
                Cmd::Data(crate::data::DataCmd::CondFormats) => {
                    self.open_cond();
                    false
                }
                Cmd::Cond(c) => {
                    self.cond_cmd(c);
                    false
                }
                Cmd::Data(crate::data::DataCmd::CircleInvalid) => {
                    let mut d = self.doc.borrow_mut();
                    d.circling = !d.circling;
                    d.status = if !d.circling {
                        String::new()
                    } else {
                        let s = d.sheet;
                        let (r, c) = d.wb.extent(s);
                        let n = d.wb.invalid_cells(&officina_grid::Rect { sheet: s, r0: 0, c0: 0, r1: r.saturating_sub(1), c1: c.saturating_sub(1) }, 100_000, 1 << 22).len();
                        match n {
                            0 => "No cell on this sheet breaks its rule.".into(),
                            1 => "1 cell breaks its rule; it is circled.".into(),
                            n => format!("{n} cells break their rules; they are circled."),
                        }
                    };
                    drop(d);
                    self.changed();
                    false
                }
                Cmd::Data(crate::data::DataCmd::Validation) => {
                    self.open_valid();
                    false
                }
                Cmd::Valid(c) => {
                    self.valid_cmd(c);
                    false
                }
                Cmd::UseSelection => {
                    let text = crate::condfmt::rect_text(&{
                        let g = self.grid.get_cloned().selection();
                        officina_grid::Rect { sheet: self.doc.borrow().sheet, r0: g.r0, c0: g.c0, r1: g.r1, c1: g.c1 }
                    });
                    if self.valid.get_cloned().open {
                        self.valid_text[crate::validation::RANGE].set(TextEdit::new(&text));
                    } else if self.cond.get_cloned().editor.is_some() {
                        self.cond_range.set(TextEdit::new(&text));
                    }
                    self.changed();
                    false
                }
                Cmd::Data(crate::data::DataCmd::PivotFields) => {
                    self.open_pivot_pane();
                    false
                }
                Cmd::Pivot(c) => {
                    self.pivot_cmd(c);
                    false
                }
                Cmd::Data(crate::data::DataCmd::ChooseFromList) => {
                    self.choose_from_list();
                    self.changed();
                    false
                }
                Cmd::ListPick(k) => {
                    self.list_pick(k);
                    self.changed();
                    false
                }
                Cmd::Data(crate::data::DataCmd::CustomLists) => {
                    self.lists_open.set(true);
                    self.changed();
                    false
                }
                Cmd::ListsForget(i) => {
                    if let Err(e) = crate::lists::forget(i) {
                        self.doc.borrow_mut().status = e;
                    }
                    self.changed();
                    false
                }
                Cmd::ListsClose => {
                    self.lists_open.set(false);
                    self.changed();
                    false
                }
                Cmd::ListCancel => {
                    self.list.set(ListPanel::default());
                    self.changed();
                    false
                }
                Cmd::Data(crate::data::DataCmd::ChooseValues) => {
                    self.choose_values(self.grid.get_cloned().cursor.1);
                    false
                }
                Cmd::FilterToggle(i) => {
                    let mut p = self.filter.get_cloned();
                    if let Some(o) = p.on.get_mut(i) {
                        *o = !*o;
                        self.filter.set(p);
                    }
                    self.changed();
                    false
                }
                Cmd::FilterAll(on) => {
                    let mut p = self.filter.get_cloned();
                    for i in p.shown(self.filter_search.get_cloned().text()) {
                        p.on[i] = on;
                    }
                    self.filter.set(p);
                    self.changed();
                    false
                }
                Cmd::FilterOpen(col) => {
                    self.choose_values(col);
                    self.changed();
                    false
                }
                Cmd::FilterApply => {
                    self.apply_values();
                    false
                }
                Cmd::CloseCancel => {
                    self.closing.set(false);
                    self.changed();
                    false
                }
                Cmd::CloseSave | Cmd::CloseDiscard => {
                    self.closing.set(false);
                    self.changed();
                    true
                }
                Cmd::FillOptions => {
                    self.fill_menu.set(MenuState::opened(None));
                    self.changed();
                    false
                }
                Cmd::FilterCancel => {
                    self.filter.set(FilterPanel::default());
                    self.changed();
                    false
                }
                Cmd::Data(dc) => {
                    let g = self.grid.get_cloned();
                    {
                        // The row and column at the top left of the view.
                        use pergola::widgets::GridSource as _;
                        let st = self.scroll.get();
                        let top = (self.source.row_at(st.physics.y), self.source.col_at(st.physics_x.y));
                        self.doc.borrow_mut().view_top = top;
                    }
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

/// The link bar's text as a link: `#Sheet2!B4`, or `Sheet2!B4` / `B4` naming
/// a place in the workbook; else an address, `https://` put in front of a
/// bare host. Empty: no link. Err: why it can't be one.
fn link_from_bar(wb: &officina_grid::Workbook, own: u16, s: &str) -> Result<Option<officina_grid::links::Link>, String> {
    use officina_grid::links::Link;
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if t.starts_with('#') {
        let l = Link::new(t).ok_or_else(String::new)?;
        return match l.place(wb, own) {
            Some(_) => Ok(Some(l)),
            None => Err(format!("There's no {} in this workbook.", &t[1..])),
        };
    }
    if let Some(l) = Link::new(&format!("#{t}")).filter(|_| !t.contains(':') || t.contains('!')).filter(|l| l.place(wb, own).is_some()) {
        return Ok(Some(l));
    }
    let scheme = t.split_once(':').map(|(a, _)| a).filter(|a| !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)));
    if scheme.is_some() {
        return Ok(Link::new(t));
    }
    if t.contains('@') && !t.contains('/') {
        return Ok(Link::new(&format!("mailto:{t}")));
    }
    if t.contains('.') && !t.contains(char::is_whitespace) {
        return Ok(Link::new(&format!("https://{t}")));
    }
    Err(format!("“{t}” isn't an address or a place in this workbook."))
}

/// `u` short enough for the status line.
fn short_url(u: &str) -> String {
    if u.chars().count() > 60 {
        format!("{}…", u.chars().take(59).collect::<String>())
    } else {
        u.to_string()
    }
}

/// Who new comments are by: `$USER`, else "Me" (as Write writes them).
pub(crate) fn comment_author() -> String {
    std::env::var("USER").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| "Me".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::app::App;
    use pergola::event::{Event, Key, KeyEventKind, Modifiers};
    use pergola::geom::Point;

    #[test]
    fn compare_with_lists_and_takes_changes() {
        use crate::review::ReviewCmd;
        use officina_grid::compare::Change;
        let (d, _) = Doc::from_csv("mine.csv", b"Item,Qty\npens,3\nink,4\npaper,5\n").unwrap();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let theirs = |csv: &[u8]| Doc::from_csv("theirs.csv", csv).unwrap().0.wb;
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let run = |c: ReviewCmd| {
            s.cmds.lock().unwrap().push(Cmd::Review(c));
            s.take_cmds();
        };
        // Data ▸ Compare with… goes to the window, which asks for the copy.
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::CompareWith));
        assert_eq!(s.take_cmds(), vec![Cmd::Data(crate::data::DataCmd::CompareWith)]);
        s.compare_with("theirs.csv", theirs(b"Item,Qty\npens,3\nink,9\npaper,5\nclips,2\n"));
        let p = s.review.get_cloned();
        assert!(p.open);
        assert_eq!(p.changes.len(), 2, "{:?}", p.changes);
        let t = texts();
        for want in ["Compare", "2 differences from “theirs.csv”", "B3: 4 → 9", "1 row added before row 5 (clips, 2)", "Take", "Keep Mine", "Take All"] {
            assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
        }
        // The first is shown: its cell picked.
        assert_eq!(s.grid.get_cloned().cursor, (2, 1));
        // Take it: one undo step; it leaves the list.
        run(ReviewCmd::Take);
        assert_eq!(s.doc.borrow().wb.value(0, 2, 1), officina_grid::Value::Num(9.0));
        assert_eq!(s.review.get_cloned().changes.len(), 1);
        // Keep mine: the row isn't added and isn't listed again.
        run(ReviewCmd::Pick(0));
        run(ReviewCmd::Keep);
        assert!(s.review.get_cloned().changes.is_empty());
        assert!(s.doc.borrow().wb.value(0, 4, 0).is_empty());
        s.cmds.lock().unwrap().push(Cmd::Undo);
        s.take_cmds();
        assert_eq!(s.doc.borrow().wb.value(0, 2, 1), officina_grid::Value::Num(4.0));
        // Take All against a copy with rows removed: the sheet becomes theirs.
        s.compare_with("short.csv", theirs(b"Item,Qty\nink,4\n"));
        assert!(s.review.get_cloned().changes.iter().any(|c| matches!(c, Change::RowsRemoved { .. })));
        run(ReviewCmd::TakeAll);
        assert!(s.review.get_cloned().changes.is_empty());
        assert_eq!(s.doc.borrow().wb.value(0, 1, 0), officina_grid::Value::Str("ink".into()));
        assert!(s.doc.borrow().wb.value(0, 2, 0).is_empty());
        run(ReviewCmd::Close);
        assert!(!s.review.get_cloned().open && s.theirs.borrow().is_none());
    }

    #[test]
    fn rows_and_columns_are_inserted_and_deleted() {
        use crate::data::DataCmd;
        let (mut d, _) = Doc::from_csv("r.csv", b"a,1\nb,2\nc,3\n").unwrap();
        d.type_into(3, 1, "=SUM(B1:B3)");
        d.wb.recalc();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: DataCmd| {
            s.cmds.lock().unwrap().push(Cmd::Data(c));
            s.take_cmds();
        };
        let f = |r: u32, c: u32| s.doc.borrow().wb.formula_text(officina_grid::CellAddr::new(0, r, c));
        // Rows 2–3 selected: two rows inserted above them.
        let mut g = s.grid.get_cloned();
        g.anchor = (1, 0);
        g.cursor = (2, 1);
        s.grid.set(g);
        let items = crate::data::menu(&s.doc.borrow(), s.grid.get_cloned().selection(), (2, 1));
        assert!(items.iter().any(|(m, c)| *c == Some(DataCmd::InsertRows) && m.label == "Insert 2 Rows Above"), "{:?}", items.iter().map(|x| x.0.label.clone()).collect::<Vec<_>>());
        run(DataCmd::InsertRows);
        assert_eq!(f(5, 1).as_deref(), Some("=SUM(B1:B5)"));
        assert_eq!(s.doc.borrow().wb.value(0, 3, 0), officina_grid::Value::Str("b".into()));
        assert_eq!(s.doc.borrow().status, "2 rows inserted.");
        // Undo puts it back; deleting column A moves the sum left.
        s.cmds.lock().unwrap().push(Cmd::Undo);
        s.take_cmds();
        assert_eq!(f(3, 1).as_deref(), Some("=SUM(B1:B3)"));
        s.grid.set(GridState::default());
        run(DataCmd::DeleteCols);
        assert_eq!(f(3, 0).as_deref(), Some("=SUM(A1:A3)"));
        assert_eq!(s.doc.borrow().wb.value(0, 3, 0), officina_grid::Value::Num(6.0));
        // Deleting a row the sum covers narrows it.
        run(DataCmd::DeleteRows);
        assert_eq!(f(2, 0).as_deref(), Some("=SUM(A1:A2)"));
        assert_eq!(s.doc.borrow().wb.value(0, 2, 0), officina_grid::Value::Num(5.0));
    }

    #[test]
    fn validation_refuses_warns_and_offers_its_list() {
        use crate::data::DataCmd;
        use officina_grid::condfmt::CmpOp;
        use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
        use officina_grid::{Rect as R, Value};
        let mut d = Doc::new("v");
        let r = |r0, c0, r1, c1| R { sheet: 0, r0, c0, r1, c1 };
        let f = |d: &Doc, s: &str| Some(d.wb.validation_formula(s, 0, (0, 0)).unwrap());
        let mut list = Validation::new(0, vec![r(0, 0, 9, 0)], Allow::List, CmpOp::Between, f(&d, "\"Yes,No,Maybe later\""), None);
        list.prompt = Message::new("Answer", "Yes or no.");
        let mut whole = Validation::new(0, vec![r(0, 1, 9, 1)], Allow::Whole, CmpOp::Between, f(&d, "1"), f(&d, "10"));
        whole.error = Message::new("Too many", "Ten at most.");
        let mut warn = Validation::new(0, vec![r(0, 2, 9, 2)], Allow::Decimal, CmpOp::LessOrEqual, f(&d, "100"), None);
        warn.style = ErrorStyle::Warning;
        for v in [list, whole, warn] {
            d.wb.add_validation(v).unwrap();
        }
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let v = |r: u32, c: u32| s.doc.borrow().wb.value(0, r, c);
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        // Not in the list: refused, with what it takes.
        assert!(!s.doc.borrow_mut().type_into(0, 0, "Perhaps"));
        assert_eq!(v(0, 0), Value::Empty);
        assert_eq!(s.doc.borrow().status, "This cell takes one of: Yes, No, Maybe later.");
        assert!(s.doc.borrow_mut().type_into(0, 0, "yes"));
        // Out of range: refused with the rule's own message.
        assert!(!s.doc.borrow_mut().type_into(0, 1, "12"));
        assert_eq!(s.doc.borrow().status, "Too many: Ten at most.");
        assert!(s.doc.borrow_mut().type_into(0, 1, "7"));
        assert_eq!(v(0, 1), Value::Num(7.0));
        // A warning keeps the value and asks; Undo takes it back.
        assert!(s.doc.borrow_mut().type_into(0, 2, "150"));
        assert_eq!(v(0, 2), Value::Num(150.0));
        assert!(texts().iter().any(|t| t == "C1: This cell expects a number at most 100. Keep it?"), "{:?}", texts());
        s.tab_cmd(crate::tabs::TabCmd::WarnUndo);
        assert_eq!(v(0, 2), Value::Empty);
        assert!(s.doc.borrow().warned.is_none());
        assert!(s.doc.borrow_mut().type_into(0, 2, "150"));
        s.tab_cmd(crate::tabs::TabCmd::WarnKeep);
        assert_eq!(v(0, 2), Value::Num(150.0));
        // The list's cell shows its prompt; the list picks a value.
        let mut g = s.grid.get_cloned();
        g.select(3, 0);
        s.grid.set(g);
        let t = texts();
        assert!(t.iter().any(|x| x == "Answer") && t.iter().any(|x| x == "Yes or no."), "{t:?}");
        s.cmds.lock().unwrap().push(Cmd::Data(DataCmd::ChooseFromList));
        s.take_cmds();
        assert!(s.list.get_cloned().open);
        assert!(texts().iter().any(|x| x == "Choose for A4"));
        s.list_search.set(TextEdit::new("later"));
        s.cmds.lock().unwrap().push(Cmd::ListPick(0));
        s.take_cmds();
        assert_eq!(v(3, 0), Value::str("Maybe later"));
        assert!(!s.list.get_cloned().open);
    }

    #[test]
    fn the_validation_panel_makes_changes_and_deletes_rules() {
        use crate::data::DataCmd;
        use crate::validation as va;
        let s = Sheets::new(Doc::new("v"), Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 900.0));
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        let mut g = s.grid.get_cloned();
        g.select(1, 1);
        s.grid.set(g);
        run(Cmd::Data(DataCmd::Validation));
        assert!(texts().iter().any(|x| x == "New Validation"));
        assert_eq!(s.valid_text[va::RANGE].get_cloned().text(), "B2");
        // The grid's selection, chosen while the panel is open, fills Apply To.
        let mut g = s.grid.get_cloned();
        g.select(1, 1);
        g.cursor = (8, 1);
        s.grid.set(g);
        run(Cmd::UseSelection);
        assert_eq!(s.valid_text[va::RANGE].get_cloned().text(), "B2:B9");
        // Whole numbers 1 to 5 over B2:B9, told with the rule's own words.
        run(Cmd::Valid(ValidCmd::Allow(1)));
        s.valid_text[va::RANGE].set(TextEdit::new("B2:B9"));
        s.valid_text[va::A].set(TextEdit::new("1"));
        run(Cmd::Valid(ValidCmd::Save));
        assert_eq!(s.valid.get_cloned().error, "Type the highest value.");
        s.valid_text[va::B].set(TextEdit::new("5"));
        s.valid_text[va::ERROR_TEXT].set(TextEdit::new("One to five, please."));
        run(Cmd::Valid(ValidCmd::Save));
        assert!(!s.valid.get_cloned().open);
        assert!(!s.doc.borrow_mut().type_into(4, 1, "9"));
        assert_eq!(s.doc.borrow().status, "One to five, please.");
        // Opened again on a cell of it: it changes that rule, and can delete it.
        run(Cmd::Data(DataCmd::Validation));
        assert!(texts().iter().any(|x| x == "Change Validation"));
        assert!(texts().iter().any(|x| x == "Delete Rule"));
        assert_eq!(s.valid_text[va::B].get_cloned().text(), "5");
        run(Cmd::Valid(ValidCmd::Remove));
        assert!(s.doc.borrow().wb.validations().is_empty());
        assert!(s.doc.borrow_mut().type_into(4, 1, "9"));
    }

    #[test]
    fn an_edge_of_selected_whole_columns_sizes_them_all() {
        use pergola::widgets::GridSource;
        let mut d = Doc::new("w");
        d.type_into(0, 2, "a rather long entry here");
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let rows = s.source.rows();
        let cols = CellRange { r0: 0, c0: 1, r1: rows - 1, c1: 3 };
        assert_eq!(sized_span(&cols, false, 2, &*s.source), (1, 3));
        // An edge outside them, or a selection not of whole columns: one.
        assert_eq!(sized_span(&cols, false, 5, &*s.source), (5, 5));
        assert_eq!(sized_span(&CellRange { r1: 9, ..cols }, false, 2, &*s.source), (2, 2));
        assert_eq!(sized_span(&CellRange { r0: 4, c0: 0, r1: 6, c1: s.source.cols() - 1 }, true, 5, &*s.source), (4, 6));
        let mut d = s.doc.borrow_mut();
        d.size_preview(false, 1, 3, 100.0);
        d.size_done();
        assert!((1..=3).all(|c| d.wb.sheet(0).col_sizes.size(c) == 100.0));
        assert!(d.undo());
        assert!((1..=3).all(|c| d.wb.sheet(0).col_sizes.size(c) == 64.0));
    }

    #[test]
    fn a_double_click_on_the_fill_handle_fills_down_beside_the_data() {
        let mut d = Doc::new("f");
        for r in 0..5 {
            d.type_into(r, 0, &format!("{}", r + 1));
        }
        d.type_into(0, 1, "=A1*2");
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let mut g = s.grid.get_cloned();
        g.select(0, 1);
        s.grid.set(g);
        let mut app = App::new(s.clone());
        app.tick();
        let handle = app.tree().iter().find_map(|(_, n)| match n { Node::Rect { rect, radius, .. } if *radius == 1.0 && rect.w() == 7.0 && rect.h() == 7.0 => Some(*rect), _ => None }).expect("the fill handle");
        let at = pergola::geom::Point::new(handle.x() + 4.0, handle.y() + 4.0);
        for _ in 0..2 {
            app.handle_event(Event::PointerDown { at });
            app.handle_event(Event::PointerUp { at });
            app.tick();
        }
        let d = s.doc.borrow();
        assert_eq!(d.wb.formula_text(officina_grid::CellAddr::new(0, 4, 1)).as_deref(), Some("=A5*2"));
        assert_eq!(d.wb.formula_text(officina_grid::CellAddr::new(0, 5, 1)), None);
        assert_eq!(s.grid.get_cloned().selection(), CellRange { r0: 0, c0: 1, r1: 4, c1: 1 });
        drop(d);
        // The fill options button under B5's right; pressed, it shows the ways.
        let tree = pergola::view::render(&s, pergola::theme::Semantic::LIGHT).0;
        assert!(tree.iter().any(|(_, n)| matches!(n, Node::Rect { rect, radius, .. } if *radius == 3.0 && rect.w() == 16.0 && rect.h() == 16.0)), "the fill options button");
        s.cmds.lock().unwrap().push(Cmd::FillOptions);
        s.take_cmds();
        let tree = pergola::view::render(&s, pergola::theme::Semantic::LIGHT).0;
        let texts: Vec<String> = tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect();
        assert!(texts.iter().any(|t| t == "Fill Series"), "{texts:?}");
    }

    #[test]
    fn a_list_cell_s_button_and_alt_down_open_its_list() {
        use officina_grid::condfmt::CmpOp;
        use officina_grid::validation::{Allow, Validation};
        let mut d = Doc::new("v");
        let f = d.wb.validation_formula("\"Yes,No\"", 0, (1, 1)).unwrap();
        d.wb.add_validation(Validation::new(0, vec![officina_grid::Rect { sheet: 0, r0: 1, c0: 1, r1: 5, c1: 1 }], Allow::List, CmpOp::Between, Some(f), None)).unwrap();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let buttons = |t: &pergola::NodeTree| t.iter().filter_map(|(_, n)| match n { Node::Rect { rect, radius, .. } if *radius == 3.0 && rect.w() == 16.0 && rect.h() == 16.0 => Some(*rect), _ => None }).collect::<Vec<_>>();
        // No button away from the rule's cells.
        assert!(buttons(&pergola::view::render(&s, pergola::theme::Semantic::LIGHT).0).is_empty());
        let mut g = s.grid.get_cloned();
        g.select(2, 1);
        s.grid.set(g);
        let mut app = App::new(s.clone());
        app.tick();
        let b = buttons(app.tree());
        assert_eq!(b.len(), 1, "{b:?}");
        let at = pergola::geom::Point::new(b[0].x() + 8.0, b[0].y() + 8.0);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        s.take_cmds();
        let p = s.list.get_cloned();
        assert!(p.open && (p.row, p.col) == (2, 1) && p.items == ["Yes", "No"], "{p:?}");
        s.cmds.lock().unwrap().push(Cmd::ListCancel);
        s.take_cmds();
        // Alt+Down does the same.
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Down, modifiers: Modifiers { alt: true, ..Modifiers::default() }, chars: String::new() });
        app.tick();
        s.take_cmds();
        assert!(s.list.get_cloned().open);
        assert_eq!(s.grid.get_cloned().cursor, (2, 1));
    }

    #[test]
    fn invalid_data_is_circled_until_turned_off() {
        use officina_grid::condfmt::CmpOp;
        use officina_grid::validation::{Allow, Validation};
        let (mut d, _) = Doc::from_csv("c.csv", b"5\n12\n7\n30\n").unwrap();
        let f = d.wb.validation_formula("10", 0, (0, 0)).unwrap();
        d.wb.add_validation(Validation::new(0, vec![officina_grid::Rect { sheet: 0, r0: 0, c0: 0, r1: 9, c1: 0 }], Allow::Whole, CmpOp::LessOrEqual, Some(f), None)).unwrap();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let rings = || {
            let fl = pergola::widgets::GridSource::floats(&*s.source, [0.0, 0.0, 1000.0, 600.0], &pergola::theme::Semantic::LIGHT);
            fl.iter().find(|f| f.key == 1 << 58).map(|f| f.nodes.len())
        };
        assert_eq!(rings(), None);
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::CircleInvalid));
        s.take_cmds();
        assert_eq!(s.doc.borrow().status, "2 cells break their rules; they are circled.");
        assert!(rings().is_some_and(|n| n >= 2));
        // Fixed, a cell's ring goes; turned off, they all go.
        s.doc.borrow_mut().type_into(1, 0, "3");
        assert_eq!(s.doc.borrow().wb.invalid_cells(&officina_grid::Rect { sheet: 0, r0: 0, c0: 0, r1: 9, c1: 0 }, 10, 100).len(), 1);
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::CircleInvalid));
        s.take_cmds();
        assert_eq!(rings(), None);
    }

    #[test]
    fn closing_with_changes_asks_first() {
        let (d, _) = Doc::from_csv("sums.csv", b"a,1\n").unwrap();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        s.closing.set(true);
        let t = texts();
        for want in ["Save the changes to “sums.csv” before closing?", "Don't Save", "Cancel", "Save"] {
            assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
        }
        s.cmds.lock().unwrap().push(Cmd::CloseCancel);
        assert!(s.take_cmds().is_empty());
        assert!(!s.closing.get());
        for c in [Cmd::CloseSave, Cmd::CloseDiscard] {
            s.closing.set(true);
            s.cmds.lock().unwrap().push(c);
            assert_eq!(s.take_cmds(), [c]);
            assert!(!s.closing.get());
        }
    }

    #[test]
    fn a_filter_column_s_values_are_chosen_from_a_list() {
        let (mut d, _) = Doc::from_csv("t.csv", b"Fruit,Qty\npear,3\napple,10\nfig,7\napple,1\n").unwrap();
        d.wb.recalc();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let hidden = |s: &Sheets| (1..=4).filter(|&r| s.doc.borrow().wb.sheet(0).row_hidden(r)).collect::<Vec<u32>>();
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::Filter));
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::ChooseValues));
        s.take_cmds();
        let p = s.filter.get_cloned();
        assert!(p.open, "{}", s.doc.borrow().status);
        assert_eq!((p.title.as_str(), p.values.iter().map(|v| (v.0.as_str(), v.1)).collect::<Vec<_>>(), p.on.clone()), ("A · Fruit", vec![("apple", 2), ("fig", 1), ("pear", 1)], vec![true; 3]));
        // A click on apple's row lets it go; Apply hides both apples, one undo step.
        let mut app = App::new(s.clone());
        app.tick();
        let (x0, ly) = (1200.0 - FILTER_W + 1.0, TOOLBAR_H + FORMULA_H + 120.0);
        let at = pergola::geom::Point::new(x0 + 12.0 + 8.0, ly + 4.0 + 8.0);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        s.take_cmds();
        assert_eq!(s.filter.get_cloned().on, [false, true, true]);
        assert_eq!(s.grid.get_cloned().cursor, (0, 0), "the press stayed off the grid");
        s.cmds.lock().unwrap().push(Cmd::FilterApply);
        s.take_cmds();
        assert!(!s.filter.get_cloned().open);
        assert_eq!(hidden(&s), [2, 4]);
        // Opened again, it shows the filter's choice; the search narrows Clear.
        s.choose_values(0);
        assert_eq!(s.filter.get_cloned().on, [false, true, true]);
        s.filter_search.set(TextEdit::new("PE"));
        s.cmds.lock().unwrap().push(Cmd::FilterAll(false));
        s.cmds.lock().unwrap().push(Cmd::FilterApply);
        s.take_cmds();
        assert_eq!(hidden(&s), [1, 2, 4]);
        // Choosing nothing is refused; choosing everything drops the condition.
        s.choose_values(0);
        s.cmds.lock().unwrap().push(Cmd::FilterAll(false));
        s.cmds.lock().unwrap().push(Cmd::FilterApply);
        s.take_cmds();
        assert!(s.filter.get_cloned().open);
        assert_eq!(s.doc.borrow().status, "Choose at least one value to show.");
        s.cmds.lock().unwrap().push(Cmd::FilterAll(true));
        s.cmds.lock().unwrap().push(Cmd::FilterApply);
        s.take_cmds();
        assert!(hidden(&s).is_empty());
        assert!(s.doc.borrow().wb.sheet(0).filter.as_ref().unwrap().conds.is_empty());
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(hidden(&s), [1, 2, 4]);
        // Cancel keeps the filter as it is.
        s.choose_values(0);
        s.cmds.lock().unwrap().push(Cmd::FilterAll(true));
        s.cmds.lock().unwrap().push(Cmd::FilterCancel);
        s.take_cmds();
        assert_eq!(hidden(&s), [1, 2, 4]);
    }

    #[test]
    fn conditional_formats_are_made_in_the_panel() {
        let (mut d, _) = Doc::from_csv("t.csv", b"Qty\n3\n10\n7\n1\n").unwrap();
        d.wb.recalc();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let mut g = s.grid.get_cloned();
        g.anchor = (1, 0);
        g.cursor = (4, 0);
        s.grid.set(g);
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let run = |c: CondCmd| {
            s.cmds.lock().unwrap().push(Cmd::Cond(c));
            s.take_cmds();
        };
        // No rules yet: the panel opens on a new rule over the selection.
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::CondFormats));
        s.take_cmds();
        let p = s.cond.get_cloned();
        assert!(p.open && p.editor.is_some());
        assert_eq!(s.cond_range.get_cloned().text(), "A2:A5");
        let t = texts();
        for want in ["New Rule", "Format cells by", "Cell Value", "between", "Look", "Light Red Fill with Dark Red Text", "Save"] {
            assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
        }
        // Greater than 5, green.
        run(CondCmd::Choice(4));
        run(CondCmd::Look(2));
        s.cond_a.set(TextEdit::new("5"));
        run(CondCmd::Save);
        let p = s.cond.get_cloned();
        assert!(p.editor.is_none(), "{}", p.error);
        let fill = |r: u32| s.doc.borrow().wb.look(officina_grid::CellAddr::new(0, r, 0)).dxf.fill;
        assert_eq!((fill(1), fill(2), fill(3), fill(4)), (None, Some(0xC6EFCE), Some(0xC6EFCE), None));
        let t = texts();
        assert!(t.iter().any(|x| x == "Cell value greater than 5"), "{t:?}");
        // A data bar as a second rule, then the first rule changed to less than 5.
        run(CondCmd::New);
        run(CondCmd::Kind(9));
        assert!(texts().iter().any(|x| x == "Blue"));
        run(CondCmd::Choice(1));
        run(CondCmd::Save);
        assert_eq!(crate::condfmt::entries(&s.doc.borrow().wb, s.cond.get_cloned().sel.unwrap()).len(), 2);
        assert!(s.doc.borrow().wb.look(officina_grid::CellAddr::new(0, 2, 0)).bar.is_some());
        // A click on the second row picks it, and stays off the grid.
        let mut app = App::new(s.clone());
        app.tick();
        let at = pergola::geom::Point::new(1200.0 - COND_W + 40.0, TOOLBAR_H + FORMULA_H + 58.0 + COND_ROW_H + 12.0);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        s.take_cmds();
        assert_eq!(s.cond.get_cloned().picked, Some(1));
        assert_eq!(s.grid.get_cloned().cursor, (4, 0));
        run(CondCmd::Edit);
        assert_eq!(s.cond_a.get_cloned().text(), "5");
        run(CondCmd::Choice(5));
        run(CondCmd::Save);
        assert_eq!((fill(1), fill(2), fill(4)), (Some(0xC6EFCE), None, Some(0xC6EFCE)));
        // A bad value says why and keeps the editor open.
        run(CondCmd::Edit);
        s.cond_a.set(TextEdit::new("=SUM("));
        run(CondCmd::Save);
        assert!(!s.cond.get_cloned().error.is_empty());
        assert!(texts().contains(&s.cond.get_cloned().error));
        run(CondCmd::Back);
        // Up, delete, and each step undoes.
        run(CondCmd::Up);
        assert_eq!(s.cond.get_cloned().picked, Some(0));
        assert!(matches!(s.doc.borrow().wb.cond_formats()[0].rules[0].kind, officina_grid::condfmt::RuleKind::CellIs { .. }));
        run(CondCmd::Delete);
        assert_eq!(s.doc.borrow().wb.cond_formats().len(), 1);
        run(CondCmd::Close);
        assert!(!s.cond.get_cloned().open);
        for _ in 0..5 {
            assert!(s.doc.borrow_mut().undo());
        }
        assert!(s.doc.borrow().wb.cond_formats().is_empty());
    }

    #[test]
    fn rows_nobody_sized_grow_for_typed_lines() {
        let (d, _) = Doc::from_csv("t.csv", b"a,1\nb,2\n").unwrap();
        let grid = Mutable::new(GridState::default());
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), grid, Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let height = |r| s.doc.borrow().wb.sheet(0).row_sizes.size(r);
        let commit = |row, text: &str| Sheets::act(&s.doc, &s.source, &s.rev, GridAction::Commit { row, col: 0, text: text.into() });
        commit(4, "one\ntwo\nthree");
        assert!(height(4) > 40.0, "{}", height(4));
        // One undo takes the text and the height back.
        assert!(s.doc.borrow_mut().undo());
        assert_eq!((height(4), s.doc.borrow().wb.value(0, 4, 0)), (20.0, officina_grid::Value::Empty));
        // One line: no change. A row sized by hand stays as it was.
        commit(5, "short");
        assert_eq!(height(5), 20.0);
        s.doc.borrow_mut().size_preview(true, 6, 6, 30.0);
        s.doc.borrow_mut().size_done();
        commit(6, "one\ntwo\nthree");
        assert_eq!(height(6), 30.0);
        // A grown row shrinks back when its text needs less.
        commit(4, "one\ntwo\nthree");
        assert!(height(4) > 40.0);
        commit(4, "one\ntwo");
        assert!(height(4) > 20.0 && height(4) < 40.0, "{}", height(4));
        commit(4, "one");
        assert_eq!(height(4), 20.0);
        // Cleared, too.
        commit(4, "one\ntwo");
        Sheets::act(&s.doc, &s.source, &s.rev, GridAction::Clear(pergola::widgets::CellRange { r0: 4, c0: 0, r1: 4, c1: 0 }));
        assert_eq!(height(4), 20.0);
    }

    #[test]
    fn the_custom_lists_panel_opens_and_closes() {
        let s = Sheets::new(Doc::new("l"), Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        let texts = || pergola::view::render(&s, pergola::theme::Semantic::LIGHT).0.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<_>>();
        assert!(!texts().iter().any(|t| t == "Custom lists"));
        run(Cmd::Data(crate::data::DataCmd::CustomLists));
        assert!(s.lists_open.get() && texts().iter().any(|t| t == "Custom lists"));
        // A list no longer there: nothing happens.
        run(Cmd::ListsForget(999));
        run(Cmd::ListsClose);
        assert!(!s.lists_open.get());
    }

    #[test]
    fn flash_fill_fills_a_column_by_its_examples() {
        let (d, _) = Doc::from_csv("t.csv", b"Name,Last\nAda Lovelace,Lovelace\nAlan Turing,\nGrace Hopper,\n").unwrap();
        let grid = Mutable::new(GridState::default());
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), grid.clone(), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let mut g = grid.get_cloned();
        g.select(2, 1);
        grid.set(g);
        s.cmds.lock().unwrap().push(Cmd::Data(crate::data::DataCmd::FlashFill));
        s.take_cmds();
        let v = |r| s.doc.borrow().wb.value(0, r, 1).display();
        assert_eq!((v(0), v(2), v(3)), ("Last".to_string(), "Turing".to_string(), "Hopper".to_string()));
        assert_eq!(s.doc.borrow().status, "Flash Fill filled 2 cells.");
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(v(2), "");
    }

    #[test]
    fn a_fill_offers_its_options_by_the_filled_area() {
        use pergola::widgets::GridSource;
        let (d, _) = Doc::from_csv("t.csv", b"1\n2\n").unwrap();
        let grid = Mutable::new(GridState::default());
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), grid.clone(), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        let button = || s.source.floats([0.0, 0.0, 2000.0, 2000.0], &pergola::theme::Semantic::default()).into_iter().find(|f| f.key == crate::source::KEY_FILL).map(|f| f.at);
        assert!(button().is_none());
        let r = |r0, r1| officina_grid::addr::Rect { sheet: 0, r0, c0: 0, r1, c1: 0 };
        assert!(s.doc.borrow_mut().fill(r(0, 1), r(0, 4), false));
        // Under the right of A5.
        assert_eq!(button(), Some([65.0, 101.0, 16.0, 16.0]));
        run(Cmd::FillOptions);
        assert!(s.fill_menu.get().open);
        // Copy Cells from it: 1, 2, 1.
        run(Cmd::Data(crate::data::DataCmd::FillCopy));
        assert_eq!(s.doc.borrow().wb.value(0, 2, 0).display(), "1");
        assert!(button().is_some());
        // Any other edit: gone.
        s.doc.borrow_mut().type_into(9, 9, "x");
        assert!(button().is_none());
    }

    #[test]
    fn the_paste_special_bar_pastes_its_choices_together() {
        use crate::tabs::TabCmd;
        let (d, _) = Doc::from_csv("t.csv", b"1,10\n2,20\n").unwrap();
        let grid = Mutable::new(GridState::default());
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), grid.clone(), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        // A1:A2 copied; pasted onto B1 as values, added, turned: B1 11, C1 22.
        s.doc.borrow_mut().take_clip(officina_grid::addr::Rect { sheet: 0, r0: 0, c0: 0, r1: 1, c1: 0 }, false, "1\n2".into());
        let mut g = grid.get_cloned();
        g.select(0, 1);
        grid.set(g);
        run(Cmd::Data(crate::data::DataCmd::PasteSpecial));
        assert_eq!(s.paste_bar.get().map(|p| p.what), Some(officina_grid::clip::What::All));
        for c in [TabCmd::PasteWhat(1), TabCmd::PasteOp(1), TabCmd::PasteTurn] {
            run(Cmd::Tab(c));
        }
        run(Cmd::Tab(TabCmd::PasteDone));
        assert!(s.paste_bar.get().is_none(), "{}", s.doc.borrow().status);
        let v = |r, c| s.doc.borrow().wb.value(0, r, c).display();
        assert_eq!((v(0, 1), v(0, 2), v(1, 1)), ("11".to_string(), "2".to_string(), "20".to_string()));
        assert_eq!(s.doc.borrow().status, "Paste Special: done.");
        // One undo step.
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(v(0, 1), "10");
        // A refusal keeps the bar open: formats can't be added.
        run(Cmd::Data(crate::data::DataCmd::PasteSpecial));
        run(Cmd::Tab(TabCmd::PasteWhat(3)));
        run(Cmd::Tab(TabCmd::PasteOp(2)));
        run(Cmd::Tab(TabCmd::PasteDone));
        assert!(s.paste_bar.get().is_some());
        assert_eq!(s.doc.borrow().status, "Formats can't be added or multiplied.");
        run(Cmd::Tab(TabCmd::PasteCancel));
        assert!(s.paste_bar.get().is_none());
    }

    #[test]
    fn the_size_bar_sets_exact_heights_and_widths() {
        use crate::tabs::TabCmd;
        let (d, _) = Doc::from_csv("t.csv", b"a,1\nb,2\n").unwrap();
        let grid = Mutable::new(GridState::default());
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), grid.clone(), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        // Columns B:C selected: the bar holds B's width.
        let mut g = grid.get_cloned();
        g.select(0, 1);
        g.cursor = (0, 2);
        grid.set(g);
        run(Cmd::Data(crate::data::DataCmd::ColWidth));
        assert_eq!(s.size_bar.get(), Some((0, false, 1, 2)));
        assert_eq!(s.size_text.get_cloned().text(), "64");
        s.size_text.set(TextEdit::new("100"));
        run(Cmd::Tab(TabCmd::SizeDone));
        assert!(s.size_bar.get().is_none());
        let width = |c| s.doc.borrow().wb.sheet(0).col_sizes.size(c);
        assert_eq!((width(1), width(2), width(3)), (100.0, 100.0, 64.0));
        // One undo step.
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(width(2), 64.0);
        // Rows; nonsense keeps the bar open with a note.
        run(Cmd::Data(crate::data::DataCmd::RowHeight));
        s.size_text.set(TextEdit::new("tall"));
        run(Cmd::Tab(TabCmd::SizeDone));
        assert!(s.size_bar.get().is_some() && s.doc.borrow().status.starts_with("Type a size"));
        s.size_text.set(TextEdit::new("40"));
        run(Cmd::Tab(TabCmd::SizeDone));
        assert_eq!(s.doc.borrow().wb.sheet(0).row_sizes.size(0), 40.0);
        // 0 hides.
        run(Cmd::Data(crate::data::DataCmd::RowHeight));
        s.size_text.set(TextEdit::new("0"));
        run(Cmd::Tab(TabCmd::SizeDone));
        assert!(s.doc.borrow().wb.sheet(0).row_hidden(0));
        // Excel's units: points for rows, characters for columns.
        let mut g = grid.get_cloned();
        g.select(1, 1);
        g.cursor = (1, 2);
        grid.set(g);
        run(Cmd::Data(crate::data::DataCmd::RowHeight));
        assert_eq!(s.doc.borrow().status, "20 px is 15 pt in Excel. Type px, or a size with pt.");
        s.size_text.set(TextEdit::new("30 pt"));
        run(Cmd::Tab(TabCmd::SizeDone));
        assert_eq!(s.doc.borrow().wb.sheet(0).row_sizes.size(1), 40.0);
        run(Cmd::Data(crate::data::DataCmd::ColWidth));
        assert!(s.doc.borrow().status.starts_with("64 px is 8.43 ch in Excel"), "{}", s.doc.borrow().status);
        s.size_text.set(TextEdit::new("10ch"));
        run(Cmd::Tab(TabCmd::SizeDone));
        assert_eq!(width(2), 75.0);
        assert_eq!((typed_px("12 ch", true), typed_px("1.5in", false), typed_px("-3", false)), (None, None, None));
    }

    #[test]
    fn links_are_inserted_followed_and_removed() {
        use crate::tabs::TabCmd;
        let (mut d, _) = Doc::from_csv("t.csv", b"a,1\nb,2\n").unwrap();
        d.wb.add_sheet("Totals").unwrap();
        d.wb.recalc();
        let grid = Mutable::new(GridState::default());
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), grid.clone(), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        let at = |r: u32, c: u32| {
            let mut g = grid.get_cloned();
            g.select(r, c);
            grid.set(g);
        };
        // A place on another sheet: the bar takes it without the #.
        at(0, 0);
        run(Cmd::Data(crate::data::DataCmd::Link));
        assert_eq!(s.linking.get(), Some((0, 0, 0)));
        s.link_text.set(TextEdit::new("Totals!C5"));
        run(Cmd::Tab(TabCmd::LinkDone));
        assert!(s.linking.get().is_none());
        assert_eq!(&*s.doc.borrow().wb.link(0, 0, 0).unwrap().target, "#Totals!C5");
        assert_eq!(pergola::widgets::GridSource::cell(&*s.source, 0, 0).unwrap().color, Some(pergola::theme::palette::info_600()));
        // With the cursor on it, where it goes shows beside the cell.
        let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
        let shown: Vec<String> = tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect();
        assert!(shown.iter().any(|t| t == "Go to Totals!C5") && shown.iter().any(|t| t == "Ctrl+click to open"), "{shown:?}");
        run(Cmd::Data(crate::data::DataCmd::OpenLink));
        assert_eq!(s.doc.borrow().sheet, 1);
        assert_eq!(grid.get_cloned().cursor, (4, 2));
        // An address on an empty cell: https:// put in front, and shown.
        s.show_sheet(0);
        at(3, 0);
        run(Cmd::Data(crate::data::DataCmd::Link));
        s.link_text.set(TextEdit::new("example.org/x"));
        run(Cmd::Tab(TabCmd::LinkDone));
        let d = s.doc.borrow();
        assert_eq!(&*d.wb.link(0, 3, 0).unwrap().target, "https://example.org/x");
        assert_eq!(d.wb.value(0, 3, 0), officina_grid::Value::Str("https://example.org/x".into()));
        drop(d);
        // Without a press it isn't handed on.
        run(Cmd::Data(crate::data::DataCmd::OpenLink));
        assert!(s.doc.borrow().status.starts_with("Ctrl+click the cell"));
        // Not a link: refused, the bar stays.
        at(1, 0);
        run(Cmd::Data(crate::data::DataCmd::Link));
        s.link_text.set(TextEdit::new("not a link"));
        run(Cmd::Tab(TabCmd::LinkDone));
        assert!(s.linking.get().is_some());
        assert!(s.doc.borrow().wb.link(0, 1, 0).is_none());
        run(Cmd::Tab(TabCmd::LinkCancel));
        // Remove, then undo brings it back with its text.
        let mut g = grid.get_cloned();
        g.select(0, 0);
        g.cursor = (3, 1);
        grid.set(g);
        run(Cmd::Data(crate::data::DataCmd::RemoveLinks));
        assert_eq!(s.doc.borrow().status, "2 links removed.");
        assert!(s.doc.borrow().wb.links(0).is_empty());
        assert!(s.doc.borrow_mut().undo());
        assert_eq!(s.doc.borrow().wb.links(0).len(), 2);
    }

    #[test]
    fn comments_are_written_shown_beside_the_cell_edited_and_deleted() {
        use crate::tabs::TabCmd;
        let (mut d, _) = Doc::from_csv("t.csv", b"a,1\nb,2\n").unwrap();
        d.wb.recalc();
        let grid = Mutable::new(GridState::default());
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), grid.clone(), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        let mut g = grid.get_cloned();
        g.select(1, 1);
        grid.set(g);
        run(Cmd::Data(crate::data::DataCmd::Comment));
        assert_eq!(s.commenting.get(), Some((0, 1, 1)));
        assert!(texts().iter().any(|t| t == "Comment on B2"));
        s.comment_text.set(TextEdit::rich("Too high\ncheck March", []));
        run(Cmd::Tab(TabCmd::CommentDone));
        assert!(s.commenting.get().is_none());
        let k = s.doc.borrow().wb.comment(0, 1, 1).cloned().unwrap();
        assert_eq!(&*k.text, "Too high\ncheck March");
        assert!(!k.author.is_empty());
        // With the cursor on it, the comment shows beside the cell.
        let shown = texts();
        assert!(shown.iter().any(|t| t == "Too high") && shown.iter().any(|t| t == "check March"), "{shown:?}");
        // Editing keeps the author; emptying the text deletes it; undo brings it back.
        run(Cmd::Data(crate::data::DataCmd::Comment));
        assert_eq!(s.comment_text.get_cloned().text(), "Too high\ncheck March");
        s.comment_text.set(TextEdit::new(""));
        run(Cmd::Tab(TabCmd::CommentDone));
        assert!(s.doc.borrow().wb.comment(0, 1, 1).is_none());
        assert!(s.doc.borrow_mut().undo());
        assert!(s.doc.borrow().wb.comment(0, 1, 1).is_some());
        run(Cmd::Data(crate::data::DataCmd::DeleteComments));
        assert!(s.doc.borrow().wb.comment(0, 1, 1).is_none());
        assert_eq!(s.doc.borrow().status, "Comment deleted.");
    }

    #[test]
    fn rich_comment_edits_render_and_undo_through_the_comment_bar() {
        use crate::tabs::TabCmd;
        use officina_grid::comments::{Comment, CommentRun, CommentStyle};
        use pergola::event::{Event, Key, KeyEventKind, Modifiers};
        use pergola::theme::Weight;

        let (mut doc, _) = Doc::from_csv("t.csv", b"1\n").unwrap();
        let original = Comment::with_runs("Ana", "é bold\nitalic ¶ text\t!", [
            CommentRun { start: 0, end: 7, style: CommentStyle::from_mask(1) },
            CommentRun { start: 8, end: 14, style: CommentStyle::from_mask(10) },
        ]);
        doc.wb.set_comments(0, vec![(0, 0, Some(original.clone()))]);
        let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let sheets = Sheets::new(doc, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()),
            Mutable::new(Ui::default()), Mutable::with_dirty(0, dirty.clone()), (1200.0, 800.0));
        let saved = || sheets.doc.borrow().wb.comment(0, 0, 0).cloned().unwrap();
        let texts = |tree: &pergola::node::NodeTree, text: &str| {
            tree.iter().find_map(|(_, node)| match node {
                Node::Text { content, rect, style } if content == text => Some((*rect, style.clone())),
                _ => None,
            }).unwrap_or_else(|| panic!("missing text run {text:?}"))
        };

        let mut app = pergola::app::App::new_with_flag(sheets.clone(), dirty);
        app.tick();
        let (bold, style) = texts(app.tree(), "é bold");
        assert_eq!(style.weight, Weight::Bold);
        let (italic, style) = texts(app.tree(), "italic");
        assert_eq!(style.family, "IBMPlexSans-Italic");
        assert!(italic.y() > bold.y());
        assert!(app.tree().iter().any(|(_, node)| matches!(node, Node::Rect { rect, fill, .. }
            if *fill == style.color && rect.x() == italic.x() && rect.w() == italic.w()
                && rect.y() > italic.y() && rect.y() < italic.y() + italic.h())));

        sheets.tab_cmd(TabCmd::Commenting);
        app.tick();
        let (label, _) = texts(app.tree(), "Comment on A1");
        let field = pergola::geom::Point::new(170.0, label.y() + 6.0);
        app.handle_event(Event::PointerDown { at: field });
        app.handle_event(Event::PointerUp { at: field });
        app.tick();
        assert_eq!(texts(app.tree(), "é bold").1.weight, Weight::Bold);
        assert_eq!(texts(app.tree(), "italic").1.family, "IBMPlexSans-Italic");
        let focus = app.focused().unwrap();
        let key = |app: &mut pergola::app::App<Sheets>, key, chars: &str, ctrl, shift| {
            app.handle_event(Event::Key { kind: KeyEventKind::Down, key, chars: chars.into(), modifiers: Modifiers { ctrl, shift, ..Default::default() } });
            app.tick();
        };
        key(&mut app, Key::Home, "", true, false);
        key(&mut app, Key::Right, "", false, true);
        let u = app.tree().iter().find_map(|(_, node)| match node {
            Node::Text { content, rect, .. } if content == "U" && (rect.y() - label.y()).abs() < 16.0 => Some(*rect),
            _ => None,
        }).expect("the underline button");
        let at = pergola::geom::Point::new(u.x() + u.w() / 2.0, u.y() + u.h() / 2.0);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        sheets.take_cmds();
        app.tick();
        assert_eq!(app.focused(), Some(focus));
        assert_eq!(sheets.comment_text.get_cloned().inline_runs().next(), Some((0..2, 5)));
        key(&mut app, Key::Char, "z", true, false);
        assert_eq!(sheets.comment_text.get_cloned().inline_runs().next(), Some((0..7, 1)));
        key(&mut app, Key::Char, "y", true, false);
        assert_eq!(sheets.comment_text.get_cloned().inline_runs().next(), Some((0..2, 5)));
        key(&mut app, Key::Char, "É", false, false);
        key(&mut app, Key::Home, "", false, false);
        key(&mut app, Key::Enter, "", false, true);
        key(&mut app, Key::Enter, "", false, false);
        sheets.take_cmds();
        let changed = saved();
        assert_eq!(&*changed.text, "\nÉ bold\nitalic ¶ text\t!");
        assert_eq!(&*changed.author, "Ana");
        assert_eq!(changed.segments().collect::<Vec<_>>(), vec![
            ("\nÉ", CommentStyle::from_mask(5)),
            (" bold", CommentStyle::from_mask(1)),
            ("\n", CommentStyle::default()),
            ("italic", CommentStyle::from_mask(10)),
            (" ¶ text\t!", CommentStyle::default()),
        ]);
        assert!(sheets.doc.borrow_mut().undo());
        assert_eq!(saved(), original);
        assert!(sheets.doc.borrow_mut().redo());
        assert_eq!(saved(), changed);

        // A formatting-only edit must also make a workbook undo entry.
        sheets.tab_cmd(TabCmd::Commenting);
        let mut edit = sheets.comment_text.get_cloned();
        edit.select_all();
        sheets.comment_text.set(edit);
        sheets.tab_cmd(TabCmd::CommentFormat(pergola::inline::Emphasis::Strike));
        sheets.tab_cmd(TabCmd::CommentDone);
        let styled = saved();
        assert_eq!(styled.text, changed.text);
        assert_ne!(styled.runs(), changed.runs());
        assert!(sheets.doc.borrow_mut().undo());
        assert_eq!(saved(), changed);
    }

    #[test]
    fn system_clipboard_round_trips_formats_between_workbooks() {
        use officina_grid::style::{HAlign, Style};
        use officina_grid::{CellAddr, Rect as Cells, Value};
        use pergola::clipboard::{self, Backend, Error, Token};
        use pergola::event::{Event, Key, KeyEventKind, Modifiers};

        type Offers = Rc<RefCell<Vec<(String, Vec<u8>)>>>;
        struct Clipboard(Offers);
        impl Backend for Clipboard {
            fn set(&mut self, _: Token, formats: &[(&str, &[u8])], sensitive: bool) -> Result<(), Error> {
                assert!(!sensitive);
                *self.0.borrow_mut() = formats.iter().map(|(mime, data)| (mime.to_string(), data.to_vec())).collect();
                Ok(())
            }

            fn get(&mut self, _: Token, accept: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error> {
                Ok(accept.iter().find_map(|mime| self.0.borrow().iter().find(|(offered, _)| offered == mime).cloned()))
            }

            fn drop_get(&mut self, _: Token, _: &[&str]) -> Result<Option<(String, Vec<u8>)>, Error> {
                unreachable!()
            }
        }

        let offers: Offers = Rc::new(RefCell::new(Vec::new()));
        clipboard::set_backend(Box::new(Clipboard(offers.clone())));
        let new = |doc| {
            let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let sheets = Sheets::new(doc, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()),
                Mutable::new(Ui::default()), Mutable::with_dirty(0, dirty.clone()), (1200.0, 800.0));
            let mut app = pergola::app::App::new_with_flag(sheets.clone(), dirty);
            app.tick();
            let at = pergola::geom::Point::new(100.0, TOOLBAR_H + FORMULA_H + 34.0);
            app.handle_event(Event::PointerDown { at });
            app.handle_event(Event::PointerUp { at });
            assert!(app.focused().is_some());
            (sheets, app)
        };
        let chord = |app: &mut pergola::app::App<Sheets>, key: &str, token| {
            app.handle_event_activated(Event::Key { kind: KeyEventKind::Down, key: Key::Char, chars: key.into(),
                modifiers: Modifiers { ctrl: true, ..Default::default() } }, token);
        };
        let rect = |r, c| Cells { sheet: 0, r0: r, c0: c, r1: r, c1: c };
        let cell = |r, c| CellAddr::new(0, r, c);

        let mut source = Doc::new("source");
        assert!(source.type_into(0, 0, "=5/4"));
        source.wb.set_value(cell(0, 1), Value::str("=literal\ntext"));
        let mut number = Style::default().with_num_fmt("0.00%");
        number.font.bold = true;
        number.fill = Some(0xAAF2CC);
        number.h_align = HAlign::Right;
        let blank = Style { wrap: true, fill: Some(0xDD77EE), ..Default::default() };
        source.wb.set_style(&rect(0, 0), number.clone());
        source.wb.set_style(&rect(0, 2), blank.clone());
        let (source, mut first) = new(source);
        let mut grid = source.grid.get_cloned();
        grid.anchor = (0, 0);
        grid.cursor = (0, 2);
        source.grid.set(grid);
        chord(&mut first, "c", None);
        assert!(offers.borrow().is_empty());
        chord(&mut first, "c", Some([1; 16]));
        let original_offer = offers.borrow().clone();
        assert_eq!(original_offer.len(), 3);
        assert_eq!(original_offer[2].0, "text/html");
        assert_eq!(original_offer[0], ("text/plain".into(), b"1.25\t=literal text\t\n".to_vec()));

        let mut target = Doc::new("target");
        let before = Style { fill: Some(0x101010), ..Default::default() };
        target.wb.set_value(cell(2, 1), Value::Bool(true));
        target.wb.set_style(&rect(2, 1), before.clone());
        let (target, mut second) = new(target);
        let mut grid = target.grid.get_cloned();
        grid.select(2, 1);
        target.grid.set(grid);
        chord(&mut second, "v", Some([2; 16]));
        {
            let doc = target.doc.borrow();
            assert_eq!(doc.wb.value(0, 2, 1), Value::Num(1.25));
            assert!(doc.wb.formula_text(cell(2, 1)).is_none());
            assert_eq!(doc.wb.value(0, 2, 2), Value::str("=literal\ntext"));
            assert_eq!(doc.wb.style(cell(2, 1)), &number);
            assert_eq!(doc.wb.value(0, 2, 3), Value::Empty);
            assert_eq!(doc.wb.style(cell(2, 3)), &blank);
        }
        assert!(target.doc.borrow_mut().undo());
        assert_eq!(target.doc.borrow().wb.value(0, 2, 1), Value::Bool(true));
        assert_eq!(target.doc.borrow().wb.style(cell(2, 1)), &before);
        assert_eq!(target.doc.borrow().wb.style(cell(2, 3)), &Style::default());

        // A configured converter also exercises HTML-only system paste and
        // the user's explicit plain-text fallback through the grid.
        if std::env::var_os("OFFICINA_CONVERT").is_some() {
            *offers.borrow_mut() = vec![original_offer[0].clone(), original_offer[2].clone()];
            chord(&mut second, "v", Some([9; 16]));
            assert!(target.doc.borrow().html_paste.is_some());
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while target.doc.borrow().html_paste.is_some() && std::time::Instant::now() < until {
                target.take_cmds();
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(target.doc.borrow().html_paste.is_none());
            assert_eq!(target.doc.borrow().wb.value(0, 2, 1), Value::Num(1.25));
            assert_eq!(target.doc.borrow().wb.style(cell(2, 1)), &number);
            assert_eq!(target.doc.borrow().wb.style(cell(2, 3)), &blank);
            assert!(target.doc.borrow_mut().undo());

            second.handle_event_activated(Event::Key {
                kind: KeyEventKind::Down,
                key: Key::Char,
                chars: "v".into(),
                modifiers: Modifiers { ctrl: true, shift: true, ..Default::default() },
            }, Some([10; 16]));
            assert!(target.doc.borrow().html_paste.is_none());
            assert_eq!(target.doc.borrow().wb.value(0, 2, 1), Value::Num(1.25));
            assert_eq!(target.doc.borrow().wb.style(cell(2, 1)), &before);
            assert!(target.doc.borrow_mut().undo());
        }

        // A malformed rich offer leaves the destination alone.
        let mut bad = original_offer.clone();
        bad[1].1.pop();
        *offers.borrow_mut() = bad;
        chord(&mut second, "v", Some([3; 16]));
        assert_eq!(target.doc.borrow().status, "The clipboard cell data is invalid.");
        assert_eq!(target.doc.borrow().wb.value(0, 2, 1), Value::Bool(true));

        // The exact local copy still pastes the original formula.
        *offers.borrow_mut() = original_offer;
        let mut grid = source.grid.get_cloned();
        grid.select(1, 0);
        source.grid.set(grid);
        chord(&mut first, "v", Some([4; 16]));
        assert_eq!(source.doc.borrow().wb.formula_text(cell(1, 0)).as_deref(), Some("=5/4"));

        // Identical plain text copied by another app does not replay a cut.
        let mut grid = source.grid.get_cloned();
        grid.select(0, 0);
        source.grid.set(grid);
        chord(&mut first, "x", Some([5; 16]));
        let cut = offers.borrow().clone();
        offers.borrow_mut().truncate(1);
        let mut grid = source.grid.get_cloned();
        grid.select(3, 0);
        source.grid.set(grid);
        chord(&mut first, "v", Some([6; 16]));
        assert_eq!(source.doc.borrow().wb.value(0, 0, 0), Value::Num(1.25));
        assert_eq!(source.doc.borrow().wb.value(0, 3, 0), Value::Num(1.25));

        // A fresh local cut can still move the original cells, once.
        let mut grid = source.grid.get_cloned();
        grid.select(0, 0);
        source.grid.set(grid);
        chord(&mut first, "x", Some([7; 16]));
        assert_ne!(*offers.borrow(), cut);
        let mut grid = source.grid.get_cloned();
        grid.select(4, 0);
        source.grid.set(grid);
        chord(&mut first, "v", Some([8; 16]));
        assert_eq!(source.doc.borrow().wb.value(0, 0, 0), Value::Empty);
        assert_eq!(source.doc.borrow().wb.formula_text(cell(4, 0)).as_deref(), Some("=5/4"));
        assert!(source.doc.borrow().clip.is_none());
    }

    #[test]
    fn threads_can_be_posted_replied_to_edited_and_undone() {
        use crate::data::DataCmd;
        use crate::tabs::TabCmd;
        use officina_grid::comments::{Comment, ThreadMention, ThreadPerson};

        let (doc, _) = Doc::from_csv("t.csv", b"1\n").unwrap();
        let dirty = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let sheets = Sheets::new(doc, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()),
            Mutable::new(Ui::default()), Mutable::with_dirty(0, dirty.clone()), (1200.0, 800.0));
        let mut app = App::new_with_flag(sheets.clone(), dirty);
        let click = |app: &mut App<Sheets>, label: &str| {
            app.tick();
            let at = app.tree().iter().find_map(|(_, node)| match node {
                Node::Text { content, rect, .. } if content == label =>
                    Some(Point::new(rect.x() + rect.w() / 2.0, rect.y() + rect.h() / 2.0)),
                _ => None,
            }).unwrap_or_else(|| panic!("missing button {label}"));
            app.handle_event(Event::PointerDown { at });
            app.handle_event(Event::PointerUp { at });
            sheets.take_cmds();
        };
        let stored = || sheets.doc.borrow().wb.comment(0, 0, 0).cloned().unwrap();
        let set_text = |text: &str| {
            let mut edit = sheets.comment_text.get_cloned();
            edit.set_text(text);
            sheets.comment_text.set(edit);
        };
        sheets.cmds.lock().unwrap().push(Cmd::Data(DataCmd::NewThread));
        sheets.take_cmds();
        assert_eq!(sheets.commenting.get(), Some((0, 0, 0)));
        set_text("🙂 @Bo please check");
        sheets.tab_cmd(TabCmd::CommentDone);
        let root = stored();
        assert_eq!(root.thread()[0].text.as_ref(), "🙂 @Bo please check");
        assert!(root.thread()[0].timestamp.ends_with('Z'));
        assert!(sheets.doc.borrow_mut().undo());
        assert!(sheets.doc.borrow().wb.comment(0, 0, 0).is_none());
        assert!(sheets.doc.borrow_mut().redo());

        // The same editor opens a message imported with a real person reference.
        let mut thread = root.thread().to_vec();
        thread[0].mentions = vec![ThreadMention {
            id: 17, start: 5, end: 8,
            person: Arc::new(ThreadPerson { id: 21, name: "Bo".into(), user_id: "bo@example.invalid".into(), provider_id: "PeoplePicker".into() }),
        }].into();
        let original = Comment::threaded(thread).unwrap();
        sheets.doc.borrow_mut().wb.set_comments(0, vec![(0, 0, Some(original.clone()))]);

        sheets.tab_cmd(TabCmd::Commenting);
        let mut editor = sheets.comment_text.get_cloned();
        editor.set_caret(0, false);
        editor.insert("é ");
        editor.set_caret(9, false);
        editor.delete(false);
        assert!(editor.tracked_ranges().is_empty());
        assert!(editor.undo());
        assert_eq!(editor.tracked_ranges(), &[(0, 8..11)]);
        editor.set_caret(editor.text().len(), false);
        editor.insert(" today");
        sheets.comment_text.set(editor);
        sheets.tab_cmd(TabCmd::CommentDone);
        let edited = stored();
        let message = &edited.thread()[0];
        assert_eq!(message.text.as_ref(), "é 🙂 @Bo please check today");
        assert_eq!(message.id, original.thread()[0].id);
        assert_eq!(message.author, original.thread()[0].author);
        assert_eq!(message.mentions[0].id, 17);
        assert_eq!(message.mentions[0].person, original.thread()[0].mentions[0].person);
        assert_eq!(&message.text[message.mentions[0].start..message.mentions[0].end], "@Bo");
        assert!(sheets.doc.borrow_mut().undo());
        assert_eq!(stored(), original);
        assert!(sheets.doc.borrow_mut().redo());

        sheets.tab_cmd(TabCmd::Commenting);
        click(&mut app, "Reply");
        assert!(sheets.comment_text.get_cloned().text().is_empty());
        set_text("Checked");
        sheets.tab_cmd(TabCmd::CommentDone);
        let replied = stored();
        assert_eq!(replied.thread().len(), 2);
        assert_eq!(replied.thread()[1].parent, Some(message.id));
        assert_eq!(replied.thread()[1].author, message.author);
        assert!(sheets.doc.borrow_mut().undo());
        assert_eq!(stored(), edited);
        assert!(sheets.doc.borrow_mut().redo());

        sheets.tab_cmd(TabCmd::Commenting);
        sheets.tab_cmd(TabCmd::ThreadMessage(1));
        assert_eq!(sheets.comment_text.get_cloned().text(), "Checked");
        set_text("Checked twice");
        sheets.tab_cmd(TabCmd::CommentDone);
        assert_eq!(stored().thread()[1].text.as_ref(), "Checked twice");
        assert_eq!(stored().thread()[1].id, replied.thread()[1].id);
        assert!(sheets.doc.borrow_mut().undo());
        assert_eq!(stored(), replied);
    }

    #[test]
    fn imported_threads_can_be_deleted_and_undone_without_flattening() {
        use crate::data::DataCmd;
        use crate::tabs::TabCmd;
        use officina_grid::comments::{Comment, ThreadMessage, ThreadPerson};

        let (mut doc, _) = Doc::from_csv("t.csv", b"1\n").unwrap();
        let author = Arc::new(ThreadPerson { id: 1, name: "Ana".into(), user_id: "ana".into(), provider_id: "".into() });
        let note = Comment::threaded(vec![
            ThreadMessage { id: 2, parent: None, author: author.clone(), timestamp: "".into(), done: false, text: "Root".into(), mentions: Default::default() },
            ThreadMessage { id: 3, parent: Some(2), author, timestamp: "".into(), done: false, text: "Reply".into(), mentions: Default::default() },
        ]).unwrap();
        doc.wb.set_comments(0, vec![(0, 0, Some(note.clone()))]);
        let sheets = Sheets::new(doc, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()),
            Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        sheets.tab_cmd(TabCmd::Commenting);
        assert_eq!(sheets.commenting.get(), Some((0, 0, 0)));
        assert_eq!(sheets.comment_text.get_cloned().text(), "Root");
        sheets.tab_cmd(TabCmd::CommentCancel);
        assert_eq!(sheets.doc.borrow().wb.comment(0, 0, 0), Some(&note));
        sheets.cmds.lock().unwrap().push(Cmd::Data(DataCmd::DeleteComments));
        sheets.take_cmds();
        assert!(sheets.doc.borrow().wb.comment(0, 0, 0).is_none());
        assert!(sheets.doc.borrow_mut().undo());
        assert_eq!(sheets.doc.borrow().wb.comment(0, 0, 0), Some(&note));
    }

    #[test]
    fn sheets_are_added_switched_renamed_and_tabbed() {
        use crate::tabs::TabCmd;
        let (mut d, _) = Doc::from_csv("t.csv", b"a,1\nb,2\n").unwrap();
        d.wb.recalc();
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        assert!(texts().iter().any(|t| t == "Sheet1"));
        run(Cmd::Data(crate::data::DataCmd::AddSheet));
        assert_eq!((s.doc.borrow().wb.sheet_count(), s.doc.borrow().sheet), (2, 1));
        assert!(s.doc.borrow_mut().type_into(0, 0, "=Sheet1!B2*10"));
        assert_eq!(s.doc.borrow().wb.value(1, 0, 0), officina_grid::Value::Num(20.0));
        assert!(texts().iter().any(|t| t == "Sheet2"));
        // Rename: a taken name is refused and said; a good one follows into formulas.
        run(Cmd::Data(crate::data::DataCmd::RenameSheet));
        assert!(s.renaming.get());
        s.rename_text.set(TextEdit::new("sheet1"));
        run(Cmd::Tab(TabCmd::RenameDone));
        assert!(s.renaming.get());
        assert!(s.doc.borrow().status.contains("exists"), "{}", s.doc.borrow().status);
        s.rename_text.set(TextEdit::new("  Totals "));
        run(Cmd::Tab(TabCmd::RenameDone));
        assert!(!s.renaming.get());
        assert_eq!(s.doc.borrow().wb.sheet_name(1), "Totals");
        // Tab colour: the bar, a colour drawn on the tab, undone with the rest.
        run(Cmd::Data(crate::data::DataCmd::TabColor));
        assert!(s.coloring.get() && texts().iter().any(|t| t == "Tab colour"));
        run(Cmd::Tab(TabCmd::Color(Some(0x2E7D32))));
        assert!(!s.coloring.get());
        assert_eq!(s.doc.borrow().wb.sheet(1).tab_color, Some(0x2E7D32));
        let green = crate::tabs::color(0x2E7D32);
        let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
        assert!(tree.iter().any(|(_, n)| matches!(n, Node::Rect { fill, .. } if *fill == green)));
        run(Cmd::Undo);
        assert_eq!(s.doc.borrow().wb.sheet(1).tab_color, None);
        run(Cmd::Redo);
        run(Cmd::Tab(TabCmd::Go(0)));
        assert_eq!(s.doc.borrow().sheet, 0);
        {
            let mut d = s.doc.borrow_mut();
            assert!(d.type_into(2, 0, "=Totals!A1+1"));
            assert_eq!(d.wb.value(0, 2, 0), officina_grid::Value::Num(21.0));
        }
        run(Cmd::Tab(TabCmd::Go(1)));
        // Undo back past the sheet's making: the view moves to a sheet that's there.
        for _ in 0..5 {
            run(Cmd::Undo);
        }
        assert_eq!(s.doc.borrow().wb.sheet_count(), 1);
        assert_eq!(s.doc.borrow().sheet, 0);
        // Move and delete: Data sheet with a formula reading it.
        run(Cmd::Data(crate::data::DataCmd::AddSheet));
        run(Cmd::Data(crate::data::DataCmd::AddSheet));
        assert_eq!(s.doc.borrow().wb.sheet_count(), 3);
        assert!(s.doc.borrow_mut().type_into(0, 0, "=Sheet1!B1+1"));
        run(Cmd::Data(crate::data::DataCmd::MoveSheetLeft));
        assert_eq!((s.doc.borrow().sheet, s.doc.borrow().wb.sheet_name(1)), (1, "Sheet3".to_string()));
        assert_eq!(s.doc.borrow().wb.value(1, 0, 0), officina_grid::Value::Num(2.0));
        run(Cmd::Tab(TabCmd::Go(0)));
        run(Cmd::Data(crate::data::DataCmd::DeleteSheet));
        assert_eq!(s.doc.borrow().wb.sheet_count(), 2);
        assert_eq!(s.doc.borrow().sheet, 0);
        assert_eq!(s.doc.borrow().wb.value(0, 0, 0), officina_grid::Value::Err(officina_grid::ErrorKind::Ref));
        run(Cmd::Undo);
        assert_eq!(s.doc.borrow().wb.sheet_count(), 3);
        assert_eq!(s.doc.borrow().wb.value(1, 0, 0), officina_grid::Value::Num(2.0));
        run(Cmd::Tab(TabCmd::Go(2)));
        run(Cmd::Data(crate::data::DataCmd::DeleteSheet));
        assert_eq!(s.doc.borrow().sheet, 1, "deleting the last shows the one before");
        // Undo the delete, the move, the formula and both sheets.
        for _ in 0..5 {
            run(Cmd::Undo);
        }
        assert_eq!(s.doc.borrow().wb.sheet_count(), 1);

        // Thousands of sheets: only the tabs that fit are drawn, quickly.
        {
            let mut d = s.doc.borrow_mut();
            for i in 2..=3000 {
                d.wb.add_sheet(&format!("Sheet{i}")).unwrap();
            }
        }
        run(Cmd::Tab(TabCmd::Go(2999)));
        let t0 = std::time::Instant::now();
        let t = texts();
        assert!(t0.elapsed() < std::time::Duration::from_millis(200), "{:?}", t0.elapsed());
        assert!(t.iter().any(|x| x == "Sheet3000"));
        assert!(!t.iter().any(|x| x == "Sheet10"));
        assert!(t.iter().filter(|x| x.starts_with("Sheet")).count() < 30);
    }

    #[test]
    fn the_pivot_field_list_moves_fields() {
        use crate::pivotpane::{Area, Totals};
        let (mut d, _) = Doc::from_csv("t.csv", b"Region,Product,Units\nNorth,Pear,3\nSouth,Fig,10\nNorth,Fig,7\n").unwrap();
        d.wb.recalc();
        crate::data::run(&mut d, CellRange::of((0, 0), (0, 0)), (0, 0), crate::data::DataCmd::InsertPivot);
        let s = Sheets::new(d, Mutable::new(ScrollState::default()), Mutable::new(GridState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1200.0, 800.0));
        let texts = || {
            let (tree, _) = pergola::view::render(&s, pergola::theme::Semantic::LIGHT);
            tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect::<Vec<String>>()
        };
        let run = |c: Cmd| {
            s.cmds.lock().unwrap().push(c);
            s.take_cmds();
        };
        // Away from any pivot it says why.
        let mut g = s.grid.get_cloned();
        g.cursor = (30, 30);
        g.anchor = (30, 30);
        s.grid.set(g);
        run(Cmd::Data(crate::data::DataCmd::PivotFields));
        assert!(!s.pivot_pane.get_cloned().open);
        assert!(s.doc.borrow().status.contains("pivot table"));
        let mut g = s.grid.get_cloned();
        g.cursor = (1, 0);
        g.anchor = (1, 0);
        s.grid.set(g);
        run(Cmd::Data(crate::data::DataCmd::PivotFields));
        assert!(s.pivot_pane.get_cloned().open);
        let t = texts();
        for want in ["“PivotTable1” Fields", "Region", "Product", "Units", "Rows: Region", "Values: Sum of Units", "Sum", "Grand total row", "Done"] {
            assert!(t.iter().any(|x| x == want), "{want} missing from {t:?}");
        }
        // A click on Product's row picks it, and stays off the grid.
        let mut app = App::new(s.clone());
        app.tick();
        let at = pergola::geom::Point::new(1200.0 - PIVOT_W + 40.0, TOOLBAR_H + FORMULA_H + 56.0 + PIVOT_ROW_H + 10.0);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        s.take_cmds();
        assert_eq!(s.pivot_pane.get_cloned().picked, Some(1));
        assert_eq!(s.grid.get_cloned().cursor, (1, 0));
        run(Cmd::Pivot(PivotCmd::To(Some(Area::Columns))));
        assert!(texts().iter().any(|x| x == "Columns: Product"));
        run(Cmd::Pivot(PivotCmd::Agg(0, 2)));
        assert!(texts().iter().any(|x| x == "Values: Average of Units"));
        run(Cmd::Pivot(PivotCmd::Totals(Totals::Subtotals)));
        assert!(!s.doc.borrow().wb.pivots()[0].subtotals);
        // Emptying the pivot is refused in words.
        for f in [0, 1, 2] {
            run(Cmd::Pivot(PivotCmd::Pick(f)));
            run(Cmd::Pivot(PivotCmd::To(None)));
        }
        let e = s.pivot_pane.get_cloned().error;
        assert!(e.contains("at least one field") && texts().contains(&e), "{e}");
        run(Cmd::Pivot(PivotCmd::Close));
        assert!(!s.pivot_pane.get_cloned().open);
        for _ in 0..5 {
            assert!(s.doc.borrow_mut().undo());
        }
        assert_eq!(s.doc.borrow().wb.pivots()[0].cols.len(), 0);
    }

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
