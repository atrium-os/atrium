//! The four desktop compute shaders build into GIR and verify.

pub fn spv(path: &str) -> Option<Vec<u8>> {
    std::fs::read(format!("{}/../bundles/{path}", env!("CARGO_MANIFEST_DIR"))).ok()
}

pub const SHADERS: [&str; 4] = [
    "atrium-core/compute/op_rectangle.comp.spv",
    "atrium-core/compute/op_path.comp.spv",
    "atrium-core/compute/op_texture.comp.spv",
    "atrium-text/compute/op_glyph_run.comp.spv",
];

#[test]
fn build_and_verify() {
    for p in SHADERS {
        let Some(bytes) = spv(p) else { eprintln!("skip {p}: not built (bundles/*/build.sh)"); continue };
        let m = atrium_spv_frontend::translate(&bytes).unwrap();
        let g = atrium_gir::build::build(&m, 0).unwrap_or_else(|e| panic!("{p}: {e:?}"));
        if std::env::var("GIR_PRINT").is_ok() { println!("{g}"); }
        atrium_gir::verify::verify(&g).unwrap_or_else(|e| panic!("{p}: {e:?}\n{g}"));
    }
}
