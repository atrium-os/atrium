//! Vertex and fragment shaders: SPIR-V → GIR, run on the interpreter.

use atrium_gir::interp::{run_graphics, Buffers, Invocation};
use atrium_gir::{OutputKind, Stage};

fn build(name: &str) -> atrium_gir::Function {
    let bytes = std::fs::read(format!("{}/tests/shaders/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let opts = atrium_spv_frontend::Options { native_subgroups: true, builtin_blocks: true, ..Default::default() };
    let m = atrium_spv_frontend::translate_with_options(&bytes, &opts).unwrap();
    let mut g = atrium_gir::build::build(&m, 0).unwrap();
    let before = g.clone();
    atrium_gir::passes::optimize(&mut g);
    if std::env::var("GIR_DUMP").is_ok() {
        eprintln!("== {name}\n{before}\n-- optimised\n{g}");
    }
    g
}

fn f(x: u32) -> f32 {
    f32::from_bits(x)
}

/// The 7b triangle's vertex shader, from GLSL: positions by vertex index.
#[test]
fn triangle_vertex() {
    let g = build("tri.vert.spv");
    assert_eq!(g.stage, Stage::Vertex);
    assert_eq!(g.outputs.len(), 1);
    assert_eq!(g.outputs[0].kind, OutputKind::Position);
    let invs: Vec<Invocation> = (0..4).map(|i| Invocation { vertex_index: i, ..Default::default() }).collect();
    let out = run_graphics(&g, &invs, &mut Buffers::new()).unwrap();
    let want = [[-0.5, -0.5], [0.5, -0.5], [0.25, 0.75], [-0.5, -0.5]];
    for (i, o) in out.iter().enumerate() {
        assert_eq!(o.len(), 4);
        assert_eq!([f(o[0]), f(o[1]), f(o[2]), f(o[3])], [want[i][0], want[i][1], 0.0, 1.0], "vertex {i}");
    }
}

/// A constant colour to location 0.
#[test]
fn triangle_fragment() {
    let g = build("tri.frag.spv");
    assert_eq!(g.stage, Stage::Fragment);
    assert_eq!(g.outputs.len(), 1);
    assert_eq!(g.outputs[0].kind, OutputKind::Location(0));
    let out = run_graphics(&g, &[Invocation::default()], &mut Buffers::new()).unwrap();
    assert_eq!(out[0].iter().map(|x| f(*x)).collect::<Vec<_>>(), [1.0, 1.0, 0.0, 1.0]);
}

/// Varyings: a colour per vertex and a flat integer.
#[test]
fn varyings() {
    let vs = build("tri_color.vert.spv");
    let kinds: Vec<OutputKind> = vs.outputs.iter().map(|o| o.kind).collect();
    assert_eq!(kinds, [OutputKind::Position, OutputKind::Location(0), OutputKind::Location(1)]);
    let invs: Vec<Invocation> = (0..3).map(|i| Invocation { vertex_index: i, ..Default::default() }).collect();
    let out = run_graphics(&vs, &invs, &mut Buffers::new()).unwrap();
    assert_eq!(out[1][4..], [0.0f32.to_bits(), 1.0f32.to_bits(), 0.0f32.to_bits(), 7]);

    let ps = build("tri_color.frag.spv");
    assert_eq!(ps.inputs.len(), 2);
    assert_eq!((ps.inputs[0].location, ps.inputs[0].interp), (0, atrium_gir::Interp::Smooth));
    assert_eq!((ps.inputs[1].location, ps.inputs[1].interp), (1, atrium_gir::Interp::Flat));
    let inv = Invocation { inputs: vec![vec![0.25f32.to_bits(), 0.5f32.to_bits(), 0.125f32.to_bits()], vec![7]], ..Default::default() };
    let out = run_graphics(&ps, &[inv], &mut Buffers::new()).unwrap();
    assert_eq!(out[0].iter().map(|x| f(*x)).collect::<Vec<_>>(), [0.25, 0.5, 0.125, 1.0]);
}
