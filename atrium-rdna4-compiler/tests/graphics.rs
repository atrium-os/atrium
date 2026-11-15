//! Vertex (NGG) and fragment shaders: SPIR-V → GIR → RDNA4. Every emitted
//! word must decode; the stage interface (exports, PS inputs) must match
//! what the pipeline sets up.

use atrium_rdna4_compiler::{compile, listing, ps_input, Binary};

fn build(name: &str) -> Binary {
    let bytes = std::fs::read(format!("{}/../atrium-gir/tests/shaders/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let opts = atrium_spv_frontend::Options { native_subgroups: true, builtin_blocks: true, ..Default::default() };
    let m = atrium_spv_frontend::translate_with_options(&bytes, &opts).unwrap();
    let mut g = atrium_gir::build::build(&m, 0).unwrap();
    atrium_gir::passes::optimize(&mut g);
    let b = compile(&g).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    let l = listing(&b.code);
    if std::env::var("RDNA4_LIST").is_ok() {
        println!("== {name}: {} dwords, {} VGPRs, {} SGPRs\n{l}", b.code.len(), b.vgprs, b.sgprs);
    }
    assert!(!l.contains('<'), "{name}: undecodable words\n{l}");
    b
}

#[test]
fn triangle_vertex() {
    let b = build("tri.vert.spv");
    assert_eq!(b.exports, vec![(12, 0xf)]);
    assert!(b.vgprs >= 5);
    assert_eq!(b.abi.user_sgprs(), 2);
}

#[test]
fn triangle_fragment() {
    let b = build("tri.frag.spv");
    assert_eq!(b.exports, vec![(0, 0xf)]);
    assert_eq!(b.ps_input_ena, ps_input::PERSP_CENTER);
}
