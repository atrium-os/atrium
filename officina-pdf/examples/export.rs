//! Exports a native `.owrite` file as PDF.
//!
//! cargo run --release --example export IN.owrite OUT.pdf

use officina_text::layout::PinnedFonts;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (inp, outp) = (a.get(1).expect("IN.owrite"), a.get(2).expect("OUT.pdf"));
    let data = std::fs::read(inp).expect("read");
    let doc = officina_text::native::load(&data).expect("an .owrite file").doc;
    let fonts = PinnedFonts::new().expect("fonts");
    let t = std::time::Instant::now();
    let f = std::io::BufWriter::new(std::fs::File::create(outp).expect("create"));
    let (_, s) = officina_pdf::export(&doc, &fonts, &fonts, &officina_pdf::Info { title: "Sample".into() }, f, &mut |_, _| {}).expect("export");
    println!("{s:?} in {:.2?}", t.elapsed());
}
