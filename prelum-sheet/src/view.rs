//! The print sheet drawn (printing.md §5.2): previews on the left, the
//! printer and its options on the right, Cancel and Print at the bottom.
//! Everything is placed at fixed positions in the sheet's fixed size, so
//! the Print button (the press frescod counts) never moves under a pointer.

use pergola::geom::Rect;
use pergola::node::{Image, Node};
use pergola::text_edit::TextEdit;
use pergola::theme::{font, radius, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::Segmented;
use pergola::{Button, Color, Mutable, TextField, TextStyle};
use prelum_ipp_codec::ops::Sides;

use crate::sheet::{Dest, Sheet, Tone, MAX_COPIES};

/// The sheet's size in points: limen's catalogue row for `print`.
pub const W: f32 = 760.0;
pub const H: f32 = 540.0;
const PAD: f32 = 20.0;
const LEFT_W: f32 = 300.0;
const RIGHT_X: f32 = PAD + LEFT_W + PAD;
const RIGHT_W: f32 = W - RIGHT_X - PAD;
const LABEL_W: f32 = 104.0;
const ROW: f32 = 28.0;
const MAX_ROWS: usize = 5;
const OPT: f32 = 36.0;

/// Where Print is: the guard frescod watches (printing.md §5.2).
pub fn print_button() -> Rect {
    Rect::new(W - PAD - 120.0, H - PAD - 32.0, 120.0, 32.0)
}

fn cancel_button() -> Rect {
    Rect::new(W - PAD - 120.0 - 12.0 - 104.0, H - PAD - 32.0, 104.0, 32.0)
}

/// What the person did that the sheet's loop acts on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Asked {
    /// A press on Print, with frescod's token for it (none: synthetic).
    pub print: Option<Option<fresco_protocol::activation::Token>>,
    pub cancel: bool,
}

#[derive(Clone)]
pub struct SheetView {
    pub sheet: Mutable<Sheet>,
    pub range: Mutable<TextEdit>,
    pub asked: Mutable<Asked>,
    /// The previews, ready to draw, by sheet number.
    pub pictures: Mutable<Vec<(u32, Image)>>,
}

fn style(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

fn edit(s: &Mutable<Sheet>, f: impl FnOnce(&mut Sheet)) {
    let mut v = s.get_cloned();
    f(&mut v);
    s.set(v);
}

impl SheetView {
    pub fn new(sheet: Mutable<Sheet>) -> SheetView {
        SheetView {
            range: sheet.sibling(TextEdit::new("")),
            asked: sheet.sibling(Asked::default()),
            pictures: sheet.sibling(Vec::new()),
            sheet,
        }
    }

    fn text(&self, ctx: &mut Ctx, x: f32, y: f32, w: f32, s: &str, st: TextStyle) {
        let m = Node::measure_text(s, &st);
        ctx.add(Node::Text { rect: Rect::new(x, y, w.min(m.w.max(1.0)), m.h), content: s.into(), style: st });
    }

    /// A segmented option: `labels`, the chosen one, and what a choice does.
    fn choice(&self, ctx: &mut Ctx, y: f32, label: &str, labels: &[&str], chosen: usize, set: impl Fn(&mut Sheet, usize) + 'static) {
        let t = ctx.theme;
        self.text(ctx, RIGHT_X, y + 8.0, LABEL_W, label, style(type_size::SM, Weight::Regular, t.text_secondary()));
        let s = self.sheet.clone();
        Segmented::new(self.sheet.sibling(chosen), labels.iter().copied())
            .at(RIGHT_X + LABEL_W, y)
            .width(RIGHT_W - LABEL_W)
            .on_change(move |i| edit(&s, |v| set(v, i)))
            .render(ctx);
    }

    fn preview(&self, ctx: &mut Ctx, s: &Sheet) {
        let t = ctx.theme;
        let area = Rect::new(PAD, PAD, LEFT_W, H - 2.0 * PAD - 44.0);
        ctx.add(Node::Rect { rect: area, fill: t.bg_surface(), radius: radius::MD });
        let pics = self.pictures.get_cloned();
        match pics.iter().find(|(n, _)| *n == s.showing) {
            Some((_, img)) => {
                let (iw, ih) = (img.data().width as f32, img.data().height as f32);
                let k = ((area.w() - 24.0) / iw).min((area.h() - 24.0) / ih);
                let (w, h) = (iw * k, ih * k);
                let r = Rect::new(area.x() + (area.w() - w) / 2.0, area.y() + (area.h() - h) / 2.0, w, h);
                // The paper's edge, then the page.
                ctx.add(Node::Rect { rect: Rect::new(r.x() - 1.0, r.y() - 1.0, r.w() + 2.0, r.h() + 2.0), fill: t.border_default(), radius: 0.0 });
                ctx.add(Node::Image { rect: r, image: img.clone() });
            }
            None => {
                let note = s.preview_note.clone().unwrap_or_else(|| "Preparing a preview…".into());
                self.text(ctx, area.x() + 16.0, area.y() + area.h() / 2.0 - 8.0, area.w() - 32.0, &note, style(type_size::SM, Weight::Regular, t.text_tertiary()));
            }
        }
        let y = H - PAD - 32.0;
        let st = self.sheet.clone();
        Button::secondary("Previous").at(PAD, y).width(80.0).on_click(move || edit(&st, |v| v.step(-1))).render(ctx);
        let st = self.sheet.clone();
        Button::secondary("Next").at(PAD + LEFT_W - 80.0, y).width(80.0).on_click(move || edit(&st, |v| v.step(1))).render(ctx);
        let words = if s.sheets == 0 { String::new() } else { format!("Sheet {} of {}", s.showing, s.sheets) };
        let m = Node::measure_text(&words, &style(type_size::SM, Weight::Regular, t.text_secondary()));
        self.text(ctx, PAD + (LEFT_W - m.w) / 2.0, y + 8.0, LEFT_W - 160.0, &words, style(type_size::SM, Weight::Regular, t.text_secondary()));
    }

    fn printers(&self, ctx: &mut Ctx, s: &Sheet, mut y: f32) -> f32 {
        let t = ctx.theme;
        if !s.listed {
            self.text(ctx, RIGHT_X, y + 6.0, RIGHT_W, "Looking for your printers…", style(type_size::SM, Weight::Regular, t.text_tertiary()));
            y += ROW;
        }
        // Keep the chosen row in view when the list is longer than the room.
        let skip = s.chosen.saturating_sub(MAX_ROWS - 1);
        for (i, d) in s.dests.iter().enumerate().skip(skip).take(MAX_ROWS) {
            let on = i == s.chosen;
            let r = Rect::new(RIGHT_X, y, RIGHT_W, ROW - 2.0);
            // The row holds its hover and its words, so a press on either
            // is the row's.
            let row = ctx.push(Node::Rect { rect: r, fill: if on { t.selection_bg() } else { Color::TRANSPARENT }, radius: radius::SM });
            if ctx.is_hovered(row) && !on {
                ctx.add(Node::Rect { rect: r, fill: t.state_hover(), radius: radius::SM });
            }
            let (name, mark) = match d {
                Dest::Saved(p) => (p.name.clone(), if p.tls { "" } else { "Unencrypted" }),
                Dest::Nearby(n) => (n.name.clone(), "Nearby"),
                Dest::SavePdf => ("Save as PDF".into(), ""),
            };
            self.text(ctx, r.x() + 10.0, r.y() + 6.0, r.w() - 120.0, &name, style(type_size::SM, if on { Weight::Semibold } else { Weight::Regular }, t.text_primary()));
            if !mark.is_empty() {
                let st = style(type_size::XS, Weight::Regular, t.text_tertiary());
                let m = Node::measure_text(mark, &st);
                self.text(ctx, r.x() + r.w() - m.w - 10.0, r.y() + 7.0, m.w, mark, st);
            }
            ctx.pop();
            let st = self.sheet.clone();
            ctx.on_click(row, move || edit(&st, |v| v.choose(i)));
            y += ROW;
        }
        let (line, tone) = s.status();
        let color = match tone {
            Tone::Quiet => t.text_secondary(),
            Tone::Warning => t.warning_fg(),
            Tone::Error => t.danger_fg(),
        };
        self.text(ctx, RIGHT_X, y + 4.0, RIGHT_W, &line, style(type_size::XS, Weight::Regular, color));
        y + 24.0
    }
}

impl View for SheetView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let s = self.sheet.get_cloned();
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, W, H), fill: t.bg_canvas(), radius: 0.0 });
        self.preview(ctx, &s);

        let head = if s.title.is_empty() { "Print".to_string() } else { format!("Print “{}”", s.title) };
        self.text(ctx, RIGHT_X, PAD, RIGHT_W, &head, style(type_size::LG, Weight::Semibold, t.text_primary()));
        let mut y = self.printers(ctx, &s, PAD + 34.0);

        if s.offers_copies() {
            self.text(ctx, RIGHT_X, y + 8.0, LABEL_W, "Copies", style(type_size::SM, Weight::Regular, t.text_secondary()));
            let st = self.sheet.clone();
            Button::secondary("−").at(RIGHT_X + LABEL_W, y).width(32.0).on_click(move || edit(&st, |v| v.copies = v.copies.saturating_sub(1).max(1))).render(ctx);
            self.text(ctx, RIGHT_X + LABEL_W + 44.0, y + 7.0, 40.0, &s.copies.to_string(), style(type_size::SM, Weight::Semibold, t.text_primary()));
            let st = self.sheet.clone();
            Button::secondary("+").at(RIGHT_X + LABEL_W + 76.0, y).width(32.0).on_click(move || edit(&st, |v| v.copies = (v.copies + 1).min(MAX_COPIES))).render(ctx);
            y += OPT;
        }

        self.choice(ctx, y, "Pages", &["All", "Some"], if s.all_pages { 0 } else { 1 }, |v, i| v.all_pages = i == 0);
        if !s.all_pages {
            // The range field sits over the right half of the row's own space
            // below it.
            y += OPT;
            TextField::new(self.range.clone()).placeholder("1-3, 5").at(RIGHT_X + LABEL_W, y).width(RIGHT_W - LABEL_W).render(ctx);
        }
        y += OPT;
        if s.offers_color() {
            self.choice(ctx, y, "Colour", &["Colour", "Black and white"], s.gray as usize, |v, i| v.gray = i == 1);
            y += OPT;
        }
        if s.offers_duplex() {
            let at = match s.sides {
                Sides::OneSided => 0,
                Sides::TwoSidedLongEdge => 1,
                Sides::TwoSidedShortEdge => 2,
            };
            self.choice(ctx, y, "Two-sided", &["Off", "Long edge", "Short edge"], at, |v, i| {
                v.sides = [Sides::OneSided, Sides::TwoSidedLongEdge, Sides::TwoSidedShortEdge][i.min(2)];
            });
            y += OPT;
        }
        let papers = s.papers();
        let labels: Vec<&str> = papers.iter().map(|p| p.1).collect();
        let at = papers.iter().position(|p| p.0 == s.media).unwrap_or(0);
        self.choice(ctx, y, "Paper", &labels, at, move |v, i| {
            if let Some(p) = papers.get(i) {
                v.media = p.0.into();
            }
        });
        y += OPT;
        if s.can_relayout {
            self.choice(ctx, y, "Orientation", &["Portrait", "Landscape"], s.landscape as usize, |v, i| v.landscape = i == 1);
            y += OPT;
        }
        let per = [1u8, 2, 4];
        self.choice(ctx, y, "Per sheet", &["1 page", "2 pages", "4 pages"], per.iter().position(|p| *p == s.per_sheet).unwrap_or(0), move |v, i| v.per_sheet = per[i.min(2)]);
        y += OPT;
        self.choice(ctx, y, "Scale", &["Actual size", "Fit to paper"], s.fit as usize, |v, i| v.fit = i == 1);

        if let Some((n, tone)) = &s.notice {
            let c = if *tone == Tone::Error { t.danger_fg() } else { t.warning_fg() };
            self.text(ctx, RIGHT_X, H - PAD - 32.0 - 24.0, RIGHT_W, n, style(type_size::XS, Weight::Regular, c));
        }
        if let Some(w) = s.working {
            self.text(ctx, RIGHT_X, H - PAD - 24.0, 140.0, w, style(type_size::SM, Weight::Regular, t.text_secondary()));
        }
        let c = cancel_button();
        let asked = self.asked.clone();
        Button::secondary("Cancel")
            .at(c.x(), c.y())
            .width(c.w())
            .on_click(move || {
                let mut a = asked.get_cloned();
                a.cancel = true;
                asked.set(a);
            })
            .render(ctx);
        let p = print_button();
        let asked = self.asked.clone();
        let busy = s.working.is_some();
        Button::primary(s.button())
            .at(p.x(), p.y())
            .width(p.w())
            .on_click(move || {
                if busy {
                    return;
                }
                let mut a = asked.get_cloned();
                a.print = Some(pergola::clipboard::current_activation());
                asked.set(a);
            })
            .render(ctx);
    }
}

/// A preview's sRGB pixels as a picture to draw.
pub fn picture(w: u32, h: u32, rgb: &[u8]) -> Option<Image> {
    if rgb.len() != (w * h * 3) as usize || w == 0 || h == 0 {
        return None;
    }
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for p in rgb.chunks_exact(3) {
        rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
    }
    Some(Image::new(w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pergola::{App, Event, Point};

    fn app() -> (App<SheetView>, SheetView) {
        let v = SheetView::new(Mutable::new(Sheet::new("Q4", false, None)));
        let mut a = App::new(v.clone());
        let _ = a.tick();
        (a, v)
    }

    fn click(a: &mut App<SheetView>, r: Rect) {
        let at = Point::new(r.x() + r.w() / 2.0, r.y() + r.h() / 2.0);
        a.handle_event(Event::PointerDown { at });
        a.handle_event(Event::PointerUp { at });
        let _ = a.tick();
    }

    #[test]
    fn print_and_cancel_are_where_the_guard_is() {
        let (mut a, v) = app();
        click(&mut a, print_button());
        assert_eq!(v.asked.get_cloned().print, Some(None), "a press with no token (synthetic)");
        click(&mut a, cancel_button());
        assert!(v.asked.get_cloned().cancel);
    }

    #[test]
    fn a_row_chooses_its_printer_and_options_follow() {
        let (mut a, v) = app();
        // Only Save as PDF before the list: no copies row.
        assert!(!v.sheet.get_cloned().offers_copies());
        let mut s = v.sheet.get_cloned();
        s.set_nearby(vec![prelum_proto::print::Nearby {
            uri: "ipp://10.0.0.9:631/ipp/print".into(),
            name: "Office".into(),
            make_and_model: String::new(),
            location: String::new(),
            announced: String::new(),
            tls: false,
            fingerprint: None,
            uuid: None,
            color: true,
            duplex: false,
            saved: None,
        }]);
        s.listed = true;
        v.sheet.set(s);
        let _ = a.tick();
        // The pointer rests on the row first (its hover drawn), then presses.
        let r = Rect::new(RIGHT_X, PAD + 34.0, RIGHT_W, ROW - 2.0);
        a.handle_event(Event::PointerMove { at: Point::new(r.x() + 20.0, r.y() + 10.0) });
        let _ = a.tick();
        click(&mut a, r);
        assert!(matches!(v.sheet.get_cloned().dest(), Dest::Nearby(_)));
        assert_eq!(v.sheet.get_cloned().button(), "Print");
    }

    #[test]
    fn pictures_from_previews() {
        let p = picture(2, 1, &[255, 0, 0, 0, 0, 255]).unwrap();
        assert_eq!(p.data().rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
        assert!(picture(2, 2, &[0; 3]).is_none());
    }
}
