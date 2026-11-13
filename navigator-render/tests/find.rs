use navigator_render::{find::find, fontset::FontSet};
use navigator_style::cascade::Env;

#[test]
fn matches_come_back_as_rects_in_document_order() {
    let fonts = FontSet::load().unwrap();
    let mut doc = String::from("<!doctype html><html><body><h1>Atrium</h1><p>An atrium is an open court. The ATRIUM had an impluvium.</p>");
    for _ in 0..80 {
        doc.push_str("<p>Filler paragraph about the rooms of the house and their uses.</p>");
    }
    doc.push_str("<p id=end>The last atrium.</p></body></html>");
    let env = Env::default();
    let m = find(&doc, &fonts, &env, &Default::default(), "atrium", 100);
    assert_eq!(m.len(), 4, "{m:?}");
    assert!(m.windows(2).all(|w| w[0][1] <= w[1][1]), "in order down the page");
    assert!(m[3][1] > m[0][1] + 1000.0, "the last is far below, past the first band: {m:?}");
    assert!(m.iter().all(|r| r[2] > 10.0 && r[3] > 5.0));
    // Bigger text moves it further down.
    let big = find(&doc, &fonts, &Env { text_scale_64: 128, ..env.clone() }, &Default::default(), "atrium", 100);
    assert_eq!(big.len(), 4);
    assert!(big[3][1] > m[3][1] * 1.5);
    assert_eq!(find(&doc, &fonts, &env, &Default::default(), "atrium", 2).len(), 2, "bounded");
    assert!(find(&doc, &fonts, &env, &Default::default(), "", 100).is_empty());
    assert!(find(&doc, &fonts, &env, &Default::default(), "zebra", 100).is_empty());
}
