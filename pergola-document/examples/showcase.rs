//! `showcase`: every Pergola control, a Canvas and a DocumentView in one
//! window, for checking them on a live frescod (headless: its PNG is the
//! screenshot). Runs `SHOWCASE_SECS` (default 40) then exits.
//!
//!   FRESCO_SOCKET=… showcase [dark] [dialog]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pergola::geom::{Axis, Point, Rect, Size};
use pergola::layout::{Align, FlexStyle, Grid, Track};
use pergola::node::Node;
use pergola::shape::{Paint, PathData, Shape, Stroke};
use pergola::text_edit::TextEdit;
use pergola::theme::{font, space, type_size, Semantic, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::canvas::{Canvas, Item};
use pergola::widgets::{Check, Checkbox, Dialog, DialogButton, Dropdown, GroupBox, MenuState, ScrollState, Section, Segmented, Slider, Tabs, Toggle};
use pergola::{App, Button, Color, Mutable, SizeSpec, TextField, TextStyle, Window, WindowDesc};
use pergola_document::{Doc, DocumentView};

struct Show {
    name: Mutable<TextEdit>,
    check: Mutable<Check>,
    dialog: Mutable<bool>,
    plan: Mutable<usize>,
    plan_menu: Mutable<MenuState>,
    toggle: Mutable<bool>,
    seg: Mutable<usize>,
    tab: Mutable<usize>,
    vol: Mutable<f64>,
    canvas: Mutable<ScrollState>,
    items: Rc<Vec<Item>>,
    doc: DocumentView,
}

fn text(ctx: &mut Ctx, s: &str) {
    let t = ctx.theme;
    ctx.add(Node::Text {
        rect: Rect::ZERO_SIZED,
        content: s.into(),
        style: TextStyle { family: font::SANS.into(), size: type_size::MD, weight: Weight::Regular, color: t.text_primary() },
    });
}

fn heading(ctx: &mut Ctx, x: f32, y: f32, s: &str) {
    let t = ctx.theme;
    ctx.add(Node::Text {
        rect: Rect::new(x, y, 0.0, 0.0),
        content: s.into(),
        style: TextStyle { family: font::SANS.into(), size: type_size::SM, weight: Weight::Semibold, color: t.text_secondary() },
    });
}

/// A row of controls on one baseline.
fn row(ctx: &mut Ctx, gap: f32, f: impl FnOnce(&mut Ctx)) {
    let r = ctx.push(Node::stack(Axis::Horizontal, Rect::ZERO_SIZED, gap));
    ctx.set_flex(r, FlexStyle { align: Align::Baseline, ..FlexStyle::default() });
    f(ctx);
    ctx.pop();
}

impl View for Show {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        ctx.add(Node::Rect { rect: Rect::new(0.0, 0.0, 1280.0, 800.0), fill: t.bg_surface(), radius: 0.0 });
        // The controls: one column of sections, laid out (no coordinates
        // below this line but the column's).
        let col = ctx.push(Node::stack(Axis::Vertical, Rect::new(24.0, 20.0, 392.0, 0.0), space::LG));
        Section::new("Buttons").show(ctx, |ctx| {
            // A wrapping row: what does not fit goes on the next line.
            let r = ctx.push(Node::stack(Axis::Horizontal, Rect::ZERO_SIZED, space::XS));
            ctx.set_flex(r, FlexStyle { align: Align::Baseline, wrap: true, ..FlexStyle::default() });
            ctx.place(Button::primary("Save").on_click(|| {}));
            ctx.place(Button::secondary("Cancel").on_click(|| {}));
            ctx.place(Button::ghost("More").on_click(|| {}).tooltip("More actions"));
            ctx.place(Button::destructive("Delete").on_click(|| {}));
            for l in ["Archive", "Share", "Export"] {
                ctx.place(Button::secondary(l).on_click(|| {}));
            }
            ctx.pop();
        });
        GroupBox::new("Account").show(ctx, |ctx| {
            let g = ctx.push(Node::stack(Axis::Vertical, Rect::ZERO_SIZED, 0.0));
            ctx.grid(g, Grid::new([Track::Auto, Track::Fr(1.0)]).gaps(space::SM, space::XS).align(Align::Baseline));
            text(ctx, "Name");
            ctx.place(TextField::new(self.name.clone()).placeholder("Your name"));
            text(ctx, "Plan");
            ctx.place(Dropdown::new(self.plan.clone(), self.plan_menu.clone(), ["Personal", "Family", "Team"]));
            ctx.pop();
            row(ctx, space::LG, |ctx| {
                ctx.place(Checkbox::new(self.check.clone(), "Remember me"));
                ctx.place(Toggle::new(self.toggle.clone(), "Wi-Fi"));
            });
        });
        Section::new("View").description("Pick the range to show, the tab, and how loud alerts are. Changes apply at once and are kept for next time.").show(ctx, |ctx| {
            row(ctx, space::SM, |ctx| {
                ctx.place(Segmented::new(self.seg.clone(), ["Day", "Week", "Month"]));
                text(ctx, "Week of 2 Nov");
            });
            ctx.place(Tabs::new(self.tab.clone(), ["General", "Privacy", "Advanced"]).width(0.0));
            ctx.place(Slider::new(self.vol.clone(), 0.0, 100.0).width(0.0));
        });
        ctx.pop();
        let _ = col;

        heading(ctx, 24.0, 508.0, "Canvas");
        Canvas::new(self.canvas.clone(), Rect::new(24.0, 532.0, 392.0, 248.0), Size::new(392.0, 400.0), self.items.clone())
            .on_pointer(|_| {})
            .emit(ctx);

        heading(ctx, 448.0, 20.0, "DocumentView");
        self.doc.render(ctx);
        Dialog::new(self.dialog.clone(), Rect::new(0.0, 0.0, 1280.0, 720.0), "Discard changes?", "Your edits to this page have not been saved. If you discard them, the page goes back to how it was when you opened it, and this cannot be undone.", vec![
            DialogButton::new("Keep editing", || {}),
            DialogButton::new("Discard", || {}).destructive(),
        ])
        .render(ctx);
    }
}

fn items() -> Rc<Vec<Item>> {
    let ink = Paint::Solid(Color::from_hex("#2B2F36"));
    let accent = Color::from_hex("#B8752E");
    Rc::new(vec![
        Item::new(1, Shape::new(PathData::rounded_rect(Rect::new(24.0, 24.0, 140.0, 64.0), 10.0))
            .fill(Paint::Solid(Color::from_hex("#EEF4FF"))).stroke(Stroke::new(ink.clone(), 2.0))),
        Item::new(2, Shape::new(PathData::rounded_rect(Rect::new(228.0, 24.0, 140.0, 64.0), 10.0))
            .fill(Paint::Linear { from: Point::new(228.0, 24.0), to: Point::new(368.0, 88.0), stops: vec![(0.0, Color::from_hex("#FFE7C2")), (1.0, accent)] })
            .stroke(Stroke::new(ink.clone(), 2.0))),
        Item::new(3, Shape::new(PathData::new().move_to(164.0, 56.0).line_to(228.0, 56.0))
            .stroke(Stroke { dash: Some((vec![6.0, 4.0], 0.0)), ..Stroke::new(ink.clone(), 2.0) })),
        Item::new(4, Shape::new(PathData::ellipse(Point::new(0.0, 0.0), 80.0, 36.0))
            .fill(Paint::Radial { center: Point::new(0.0, 0.0), radius: 80.0, stops: vec![(0.0, Color::from_hex("#FFFFFF")), (1.0, Color::from_hex("#7FA7D9"))] })
            .transform({ let (s, c) = 20f32.to_radians().sin_cos(); [c, s, -s, c, 196.0, 200.0] })),
        Item::new(5, Shape::new(PathData::new().move_to(40.0, 340.0).cubic_to(120.0, 240.0, 260.0, 420.0, 352.0, 300.0))
            .stroke(Stroke { cap: pergola::shape::Cap::Round, ..Stroke::new(Paint::Solid(accent), 6.0) })),
    ])
}

fn page() -> String {
    let mut b = String::from(r#"<html><head><style>body { margin: 16px; font-size: 15px }
        h1 { font-size: 26px; margin: 0px 0px 12px } h2 { font-size: 19px; margin: 18px 0px 8px }
        p { margin: 0px 0px 10px } .note { background-color: #eef4ff; padding: 8px 12px; border-radius: 6px }
        code { background-color: #f1efe9 }
        @media (prefers-color-scheme: dark) { body { background-color: #16181d; color: #e6e8ec } .note { background-color: #1f2b3d } code { background-color: #2a2d33 } }</style></head><body><h1>Atrium document view</h1>
        <p class="note">This page is laid out by the document engine and drawn by Pergola: bands of the page become glyph runs, rects and fills.</p>"#);
    for s in 1..=40 {
        b.push_str(&format!("<h2>Section {s}</h2>"));
        for i in 0..3 {
            b.push_str(&format!("<p>Paragraph {s}.{i}. Scrolling, zoom and anchoring belong to <code>ScrollView</code>; the content source decides where bands are cut and what stays resident. <b>Bold</b> and <i>italic</i> runs use their own faces.</p>"));
        }
    }
    b.push_str("</body></html>");
    b
}

fn main() -> std::io::Result<()> {
    let dark = std::env::args().any(|a| a == "dark");
    let secs: u64 = std::env::var("SHOWCASE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut window = Window::open(&WindowDesc::new("Pergola showcase", SizeSpec::Fixed(1280, 800)))?;
    window.follow_appearance(false);
    let dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let doc = Rc::new(RefCell::new(Doc::html(page(), Default::default(), 808.0, 740.0, dark, |_| None).map_err(std::io::Error::other)?));
    let view = Show {
        name: Mutable::with_dirty(TextEdit::new("Shankar"), dirty.clone()),
        check: Mutable::with_dirty(Check::On, dirty.clone()),
        dialog: Mutable::with_dirty(std::env::args().any(|a| a == "dialog"), dirty.clone()),
        plan: Mutable::with_dirty(1usize, dirty.clone()),
        plan_menu: Mutable::with_dirty(MenuState::default(), dirty.clone()),
        toggle: Mutable::with_dirty(true, dirty.clone()),
        seg: Mutable::with_dirty(1usize, dirty.clone()),
        tab: Mutable::with_dirty(0usize, dirty.clone()),
        vol: Mutable::with_dirty(65.0f64, dirty.clone()),
        canvas: Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        items: items(),
        doc: DocumentView::new(Mutable::with_dirty(ScrollState::default(), dirty.clone()), Rect::new(448.0, 44.0, 808.0, 740.0), doc.clone(), Mutable::with_dirty(0u64, dirty.clone())),
    };
    let mut app = App::new_with_flag(view, dirty).with_theme(if dark { Semantic::DARK } else { Semantic::LIGHT });
    let end = Instant::now() + Duration::from_secs(secs);
    window.paint(&mut app)?;
    while Instant::now() < end {
        window.step(&mut app, Some(Duration::from_millis(200)))?;
    }
    let (h, settled) = doc.borrow().extent();
    eprintln!("showcase: document {h:.0} px (settled {settled}), skipped {:?}", doc.borrow().skipped);
    Ok(())
}
