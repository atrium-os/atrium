//! Every pass preserves behaviour: the interpreter gives identical buffers
//! before and after each pass, and the verifier passes after each one.

use atrium_gir::interp::{self, Buffers};
use atrium_gir::passes::{cse, dce, fold, mem2reg, uniformity};
use atrium_gir::{verify::verify, Function, Node, Op, Space};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

fn desktop_inputs(shader: &str, nodes: u32) -> (Buffers, u32) {
    let mut r = Rng(7);
    let mut f = |n: usize| -> Vec<u8> { (0..n).flat_map(|_| (1.0 + (r.next() % 2000) as f32 / 8.0).to_le_bytes()).collect() };
    let mut scene = f(4 + 16 * nodes as usize);
    scene[0..4].copy_from_slice(&nodes.to_le_bytes());
    let mut b = Buffers::new();
    if shader.contains("glyph") {
        let mut total = 0u32;
        for i in 0..nodes as usize {
            let count = (i as u32 * 7) % 4;
            let meta = 16 + 64 * i + 48;
            scene[meta..meta + 4].copy_from_slice(&count.to_le_bytes());
            scene[meta + 4..meta + 8].copy_from_slice(&total.to_le_bytes());
            total += count;
        }
        b.insert((0, 1), f(12 * total as usize));
        b.insert((0, 2), vec![0; 48 * total as usize]);
        b.insert((0, 3), vec![0; 4]);
    } else {
        b.insert((0, 1), vec![0; 64 * nodes as usize]);
        b.insert((0, 2), vec![0; 4]);
    }
    b.insert((0, 0), scene);
    (b, (nodes + 63) / 64 + 1)
}

fn tierb_inputs(g: &Function) -> Buffers {
    let fill = |n: u32, k: u32| -> Vec<u8> { (0..n / 4).flat_map(|i| (i % 7 + k).to_le_bytes()).collect() };
    let mut b: Buffers = g.buffers.iter().map(|&(s, bi)| ((s, bi), fill(1024, bi + 1))).collect();
    b.insert(interp::PUSH, fill(64, 3));
    b
}

fn run(g: &Function, b: &Buffers, groups: u32) -> Buffers {
    let mut b = b.clone();
    interp::run(g, [groups, 1, 1], &mut b).expect("interp");
    b
}

fn count(r: &[Node]) -> usize {
    r.iter().map(|n| match n {
        Node::Inst(_) => 1,
        Node::If { then, els, .. } => 1 + count(then) + count(els),
        Node::Loop { body, .. } => 1 + count(body),
    }).sum()
}

fn private_loads(r: &[Node]) -> usize {
    r.iter().map(|n| match n {
        Node::Inst(i) => matches!(i.op, Op::Load(Space::Private(_), _)) as usize,
        Node::If { then, els, .. } => private_loads(then) + private_loads(els),
        Node::Loop { body, .. } => private_loads(body),
    }).sum()
}

fn check(name: &str, mut g: Function, inputs: &Buffers, groups: u32) -> (usize, usize, Function) {
    let before = count(&g.body);
    let want = run(&g, inputs, groups);
    let steps: [(&str, fn(&mut Function)); 4] = [
        ("mem2reg", |f| { mem2reg::mem2reg(f); }),
        ("fold", |f| { fold::fold(f); }),
        ("cse", |f| { cse::cse(f); }),
        ("dce", |f| { dce::dce(f); }),
    ];
    for round in 0..3 {
        for (pass, p) in steps {
            p(&mut g);
            verify(&g).unwrap_or_else(|e| panic!("{name}: verify after {pass} (round {round}): {e:?}\n{g}"));
            let got = run(&g, inputs, groups);
            assert_eq!(got, want, "{name}: {pass} (round {round}) changed the result\n{g}");
        }
    }
    let _ = uniformity::analyse(&g);
    (before, count(&g.body), g)
}

#[test]
fn desktop_passes_preserve_results() {
    for p in ["atrium-core/compute/op_rectangle.comp.spv", "atrium-core/compute/op_path.comp.spv",
              "atrium-core/compute/op_texture.comp.spv", "atrium-text/compute/op_glyph_run.comp.spv"] {
        let Ok(bytes) = std::fs::read(format!("{}/../bundles/{p}", env!("CARGO_MANIFEST_DIR"))) else { continue };
        let g = atrium_gir::build::build(&atrium_spv_frontend::translate(&bytes).unwrap(), 0).unwrap();
        for nodes in [1, 70] {
            let (b, groups) = desktop_inputs(p, nodes);
            let (before, after, g2) = check(p, g.clone(), &b, groups);
            if nodes == 1 {
                eprintln!("{p}: {before} -> {after} nodes, {} private loads left", private_loads(&g2.body));
                if std::env::var("GIR_PRINT").is_ok() { eprintln!("{g2}"); }
                assert_eq!(private_loads(&g2.body), 0, "{p}: locals left after mem2reg");
            }
        }
    }
}

#[test]
fn tierb_passes_preserve_results() {
    let dir = format!("{}/../atrium-vk-icd/examples/shaders", env!("CARGO_MANIFEST_DIR"));
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut paths: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".comp.spv")).collect();
    paths.sort();
    for p in paths {
        let n = p.file_name().unwrap().to_string_lossy().to_string();
        let Ok(m) = atrium_spv_frontend::translate(&std::fs::read(&p).unwrap()) else { continue };
        let Ok(g) = atrium_gir::build::build(&m, 0) else { continue };
        let b = tierb_inputs(&g);
        let (before, after, _) = check(&n, g, &b, 1);
        eprintln!("{n}: {before} -> {after}");
    }
}

#[test]
fn uniformity_on_op_rectangle() {
    let Ok(bytes) = std::fs::read(format!("{}/../bundles/atrium-core/compute/op_rectangle.comp.spv", env!("CARGO_MANIFEST_DIR"))) else { return };
    let mut g = atrium_gir::build::build(&atrium_spv_frontend::translate(&bytes).unwrap(), 0).unwrap();
    atrium_gir::passes::optimize(&mut g);
    let u = uniformity::analyse(&g);
    let mut seen = (false, false, false);
    fn walk(r: &[Node], g: &Function, u: &uniformity::Uniformity, seen: &mut (bool, bool, bool)) {
        for n in r {
            match n {
                Node::Inst(i) => match (&i.op, i.result) {
                    // node_count: read-only buffer at a constant offset.
                    (Op::Load(Space::Buffer { binding: 0, .. }, o), Some(v)) if matches!(find_const(g, *o), Some(0)) => {
                        assert!(u.is_uniform(v), "node_count load should be uniform");
                        seen.0 = true;
                    }
                    (Op::Builtin(_), Some(v)) => { assert!(!u.is_uniform(v)); seen.1 = true; }
                    (Op::Atomic(..), Some(v)) => { assert!(!u.is_uniform(v)); seen.2 = true; }
                    _ => {}
                },
                Node::If { cond, then, els, .. } => {
                    assert!(!u.is_uniform(*cond), "id >= node_count is divergent");
                    walk(then, g, u, seen);
                    walk(els, g, u, seen);
                }
                Node::Loop { body, .. } => walk(body, g, u, seen),
            }
        }
    }
    fn find_const(g: &Function, v: atrium_gir::ValueId) -> Option<u32> {
        g.body.iter().find_map(|n| match n {
            Node::Inst(i) if i.result == Some(v) => match i.op { Op::Const(b) => Some(b), _ => None },
            _ => None,
        })
    }
    walk(&g.body, &g, &u, &mut seen);
    assert_eq!(seen, (true, true, true));
}
