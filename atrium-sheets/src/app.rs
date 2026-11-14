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
use pergola::widgets::{Button, CellRange, GridAction, GridState, GridView, ScrollState};
use pergola::{clipboard, Color, Mutable, TextStyle};

use crate::doc::Doc;
use crate::source::SheetSource;

/// The slot the system picker's sheet goes into.
pub const PICKER_SLOT: u64 = 1;
pub const TOOLBAR_H: f32 = 48.0;
pub const FORMULA_H: f32 = 30.0;
pub const STATUS_H: f32 = 26.0;

/// What the toolbar asks the event loop for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Open,
    Save,
    SaveAs,
    Undo,
    Redo,
    /// Drop the file's macros (kept inert; never run).
    RemoveMacros,
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
}

impl Sheets {
    pub fn new(doc: Doc, scroll: Mutable<ScrollState>, grid: Mutable<GridState>, ui: Mutable<Ui>, rev: Mutable<u64>, size: (f32, f32)) -> Self {
        let doc = Rc::new(RefCell::new(doc));
        let source = Rc::new(SheetSource::new(doc.clone()));
        Self { doc, source, scroll, grid, ui, rev, cmds: Arc::new(Mutex::new(Vec::new())), size: Rc::new(Cell::new(size)) }
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

    fn act(doc: &Rc<RefCell<Doc>>, rev: &Mutable<u64>, a: GridAction) {
        let mut d = doc.borrow_mut();
        match a {
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
        GridView::new(self.source.clone(), self.scroll.clone(), self.grid.clone(), Self::grid_rect(w, h)).on_action(move |a| Self::act(&doc, &rev, a)).emit(ctx);

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
                    Self::act(&self.doc, &self.rev, GridAction::Undo);
                    false
                }
                Cmd::Redo => {
                    Self::act(&self.doc, &self.rev, GridAction::Redo);
                    false
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
