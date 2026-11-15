//! Text-size checks that set the process-wide text size: their own test
//! binary, so the crate's other tests keep 100%.

use scrinium::view::delete_question;

/// The text size is process-wide and tests run in parallel: each holds
/// this while it sets one, so none reads another's.
static SCALE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// At 200% the delete question keeps a readable piece of the name with
/// "It can't be brought back.", or drops that sentence; it never keeps the
/// sentence over a two-letter stub (the 60 px limit didn't grow with the text).
#[test]
fn at_200_the_delete_question_keeps_a_readable_name() {
    let _one = SCALE.lock().unwrap_or_else(|e| e.into_inner());
    pergola::text::set_text_scale(2000);
    let names = vec!["budget.csv".to_string()];
    let mut saw_bare = false;
    for room in (300..2000).step_by(5) {
        let q = delete_question(&names, room as f32);
        let name = q.split('“').nth(1).and_then(|r| r.split('”').next()).unwrap();
        if q.ends_with("brought back.") && name != "budget.csv" {
            let kept = name.trim_end_matches('…').chars().count();
            assert!(kept >= 5, "at {room} pt: {q:?} keeps the sentence over a {kept}-letter name");
        }
        saw_bare |= q.ends_with('?');
    }
    assert!(saw_bare, "narrow rooms drop the second sentence");
    pergola::text::set_text_scale(1000);
}

/// The picker at 200% in its catalogue sheet (640 x 440 pt, which doesn't
/// grow): its heading's two lines stay apart, the path starts after the up
/// button, the buttons stay inside the sheet, and a short name in the list
/// is shown whole (the places took over half the sheet, eliding every name).
#[test]
fn at_200_the_picker_fits_its_sheet() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use pergola::node::Node;
    use pergola::text_edit::TextEdit;
    use pergola::widgets::ScrollState;
    use pergola::{App, Mutable};
    use scrinium::browser::{Browser, Mode, Place};
    use scrinium::view::ScriniumView;

    let _one = SCALE.lock().unwrap_or_else(|e| e.into_inner());
    pergola::text::set_text_scale(2000);
    let d = std::env::temp_dir().join(format!("scrinium-pick-200-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("hello.txt"), "hello").unwrap();
    let f = limen::picker::Filter { title: "Open a note".into(), ..limen::picker::Filter::file() };
    let mut b = Browser::new(Mode::Picker { filter: f, app: "org.test.notes".into() });
    b.set_places(vec![Place { id: "place:0".into(), label: "Documents".into(), root: d.clone(), stick: false, read_only: false, size: 0, free: 0, network: false, offline: false }]);
    let (w, h) = (640.0, 440.0);
    let dirty = Arc::new(AtomicBool::new(true));
    let v = ScriniumView::new(Mutable::with_dirty(b, dirty.clone()), Mutable::with_dirty(TextEdit::new(""), dirty.clone()), Mutable::with_dirty(ScrollState::default(), dirty.clone()), Mutable::with_dirty((w, h), dirty.clone()));
    let mut app = App::new_with_flag(v, dirty);
    app.tick();

    // Every text node: (content, rect, size).
    let tree = app.tree();
    let mut texts = Vec::new();
    let mut stack: Vec<_> = tree.roots().collect();
    while let Some(id) = stack.pop() {
        if let Some(Node::Text { content, rect, style }) = tree.get(id) {
            texts.push((content.clone(), *rect, style.clone()));
        }
        stack.extend(tree.children_of(id).iter().copied());
    }
    let find = |s: &str| texts.iter().find(|t| t.0 == s).unwrap_or_else(|| panic!("{s:?} in {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>())).clone();
    let cy = |r: pergola::geom::Rect| r.y() + r.h() / 2.0;
    // A text's drawn size: its line box is 1.5 of it (the style holds the
    // 100% size), and its width grows in step.
    let size = |t: &(String, pergola::geom::Rect, pergola::TextStyle)| t.1.h() / 1.5;
    let width = |t: &(String, pergola::geom::Rect, pergola::TextStyle)| pergola::text::measure(&t.0, &t.2).w * size(t) / t.2.em();

    // The heading: centres at least half of both lines' sizes apart.
    let (title, who) = (find("Open a note"), find("for org.test.notes, to read"));
    assert!(cy(who.1) - cy(title.1) >= (size(&title) + size(&who)) / 2.0, "heading lines overlap: {title:?} {who:?}");
    // The path's crumb starts after the up button: a 28 em-pt square whose
    // glyph sits 4 em-pt in.
    let em = pergola::theme::size::em;
    let crumb = texts.iter().filter(|t| t.0 == "Documents").max_by(|a, b| a.1.x().total_cmp(&b.1.x())).unwrap().clone();
    let up = find(&scrinium::view::ph::ARROW_UP.to_string());
    assert!(crumb.1.x() >= up.1.x() - em(4.0) + em(28.0), "the path {crumb:?} under the up button {up:?}");
    // The buttons: inside the sheet, Cancel left of Open.
    let (open, cancel) = (find("Open"), find("Cancel"));
    // Open's box ends its em(14) padding after its label, inside the
    // sheet's 16 pt margin (give or take).
    assert!(open.1.x() + width(&open) + em(14.0) <= w - 8.0, "Open's button past the sheet's edge: {open:?}");
    assert!(cancel.1.x() + width(&cancel) < open.1.x(), "Cancel runs into Open: {cancel:?} {open:?}");
    // The list keeps room for a short name.
    assert!(texts.iter().any(|t| t.0 == "hello.txt"), "hello.txt elided: {:?}", texts.iter().map(|t| &t.0).collect::<Vec<_>>());

    let _ = std::fs::remove_dir_all(&d);
    pergola::text::set_text_scale(1000);
}
