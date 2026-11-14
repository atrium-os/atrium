//! Our reader, our writer, our reader again: prints both texts of a `.docx`.
//!
//! cargo run --example rt_text FILE
fn main() {
    let p = std::env::args().nth(1).expect("FILE");
    let data = std::fs::read(&p).expect("read");
    let (d, _) = officina_convert::docx::read(&data).expect("read");
    println!("{:?}", d.plain_text());
    let b = officina_convert::docx::write(&d, Vec::new()).expect("write");
    let (e, _) = officina_convert::docx::read(&b).expect("read again");
    println!("{:?}", e.plain_text());
}
