//! The toolbar and the Aa pop-up grow with the system text size
//! (resolution-independence.md §6.2, §10). Its own process: the text size
//! is process-wide.

use navigator_ui::fake::Scene;
use navigator_ui::shell::{self, Shell};

/// The tests take turns: the text size is the process's.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_toolbar_grows_with_the_text_size() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let at = |milli| {
        pergola::text::set_text_scale(milli);
        let mut s = Shell::scene(Scene::Main, 1400.0, 900.0);
        s.reading_open = true;
        s.geo()
    };
    let one = at(1000);
    let big = at(1500);
    pergola::text::set_text_scale(1000);
    assert_eq!(shell::BAR * 1.5, big.page.y() - 1.0, "the bar is 1.5x as tall");
    assert_eq!(one.page.y(), shell::BAR + 1.0);
    assert_eq!((big.aa.w(), big.where_.h(), big.presence.h()), (one.aa.w() * 1.5, one.where_.h() * 1.5, one.presence.h() * 1.5));
    assert!(big.presence.w() > one.presence.w(), "the presence pill holds its bigger label");
    let (r1, r2) = (one.reading.unwrap(), big.reading.unwrap());
    assert_eq!((r2.w(), r2.h()), (r1.w() * 1.5, r1.h() * 1.5), "the Aa pop-up too");
    assert!(r2.y() >= big.page.y() - 1.0, "and hangs below the bigger bar");
    // Everything on the bar stays inside it.
    for r in [big.aa, big.where_, big.presence] {
        assert!(r.y() + r.h() <= shell::bar_at(1.5), "{r:?}");
    }
}

/// The rail, the rows under the bar, the status bar and the other pop-ups
/// grow with it too.
#[test]
fn the_rail_rows_and_pop_ups_grow_with_the_text_size() {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let at = |milli, scene| {
        pergola::text::set_text_scale(milli);
        let mut s = Shell::scene(scene, 1600.0, 900.0);
        s.popover = true;
        s.presence_open = true;
        s.status_bar = true;
        if scene != Scene::Start {
            s.open_find();
        }
        s.geo()
    };
    let (one, big) = (at(1000, Scene::Prompt), at(1500, Scene::Prompt));
    let (st1, st2) = (at(1000, Scene::Start), at(1500, Scene::Start));
    pergola::text::set_text_scale(1000);
    assert_eq!(big.rail, one.rail * 1.5, "the rail");
    assert_eq!(st2.rail, st1.rail * 1.5, "the collapsed rail");
    let h = |r: Option<pergola::Rect>| r.map(|r| r.h()).expect("shown");
    let w = |r: Option<pergola::Rect>| r.map(|r| r.w()).unwrap();
    assert_eq!(h(big.find), h(one.find) * 1.5, "the find bar");
    assert_eq!(h(big.prompt), h(one.prompt) * 1.5, "the prompt row");
    assert_eq!(h(big.status), h(one.status) * 1.5, "the status bar");
    assert_eq!((w(big.popover), h(big.popover)), (w(one.popover) * 1.5, h(one.popover) * 1.5), "the tier popover");
    assert_eq!((w(big.presence_menu), h(big.presence_menu)), (w(one.presence_menu) * 1.5, h(one.presence_menu) * 1.5), "the presence menu");
    assert_eq!(h(st2.drop), h(st1.drop) * 1.5, "the suggestion list");
    let (cx1, cw1) = shell::history_clear_all(0.0, 720.0);
    pergola::text::set_text_scale(1500);
    let (cx2, cw2) = shell::history_clear_all(0.0, 720.0);
    pergola::text::set_text_scale(1000);
    assert_eq!(cw2, cw1 * 1.5, "history's Clear all");
    assert!(cx2 < cx1);
}

/// Icons are drawn at their point size times the text size, once: the
/// renderer applies the text size to a glyph's font size, so the size
/// handed to it stays in points (at 1.5x a 16 pt icon was drawn 2.25x,
/// low and right of its label, in the live check).
#[test]
fn icons_grow_with_the_text_size_once() {
    use navigator_ui::ui::ShellView;
    use pergola::theme::{font, Semantic};
    use pergola::{Mutable, Node};
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let sizes = |milli, scene| {
        pergola::text::set_text_scale(milli);
        let mut s = Shell::scene(scene, 1600.0, 900.0);
        s.popover = true;
        s.presence_open = true;
        s.status_bar = true;
        if scene != Scene::Start {
            s.open_find();
        }
        let (tree, _) = pergola::render(&ShellView { state: Mutable::new(s) }, Semantic::LIGHT);
        let mut v: Vec<(f32, f32)> = tree.iter().filter_map(|(_, n)| match n {
            Node::Text { style, .. } if style.family == font::ICONS => Some((style.size, style.em())),
            _ => None,
        }).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v
    };
    for scene in [Scene::Main, Scene::Prompt, Scene::Start, Scene::Stop] {
        let (one, big) = (sizes(1000, scene), sizes(1500, scene));
        pergola::text::set_text_scale(1000);
        assert!(!big.is_empty(), "{scene:?} draws icons");
        assert_eq!(big.iter().map(|s| s.0).collect::<Vec<_>>(), one.iter().map(|s| s.0).collect::<Vec<_>>(), "{scene:?}: the same point sizes");
        for (pt, drawn) in big {
            assert!(pt <= 32.0, "{scene:?}: an icon of {pt} pt is not in points");
            assert_eq!(drawn, pt * 1.5, "{scene:?}: drawn once at the text size");
        }
    }
}

/// The tier popover holds all it says at every text size: its height is
/// measured from its wrapped lines (the last one hung out of the box at
/// 200% in the live check).
#[test]
fn the_tier_popover_holds_its_last_line() {
    use navigator_ui::ui::ShellView;
    use pergola::theme::Semantic;
    use pergola::{Mutable, Node};
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for milli in [1000, 1500, 2000] {
        pergola::text::set_text_scale(milli);
        let mut s = Shell::scene(Scene::Prompt, 1920.0, 900.0);
        s.popover = true;
        let pop = s.geo().popover.expect("the popover");
        let (tree, _) = pergola::render(&ShellView { state: Mutable::new(s) }, Semantic::LIGHT);
        let last = tree.iter().filter_map(|(_, n)| match n {
            Node::Text { rect, content, .. } if content.ends_with("the strip.") => Some(*rect),
            _ => None,
        }).next();
        pergola::text::set_text_scale(1000);
        let last = last.expect("the popover's last line is drawn");
        assert!(last.y() + last.h() <= pop.y() + pop.h(), "{milli}: {last:?} out of {pop:?}");
    }
}
