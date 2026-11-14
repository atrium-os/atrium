//! Writes two versions of a document, for looking at Compare:
//! `scratch/agreement-v1.owrite` and `scratch/agreement-v2.owrite`.
//!
//! cargo run --example compare_docs [DIR]

use officina_text::model::{CharAttrs, TextDoc};
use officina_text::native;

fn version(paras: &[(&str, &str)], bold: &[&str]) -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let b = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
    for (style, text) in paras {
        let st = d.styles.by_name(style).unwrap_or(0);
        let mut runs = vec![(0, 0)];
        for w in bold {
            if let Some(i) = text.find(w) {
                runs = vec![(0, 0), (i as u32, b), ((i + w.len()) as u32, 0)];
            }
        }
        d.push_para(text, st, runs);
    }
    d.take_changes();
    d
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "../scratch".into());
    let v1 = version(&[
        ("Title", "Supply agreement"),
        ("Heading 1", "1. Delivery"),
        ("", "The supplier shall deliver the goods within thirty days of each order, to the address the buyer names in the order."),
        ("", "Late delivery entitles the buyer to a discount of two percent for each week of delay."),
        ("Heading 1", "2. Payment"),
        ("", "The buyer shall pay each invoice within sixty days. Payment is made by bank transfer to the account named on the invoice."),
        ("", "Disputed amounts may be withheld until the dispute is settled."),
        ("Heading 1", "3. Warranty"),
        ("", "The supplier warrants that the goods are free from defects for twelve months after delivery."),
    ], &[]);
    let v2 = version(&[
        ("Title", "Supply agreement"),
        ("Heading 1", "1. Delivery"),
        ("", "The supplier shall deliver the goods within fourteen days of each order, to the address the buyer names in the order."),
        ("", "Late delivery entitles the buyer to a discount of five percent for each week of delay."),
        ("Heading 1", "2. Payment"),
        ("", "Disputed amounts may be withheld until the dispute is settled."),
        ("", "The buyer shall pay each invoice within thirty days. Payment is made by bank transfer to the account named on the invoice."),
        ("Heading 1", "3. Warranty"),
        ("", "The supplier warrants that the goods are free from defects for twenty-four months after delivery."),
        ("", "The warranty does not cover wear and tear."),
    ], &["free from defects"]);
    for (name, d) in [("agreement-v1.owrite", &v1), ("agreement-v2.owrite", &v2)] {
        std::fs::write(format!("{dir}/{name}"), native::save_to_vec(d, &[])).unwrap();
    }
    let c = v1.compare(&v2, "Compare", "");
    for e in c.edits() {
        println!("{}", e.summary);
    }
}
