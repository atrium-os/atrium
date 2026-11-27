//! The Page Background bar (Insert ▸ Page Background…): Word's Design tab's
//! page colour, page borders and watermark, as two rows in the paragraph
//! bars' place. Each pick is one undo step; the border takes the Borders
//! bar's style, width and colour.

use officina_text::model::{PageBorders, Watermark};
use pergola::geom::Rect;
use pergola::node::Node;
use pergola::theme::{font, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Button, MenuButton, MenuItem};
use pergola::TextStyle;
use pergola_richtext::Editor;

use crate::app::{Cmd, Write, FIND_H, TOOLBAR_H};
use crate::para::{ParaBar, ParaCmd, SHADES};

/// The Page Background bar's commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookCmd {
    /// The page's colour: 0 none, else [`SHADES`].
    Color(usize),
    /// A box round every page in the border look.
    Box,
    NoBorder,
    /// The box measured from the sheet's edge (or from the text).
    FromEdge,
    /// [`WATERMARKS`]'s words at this place (0: none).
    Words(usize),
    Diagonal,
    /// The watermark's colour: [`MARK_COLORS`] at this place.
    MarkColor(usize),
    Washout,
    /// A picture watermark a step bigger (true) or smaller.
    Scale(bool),
}

/// Word's watermark gallery.
pub const WATERMARKS: [&str; 7] = ["No Watermark", "DRAFT", "CONFIDENTIAL", "SAMPLE", "DO NOT COPY", "ASAP", "URGENT"];
/// Watermark colours: Word's semi-transparent silver first.
pub const MARK_COLORS: [(&str, u32); 5] = [("Silver", 0xC0C0C0), ("Grey", 0x808080), ("Red", 0xFF9999), ("Blue", 0x9DC3E6), ("Green", 0xA9D18E)];
/// A border box's distance from the text, points (Word's default 4 pt from
/// the text, 24 pt from the page's edge).
pub const BOX_FROM_TEXT: u16 = 4;
pub const BOX_FROM_EDGE: u16 = 24;
pub const SCALE_STEP: u16 = 10;

/// Runs page look command `c` on the editor.
pub fn look_cmd(e: &mut Editor, b: &ParaBar, c: LookCmd) {
    let st = e.doc.settings.clone();
    let (text, color, diagonal) = match &st.watermark {
        Some(Watermark::Text { text, color, diagonal, .. }) => (text.to_string(), *color, *diagonal),
        _ => (String::new(), MARK_COLORS[0].1, true),
    };
    match c {
        LookCmd::Color(i) => e.set_page_color(i.checked_sub(1).map(|k| SHADES[k.min(SHADES.len() - 1)].1)),
        LookCmd::Box => {
            let from_page = st.page_borders.is_some_and(|p| p.from_page);
            let side = Some(officina_text::model::Border { space: if from_page { BOX_FROM_EDGE } else { BOX_FROM_TEXT }, ..b.look });
            e.set_page_borders(Some(PageBorders { top: side, bottom: side, start: side, end: side, from_page }));
        }
        LookCmd::NoBorder => e.set_page_borders(None),
        LookCmd::FromEdge => {
            if let Some(mut p) = st.page_borders {
                p.from_page = !p.from_page;
                let space = if p.from_page { BOX_FROM_EDGE } else { BOX_FROM_TEXT };
                for s in [&mut p.top, &mut p.bottom, &mut p.start, &mut p.end].into_iter().flatten() {
                    s.space = space;
                }
                e.set_page_borders(Some(p));
            }
        }
        LookCmd::Words(i) => match i {
            0 => e.remove_watermark(),
            _ => e.set_text_watermark(WATERMARKS[i.min(WATERMARKS.len() - 1)], color, diagonal),
        },
        LookCmd::Diagonal if !text.is_empty() => e.set_text_watermark(&text, color, !diagonal),
        LookCmd::MarkColor(i) if !text.is_empty() => e.set_text_watermark(&text, MARK_COLORS[i.min(MARK_COLORS.len() - 1)].1, diagonal),
        LookCmd::Washout | LookCmd::Scale(_) => {
            if let Some(Watermark::Picture { image, scale, washout }) = st.watermark {
                let mut s = st.clone();
                let (scale, washout) = match c {
                    LookCmd::Washout => (scale, !washout),
                    LookCmd::Scale(up) => (if up { scale.saturating_add(SCALE_STEP).min(500) } else { scale.saturating_sub(SCALE_STEP).max(SCALE_STEP) }, washout),
                    _ => (scale, washout),
                };
                s.watermark = Some(Watermark::Picture { image, scale, washout });
                e.set_settings(s);
            }
        }
        LookCmd::Diagonal | LookCmd::MarkColor(_) => {}
    }
}

impl Write {
    /// Draws the Page Background bar's two rows.
    pub(crate) fn page_look_rows(&self, ctx: &mut Ctx, w: f32) {
        let t = ctx.theme;
        let st = |size, weight, color| TextStyle { family: font::SANS.into(), size, weight, color };
        let (ry, ry2) = (TOOLBAR_H + 8.0, TOOLBAR_H + FIND_H + 8.0);
        let push = |c: Cmd| {
            let cmds = self.cmds.clone();
            move || cmds.lock().unwrap().push(c)
        };
        let menu_push = |f: fn(usize) -> LookCmd| {
            let cmds = self.cmds.clone();
            move |i: usize| cmds.lock().unwrap().push(Cmd::Para(ParaCmd::Look(f(i))))
        };
        let lbl = |ctx: &mut Ctx, x: f32, y: f32, wd: f32, text: &str| ctx.add(Node::Text { rect: Rect::new(x, y + 6.0, wd, 20.0), content: text.into(), style: st(type_size::SM, Weight::Regular, t.text_secondary()) });
        let on = |b: bool, l: &str| if b { format!("{l} ✓") } else { l.to_string() };
        let s = self.doc.borrow().ed.borrow().doc.settings.clone();
        let look = |c: LookCmd| Cmd::Para(ParaCmd::Look(c));
        // Row one: the page's colour and its border.
        let mut x = 12.0;
        lbl(ctx, x, ry, 90.0, "Page Colour");
        x += 92.0;
        let items: Vec<MenuItem> = std::iter::once(MenuItem::new("No Colour").radio(s.page_color.is_none())).chain(SHADES.iter().map(|(n, c)| MenuItem::new(*n).radio(s.page_color == Some(*c)))).collect();
        let now = s.page_color.map_or("No Colour", |c| SHADES.iter().find(|x| x.1 == c).map_or("Other", |x| x.0));
        MenuButton::new(now.to_string(), self.para_menus.page_color.clone(), items, menu_push(LookCmd::Color)).at(x, ry).width(130.0).render(ctx);
        x += 146.0;
        lbl(ctx, x, ry, 96.0, "Page Border");
        x += 98.0;
        let boxed = s.page_borders.is_some();
        for (label, c, wd) in [(on(boxed, "Box"), LookCmd::Box, 64.0), ("None".to_string(), LookCmd::NoBorder, 56.0), (on(s.page_borders.is_some_and(|p| p.from_page), "From Page Edge"), LookCmd::FromEdge, 140.0)] {
            Button::secondary(label).at(x, ry).width(wd).on_click(push(look(c))).render(ctx);
            x += wd + 4.0;
        }
        if x + 260.0 < w - 84.0 {
            lbl(ctx, x + 8.0, ry, 260.0, "Style, width and colour: the Borders bar's.");
        }
        // Row two: the watermark.
        let mut x = 12.0;
        lbl(ctx, x, ry2, 84.0, "Watermark");
        x += 86.0;
        let words = match &s.watermark {
            Some(Watermark::Text { text, .. }) => Some(text.to_string()),
            _ => None,
        };
        let now = match &s.watermark {
            None => "No Watermark".to_string(),
            Some(Watermark::Picture { .. }) => "Picture".to_string(),
            Some(Watermark::Text { text, .. }) => text.to_string(),
        };
        let items: Vec<MenuItem> = WATERMARKS.iter().enumerate().map(|(i, n)| MenuItem::new(*n).radio(if i == 0 { s.watermark.is_none() } else { words.as_deref() == Some(*n) })).collect();
        MenuButton::new(now, self.para_menus.watermark.clone(), items, menu_push(LookCmd::Words)).at(x, ry2).width(150.0).render(ctx);
        x += 158.0;
        Button::secondary("Picture…").at(x, ry2).width(84.0).on_click(push(Cmd::WatermarkPicture)).render(ctx);
        x += 92.0;
        match &s.watermark {
            Some(Watermark::Text { color, diagonal, .. }) => {
                Button::secondary(on(*diagonal, "Diagonal")).at(x, ry2).width(96.0).on_click(push(look(LookCmd::Diagonal))).render(ctx);
                x += 104.0;
                let items: Vec<MenuItem> = MARK_COLORS.iter().map(|(n, c)| MenuItem::new(*n).radio(c == color)).collect();
                let now = MARK_COLORS.iter().find(|m| m.1 == *color).map_or("Colour", |m| m.0);
                MenuButton::new(now.to_string(), self.para_menus.mark_color.clone(), items, menu_push(LookCmd::MarkColor)).at(x, ry2).width(100.0).render(ctx);
            }
            Some(Watermark::Picture { scale, washout, .. }) => {
                Button::secondary(on(*washout, "Washout")).at(x, ry2).width(96.0).on_click(push(look(LookCmd::Washout))).render(ctx);
                x += 104.0;
                lbl(ctx, x, ry2, 44.0, "Size");
                x += 46.0;
                lbl(ctx, x, ry2, 52.0, &format!("{scale}%"));
                x += 54.0;
                Button::secondary("−").at(x, ry2).width(28.0).on_click(push(look(LookCmd::Scale(false)))).render(ctx);
                x += 30.0;
                Button::secondary("+").at(x, ry2).width(28.0).on_click(push(look(LookCmd::Scale(true)))).render(ctx);
            }
            None => {}
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
    fn lc(w: &Write, c: LookCmd) {
        run(w, Cmd::Para(ParaCmd::Look(c)));
    }

    /// The bar opens from the Insert menu and sets the page's colour, its
    /// border and a watermark, each undone in one step.
    #[test]
    fn page_background_bar_opens_and_works() {
        let w = write("Some text.");
        let i = INSERT.iter().position(|l| *l == "Page Background…").unwrap();
        run(&w, Cmd::insert(i));
        assert_eq!(w.para.get_cloned().open, Some(ParaKind::Page));
        assert!(texts(&w).iter().any(|t| t == "Page Colour"));
        let ed = w.doc.borrow().ed.clone();
        lc(&w, LookCmd::Color(3));
        assert_eq!(ed.borrow().doc.settings.page_color, Some(SHADES[2].1));
        lc(&w, LookCmd::Box);
        let pb = ed.borrow().doc.settings.page_borders.unwrap();
        assert!(pb.top.is_some_and(|b| b.space == BOX_FROM_TEXT) && !pb.from_page);
        lc(&w, LookCmd::FromEdge);
        let pb = ed.borrow().doc.settings.page_borders.unwrap();
        assert!(pb.from_page && pb.end.is_some_and(|b| b.space == BOX_FROM_EDGE));
        lc(&w, LookCmd::Words(1));
        assert!(matches!(&ed.borrow().doc.settings.watermark, Some(Watermark::Text { text, diagonal: true, .. }) if &**text == "DRAFT"));
        lc(&w, LookCmd::Diagonal);
        lc(&w, LookCmd::MarkColor(2));
        assert!(matches!(&ed.borrow().doc.settings.watermark, Some(Watermark::Text { diagonal: false, color: 0xFF9999, .. })));
        assert!(texts(&w).iter().any(|t| t == "Red"));
        ed.borrow_mut().undo();
        assert!(matches!(&ed.borrow().doc.settings.watermark, Some(Watermark::Text { diagonal: false, color: 0xC0C0C0, .. })));
        lc(&w, LookCmd::Words(0));
        lc(&w, LookCmd::NoBorder);
        assert!(ed.borrow().doc.settings.watermark.is_none() && ed.borrow().doc.settings.page_borders.is_none());
        // A picture watermark: washed out by default, sized in steps.
        assert!(ed.borrow_mut().set_picture_watermark(officina_text::model::ImageData { bytes: std::sync::Arc::from(vec![1u8; 8]), width: 4, height: 2 }, 100, true));
        lc(&w, LookCmd::Scale(false));
        lc(&w, LookCmd::Washout);
        assert!(matches!(ed.borrow().doc.settings.watermark, Some(Watermark::Picture { scale: 90, washout: false, .. })));
        assert!(texts(&w).iter().any(|t| t == "90%"));
    }
}
