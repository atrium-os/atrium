//! The system text size (resolution-independence.md §6): text, and the
//! controls sized from it, grow together; nothing else does. Its own test
//! binary, since the text size is one per process.

use pergola::node::Node;
use pergola::theme::Semantic;
use pergola::widgets::Button;

fn sizes() -> (f32, f32) {
    let (tree, _) = pergola::view::render(&Button::primary("Save the document"), Semantic::LIGHT);
    let mut h = 0.0f32;
    let mut text = 0.0f32;
    for (_, n) in tree.iter() {
        match n {
            Node::Stack { rect, .. } | Node::Rect { rect, .. } => h = h.max(rect.h()),
            Node::Text { style, .. } => text = text.max(style.em()),
            _ => {}
        }
    }
    (h, text)
}

/// One test: the text size is one per process, and tests run in parallel.
#[test]
fn text_size() {
    grows();
    lint();
}

fn grows() {
    pergola::text::set_text_scale(1000);
    let (h1, t1) = sizes();
    // The tallest rect is the focus halo, 3 pt (not em) around the body.
    assert_eq!(h1, 32.0 + 6.0);
    assert!(pergola::text::set_text_scale(1500));
    let (h2, t2) = sizes();
    assert_eq!(h2, 48.0 + 6.0, "the button is em-based; its halo is not");
    assert!((t2 - 1.5 * t1).abs() < 1e-3, "its label too: {t1} -> {t2}");
    let wide = pergola::text::measure("Save", &pergola::node::TextStyle {
        family: pergola::theme::font::SANS.into(), size: 15.0, weight: pergola::theme::tokens::Weight::Regular, color: pergola::Color::TRANSPARENT,
    });
    assert!((wide.h - 22.5).abs() < 1e-3, "measured at the text size");
    assert!(!pergola::text::set_text_scale(1500), "no change, no relayout");
    pergola::text::set_text_scale(0);
    assert_eq!(pergola::text::text_scale(), 1.0, "0 is 100%");
    pergola::text::set_text_scale(5000);
    assert_eq!(pergola::text::text_scale(), 2.0, "clamped to 200%");
}

/// The catalogue lint (resolution-independence.md §6.2): each widget at
/// each text size, 85% to 200%, laid out; every label fits inside the
/// smallest box it starts in (its control's body).
fn lint() {
    use pergola::reactive::Mutable;
    use pergola::view::View;
    use pergola::widgets::{Checkbox, Check, RadioGroup, Segmented, Tabs, Toggle};
    let catalogue: Vec<(&str, Box<dyn Fn() -> Box<dyn View>>)> = vec![
        ("button", Box::new(|| Box::new(Button::primary("Save the document")))),
        ("checkbox", Box::new(|| Box::new(Checkbox::new(Mutable::new(Check::On), "Remember me")))),
        ("toggle", Box::new(|| Box::new(Toggle::new(Mutable::new(true), "Wi-Fi")))),
        ("radios", Box::new(|| Box::new(RadioGroup::new(Mutable::new(0), ["Day", "Week", "Month"])))),
        ("segmented", Box::new(|| Box::new(Segmented::new(Mutable::new(0), ["Day", "Week", "Month"])))),
        ("tabs", Box::new(|| Box::new(Tabs::new(Mutable::new(0), ["General", "Displays", "Sound"])))),
    ];
    let mut bad = Vec::new();
    let mut checked = 0;
    for t in fresco_protocol::TEXT_SIZES {
        pergola::text::set_text_scale(t);
        for (name, make) in &catalogue {
            let mut app = pergola::App::new(BoxView(make()));
            app.tick();
            let tree = app.tree();
            let boxes: Vec<pergola::Rect> = tree.iter().filter_map(|(_, n)| match n {
                Node::Rect { rect, .. } | Node::Stack { rect, .. } if rect.w() > 0.0 && rect.h() > 0.0 => Some(*rect),
                _ => None,
            }).collect();
            for (_, n) in tree.iter() {
                let Node::Text { rect, content, style } = n else { continue };
                let m = pergola::text::measure(content, style);
                let cy = rect.y() + rect.h() / 2.0;
                let (x0, x1, y0, y1) = (rect.x(), rect.x() + m.w, cy - m.h / 2.0, cy + m.h / 2.0);
                let Some(b) = boxes.iter().filter(|b| b.x() <= rect.x() + 0.5 && rect.x() < b.x() + b.w() && b.y() <= cy && cy <= b.y() + b.h())
                    .min_by(|a, b| (a.w() * a.h()).total_cmp(&(b.w() * b.h()))) else { continue };
                checked += 1;
                if x1 > b.x() + b.w() + 0.5 || y0 < b.y() - 0.5 || y1 > b.y() + b.h() + 0.5 || x0 < b.x() - 0.5 {
                    bad.push(format!("{name} at {t}: {content:?} {:.1}x{:.1} at ({:.1},{:.1}) in box ({:.1},{:.1} {:.1}x{:.1})",
                        m.w, m.h, x0, y0, b.x(), b.y(), b.w(), b.h()));
                }
            }
        }
    }
    pergola::text::set_text_scale(1000);
    assert!(checked > 30, "labels checked: {checked}");
    assert!(bad.is_empty(), "clipped:\n{}", bad.join("\n"));
}

struct BoxView(Box<dyn pergola::view::View>);
impl pergola::view::View for BoxView {
    fn render(&self, ctx: &mut pergola::view::Ctx) {
        self.0.render(ctx)
    }
}
