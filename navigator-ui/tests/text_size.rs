//! The toolbar and the Aa pop-up grow with the system text size
//! (resolution-independence.md §6.2, §10). Its own process: the text size
//! is process-wide.

use navigator_ui::fake::Scene;
use navigator_ui::shell::{self, Shell};

#[test]
fn the_toolbar_grows_with_the_text_size() {
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
