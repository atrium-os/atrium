//! Writes `scratch/newsletter.owrite`: a newsletter page with a one-column
//! title, two balanced columns, three columns with lines between, and one
//! column again, for looking at sections and columns in Write.
//!
//! cargo run --example columns_docs [DIR]

use officina_text::model::{Section, SectionStart, TextDoc};
use officina_text::native;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "../scratch".into());
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let title = d.styles.by_name("Title").unwrap();
    let h2 = d.styles.by_name("Heading 2").unwrap();
    d.push_para("The Office Newsletter", title, vec![]);
    let body = "Every office sent in its news this month, and most of it is good: the new building opens in spring, the cafeteria has a new menu and the library stays open later. ";
    let mut marks = Vec::new();
    marks.push((d.blocks.len(), Section { start: SectionStart::Continuous, columns: 2, ..Section::default() }));
    for h in ["News from the offices", "The new building"] {
        d.push_para(h, h2, vec![]);
        d.push_para(&body.repeat(2), 0, vec![]);
        d.push_para(&body.repeat(1), 0, vec![]);
    }
    marks.push((d.blocks.len(), Section { start: SectionStart::Continuous, columns: 3, gap: 360, rule: true, page: None }));
    for h in ["Menu", "Library", "Sports"] {
        d.push_para(h, h2, vec![]);
        d.push_para(&body.repeat(2), 0, vec![]);
    }
    marks.push((d.blocks.len(), Section { start: SectionStart::Continuous, ..Section::default() }));
    d.push_para("Send your news for next month to the editors by the twentieth.", 0, vec![]);
    for (k, s) in marks {
        d.blocks.update(k, |b| b.as_para_mut().unwrap().props.section = Some(s));
    }
    std::fs::write(format!("{dir}/newsletter.owrite"), native::save_to_vec(&d, &[])).unwrap();
    println!("wrote {dir}/newsletter.owrite");
}
