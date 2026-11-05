//! Op-level differential tests for the bespoke x86 backend: integer math,
//! division, shifts, every int and float compare, saturating conversions,
//! a loop whose phis swap (the parallel-copy case) and a switch with a phi
//! at the merge. Each shader reads `{i32 a; i32 b; f32 x; f32 y}` from push
//! constants and writes four results, ints bitcast to float. Results are
//! compared bit for bit across the interpreter, Cranelift and bespoke-x86,
//! over inputs that include NaN, infinities, signed zeros and INT_MIN.
//!
//! Division by zero and INT_MIN / -1 are undefined in SPIR-V and trap in
//! Cranelift's x86 code, so those inputs check bespoke-x86 alone against
//! the values the arm64 backend produces.

use atrium_spv_differential::{BespokeX86Runner, CraneliftRunner};
use atrium_spv_tests::harness::{BackendError, InterpreterRunner, ShaderRunner};
use atrium_spv_tests::interpreter::ShaderInputs;
use rspirv::dr::Builder;
use rspirv::spirv::Word;

struct Ctx {
    i32_ty: Word,
    f32_ty: Word,
    bool_ty: Word,
    a: Word,
    b: Word,
    x: Word,
    y: Word,
    one: Word,
    zero: Word,
    entry: Word,
}

impl Ctx {
    fn bits(&self, b: &mut Builder, v: Word) -> Word {
        b.bitcast(self.f32_ty, None, v).unwrap()
    }
    fn sel(&self, b: &mut Builder, c: Word) -> Word {
        b.select(self.f32_ty, None, c, self.one, self.zero).unwrap()
    }
    fn int(&self, b: &mut Builder, v: i32) -> Word {
        b.constant_bit32(self.i32_ty, v as u32)
    }
}

/// Build a fragment shader. `body` emits the entry block's code (it may add
/// blocks) and returns the four output components.
fn build(body: impl FnOnce(&mut Builder, &Ctx) -> [Word; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand::LiteralBit32;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc = b.type_struct(vec![i32_ty, i32_ty, f32_ty, f32_ty]);
    for (m, off) in [0u32, 4, 8, 12].iter().enumerate() {
        b.member_decorate(pc, m as u32, Decoration::Offset, vec![LiteralBit32(*off)]);
    }
    b.decorate(pc, Decoration::Block, vec![]);
    let ptr_pc = b.type_pointer(None, StorageClass::PushConstant, pc);
    let ptr_i = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_f = b.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let idx: Vec<Word> = (0..4).map(|i| b.constant_bit32(i32_ty, i)).collect();
    let one = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let zero = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let pc_var = b.variable(ptr_pc, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    let entry = b.id();
    b.begin_block(Some(entry)).unwrap();
    let load = |b: &mut Builder, k: usize, pt: Word, t: Word| {
        let p = b.access_chain(pt, None, pc_var, vec![idx[k]]).unwrap();
        b.load(t, None, p, None, vec![]).unwrap()
    };
    let a = load(&mut b, 0, ptr_i, i32_ty);
    let bb = load(&mut b, 1, ptr_i, i32_ty);
    let x = load(&mut b, 2, ptr_f, f32_ty);
    let y = load(&mut b, 3, ptr_f, f32_ty);
    let ctx = Ctx { i32_ty, f32_ty, bool_ty, a, b: bb, x, y, one, zero, entry };
    let comps = body(&mut b, &ctx);
    let color = b.composite_construct(vec4, None, comps.to_vec()).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn inputs(a: i32, b: i32, x: f32, y: f32) -> ShaderInputs {
    let mut s = ShaderInputs::default();
    s.push_constants[0..4].copy_from_slice(&a.to_le_bytes());
    s.push_constants[4..8].copy_from_slice(&b.to_le_bytes());
    s.push_constants[8..12].copy_from_slice(&x.to_le_bytes());
    s.push_constants[12..16].copy_from_slice(&y.to_le_bytes());
    s
}

const INTS: &[(i32, i32)] = &[
    (7, 3),
    (-7, 3),
    (7, -3),
    (-7, -3),
    (i32::MIN, 3),
    (i32::MAX, -2),
    (0, 5),
    (12_345_678, 31),
    (-1, 33),
    (100, 100),
    (-100, 7),
];

const FLOATS: &[(f32, f32)] = &[
    (1.5, 2.5),
    (2.5, 1.5),
    (1.0, 1.0),
    (f32::NAN, 1.0),
    (1.0, f32::NAN),
    (0.0, -0.0),
    (-3.7, f32::INFINITY),
    (3e9, -3e9),
    (-0.5, 1e10),
    (4.5e9, f32::NEG_INFINITY),
    (3.7, -2.2),
];

fn run_bits(r: &dyn ShaderRunner, spirv: &[u8], inp: &ShaderInputs) -> [u32; 4] {
    let out = r.run(spirv, inp).unwrap_or_else(|e| panic!("{}: {e}", r.name()));
    out.pixels[0].map(f32::to_bits)
}

/// Three-way bit-exact agreement over every input pair.
fn agree(name: &str, spirv: &[u8], skip: impl Fn(i32, i32) -> bool) {
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    let (interp, cl, x86) = (InterpreterRunner::default(), CraneliftRunner, BespokeX86Runner);
    for (k, &(a, b)) in INTS.iter().enumerate() {
        if skip(a, b) {
            continue;
        }
        let (x, y) = FLOATS[k % FLOATS.len()];
        let inp = inputs(a, b, x, y);
        let want = run_bits(&cl, spirv, &inp);
        let got = run_bits(&x86, spirv, &inp);
        assert_eq!(got, want, "{name}: bespoke-x86 vs cranelift, a={a} b={b} x={x} y={y}");
        // The interpreter lacks a few opcodes (Bitcast); where it runs, it
        // must agree too.
        match interp.run(spirv, &inp) {
            Ok(o) => assert_eq!(
                o.pixels[0].map(f32::to_bits),
                want,
                "{name}: interpreter vs cranelift, a={a} b={b} x={x} y={y}"
            ),
            Err(BackendError::Unsupported(_)) => {}
            Err(e) => panic!("{name}: interpreter: {e}"),
        }
    }
}

fn none(_: i32, _: i32) -> bool {
    false
}

#[test]
fn int_arith_and_shifts() {
    let s = build(|b, c| {
        let m31 = c.int(b, 31);
        let sh = b.bitwise_and(c.i32_ty, None, c.b, m31).unwrap();
        let v = [
            b.i_mul(c.i32_ty, None, c.a, c.b).unwrap(),
            b.shift_left_logical(c.i32_ty, None, c.a, sh).unwrap(),
            b.shift_right_logical(c.i32_ty, None, c.a, sh).unwrap(),
            b.shift_right_arithmetic(c.i32_ty, None, c.a, sh).unwrap(),
        ];
        v.map(|w| c.bits(b, w))
    });
    agree("int_arith_and_shifts", &s, none);
    let s = build(|b, c| {
        let n = b.not(c.i32_ty, None, c.a).unwrap();
        let v = [
            b.bitwise_xor(c.i32_ty, None, n, c.b).unwrap(),
            b.s_negate(c.i32_ty, None, c.a).unwrap(),
            b.i_sub(c.i32_ty, None, c.a, c.b).unwrap(),
            b.bitwise_or(c.i32_ty, None, c.a, c.b).unwrap(),
        ];
        v.map(|w| c.bits(b, w))
    });
    agree("int_bitwise", &s, none);
}

fn div_shader() -> Vec<u8> {
    build(|b, c| {
        let v = [
            b.s_div(c.i32_ty, None, c.a, c.b).unwrap(),
            b.u_div(c.i32_ty, None, c.a, c.b).unwrap(),
            b.u_mod(c.i32_ty, None, c.a, c.b).unwrap(),
            b.s_mod(c.i32_ty, None, c.a, c.b).unwrap(),
        ];
        v.map(|w| c.bits(b, w))
    })
}

#[test]
fn int_division() {
    let s = div_shader();
    agree("int_division", &s, |a, b| b == 0 || (a == i32::MIN && b == -1));
}

#[test]
fn int_division_undefined_cases_are_safe() {
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    let s = div_shader();
    let x86 = BespokeX86Runner;
    // x / 0 = 0, x % 0 = x (both kinds), INT_MIN / -1 = INT_MIN, % = 0.
    let got = run_bits(&x86, &s, &inputs(-7, 0, 0.0, 0.0));
    assert_eq!(got, [0, 0, (-7i32) as u32, (-7i32) as u32]);
    let got = run_bits(&x86, &s, &inputs(i32::MIN, -1, 0.0, 0.0));
    // udiv/umod treat -1 as u32::MAX: 0x8000_0000 / 0xffff_ffff = 0 rem itself.
    assert_eq!(got, [i32::MIN as u32, 0, i32::MIN as u32, 0]);
}

#[test]
fn int_compares() {
    use rspirv::dr::Builder as B;
    type Cmp = fn(&mut B, Word, Word, Word) -> Word;
    let ops: [Cmp; 10] = [
        |b, t, x, y| b.i_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.i_not_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.s_less_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.s_less_than_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.s_greater_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.s_greater_than_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.u_less_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.u_less_than_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.u_greater_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.u_greater_than_equal(t, None, x, y).unwrap(),
    ];
    for chunk in ops.chunks(4) {
        let s = build(|b, c| {
            let mut out = [c.zero; 4];
            for (k, op) in chunk.iter().enumerate() {
                let v = op(b, c.bool_ty, c.a, c.b);
                out[k] = c.sel(b, v);
            }
            out
        });
        agree("int_compares", &s, none);
    }
}

#[test]
fn float_compares_with_nan() {
    use rspirv::dr::Builder as B;
    type Cmp = fn(&mut B, Word, Word, Word) -> Word;
    let ops: [Cmp; 12] = [
        |b, t, x, y| b.f_ord_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_ord_not_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_ord_less_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_ord_less_than_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_ord_greater_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_ord_greater_than_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_unord_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_unord_not_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_unord_less_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_unord_less_than_equal(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_unord_greater_than(t, None, x, y).unwrap(),
        |b, t, x, y| b.f_unord_greater_than_equal(t, None, x, y).unwrap(),
    ];
    for chunk in ops.chunks(4) {
        let s = build(|b, c| {
            let mut out = [c.zero; 4];
            for (k, op) in chunk.iter().enumerate() {
                let v = op(b, c.bool_ty, c.x, c.y);
                out[k] = c.sel(b, v);
            }
            out
        });
        agree("float_compares", &s, none);
    }
}

#[test]
fn conversions_saturate() {
    let s = build(|b, c| {
        let fs = b.convert_f_to_s(c.i32_ty, None, c.x).unwrap();
        let fu = b.convert_f_to_u(c.i32_ty, None, c.y).unwrap();
        [
            c.bits(b, fs),
            c.bits(b, fu),
            b.convert_s_to_f(c.f32_ty, None, c.a).unwrap(),
            b.convert_u_to_f(c.f32_ty, None, c.b).unwrap(),
        ]
    });
    agree("conversions", &s, none);
}

#[test]
fn select_float_and_int() {
    let s = build(|b, c| {
        let lt = b.s_less_than(c.bool_ty, None, c.a, c.b).unwrap();
        let fl = b.f_ord_less_than(c.bool_ty, None, c.x, c.y).unwrap();
        let si = b.select(c.i32_ty, None, fl, c.a, c.b).unwrap();
        [
            b.select(c.f32_ty, None, lt, c.x, c.y).unwrap(),
            b.select(c.f32_ty, None, fl, c.y, c.x).unwrap(),
            c.bits(b, si),
            c.sel(b, lt),
        ]
    });
    agree("select", &s, none);
}

/// Fibonacci by swapping phis: (p, q) ← (q, p + q), (a & 15) times.
#[test]
fn loop_with_swapping_phis() {
    use rspirv::spirv::LoopControl;
    let s = build(|b, c| {
        let entry = c.entry;
        let (h, body, cont, merge) = (b.id(), b.id(), b.id(), b.id());
        let (i, p, q, i1, s) = (b.id(), b.id(), b.id(), b.id(), b.id());
        let m15 = c.int(b, 15);
        let n = b.bitwise_and(c.i32_ty, None, c.a, m15).unwrap();
        let z = c.int(b, 0);
        let o = c.int(b, 1);
        b.branch(h).unwrap();

        b.begin_block(Some(h)).unwrap();
        b.phi(c.i32_ty, Some(i), vec![(z, entry), (i1, cont)]).unwrap();
        b.phi(c.i32_ty, Some(p), vec![(z, entry), (q, cont)]).unwrap();
        b.phi(c.i32_ty, Some(q), vec![(o, entry), (s, cont)]).unwrap();
        let cond = b.s_less_than(c.bool_ty, None, i, n).unwrap();
        b.loop_merge(merge, cont, LoopControl::NONE, vec![]).unwrap();
        b.branch_conditional(cond, body, merge, vec![]).unwrap();

        b.begin_block(Some(body)).unwrap();
        b.branch(cont).unwrap();

        b.begin_block(Some(cont)).unwrap();
        b.i_add(c.i32_ty, Some(s), p, q).unwrap();
        b.i_add(c.i32_ty, Some(i1), i, o).unwrap();
        b.branch(h).unwrap();

        b.begin_block(Some(merge)).unwrap();
        [c.bits(b, p), c.bits(b, q), c.bits(b, i), c.one]
    });
    agree("loop_with_swapping_phis", &s, none);
}

/// switch (a & 3) with a phi at the merge.
#[test]
fn switch_with_merge_phi() {
    use rspirv::dr::Operand::LiteralBit32;
    use rspirv::spirv::SelectionControl;
    let s = build(|b, c| {
        let entry = c.entry;
        let m3 = c.int(b, 3);
        let sel = b.bitwise_and(c.i32_ty, None, c.a, m3).unwrap();
        let (c0, c1, c2, merge) = (b.id(), b.id(), b.id(), b.id());
        let k15 = b.constant_bit32(c.f32_ty, 1.5f32.to_bits());
        let k25 = b.constant_bit32(c.f32_ty, 2.5f32.to_bits());
        b.selection_merge(merge, SelectionControl::NONE).unwrap();
        b.switch(sel, merge, vec![(LiteralBit32(0), c0), (LiteralBit32(1), c1), (LiteralBit32(2), c2)])
            .unwrap();
        for blk in [c0, c1, c2] {
            b.begin_block(Some(blk)).unwrap();
            b.branch(merge).unwrap();
        }
        b.begin_block(Some(merge)).unwrap();
        let v = b
            .phi(c.f32_ty, None, vec![(k15, c0), (k25, c1), (c.x, c2), (c.y, entry)])
            .unwrap();
        let sum = b.f_add(c.f32_ty, None, v, c.one).unwrap();
        [v, c.bits(b, sel), sum, c.zero]
    });
    agree("switch_with_merge_phi", &s, none);
}

// ── memory: dynamic indexing, locals, MRT, depth, builtins ──────────────

fn assemble(b: Builder) -> Vec<u8> {
    use rspirv::binary::Assemble;
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// `uniform { vec4 data[4]; }`, a local `float arr[4]` filled with
/// constant indices, then read back with dynamic ones:
/// out = (arr[a & 3], data[(a + 1) & 3].y, data[b & 3].z, x).
fn build_dynamic_index_shader() -> Vec<u8> {
    use rspirv::dr::Operand::LiteralBit32;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass as SC,
    };
    let mut b = Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let c4 = b.constant_bit32(i32_ty, 4);
    let arr_v4 = b.type_array(vec4, c4);
    b.decorate(arr_v4, Decoration::ArrayStride, vec![LiteralBit32(16)]);
    let ub = b.type_struct(vec![arr_v4]);
    b.member_decorate(ub, 0, Decoration::Offset, vec![LiteralBit32(0)]);
    b.decorate(ub, Decoration::Block, vec![]);
    let pc = b.type_struct(vec![i32_ty, i32_ty, f32_ty, f32_ty]);
    for (m, off) in [0u32, 4, 8, 12].iter().enumerate() {
        b.member_decorate(pc, m as u32, Decoration::Offset, vec![LiteralBit32(*off)]);
    }
    b.decorate(pc, Decoration::Block, vec![]);
    let arr_f = b.type_array(f32_ty, c4);
    let p_ub = b.type_pointer(None, SC::Uniform, ub);
    let p_ub_v4 = b.type_pointer(None, SC::Uniform, vec4);
    let p_pc = b.type_pointer(None, SC::PushConstant, pc);
    let p_pc_i = b.type_pointer(None, SC::PushConstant, i32_ty);
    let p_pc_f = b.type_pointer(None, SC::PushConstant, f32_ty);
    let p_fn_arr = b.type_pointer(None, SC::Function, arr_f);
    let p_fn_f = b.type_pointer(None, SC::Function, f32_ty);
    let p_out = b.type_pointer(None, SC::Output, vec4);
    let idx: Vec<u32> = (0..4).map(|i| b.constant_bit32(i32_ty, i)).collect();
    let c3 = b.constant_bit32(i32_ty, 3);
    let c1 = b.constant_bit32(i32_ty, 1);
    let ubv = b.variable(p_ub, None, SC::Uniform, None);
    b.decorate(ubv, Decoration::DescriptorSet, vec![LiteralBit32(0)]);
    b.decorate(ubv, Decoration::Binding, vec![LiteralBit32(0)]);
    let pcv = b.variable(p_pc, None, SC::PushConstant, None);
    let out = b.variable(p_out, None, SC::Output, None);
    b.decorate(out, Decoration::Location, vec![LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let arr = b.variable(p_fn_arr, None, SC::Function, None);
    let pa = b.access_chain(p_pc_i, None, pcv, vec![idx[0]]).unwrap();
    let a = b.load(i32_ty, None, pa, None, vec![]).unwrap();
    let pb = b.access_chain(p_pc_i, None, pcv, vec![idx[1]]).unwrap();
    let bv = b.load(i32_ty, None, pb, None, vec![]).unwrap();
    let px = b.access_chain(p_pc_f, None, pcv, vec![idx[2]]).unwrap();
    let x = b.load(f32_ty, None, px, None, vec![]).unwrap();
    for k in 0..4u32 {
        let kf = b.constant_bit32(f32_ty, (k as f32 + 0.25).to_bits());
        let v = b.f_mul(f32_ty, None, x, kf).unwrap();
        let p = b.access_chain(p_fn_f, None, arr, vec![idx[k as usize]]).unwrap();
        b.store(p, v, None, vec![]).unwrap();
    }
    let i = b.bitwise_and(i32_ty, None, a, c3).unwrap();
    let pi = b.access_chain(p_fn_f, None, arr, vec![i]).unwrap();
    let r0 = b.load(f32_ty, None, pi, None, vec![]).unwrap();
    let a1 = b.i_add(i32_ty, None, a, c1).unwrap();
    let j = b.bitwise_and(i32_ty, None, a1, c3).unwrap();
    let pj = b.access_chain(p_ub_v4, None, ubv, vec![idx[0], j]).unwrap();
    let dj = b.load(vec4, None, pj, None, vec![]).unwrap();
    let r1 = b.composite_extract(f32_ty, None, dj, vec![1]).unwrap();
    let kk = b.bitwise_and(i32_ty, None, bv, c3).unwrap();
    let pk = b.access_chain(p_ub_v4, None, ubv, vec![idx[0], kk]).unwrap();
    let dk = b.load(vec4, None, pk, None, vec![]).unwrap();
    let r2 = b.composite_extract(f32_ty, None, dk, vec![2]).unwrap();
    let color = b.composite_construct(vec4, None, vec![r0, r1, r2, x]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![ubv, pcv, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    assemble(b)
}

#[test]
fn dynamic_index_uniform_and_local_array() {
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    let s = build_dynamic_index_shader();
    let (interp, cl, x86) = (InterpreterRunner::default(), CraneliftRunner, BespokeX86Runner);
    let uniforms: Vec<u8> = (0..16).flat_map(|k| (k as f32 * 1.5 - 3.0).to_le_bytes()).collect();
    for &(a, bb) in INTS {
        let mut inp = inputs(a, bb, 2.5, 0.0);
        inp.uniforms = uniforms.clone();
        let want = run_bits(&cl, &s, &inp);
        assert_eq!(run_bits(&x86, &s, &inp), want, "a={a} b={bb}");
        if let Ok(o) = interp.run(&s, &inp) {
            assert_eq!(o.pixels[0].map(f32::to_bits), want, "interpreter a={a} b={bb}");
        }
    }
}

/// Two colour outputs (Location 0 and 1), gl_FragDepth, gl_FrontFacing
/// and gl_PrimitiveID, called through the full FsMain ABI (12 arguments,
/// two on the stack), compared byte for byte with Cranelift.
#[test]
fn mrt_depth_and_stack_builtins() {
    use rspirv::dr::Operand::LiteralBit32;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SC,
    };
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    let mut b = Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.capability(Capability::Geometry); // PrimitiveId in fragment
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let p_out4 = b.type_pointer(None, SC::Output, vec4);
    let p_outf = b.type_pointer(None, SC::Output, f32_ty);
    let p_in_b = b.type_pointer(None, SC::Input, bool_ty);
    let p_in_i = b.type_pointer(None, SC::Input, i32_ty);
    let p_in4 = b.type_pointer(None, SC::Input, vec4);
    let o0 = b.variable(p_out4, None, SC::Output, None);
    b.decorate(o0, Decoration::Location, vec![LiteralBit32(0)]);
    let o1 = b.variable(p_out4, None, SC::Output, None);
    b.decorate(o1, Decoration::Location, vec![LiteralBit32(1)]);
    let depth = b.variable(p_outf, None, SC::Output, None);
    b.decorate(depth, Decoration::BuiltIn, vec![rspirv::dr::Operand::BuiltIn(BuiltIn::FragDepth)]);
    let ff = b.variable(p_in_b, None, SC::Input, None);
    b.decorate(ff, Decoration::BuiltIn, vec![rspirv::dr::Operand::BuiltIn(BuiltIn::FrontFacing)]);
    let pid = b.variable(p_in_i, None, SC::Input, None);
    b.decorate(pid, Decoration::BuiltIn, vec![rspirv::dr::Operand::BuiltIn(BuiltIn::PrimitiveId)]);
    b.decorate(pid, Decoration::Flat, vec![]);
    let fc = b.variable(p_in4, None, SC::Input, None);
    b.decorate(fc, Decoration::BuiltIn, vec![rspirv::dr::Operand::BuiltIn(BuiltIn::FragCoord)]);
    let one = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let zero = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let two = b.constant_bit32(f32_ty, 2.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fcv = b.load(vec4, None, fc, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, fcv, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, fcv, vec![1]).unwrap();
    let c0 = b.composite_construct(vec4, None, vec![x, y, one, zero]).unwrap();
    b.store(o0, c0, None, vec![]).unwrap();
    let ffv = b.load(bool_ty, None, ff, None, vec![]).unwrap();
    let fff = b.select(f32_ty, None, ffv, one, zero).unwrap();
    let pidv = b.load(i32_ty, None, pid, None, vec![]).unwrap();
    let pidf = b.convert_s_to_f(f32_ty, None, pidv).unwrap();
    let xy = b.f_add(f32_ty, None, x, y).unwrap();
    let c1 = b.composite_construct(vec4, None, vec![fff, pidf, xy, two]).unwrap();
    b.store(o1, c1, None, vec![]).unwrap();
    let d = b.f_mul(f32_ty, None, x, half).unwrap();
    b.store(depth, d, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![o0, o1, depth, ff, pid, fc]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    b.execution_mode(main, ExecutionMode::DepthReplacing, vec![]);
    let spirv = assemble(b);

    let module = atrium_spv_frontend::translate(&spirv).expect("frontend");
    let objs = [
        ("cranelift", atrium_spv_backend_cranelift::compile(
            &module, atrium_spv_backend_cranelift::Target::host()).expect("cranelift").object),
        ("bespoke-x86", atrium_spv_backend_bespoke_x86::compile(
            &module, atrium_spv_backend_bespoke_x86::Target::host()).expect("bespoke-x86").object),
    ];
    let mut results = Vec::new();
    for (name, obj) in &objs {
        let dir = tempfile::tempdir().unwrap();
        let o = dir.path().join("s.o");
        let so = dir.path().join("s.so");
        std::fs::write(&o, obj).unwrap();
        assert!(std::process::Command::new("cc").arg("-shared").arg("-o").arg(&so).arg(&o)
            .status().unwrap().success());
        let lib = unsafe { libloading::Library::new(&so) }.unwrap();
        type FsMain = unsafe extern "C" fn(
            *const u8, *const u8, *const u8, f32, f32, f32, f32, u32, *mut f32, *mut f32, u32, u32,
        );
        let f: libloading::Symbol<FsMain> = unsafe { lib.get(b"atrium_fs_main").unwrap() };
        let mut per_call = Vec::new();
        for (ffv, pidv) in [(1u32, 7u32), (0, 12345)] {
            let mut color = [-1.0f32; 12];
            let mut depth = -1.0f32;
            unsafe {
                f(std::ptr::null(), std::ptr::null(), std::ptr::null(), 3.5, 4.25, 0.5, 1.0, 1,
                  color.as_mut_ptr(), &mut depth, ffv, pidv);
            }
            per_call.push((color.map(f32::to_bits), depth.to_bits()));
        }
        results.push((name, per_call));
    }
    assert_eq!(results[0].1, results[1].1, "cranelift vs bespoke-x86");
    // And the values are the intended ones, not just equal garbage.
    let (color, depth) = results[1].1[0];
    assert_eq!(color[..8].iter().map(|b| f32::from_bits(*b)).collect::<Vec<_>>(),
               vec![3.5, 4.25, 1.0, 0.0, 1.0, 7.0, 7.75, 2.0]);
    assert_eq!(f32::from_bits(depth), 1.75);
    assert_eq!(f32::from_bits(results[1].1[1].0[4]), 0.0);
}

/// dFdx/dFdy through the derivative helper (uniforms table slot 72), with
/// a stub helper: checks the float and immediate arguments, the xmm0
/// return, and that values live across the call survive it.
#[test]
fn derivative_helper_call() {
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SC,
    };
    use std::sync::Mutex;
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    static SITES: Mutex<Vec<(i32, i32)>> = Mutex::new(Vec::new());
    extern "C" fn deriv(v: f32, site: i32, axis: i32) -> f32 {
        SITES.lock().unwrap().push((site, axis));
        v * 10.0 + axis as f32 * 1000.0
    }
    let mut b = Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let p_out4 = b.type_pointer(None, SC::Output, vec4);
    let p_in4 = b.type_pointer(None, SC::Input, vec4);
    let o0 = b.variable(p_out4, None, SC::Output, None);
    b.decorate(o0, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let fc = b.variable(p_in4, None, SC::Input, None);
    b.decorate(fc, Decoration::BuiltIn, vec![rspirv::dr::Operand::BuiltIn(BuiltIn::FragCoord)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fcv = b.load(vec4, None, fc, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, fcv, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, fcv, vec![1]).unwrap();
    let e = b.f_mul(f32_ty, None, x, y).unwrap();
    let dx = b.d_pdx(f32_ty, None, e).unwrap();
    let dy = b.d_pdy(f32_ty, None, e).unwrap();
    let c0 = b.composite_construct(vec4, None, vec![dx, dy, x, y]).unwrap();
    b.store(o0, c0, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![o0, fc]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let spirv = assemble(b);

    let module = atrium_spv_frontend::translate(&spirv).expect("frontend");
    let objs = [
        ("cranelift", atrium_spv_backend_cranelift::compile(
            &module, atrium_spv_backend_cranelift::Target::host()).expect("cranelift").object),
        ("bespoke-x86", atrium_spv_backend_bespoke_x86::compile(
            &module, atrium_spv_backend_bespoke_x86::Target::host()).expect("bespoke-x86").object),
    ];
    let mut table = [0usize; 10];
    table[72 / 8] = deriv as *const () as usize;
    let mut results = Vec::new();
    for (name, obj) in &objs {
        let dir = tempfile::tempdir().unwrap();
        let o = dir.path().join("s.o");
        let so = dir.path().join("s.so");
        std::fs::write(&o, obj).unwrap();
        assert!(std::process::Command::new("cc").arg("-shared").arg("-o").arg(&so).arg(&o)
            .status().unwrap().success());
        let lib = unsafe { libloading::Library::new(&so) }.unwrap();
        type FsMain = unsafe extern "C" fn(
            *const u8, *const u8, *const u8, f32, f32, f32, f32, u32, *mut f32, *mut f32, u32, u32,
        );
        let f: libloading::Symbol<FsMain> = unsafe { lib.get(b"atrium_fs_main").unwrap() };
        let mut color = [-1.0f32; 4];
        let mut depth = 0.0f32;
        SITES.lock().unwrap().clear();
        unsafe {
            f(std::ptr::null(), table.as_ptr() as *const u8, std::ptr::null(), 3.5, 4.25, 0.5, 1.0, 1,
              color.as_mut_ptr(), &mut depth, 1, 0);
        }
        let sites = SITES.lock().unwrap().clone();
        results.push((name, color.map(f32::to_bits), sites));
    }
    assert_eq!(results[0].1, results[1].1, "cranelift vs bespoke-x86");
    assert_eq!(results[0].2, results[1].2, "helper arguments");
    let e = 3.5f32 * 4.25;
    assert_eq!(results[1].1.map(f32::from_bits), [e * 10.0, e * 10.0 + 1000.0, 3.5, 4.25]);
    assert_eq!(results[1].2.iter().map(|s| s.1).collect::<Vec<_>>(), vec![0, 1]);
}

/// The span entry (`atrium_fs_main_span`) against per-lane calls of
/// `atrium_fs_main`: a varying- and FragCoord-dependent shader, a mask
/// with holes, masked-off lanes left untouched.
#[test]
fn span_thunk_matches_per_lane_fs_main() {
    use rspirv::dr::Operand::LiteralBit32;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SC,
    };
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    let mut b = Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let p_out4 = b.type_pointer(None, SC::Output, vec4);
    let p_in4 = b.type_pointer(None, SC::Input, vec4);
    let p_inf = b.type_pointer(None, SC::Input, f32_ty);
    let o0 = b.variable(p_out4, None, SC::Output, None);
    b.decorate(o0, Decoration::Location, vec![LiteralBit32(0)]);
    let fc = b.variable(p_in4, None, SC::Input, None);
    b.decorate(fc, Decoration::BuiltIn, vec![rspirv::dr::Operand::BuiltIn(BuiltIn::FragCoord)]);
    let u = b.variable(p_inf, None, SC::Input, None);
    b.decorate(u, Decoration::Location, vec![LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fcv = b.load(vec4, None, fc, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, fcv, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, fcv, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, fcv, vec![2]).unwrap();
    let w = b.composite_extract(f32_ty, None, fcv, vec![3]).unwrap();
    let uv = b.load(f32_ty, None, u, None, vec![]).unwrap();
    let xu = b.f_mul(f32_ty, None, x, uv).unwrap();
    let c = b.composite_construct(vec4, None, vec![xu, y, z, w]).unwrap();
    b.store(o0, c, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![o0, fc, u]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let spirv = assemble(b);
    let module = atrium_spv_frontend::translate(&spirv).expect("frontend");
    let obj = atrium_spv_backend_bespoke_x86::compile(
        &module, atrium_spv_backend_bespoke_x86::Target::host()).expect("bespoke-x86").object;
    let dir = tempfile::tempdir().unwrap();
    let o = dir.path().join("s.o");
    let so = dir.path().join("s.so");
    std::fs::write(&o, obj).unwrap();
    assert!(std::process::Command::new("cc").arg("-shared").arg("-o").arg(&so).arg(&o)
        .status().unwrap().success());
    let lib = unsafe { libloading::Library::new(&so) }.unwrap();
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, f32, f32, f32, f32, u32, *mut f32, *mut f32, u32, u32,
    );
    type FsSpanMain = unsafe extern "C" fn(
        *const u8, u32, *const u8, *const u8, *const f32, *const f32, *const f32, *const f32,
        u64, u32, *mut f32, *mut f32, u32, u32, u32,
    );
    let f: libloading::Symbol<FsMain> = unsafe { lib.get(b"atrium_fs_main").unwrap() };
    let fs: libloading::Symbol<FsSpanMain> = unsafe { lib.get(b"atrium_fs_main_span").unwrap() };
    const N: usize = 7;
    let stride = 16u32;
    let mut vary = vec![0u8; N * stride as usize];
    for l in 0..N {
        vary[l * 16..l * 16 + 4].copy_from_slice(&(1.5f32 + l as f32).to_le_bytes());
    }
    let fx: Vec<f32> = (0..N).map(|l| 10.0 + l as f32).collect();
    let fy: Vec<f32> = (0..N).map(|l| 20.0 + l as f32).collect();
    let fz: Vec<f32> = (0..N).map(|l| 0.25 * l as f32).collect();
    let fw: Vec<f32> = (0..N).map(|l| 1.0 / (1.0 + l as f32)).collect();
    let mask: u64 = 0b1011101 | (1 << 40); // bit 40 is past lane_count
    let mut span_c = [-1.0f32; N * 4];
    let mut span_d = [-1.0f32; N];
    unsafe {
        fs(vary.as_ptr(), stride, std::ptr::null(), std::ptr::null(), fx.as_ptr(), fy.as_ptr(),
           fz.as_ptr(), fw.as_ptr(), mask, 1, span_c.as_mut_ptr(), span_d.as_mut_ptr(), 1, 3, N as u32);
    }
    for l in 0..N {
        let mut c = [-1.0f32; 4];
        let mut d = -1.0f32;
        if mask >> l & 1 != 0 {
            unsafe {
                f(vary.as_ptr().add(l * 16), std::ptr::null(), std::ptr::null(), fx[l], fy[l], fz[l], fw[l],
                  1, c.as_mut_ptr(), &mut d, 1, 3);
            }
            assert_eq!(c[0], fx[l] * (1.5 + l as f32));
        }
        assert_eq!(&span_c[l * 4..l * 4 + 4], &c, "lane {l}");
        assert_eq!(span_d[l].to_bits(), d.to_bits(), "lane {l} depth");
    }
}

/// PackHalf2x16 / UnpackHalf2x16 over a wide sweep (rounding ties,
/// subnormals, overflow, infinities, NaN payloads, most half patterns
/// for unpack), bit for bit against Cranelift, through direct FsMain calls.
#[test]
fn pack_unpack_half_sweep() {
    use rspirv::dr::Operand;
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    let spirv = build(|b, c| {
        let glsl = b.ext_inst_import("GLSL.std.450");
        let u32_ty = b.type_int(32, 0);
        let v2 = b.type_vector(c.f32_ty, 2);
        let xy = b.composite_construct(v2, None, vec![c.x, c.y]).unwrap();
        let p = b.ext_inst(u32_ty, None, glsl, 58, vec![Operand::IdRef(xy)]).unwrap();
        let au = b.bitcast(u32_ty, None, c.a).unwrap();
        let un = b.ext_inst(v2, None, glsl, 62, vec![Operand::IdRef(au)]).unwrap();
        let u0 = b.composite_extract(c.f32_ty, None, un, vec![0]).unwrap();
        let u1 = b.composite_extract(c.f32_ty, None, un, vec![1]).unwrap();
        let bu = b.bitcast(u32_ty, None, c.b).unwrap();
        let ub = b.ext_inst(v2, None, glsl, 62, vec![Operand::IdRef(bu)]).unwrap();
        let rp = b.ext_inst(u32_ty, None, glsl, 58, vec![Operand::IdRef(ub)]).unwrap();
        [c.bits(b, p), u0, u1, c.bits(b, rp)]
    });
    let module = atrium_spv_frontend::translate(&spirv).expect("frontend");
    let objs = [
        atrium_spv_backend_cranelift::compile(&module, atrium_spv_backend_cranelift::Target::host())
            .expect("cranelift").object,
        atrium_spv_backend_bespoke_x86::compile(&module, atrium_spv_backend_bespoke_x86::Target::host())
            .expect("bespoke-x86").object,
    ];
    let dir = tempfile::tempdir().unwrap();
    let libs: Vec<libloading::Library> = objs.iter().enumerate().map(|(k, obj)| {
        let o = dir.path().join(format!("s{k}.o"));
        let so = dir.path().join(format!("s{k}.so"));
        std::fs::write(&o, obj).unwrap();
        assert!(std::process::Command::new("cc").arg("-shared").arg("-o").arg(&so).arg(&o)
            .status().unwrap().success());
        unsafe { libloading::Library::new(&so) }.unwrap()
    }).collect();
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, f32, f32, f32, f32, u32, *mut f32, *mut f32, u32, u32,
    );
    let fs: Vec<FsMain> = libs.iter().map(|l| unsafe { *l.get::<FsMain>(b"atrium_fs_main").unwrap() }).collect();
    let run = |f: FsMain, a: u32, b: u32, x: u32, y: u32| -> [u32; 4] {
        let pc: [u32; 4] = [a, b, x, y];
        let mut c = [0f32; 4];
        let mut d = 0f32;
        unsafe {
            f(std::ptr::null(), std::ptr::null(), pc.as_ptr() as *const u8, 0.0, 0.0, 0.0, 1.0, 1,
              c.as_mut_ptr(), &mut d, 1, 0);
        }
        c.map(f32::to_bits)
    };
    // Floats: special values, the f16 boundaries (±1 ulp) and a stride
    // through all of f32.
    let mut fl: Vec<u32> = vec![
        0, 0x8000_0000, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0x7f80_0001, 0xffc1_2345, 0x7fbf_ffff,
        0x3f80_0000, 0x477f_e000, 0x477f_f000, 0x477f_efff, 0x4780_0000, 0x3880_0000, 0x387f_ffff,
        0x3380_0000, 0x3300_0000, 0x3300_0001, 0x0000_0001, 0x3f80_1000, 0x3f80_3000,
    ];
    for b in [0x4780_0000u32, 0x3880_0000, 0x3380_0000, 0x3f80_1000] {
        for d in 0..4u32 {
            fl.extend([b.wrapping_add(d), b.wrapping_sub(d), b.wrapping_add(d) | 0x8000_0000]);
        }
    }
    fl.extend((0..20_000u32).map(|i| i.wrapping_mul(214_749)));
    for (i, &x) in fl.iter().enumerate() {
        let y = fl[(i * 7 + 3) % fl.len()];
        // Unpack sees about 40k half patterns, two per call.
        let h = (i as u32 * 2) & 0xffff;
        let a = h | ((h + 1) << 16);
        let bb = (i as u32).wrapping_mul(2_654_435_761);
        let want = run(fs[0], a, bb, x, y);
        let got = run(fs[1], a, bb, x, y);
        assert_eq!(got, want, "x={x:#x} y={y:#x} a={a:#x} b={bb:#x}");
    }
    // Spot values.
    let r = run(fs[1], 0x3c00_c000, 0, 1.0f32.to_bits(), (-2.0f32).to_bits());
    assert_eq!(r[0], 0xc000_3c00);
    assert_eq!([f32::from_bits(r[1]), f32::from_bits(r[2])], [-2.0, 1.0]);
}

/// Packed vector math: vec4/vec3/vec2 varyings loaded packed, lane-wise
/// mul/add/div/min/max/sub with scalar broadcasts, lanes extracted and
/// mixed back with scalar code, bit for bit against Cranelift.
#[test]
fn packed_vector_ops() {
    use rspirv::dr::Operand::{self, LiteralBit32};
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass as SC,
    };
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    let mut b = Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    let glsl = b.ext_inst_import("GLSL.std.450");
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f = b.type_float(32, None);
    let v2 = b.type_vector(f, 2);
    let v3 = b.type_vector(f, 3);
    let v4 = b.type_vector(f, 4);
    let void_fn = b.type_function(void, vec![]);
    let pin4 = b.type_pointer(None, SC::Input, v4);
    let pin3 = b.type_pointer(None, SC::Input, v3);
    let pin2 = b.type_pointer(None, SC::Input, v2);
    let pout4 = b.type_pointer(None, SC::Output, v4);
    let i0 = b.variable(pin4, None, SC::Input, None);
    b.decorate(i0, Decoration::Location, vec![LiteralBit32(0)]);
    let i1 = b.variable(pin4, None, SC::Input, None);
    b.decorate(i1, Decoration::Location, vec![LiteralBit32(1)]);
    let i2 = b.variable(pin3, None, SC::Input, None);
    b.decorate(i2, Decoration::Location, vec![LiteralBit32(2)]);
    let i3 = b.variable(pin2, None, SC::Input, None);
    b.decorate(i3, Decoration::Location, vec![LiteralBit32(3)]);
    let out = b.variable(pout4, None, SC::Output, None);
    b.decorate(out, Decoration::Location, vec![LiteralBit32(0)]);
    let two = b.constant_bit32(f, 2.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let a = b.load(v4, None, i0, None, vec![]).unwrap();
    let c = b.load(v4, None, i1, None, vec![]).unwrap();
    let e = b.load(v3, None, i2, None, vec![]).unwrap();
    let g = b.load(v2, None, i3, None, vec![]).unwrap();
    // (GLSL Fma on vectors types its product as a scalar in the frontend,
    // so spell it out.)
    let ac = b.f_mul(v4, None, a, c).unwrap();
    let r = b.f_add(v4, None, ac, a).unwrap();
    let r2 = b.vector_times_scalar(v4, None, r, two).unwrap();
    let m = b.ext_inst(v4, None, glsl, 37, vec![Operand::IdRef(r2), Operand::IdRef(c)]).unwrap();
    let mx = b.ext_inst(v4, None, glsl, 40, vec![Operand::IdRef(r), Operand::IdRef(a)]).unwrap();
    let q = b.f_div(v4, None, c, a).unwrap();
    let s = b.f_sub(v4, None, a, q).unwrap();
    let e2 = b.f_mul(v3, None, e, e).unwrap();
    let ey = b.composite_extract(f, None, e2, vec![1]).unwrap();
    let g2 = b.f_add(v2, None, g, g).unwrap();
    let gx = b.composite_extract(f, None, g2, vec![1]).unwrap();
    let mxw = b.composite_extract(f, None, m, vec![0]).unwrap();
    let mw = b.composite_extract(f, None, m, vec![3]).unwrap();
    let l0 = b.f_add(f, None, mxw, mw).unwrap();
    let l1 = b.composite_extract(f, None, mx, vec![2]).unwrap();
    let sx = b.composite_extract(f, None, s, vec![1]).unwrap();
    let l2 = b.f_add(f, None, sx, ey).unwrap();
    let col = b.composite_construct(v4, None, vec![l0, l1, l2, gx]).unwrap();
    let fin = b.f_add(v4, None, col, s).unwrap();
    b.store(out, fin, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![i0, i1, i2, i3, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let spirv = assemble(b);
    let module = atrium_spv_frontend::translate(&spirv).expect("frontend");
    let objs = [
        atrium_spv_backend_cranelift::compile(&module, atrium_spv_backend_cranelift::Target::host())
            .expect("cranelift").object,
        atrium_spv_backend_bespoke_x86::compile(&module, atrium_spv_backend_bespoke_x86::Target::host())
            .expect("bespoke-x86").object,
    ];
    let dir = tempfile::tempdir().unwrap();
    let libs: Vec<libloading::Library> = objs.iter().enumerate().map(|(k, obj)| {
        let o = dir.path().join(format!("s{k}.o"));
        let so = dir.path().join(format!("s{k}.so"));
        std::fs::write(&o, obj).unwrap();
        assert!(std::process::Command::new("cc").arg("-shared").arg("-o").arg(&so).arg(&o)
            .status().unwrap().success());
        unsafe { libloading::Library::new(&so) }.unwrap()
    }).collect();
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, f32, f32, f32, f32, u32, *mut f32, *mut f32, u32, u32,
    );
    let fs: Vec<FsMain> = libs.iter().map(|l| unsafe { *l.get::<FsMain>(b"atrium_fs_main").unwrap() }).collect();
    let vals = [1.5f32, -2.25, 0.0, -0.0, f32::NAN, f32::INFINITY, 3e-39, 7.0, -1e30, 0.333];
    for k in 0..40 {
        // The varying block: four vec4 slots (Location n at 16n).
        let vary: Vec<f32> = (0..16).map(|i| vals[(k * 7 + i * 3 + i / 4) % vals.len()]).collect();
        let res: Vec<[u32; 4]> = fs.iter().map(|&f| {
            let mut c = [0f32; 4];
            let mut d = 0f32;
            unsafe {
                f(vary.as_ptr() as *const u8, std::ptr::null(), std::ptr::null(), 0.0, 0.0, 0.0, 1.0, 1,
                  c.as_mut_ptr(), &mut d, 1, 0);
            }
            c.map(f32::to_bits)
        }).collect();
        assert_eq!(res[1], res[0], "inputs {vary:?}");
    }
}

/// Every component-wise GLSL.std.450 op that the frontend expands into
/// several IR ops, on vec4 operands (Slang emits these for its built-ins):
/// bespoke-x86 must compile each one and agree with Cranelift bit for bit.
#[test]
fn glsl_ext_ops_on_vectors() {
    use rspirv::dr::Operand::{self, LiteralBit32};
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass as SC,
    };
    if !cfg!(target_arch = "x86_64") {
        return;
    }
    // (enum, operand count, scalar result, name)
    let ops: &[(u32, usize, bool, &str)] = &[
        (1, 1, false, "Round"),
        (2, 1, false, "RoundEven"),
        (6, 1, false, "FSign"),
        (10, 1, false, "Fract"),
        (37, 2, false, "FMin"),
        (40, 2, false, "FMax"),
        (43, 3, false, "FClamp"),
        (46, 3, false, "FMix"),
        (48, 2, false, "Step"),
        (49, 3, false, "SmoothStep"),
        (50, 3, false, "Fma"),
        (66, 1, true, "Length"),
        (67, 2, true, "Distance"),
        (69, 1, false, "Normalize"),
        (70, 3, false, "FaceForward"),
        (71, 2, false, "Reflect"),
    ];
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, f32, f32, f32, f32, u32, *mut f32, *mut f32, u32, u32,
    );
    let vals = [1.5f32, -2.25, 0.0, -0.0, 0.5, -0.5, 2.5, 7.0, -1e3, 0.333, 0.75, 1.0];
    let mut failures = Vec::new();
    for &(e, nargs, scalar, name) in ops {
        let mut b = Builder::new();
        b.set_version(1, 0);
        b.capability(Capability::Shader);
        let glsl = b.ext_inst_import("GLSL.std.450");
        b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
        let void = b.type_void();
        let f = b.type_float(32, None);
        let v4 = b.type_vector(f, 4);
        let void_fn = b.type_function(void, vec![]);
        let pin4 = b.type_pointer(None, SC::Input, v4);
        let pout4 = b.type_pointer(None, SC::Output, v4);
        let ins: Vec<_> = (0..3).map(|k| {
            let v = b.variable(pin4, None, SC::Input, None);
            b.decorate(v, Decoration::Location, vec![LiteralBit32(k)]);
            v
        }).collect();
        let out = b.variable(pout4, None, SC::Output, None);
        b.decorate(out, Decoration::Location, vec![LiteralBit32(0)]);
        let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
        b.begin_block(None).unwrap();
        let args: Vec<Operand> = ins[..nargs].iter()
            .map(|&v| Operand::IdRef(b.load(v4, None, v, None, vec![]).unwrap())).collect();
        let r = b.ext_inst(if scalar { f } else { v4 }, None, glsl, e, args).unwrap();
        let r4 = if scalar { b.composite_construct(v4, None, vec![r, r, r, r]).unwrap() } else { r };
        b.store(out, r4, None, vec![]).unwrap();
        b.ret().unwrap();
        b.end_function().unwrap();
        let mut iface = ins.clone();
        iface.push(out);
        b.entry_point(ExecutionModel::Fragment, main, "main", iface);
        b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
        let spirv = assemble(b);
        let module = match atrium_spv_frontend::translate(&spirv) {
            Ok(m) => m,
            Err(err) => { failures.push(format!("{name}: frontend {err}")); continue; }
        };
        let cl = atrium_spv_backend_cranelift::compile(&module, atrium_spv_backend_cranelift::Target::host());
        let x86 = atrium_spv_backend_bespoke_x86::compile(&module, atrium_spv_backend_bespoke_x86::Target::host());
        let (cl, x86) = match (cl, x86) {
            (Ok(c), Ok(x)) => (c.object, x.object),
            (c, x) => {
                failures.push(format!("{name}: cranelift {:?} bespoke {:?}", c.err(), x.err()));
                continue;
            }
        };
        let dir = tempfile::tempdir().unwrap();
        let libs: Vec<libloading::Library> = [cl, x86].iter().enumerate().map(|(k, obj)| {
            let o = dir.path().join(format!("s{k}.o"));
            let so = dir.path().join(format!("s{k}.so"));
            std::fs::write(&o, obj).unwrap();
            assert!(std::process::Command::new("cc").arg("-shared").arg("-o").arg(&so).arg(&o)
                .status().unwrap().success());
            unsafe { libloading::Library::new(&so) }.unwrap()
        }).collect();
        let fs: Vec<FsMain> = libs.iter().map(|l| unsafe { *l.get::<FsMain>(b"atrium_fs_main").unwrap() }).collect();
        for k in 0..24 {
            let vary: Vec<f32> = (0..12).map(|i| vals[(k * 5 + i * 7 + i / 4) % vals.len()]).collect();
            let res: Vec<[u32; 4]> = fs.iter().map(|&f| {
                let mut c = [0f32; 4];
                let mut d = 0f32;
                unsafe {
                    f(vary.as_ptr() as *const u8, std::ptr::null(), std::ptr::null(), 0.0, 0.0, 0.0, 1.0, 1,
                      c.as_mut_ptr(), &mut d, 1, 0);
                }
                c.map(f32::to_bits)
            }).collect();
            if res[0] != res[1] {
                failures.push(format!("{name}: inputs {vary:?}: cranelift {:?} bespoke {:?}", res[0], res[1]));
                break;
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
