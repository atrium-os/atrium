//! Prints our reader's text of a `.docx` or `.odt` (one paragraph a line) and its notes.
//!
//! cargo run --example dump_text FILE
fn main() {
    let p = std::env::args().nth(1).expect("FILE");
    let data = std::fs::read(&p).expect("read");
    let r = if p.ends_with(".odt") { officina_convert::odt::read(&data).map(|(d, l)| (d, l.report.lines())).map_err(|e| e.to_string()) } else { officina_convert::docx::read(&data).map(|(d, l)| (d, l.report.lines())).map_err(|e| e.to_string()) };
    match r {
        Ok((d, notes)) => {
            println!("{}", d.plain_text());
            for n in notes {
                eprintln!("note {n}");
            }
        }
        Err(e) => eprintln!("error {e}"),
    }
}
