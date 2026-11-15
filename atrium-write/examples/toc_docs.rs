//! Writes `scratch/report-toc.owrite`: a report of a few pages with headings,
//! a table of contents after the title and a cross-reference to a section,
//! for looking at contents and references in Write.
//!
//! cargo run --example toc_docs [DIR]

use officina_core::history::Op;
use officina_text::fields;
use officina_text::model::{CharAttrs, Field, TextDoc, OBJECT_CHAR};
use officina_text::native;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "../scratch".into());
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let title = d.styles.by_name("Title").unwrap();
    let h1 = d.styles.by_name("Heading 1").unwrap();
    let h2 = d.styles.by_name("Heading 2").unwrap();
    d.push_para("Annual Report", title, vec![]);
    let body = "The figures in this part were gathered over the year from every office and checked twice before they were written down here. ";
    let chapters = [("Overview", &["Highlights", "The year in numbers"][..]), ("Operations", &["Offices", "Suppliers", "Logistics"][..]), ("Finances", &["Revenue", "Costs"][..]), ("Outlook", &["Plans for next year"][..])];
    for (c, secs) in chapters {
        d.push_para(c, h1, vec![]);
        for s in secs {
            d.push_para(s, h2, vec![]);
            for _ in 0..4 {
                d.push_para(&body.repeat(3), 0, vec![]);
            }
        }
    }
    for op in fields::toc_ops(&mut d, 1, 3) {
        op.apply(&mut d);
    }
    // In Outlook: "As set out in Costs (page N), …"
    let costs = d.blocks.iter().filter_map(|b| b.as_para()).find(|p| p.text == "Costs" && p.props.toc.is_none()).unwrap().id;
    let tr = d.attrs.intern(CharAttrs { field: Some(Field::TextRef(costs)), italic: Some(true), ..Default::default() });
    let pr = d.attrs.intern(CharAttrs { field: Some(Field::PageRef(costs)), ..Default::default() });
    let at = d.blocks.iter().position(|b| b.as_para().is_some_and(|p| p.text == "Plans for next year")).unwrap() + 1;
    let text = format!("As set out in {OBJECT_CHAR} (page {OBJECT_CHAR}), next year keeps spending where it was and grows only where the offices asked for it.");
    let a = "As set out in ".len() as u32;
    let b = a + 3 + " (page ".len() as u32;
    let k = d.push_para(&text, 0, vec![(0, 0), (a, tr), (a + 3, 0), (b, pr), (b + 3, 0)]);
    let blk = d.blocks.remove(k).unwrap();
    d.blocks.insert(at, blk);
    std::fs::write(format!("{dir}/report-toc.owrite"), native::save_to_vec(&d, &[])).unwrap();
    println!("wrote {dir}/report-toc.owrite");
}
