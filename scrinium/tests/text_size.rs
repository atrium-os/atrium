//! Text-size checks that set the process-wide text size: their own test
//! binary, so the crate's other tests keep 100%.

use scrinium::view::delete_question;

/// At 200% the delete question keeps a readable piece of the name with
/// "It can't be brought back.", or drops that sentence; it never keeps the
/// sentence over a two-letter stub (the 60 px limit didn't grow with the text).
#[test]
fn at_200_the_delete_question_keeps_a_readable_name() {
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
