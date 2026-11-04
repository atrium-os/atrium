//! spill_listing <desktop-shader> <cap>: disassembly of a shader compiled
//! under a VGPR cap, to inspect spill code.
fn main() {
    let n = std::env::args().nth(1).unwrap();
    let cap: u32 = std::env::args().nth(2).map_or(16, |c| c.parse().unwrap());
    let (_, p, _) = atrium_rdna4_oracle::cases::DESKTOP.iter().find(|d| d.0 == n).unwrap();
    let bytes = std::fs::read(format!("{}/../bundles/{p}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let m = atrium_spv_frontend::translate_with_options(&bytes, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).unwrap();
    let mut g = atrium_gir::build::build(&m, 0).unwrap();
    atrium_gir::passes::optimize(&mut g);
    let b = atrium_rdna4_compiler::compile_with(&g, &atrium_rdna4_compiler::Options { max_vgprs: cap }).unwrap();
    print!("{}", atrium_rdna4_compiler::listing(&b.code));
}
