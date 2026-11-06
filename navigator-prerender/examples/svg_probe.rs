//! svg_probe <file.svg>: what the worker's SVG profile and vector
//! conversion make of one SVG (review tooling).
fn main() {
    let p = std::env::args().nth(1).unwrap();
    let b = std::fs::read(&p).unwrap();
    match navigator_svg::load(&b) {
        Ok((tree, _)) => {
            println!("size {:?}", tree.size());
            match navigator_svg::vector::fills(&tree) { Ok(v) => println!("fills {}", v.len()), Err(e) => println!("vector refused: {e}") }
        }
        Err(e) => println!("refused: {e:?}"),
    }
}
