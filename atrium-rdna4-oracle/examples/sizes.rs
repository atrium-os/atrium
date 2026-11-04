//! sizes: compiled size of the desktop compute shaders, in dwords.
fn main() {
    for (n, p, _) in atrium_rdna4_oracle::cases::DESKTOP {
        let bytes = std::fs::read(format!("{}/../bundles/{p}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let m = atrium_spv_frontend::translate_with_options(&bytes, &atrium_spv_frontend::Options { native_subgroups: true, ..Default::default() }).unwrap();
        let mut g = atrium_gir::build::build(&m, 0).unwrap();
        atrium_gir::passes::optimize(&mut g);
        let b = atrium_rdna4_compiler::compile(&g).unwrap();
        println!("{n:<14} {:4} dwords {:3} vgprs", b.code.len(), b.vgprs);
    }
}
