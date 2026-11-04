//! list <name>: compile a Tier-B example and print the listing.
fn main() {
    let n = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(format!("{}/../atrium-vk-icd/examples/shaders/{n}.comp.spv", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let m = atrium_spv_frontend::translate(&bytes).unwrap();
    let mut g = atrium_gir::build::build(&m, 0).unwrap();
    atrium_gir::passes::optimize(&mut g);
    let b = atrium_rdna4_compiler::compile(&g).unwrap();
    print!("{}", atrium_rdna4_compiler::listing(&b.code));
}
