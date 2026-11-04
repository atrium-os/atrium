//! gir <name>: print the optimised GIR of a Tier-B example.
fn main() {
    let n = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(format!("{}/shaders/{n}.comp.spv", env!("CARGO_MANIFEST_DIR")))
        .or_else(|_| std::fs::read(format!("{}/../atrium-vk-icd/examples/shaders/{n}.comp.spv", env!("CARGO_MANIFEST_DIR"))))
        .unwrap();
    let m = atrium_spv_frontend::translate_with_options(&bytes, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).unwrap();
    let mut g = atrium_gir::build::build(&m, 0).unwrap();
    if std::env::var("RAW").is_err() { atrium_gir::passes::optimize(&mut g); }
    if let Err(e) = atrium_gir::verify::verify(&g) { eprintln!("VERIFY: {e:?}"); }
    println!("{g}");
}
