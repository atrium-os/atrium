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

#[test]
fn a_match_at_the_end_of_a_run_covers_its_last_glyph() {
    let fonts = FontSet::load().unwrap();
    // The same word ending a run, and inside one: the same width.
    let doc = "<!doctype html><html><body><p>An <b>atrium</b> is open.</p><p>An <b>atrium is</b> open.</p></body></html>";
    let m = find(doc, &fonts, &Env::default(), &Default::default(), "atrium", 10);
    assert_eq!(m.len(), 2, "{m:?}");
    assert!((m[0][2] - m[1][2]).abs() < 0.5, "{m:?}");
}

#[test]
fn an_element_is_found_by_its_id() {
    use navigator_render::find::element_top;
    let fonts = FontSet::load().unwrap();
    let mut doc = String::from("<!doctype html><html><body><h1>Top</h1>");
    for _ in 0..40 {
        doc.push_str("<p>Filler paragraph about the rooms of the house and their uses.</p>");
    }
    doc.push_str("<h2 id=\"plan\">The plan</h2><p>Text <span id=\"inl\">inline</span>.</p></body></html>");
    let env = Env::default();
    let (y, h) = element_top(&doc, &fonts, &env, &Default::default(), "plan").expect("found");
    assert!(y > 1000.0 && h > y, "far down: {y} of {h}");
    let (inl, _) = element_top(&doc, &fonts, &env, &Default::default(), "inl").expect("an inline element too");
    assert!(inl > y);
    assert_eq!(element_top(&doc, &fonts, &env, &Default::default(), "nope"), None);
    assert_eq!(element_top(&doc, &fonts, &env, &Default::default(), "a\"b"), None);
}

/// Keeping a reader's place across a text size change
/// (resolution-independence.md §10): the word at the top of the view in one
/// layout is found again in the other, in the same paragraph, while the
/// same document px would show text from much further up.
#[test]
fn the_word_at_the_top_is_found_again_at_another_text_size() {
    use navigator_render::find::word_at;
    let fonts = FontSet::load().unwrap();
    let mut doc = String::from("<!doctype html><html><body>");
    for i in 0..60 {
        doc.push_str(&format!("<p>Paragraph{i} says the rooms of the house have their uses and their light.</p>"));
    }
    doc.push_str("</body></html>");
    let small = Env::default();
    let big = Env { text_scale_64: 96, ..small.clone() };
    let subs = Default::default();
    let top = 1200.0;
    let (word, wtop) = word_at(&doc, &fonts, &small, &subs, top).expect("a word at the top");
    assert!(wtop >= top && wtop < top + 60.0, "{word} at {wtop}");
    let before = find(&doc, &fonts, &small, &subs, &word, 1000);
    let nth = before.iter().position(|m| (m[1] - wtop).abs() < 1.0).expect("it is one of its matches");
    let after = find(&doc, &fonts, &big, &subs, &word, nth + 1);
    let y = after[nth][1] - (wtop - top);
    let (again, _) = word_at(&doc, &fonts, &big, &subs, y).unwrap();
    assert_eq!(again, word, "the same word at the top");
    assert!(y > top * 1.3, "further down the document at 150%: {y}");
    let (same_px, _) = word_at(&doc, &fonts, &big, &subs, top).unwrap();
    assert_ne!(same_px, word, "the same px shows other words");
}
