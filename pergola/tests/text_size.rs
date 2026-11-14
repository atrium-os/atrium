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

#[test]
fn a_button_and_its_label_grow_with_the_text_size() {
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
