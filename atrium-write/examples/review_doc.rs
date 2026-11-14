//! Writes a document with tracked changes by two people, for looking at the
//! review mode: `scratch/review.owrite`.
//!
//! cargo run --example review_doc [OUT]

use officina_text::model::TextDoc;
use officina_text::native;
use pergola_richtext::editor::Toggle;
use pergola_richtext::{Editor, Pos};

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "../scratch/review.owrite".into());
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let st = |d: &TextDoc, n: &str| d.styles.by_name(n).unwrap();
    d.push_para("Supply agreement", st(&d, "Title"), vec![]);
    d.push_para("1. Delivery", st(&d, "Heading 1"), vec![]);
    d.push_para("The supplier shall deliver the goods within thirty days of each order, to the address the buyer names in the order. Late delivery entitles the buyer to a discount of two percent for each week of delay.", 0, vec![]);
    d.push_para("2. Payment", st(&d, "Heading 1"), vec![]);
    d.push_para("The buyer shall pay each invoice within sixty days. Payment is made by bank transfer to the account named on the invoice. Disputed amounts may be withheld until the dispute is settled.", 0, vec![]);
    d.push_para("3. Warranty", st(&d, "Heading 1"), vec![]);
    d.push_para("The supplier warrants that the goods are free from defects for twelve months after delivery. Defective goods are repaired or replaced at the supplier's cost.", 0, vec![]);
    let mut e = Editor::new(d, atrium_write::doc::fonts());
    let find = |e: &Editor, b: usize, s: &str| e.doc.para(b).unwrap().text.find(s).unwrap() as u32;
    let select = |e: &mut Editor, b: usize, s: &str| {
        let a = find(e, b, s);
        e.set_caret(Pos::new(b, a), false);
        e.set_caret(Pos::new(b, a + s.len() as u32), true);
    };
    e.set_tracking(true);
    // Asha's pass.
    e.author = "Asha".into();
    e.now = "2026-11-14T09:00:00Z".into();
    select(&mut e, 2, "thirty");
    e.insert("fourteen");
    select(&mut e, 2, "two percent");
    e.insert("five percent");
    select(&mut e, 4, "sixty");
    e.insert("thirty");
    select(&mut e, 4, "Disputed amounts may be withheld until the dispute is settled.");
    e.toggle(Toggle::Bold);
    // Raj's pass.
    e.author = "Raj".into();
    e.now = "2026-11-14T15:00:00Z".into();
    select(&mut e, 4, " Payment is made by bank transfer to the account named on the invoice.");
    e.backspace(false);
    let end = e.doc.para(6).unwrap().text.len() as u32;
    e.set_caret(Pos::new(6, end), false);
    e.insert(" The warranty does not cover wear and tear.");
    select(&mut e, 6, "twelve");
    e.insert("twenty-four");
    e.set_caret(Pos::new(0, 0), false);
    let mut doc = e.doc.clone();
    doc.take_changes();
    println!("{} edits by Asha and Raj", doc.edits().len());
    for x in doc.edits() {
        println!("  {}", x.summary);
    }
    std::fs::write(&out, native::save_to_vec(&doc, &[])).unwrap();
    println!("{out}");
}
