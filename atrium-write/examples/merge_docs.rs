//! Writes two reviewed copies of one agreement, for looking at Merge:
//! `scratch/agreement-ann.owrite` (Ann's edits tracked) and
//! `scratch/Ben.owrite` (Ben's, edited without tracking).
//!
//! cargo run --example merge_docs [DIR]

use officina_text::model::TextDoc;
use officina_text::native;

fn version(paras: &[(&str, &str)]) -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    for (style, text) in paras {
        let st = d.styles.by_name(style).unwrap_or(0);
        d.push_para(text, st, vec![(0, 0)]);
    }
    d.take_changes();
    d
}

const ORIGINAL: [(&str, &str); 9] = [
    ("Title", "Supply agreement"),
    ("Heading 1", "1. Delivery"),
    ("", "The supplier shall deliver the goods within thirty days of each order, to the address the buyer names in the order."),
    ("", "Late delivery entitles the buyer to a discount of two percent for each week of delay."),
    ("Heading 1", "2. Payment"),
    ("", "The buyer shall pay each invoice within sixty days. Payment is made by bank transfer to the account named on the invoice."),
    ("", "Disputed amounts may be withheld until the dispute is settled."),
    ("Heading 1", "3. Warranty"),
    ("", "The supplier warrants that the goods are free from defects for twelve months after delivery."),
];

fn edited(changes: &[(usize, &str)], extra: Option<(usize, &str)>) -> TextDoc {
    let mut v: Vec<(&str, &str)> = ORIGINAL.to_vec();
    for &(k, t) in changes {
        v[k].1 = t;
    }
    if let Some((k, t)) = extra {
        v.insert(k, ("", t));
    }
    version(&v)
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "../scratch".into());
    let original = version(&ORIGINAL);
    // Ann: fourteen days for delivery, twenty-four months of warranty, tracked.
    let ann = edited(&[(2, "The supplier shall deliver the goods within fourteen days of each order, to the address the buyer names in the order."), (8, "The supplier warrants that the goods are free from defects for twenty-four months after delivery.")], None);
    let ann = original.compare(&ann, "Ann", "2026-11-15T09:00:00Z");
    // Ben: twenty-one days (a conflict with Ann), five percent, thirty days to
    // pay, and a sentence of his own; no tracking.
    let ben = edited(
        &[
            (2, "The supplier shall deliver the goods within twenty-one days of each order, to the address the buyer names in the order."),
            (3, "Late delivery entitles the buyer to a discount of five percent for each week of delay."),
            (5, "The buyer shall pay each invoice within thirty days. Payment is made by bank transfer to the account named on the invoice."),
        ],
        Some((9, "The warranty does not cover wear and tear.")),
    );
    for (name, d) in [("agreement-ann.owrite", &ann), ("Ben.owrite", &ben)] {
        std::fs::write(format!("{dir}/{name}"), native::save_to_vec(d, &[])).unwrap();
    }
    let (m, mg) = ann.merged(&ben, "Ben", "2026-11-15T10:00:00Z").unwrap();
    println!("{} added, {} conflicts, {} alike", mg.applied, mg.conflicts, mg.same);
    for e in m.edits() {
        let who = m.revisions.get(e.rev).map(|r| r.author.to_string()).unwrap_or_default();
        println!("{who}: {}", e.summary);
    }
}
