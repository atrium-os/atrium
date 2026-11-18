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
    b.set_places(vec![Place { id: "place:0".into(), label: "Documents".into(), root: d.clone(), stick: false, read_only: false, size: 0, free: 0, network: false, offline: false, seal: Default::default() }]);
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

/// Arrow keys at 200% in a short picker list (live: 60 pt rows in 48 pt
/// of list), with keys handed out as the window does: the browser first
/// (take_key), the app only what it left. The app also scrolled the list a
/// line (40 pt) for each arrow, on top of the cursor's reveal, so the row
/// ended 40 pt off: only its bottom showed going down, the row above it
/// going up.
#[test]
fn at_200_an_arrow_shows_the_cursor_row_and_does_not_scroll_past_it() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use pergola::text_edit::TextEdit;
    use pergola::widgets::ScrollState;
    use pergola::{App, Event, Key, KeyEventKind, Mutable};
    use scrinium::browser::{Browser, Mode, Place};
    use scrinium::view::{header_h, pick_foot_h, pick_path_h, pick_title_h, row_h, take_key, ScriniumView};

    let _one = SCALE.lock().unwrap_or_else(|e| e.into_inner());
    pergola::text::set_text_scale(2000);
    let d = std::env::temp_dir().join(format!("scrinium-keys-200-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for i in 0..20 {
        std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
    }
    let mut b = Browser::new(Mode::Picker { filter: limen::picker::Filter::file(), app: "org.test.notes".into() });
    b.set_places(vec![Place { id: "place:0".into(), label: "Documents".into(), root: d.clone(), stick: false, read_only: false, size: 0, free: 0, network: false, offline: false, seal: Default::default() }]);
    // The list 0.8 of a row, as live.
    let lh = 0.8 * row_h() as f64;
    let h = pick_title_h() + pick_path_h() + header_h() + lh as f32 + pick_foot_h();
    let dirty = Arc::new(AtomicBool::new(true));
    let state = Mutable::with_dirty(b, dirty.clone());
    let field = Mutable::with_dirty(TextEdit::new(""), dirty.clone());
    let scroll = Mutable::with_dirty(ScrollState::default(), dirty.clone());
    let v = ScriniumView::new(state.clone(), field.clone(), scroll.clone(), Mutable::with_dirty((928.0, h), dirty.clone()));
    let mut app = App::new_with_flag(v, dirty);
    let mut t = 0.0;
    app.tick();
    // A click on the first row, as live: the list has focus, so an arrow
    // the app sees goes to its scroll view.
    let at = pergola::geom::Point::new(700.0, pick_title_h() + pick_path_h() + header_h() + 10.0);
    app.handle_event(Event::PointerDown { at });
    app.handle_event(Event::PointerUp { at });
    app.tick();
    assert_eq!(state.get_cloned().cursor, Some(0), "the click landed on the first row");
    // As Window::paint: step the animations, then draw.
    let paint = |app: &mut App<ScriniumView>, t: f64| {
        app.set_time(t);
        app.frame();
        app.tick();
    };
    let mut press = |app: &mut App<ScriniumView>, key: Key| {
        for kind in [KeyEventKind::Down, KeyEventKind::Up] {
            let ev = Event::Key { kind, key, modifiers: Default::default(), chars: String::new() };
            if !take_key(&state, &field, &ev) {
                app.handle_event(ev);
            }
            paint(app, t);
        }
        // Frames until an eased scroll (200 ms) is long done.
        for _ in 0..50 {
            t += 8.0;
            paint(app, t);
        }
    };
    // A row taller than the list shows its top: the top at the list's top.
    let at_top = |what: &str| {
        let cur = state.get_cloned().cursor.unwrap() as f64;
        let y = scroll.get().physics.y;
        assert!((y - cur * row_h() as f64).abs() < 0.5, "{what}: row {cur} starts at {}, the list shows from {y}", cur * row_h() as f64);
    };
    for i in 0..6 {
        press(&mut app, Key::Down);
        at_top(&format!("down {i}"));
    }
    for i in 0..3 {
        press(&mut app, Key::Up);
        at_top(&format!("up {i}"));
    }
    press(&mut app, Key::End);
    assert_eq!(state.get_cloned().cursor, Some(19));
    at_top("end");
    press(&mut app, Key::Home);
    at_top("home");

    let _ = std::fs::remove_dir_all(&d);
    pergola::text::set_text_scale(1000);
}
