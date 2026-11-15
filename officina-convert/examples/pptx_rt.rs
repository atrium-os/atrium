//! Reads a `.pptx`, writes it back with our writer, reads that again; says
//! what failed (development aid for O6c).
//!
//! cargo run --example pptx_rt -- IN.pptx [OUT.pptx]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let (mut d, l) = officina_convert::pptx::read(&data).unwrap();
    for n in l.report.lines() {
        eprintln!("note {n}");
    }
    if std::env::var("TEXT").is_ok() {
        for i in 0..d.slide_count() {
            let s = d.slide(i).unwrap().clone();
            for sh in &s.shapes {
                println!("slide {i} shape {:?} {:?} text {:?}", sh.name, sh.ph, sh.text().map(|t| t.text()));
            }
        }
    }
    let mut out = Vec::new();
    officina_convert::pptx::write(&mut d, &mut out).unwrap();
    if let Some(o) = a.get(2) {
        std::fs::write(o, &out).unwrap();
    }
    match officina_convert::pptx::read(&out) {
        Ok((d2, _)) => println!("ok: {} slides -> {}", d.slide_count(), d2.slide_count()),
        Err(e) => println!("reading ours: {e}"),
    }
}
