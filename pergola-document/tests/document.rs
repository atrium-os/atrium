//! A long document through DocumentView, in an App: the first screen is
//! drawn as glyph runs and rects, the scene stays a few screens however far
//! it scrolls, the scroll range grows to the whole document as it settles,
//! and a band keeps its node ids while it stays resident.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use navigator_render::fontset::FontSet;
use pergola::app::App;
use pergola::geom::Rect;
use pergola::node::{Node, NodeDelta};
use pergola::reactive::Mutable;
use pergola::view::{Ctx, View};
use pergola::widgets::scroll_view::ScrollState;
use pergola_document::{Doc, DocumentView};

fn fonts() -> &'static FontSet {
    Box::leak(Box::new(FontSet::load().unwrap()))
}

fn html(sections: usize) -> String {
    let mut b = String::from(r#"<html><head><style>body { margin: 8px } p { margin: 0px 0px 10px }
        section { background-color: #eef4ff; padding: 6px; margin: 0px 0px 12px; border-radius: 6px }</style></head><body>"#);
    for s in 0..sections {
        b.push_str("<section>");
        for i in 0..6 {
            b.push_str(&format!("<p>Paragraph {s}.{i}: words enough to wrap once across the width of the page at the default size here.</p>"));
        }
        b.push_str("</section>");
    }
    b.push_str("</body></html>");
    b
}

struct Page {
    view: DocumentView,
}

impl View for Page {
    fn render(&self, ctx: &mut Ctx) {
        self.view.render(ctx);
    }
}

fn count(d: &[NodeDelta], f: impl Fn(&Node) -> bool) -> usize {
    d.iter().filter(|d| matches!(d, NodeDelta::Added { node, .. } if f(node))).count()
}

#[test]
fn a_long_document_scrolls_with_a_few_screens_resident() {
    let dirty = Arc::new(AtomicBool::new(true));
    let state = Mutable::with_dirty(ScrollState::default(), dirty.clone());
    let more = Mutable::with_dirty(0u64, dirty.clone());
    let doc = Rc::new(RefCell::new(Doc::html(html(200), fonts(), Default::default(), 800.0, 600.0, |_| None)));
    let view = DocumentView::new(state.clone(), Rect::new(0.0, 0.0, 800.0, 600.0), doc.clone(), more);
    let mut app = App::new_with_flag(Page { view }, dirty);

    let first = app.tick();
    let runs = count(&first, |n| matches!(n, Node::Glyphs { .. }));
    assert!(runs > 20, "the first screen's text is glyph runs: {runs}");
    assert!(app.tree().iter().all(|(_, n)| n.rect().y().abs() < 1e7), "nothing runs off the page: band clips hold");
    let (h0, settled0) = doc.borrow().extent();
    assert!(h0 > 1200.0, "an extent beyond the first screens: {h0}");

    // Background layout runs a render at a time until the height settles.
    let mut ticks = 0;
    while !doc.borrow().extent().1 && ticks < 500 {
        app.tick();
        ticks += 1;
    }
    let (h, settled) = doc.borrow().extent();
    assert!(settled && !settled0 || settled, "settles in the background ({ticks} renders)");
    assert!(h > 40_000.0, "200 sections: {h}");
    let content_h = app.tree().iter().find_map(|(_, n)| match n {
        Node::Scroll { content_h, .. } => Some(*content_h),
        _ => None,
    });
    assert_eq!(content_h, Some(h), "the scroll range is the whole document");

    // The first band's ids hold while it stays resident.
    let band_ids = |app: &App<Page>| -> Vec<u32> {
        app.tree().iter().filter(|(_, n)| matches!(n, Node::Glyphs { .. })).map(|(id, _)| id.0).take(5).collect()
    };
    let before = band_ids(&app);
    let mut st = state.get();
    st.physics.y = 100.0;
    state.set(st);
    app.tick();
    assert_eq!(band_ids(&app), before, "a small scroll keeps the band's nodes");

    // Far down: the window of resident bands moves; the scene stays small.
    let resident = |app: &App<Page>| app.tree().iter().filter(|(_, n)| matches!(n, Node::Glyphs { .. })).count();
    let near = resident(&app);
    let mut st = state.get();
    st.physics.y = (h * 0.6) as f64;
    state.set(st);
    let d = app.tick();
    assert!(count(&d, |n| matches!(n, Node::Glyphs { .. })) > 20, "the new place's text arrives");
    let far = resident(&app);
    assert!(far < near * 3, "a few screens resident: {far} runs there, {near} at the top");
    let bands = doc.borrow().bands();
    assert!(bands.iter().all(|(_, r)| (r.end as f32 / 64.0) > h * 0.5), "the top's bands went: {bands:?}");
    // A scroll frame's cost: one small step, rendered and diffed.
    let t = std::time::Instant::now();
    for i in 0..60 {
        let mut st = state.get();
        st.physics.y += 7.0 + i as f64 * 0.0;
        state.set(st);
        app.tick();
    }
    let per = t.elapsed().as_secs_f64() * 1e3 / 60.0;
    let nodes = app.tree().iter().count();
    eprintln!("document: {h:.0} px, settled in {ticks} renders; {far} runs resident, {nodes} nodes; {per:.2} ms per scroll frame; skipped {:?}", doc.borrow().skipped);
}
