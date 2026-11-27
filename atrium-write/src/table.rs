//! The Table Tools bar (Insert ▸ Table Tools…): Word's Table Design and
//! Layout tabs as two rows under the toolbar, in the paragraph bars'
//! place. Each pick is one undo step on the caret's table (or the
//! selected block of its cells).

use officina_text::model::{builtin_table_styles, HAlign, VAlign, LOOK_BANDED_COLUMNS, LOOK_BANDED_ROWS, LOOK_FIRST_COLUMN, LOOK_HEADER, LOOK_LAST_COLUMN, LOOK_TOTAL};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Button, MenuButton, MenuItem};
use pergola::TextStyle;
use pergola_richtext::{CellEdges, Editor};

use crate::app::{Cmd, Write, FIND_H, TOOLBAR_H};
use crate::para::{inches, ParaBar, ParaCmd, SHADES};

/// The Table Tools bar's commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableCmd {
    RowAbove,
    RowBelow,
    ColumnLeft,
    ColumnRight,
    DeleteRow,
    DeleteColumn,
    DeleteTable,
    Merge,
    Split,
    /// The gallery's style at this place (0: none).
    Style(usize),
    /// A `LOOK_*` bit on or off.
    Look(u8),
    /// Shading of the selected cells: 0 none, else [`SHADES`].
    Shading(usize),
    /// [`VALIGNS`]'s alignment at this place.
    VAlign(usize),
    /// [`EDGES`]'s edges at this place, in the paragraph bars' border look.
    Borders(usize),
    /// The selected rows' height a step more (true) or less.
    Height(bool),
    /// The height exact or at least.
    Exact,
    Header,
    CantSplit,
    DistributeColumns,
    DistributeRows,
    FitContents,
    FitWindow,
    /// Sort A to Z (false) or Z to A.
    Sort(bool),
    ToText,
    ToTable,
    Sum,
    /// [`ALIGNS`]'s place across the page at this place.
    Align(usize),
}

pub const VALIGNS: [(&str, VAlign); 3] = [("Top", VAlign::Top), ("Centre", VAlign::Center), ("Bottom", VAlign::Bottom)];
pub const EDGES: [(&str, CellEdges); 6] = [("All Borders", CellEdges::All), ("Outside", CellEdges::Outside), ("Inside", CellEdges::Inside), ("Top", CellEdges::Top), ("Bottom", CellEdges::Bottom), ("No Border", CellEdges::None)];
pub const ALIGNS: [(&str, HAlign); 3] = [("Table Left", HAlign::Start), ("Table Centre", HAlign::Center), ("Table Right", HAlign::End)];
/// The style options the bar toggles: (label, bit).
pub const LOOKS: [(&str, u8); 6] = [("Header Row", LOOK_HEADER), ("Total Row", LOOK_TOTAL), ("Banded Rows", LOOK_BANDED_ROWS), ("First Column", LOOK_FIRST_COLUMN), ("Last Column", LOOK_LAST_COLUMN), ("Banded Columns", LOOK_BANDED_COLUMNS)];
/// A row height step, twips (a tenth of an inch).
pub const HEIGHT_STEP: u32 = 144;

/// The gallery: "No Style", then the built-in styles.
pub fn style_names() -> Vec<&'static str> {
    std::iter::once("No Style").chain(builtin_table_styles().into_iter().map(|s| s.0)).collect()
}

/// Runs table command `c` on the editor.
pub fn table_cmd(e: &mut Editor, b: &ParaBar, c: TableCmd) {
    let row = e.doc.cell(e.caret.block).map(|c| c.row_look).unwrap_or_default();
    match c {
        TableCmd::RowAbove => drop(e.insert_row_above()),
        TableCmd::RowBelow => drop(e.insert_row_below()),
        TableCmd::ColumnLeft => drop(e.insert_column_left()),
        TableCmd::ColumnRight => drop(e.insert_column_right()),
        TableCmd::DeleteRow => drop(e.delete_row()),
        TableCmd::DeleteColumn => drop(e.delete_column()),
        TableCmd::DeleteTable => drop(e.delete_table()),
        TableCmd::Merge => drop(e.merge_cells()),
        TableCmd::Split => drop(e.split_cell()),
        TableCmd::Style(i) => {
            let names = style_names();
            let name = if i == 0 { None } else { names.get(i).copied() };
            e.set_table_style(name);
        }
        TableCmd::Look(bit) => {
            if let Some(t) = e.table_look_at_caret() {
                e.set_table_look_bits(t.look ^ bit);
            }
        }
        TableCmd::Shading(i) => drop(e.set_cell_shading(i.checked_sub(1).map(|k| SHADES[k.min(SHADES.len() - 1)].1))),
        TableCmd::VAlign(i) => drop(e.set_cell_valign(VALIGNS[i.min(VALIGNS.len() - 1)].1)),
        TableCmd::Borders(i) => {
            let edges = EDGES[i.min(EDGES.len() - 1)].1;
            e.set_cell_borders(edges, (edges != CellEdges::None).then_some(b.look));
        }
        TableCmd::Height(up) => {
            let h = row.height as u32;
            let h = if up { h / HEIGHT_STEP * HEIGHT_STEP + HEIGHT_STEP } else { h.saturating_sub(1) / HEIGHT_STEP * HEIGHT_STEP };
            e.set_row_height(h, row.exact);
        }
        TableCmd::Exact => drop(e.set_row_height(row.height as u32, !row.exact)),
        TableCmd::Header => drop(e.set_header_rows(!row.header)),
        TableCmd::CantSplit => drop(e.set_cant_split(!row.cant_split)),
        TableCmd::DistributeColumns => drop(e.distribute_columns()),
        TableCmd::DistributeRows => drop(e.distribute_rows()),
        TableCmd::FitContents => drop(e.autofit_contents()),
        TableCmd::FitWindow => drop(e.autofit_window()),
        TableCmd::Sort(down) => drop(e.sort_rows(down)),
        TableCmd::ToText => drop(e.table_to_text()),
        TableCmd::ToTable => drop(e.text_to_table()),
        TableCmd::Sum => drop(e.insert_sum_above()),
        TableCmd::Align(i) => drop(e.set_table_align(ALIGNS[i.min(ALIGNS.len() - 1)].1)),
    }
}

impl Write {
    /// Draws the Table Tools bar's two rows.
    pub(crate) fn table_rows(&self, ctx: &mut Ctx, w: f32) {
        let t = ctx.theme;
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        let (ry, ry2) = (TOOLBAR_H + 8.0, TOOLBAR_H + FIND_H + 8.0);
        let push = |c: TableCmd| {
            let cmds = self.cmds.clone();
            move || cmds.lock().unwrap().push(Cmd::Para(ParaCmd::Table(c)))
        };
        let menu_push = |f: fn(usize) -> TableCmd| {
            let cmds = self.cmds.clone();
            move |i: usize| cmds.lock().unwrap().push(Cmd::Para(ParaCmd::Table(f(i))))
        };
        let lbl = |ctx: &mut Ctx, x: f32, y: f32, wd: f32, text: &str| ctx.add(Node::Text { rect: Rect::new(x, y + 6.0, wd, 20.0), content: text.into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
        let on = |b: bool, l: &str| if b { format!("{l} ✓") } else { l.to_string() };
        let ed = self.doc.borrow().ed.clone();
        let (cell, look) = {
            let e = ed.borrow();
            (e.doc.cell(e.caret.block), e.table_look_at_caret())
        };
        let (Some(cell), Some(look)) = (cell, look) else {
            lbl(ctx, 12.0, ry, 420.0, "Put the caret in a table, or select paragraphs with tabs to make one.");
            Button::secondary("Convert Text to Table").at(12.0, ry2).width(180.0).on_click(push(TableCmd::ToTable)).render(ctx);
            return;
        };
        let buttons = |ctx: &mut Ctx, x: &mut f32, y: f32, list: &[(String, TableCmd, f32)]| {
            for (label, c, wd) in list {
                if *x + wd > w - 84.0 {
                    break;
                }
                Button::secondary(label.clone()).at(*x, y).width(*wd).on_click(push(*c)).render(ctx);
                *x += wd + 4.0;
            }
        };
        // Row one: rows and columns, merging, the style and its options.
        let mut x = 12.0;
        let s = |l: &str| l.to_string();
        buttons(ctx, &mut x, ry, &[(s("Row Above"), TableCmd::RowAbove, 88.0), (s("Row Below"), TableCmd::RowBelow, 88.0), (s("Column Left"), TableCmd::ColumnLeft, 96.0), (s("Column Right"), TableCmd::ColumnRight, 104.0), (s("Delete Row"), TableCmd::DeleteRow, 92.0), (s("Delete Column"), TableCmd::DeleteColumn, 112.0), (s("Delete Table"), TableCmd::DeleteTable, 100.0), (s("Merge"), TableCmd::Merge, 64.0), (s("Split"), TableCmd::Split, 56.0)]);
        x += 8.0;
        let names = style_names();
        let now = look.style.as_deref().unwrap_or("No Style").to_string();
        let items: Vec<MenuItem> = names.iter().map(|n| MenuItem::new(*n).radio(*n == now)).collect();
        if x + 140.0 < w - 84.0 {
            MenuButton::new(now, self.para_menus.table_style.clone(), items, menu_push(TableCmd::Style)).at(x, ry).width(140.0).render(ctx);
            x += 148.0;
        }
        let looks: Vec<(String, TableCmd, f32)> = LOOKS.iter().map(|(l, bit)| (on(look.look & bit != 0, l), TableCmd::Look(*bit), 8.0 * l.len() as f32 + 28.0)).collect();
        buttons(ctx, &mut x, ry, &looks);
        // Row two: the cells' look, the rows' size and the whole table.
        let mut x = 12.0;
        let shade_now = cell.look.shading.flatten();
        let items: Vec<MenuItem> = std::iter::once(MenuItem::new("No Colour").radio(shade_now.is_none())).chain(SHADES.iter().map(|(n, c)| MenuItem::new(*n).radio(shade_now == Some(*c)))).collect();
        let now = shade_now.map_or("No Shading", |c| SHADES.iter().find(|x| x.1 == c).map_or("Shaded", |x| x.0));
        MenuButton::new(now.to_string(), self.para_menus.cell_shade.clone(), items, menu_push(TableCmd::Shading)).at(x, ry2).width(124.0).render(ctx);
        x += 132.0;
        let items: Vec<MenuItem> = VALIGNS.iter().map(|v| MenuItem::new(v.0).radio(v.1 == cell.look.valign)).collect();
        let now = VALIGNS.iter().find(|v| v.1 == cell.look.valign).map_or("Top", |v| v.0);
        MenuButton::new(now.to_string(), self.para_menus.cell_valign.clone(), items, menu_push(TableCmd::VAlign)).at(x, ry2).width(84.0).render(ctx);
        x += 92.0;
        let items: Vec<MenuItem> = EDGES.iter().map(|v| MenuItem::new(v.0)).collect();
        MenuButton::new("Borders".to_string(), self.para_menus.cell_edges.clone(), items, menu_push(TableCmd::Borders)).at(x, ry2).width(92.0).render(ctx);
        x += 100.0;
        let rl = cell.row_look;
        let height = if rl.height == 0 { "Auto".to_string() } else { inches(rl.height as i32) };
        lbl(ctx, x, ry2, 52.0, "Height");
        x += 54.0;
        lbl(ctx, x, ry2, 44.0, &height);
        x += 46.0;
        buttons(ctx, &mut x, ry2, &[(s("−"), TableCmd::Height(false), 28.0), (s("+"), TableCmd::Height(true), 28.0), (on(rl.exact, "Exactly"), TableCmd::Exact, 80.0), (on(rl.header, "Repeat Header"), TableCmd::Header, 124.0), (on(rl.cant_split, "Don't Split"), TableCmd::CantSplit, 100.0), (s("Even Columns"), TableCmd::DistributeColumns, 108.0), (s("Even Rows"), TableCmd::DistributeRows, 88.0), (s("Fit Contents"), TableCmd::FitContents, 100.0), (s("Fit Window"), TableCmd::FitWindow, 92.0), (s("Sort A–Z"), TableCmd::Sort(false), 80.0), (s("Sort Z–A"), TableCmd::Sort(true), 80.0), (s("=SUM(ABOVE)"), TableCmd::Sum, 112.0), (s("To Text"), TableCmd::ToText, 72.0)]);
        if x + 120.0 < w - 84.0 {
            let items: Vec<MenuItem> = ALIGNS.iter().map(|a| MenuItem::new(a.0).radio(a.1 == look.align)).collect();
            let now = ALIGNS.iter().find(|a| a.1 == look.align).map_or("Table Left", |a| a.0);
            MenuButton::new(now.to_string(), self.para_menus.table_align.clone(), items, menu_push(TableCmd::Align)).at(x, ry2).width(120.0).render(ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Ui, INSERT};
    use crate::doc::Doc;
    use crate::para::ParaKind;
    use pergola::interaction::Interactions;
    use pergola::node::NodeTree;
    use pergola::theme::Semantic;
    use pergola::widgets::ScrollState;
    use pergola::Mutable;
    use pergola_richtext::Pos;

    fn write(text: &str) -> Write {
        let doc = Doc::from_text("t.txt", text.as_bytes()).unwrap();
        Write::new(doc, Mutable::new(ScrollState::default()), Mutable::new(Ui::default()), Mutable::new(0), (1600.0, 720.0))
    }
    fn run(w: &Write, c: Cmd) {
        w.cmds.lock().unwrap().push(c);
        w.take_cmds();
    }
    fn texts(w: &Write) -> Vec<String> {
        let (mut tree, mut inter) = (NodeTree::new(), Interactions::new());
        let mut ctx = Ctx::new(&mut tree, &mut inter, Semantic::LIGHT);
        w.render(&mut ctx);
        tree.iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.to_string()) } else { None }).collect()
    }
    fn tc(w: &Write, c: TableCmd) {
        run(w, Cmd::Para(ParaCmd::Table(c)));
    }

    /// The bar opens from the Insert menu; outside a table it offers to
    /// convert text; in one, its commands change the table.
    #[test]
    fn table_tools_bar_opens_and_works() {
        let w = write("a\tb\tc\n1\t2\t3\n4\t5\t6\nafter");
        let i = INSERT.iter().position(|l| *l == "Table Tools…").unwrap();
        run(&w, Cmd::insert(i));
        assert_eq!(w.para.get_cloned().open, Some(ParaKind::Table));
        assert!(texts(&w).iter().any(|t| t == "Convert Text to Table"));
        let ed = w.doc.borrow().ed.clone();
        ed.borrow_mut().set_caret(Pos::new(0, 0), false);
        ed.borrow_mut().set_caret(Pos::new(2, 1), true);
        tc(&w, TableCmd::ToTable);
        assert_eq!(ed.borrow().doc.cell(4).map(|c| (c.row, c.col)), Some((1, 1)));
        let t = texts(&w);
        assert!(t.iter().any(|s| s == "Row Above") && t.iter().any(|s| s == "No Style") && t.iter().any(|s| s == "Height"), "{t:?}");
        // A style from the gallery, its options, shading and a header row.
        let blue = style_names().iter().position(|n| *n == "Blue Table").unwrap();
        tc(&w, TableCmd::Style(blue));
        assert_eq!(ed.borrow().table_look_at_caret().unwrap().style.as_deref(), Some("Blue Table"));
        tc(&w, TableCmd::Look(LOOK_TOTAL));
        assert!(ed.borrow().table_look_at_caret().unwrap().look & LOOK_TOTAL != 0);
        tc(&w, TableCmd::Shading(1));
        assert_eq!(ed.borrow().doc.cell(ed.borrow().caret.block).unwrap().look.shading, Some(Some(SHADES[0].1)));
        ed.borrow_mut().set_caret(Pos::new(0, 0), false);
        tc(&w, TableCmd::Header);
        tc(&w, TableCmd::Height(true));
        tc(&w, TableCmd::Height(true));
        let rl = ed.borrow().doc.cell(0).unwrap().row_look;
        assert!(rl.header && rl.height == 2 * HEIGHT_STEP as u16);
        assert!(texts(&w).iter().any(|s| s == "0.20\""));
        // Sort under the header, then rows and columns.
        ed.borrow_mut().set_caret(Pos::new(3, 0), false);
        tc(&w, TableCmd::Sort(true));
        assert_eq!(ed.borrow().doc.para(3).unwrap().text, "4");
        let n = ed.borrow().doc.blocks.len();
        tc(&w, TableCmd::RowBelow);
        tc(&w, TableCmd::ColumnRight);
        assert_eq!(ed.borrow().doc.blocks.len(), n + 3 + 4);
        tc(&w, TableCmd::DeleteTable);
        assert!(ed.borrow().doc.cell(0).is_none());
        tc(&w, TableCmd::Merge);
        run(&w, Cmd::Para(ParaCmd::Close));
        assert!(!w.para_open());
    }
}
