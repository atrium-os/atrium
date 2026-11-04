//! Compile the four desktop compute shaders: SPIR-V → GIR → optimise →
//! RDNA4. Every emitted word must decode.

fn compile(path: &str) -> Option<(atrium_gir::Function, atrium_rdna4_compiler::Binary)> {
    let bytes = std::fs::read(format!("{}/../bundles/{path}", env!("CARGO_MANIFEST_DIR"))).ok()?;
    let m = atrium_spv_frontend::translate(&bytes).unwrap();
    let mut g = atrium_gir::build::build(&m, 0).unwrap();
    atrium_gir::passes::optimize(&mut g);
    let b = atrium_rdna4_compiler::compile(&g).unwrap_or_else(|e| panic!("{path}: {e:?}"));
    Some((g, b))
}

#[test]
fn desktop_shaders_compile() {
    for p in ["atrium-core/compute/op_rectangle.comp.spv", "atrium-core/compute/op_path.comp.spv",
              "atrium-core/compute/op_texture.comp.spv", "atrium-text/compute/op_glyph_run.comp.spv"] {
        let Some((g, b)) = compile(p) else { continue };
        let l = atrium_rdna4_compiler::listing(&b.code);
        if std::env::var("RDNA4_GIR").is_ok() { println!("== {p}\n{g}"); }
        if std::env::var("RDNA4_LIST").is_ok() { println!("== {p}: {} dwords, {} VGPRs, {} SGPRs\n{l}", b.code.len(), b.vgprs, b.sgprs); }
        assert!(!l.contains('<'), "{p}: undecodable words\n{l}");
        eprintln!("{p}: {} dwords, {} VGPRs, {} SGPRs", b.code.len(), b.vgprs, b.sgprs);
    }
}
