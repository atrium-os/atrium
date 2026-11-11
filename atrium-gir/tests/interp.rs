//! Interpreter semantics on hand-built GIR: loop parameters and results,
//! if results, divergent break and return, and the verifier's checks.

use atrium_gir::*;
use std::collections::HashMap;

struct K {
    f: Function,
}

impl K {
    fn new() -> Self {
        K { f: Function { name: "t".into(), local_size: [8, 1, 1], types: vec![], privates: vec![], workgroup_bytes: 0, buffers: vec![(0, 0)], push_bytes: 0, body: vec![] } }
    }
    fn v(&mut self, ty: Ty) -> ValueId {
        self.f.new_value(ty)
    }
    fn i(r: &mut Region, res: Option<ValueId>, op: Op) {
        r.push(Node::Inst(Inst { result: res, op }));
    }
    fn c(&mut self, r: &mut Region, bits: u32) -> ValueId {
        let v = self.v(Ty::I32);
        Self::i(r, Some(v), Op::Const(bits));
        v
    }
}

/// out[lid] = sum_{i < lid} i, via a loop with a parameter and a result,
/// breaking divergently; lanes >= 6 return before the store.
fn kernel() -> Function {
    let mut k = K::new();
    let mut top: Region = vec![];
    let zero = k.c(&mut top, 0);
    let one = k.c(&mut top, 1);
    let four = k.c(&mut top, 4);
    let six = k.c(&mut top, 6);
    let lid3 = k.v(Ty::V(Scalar::I32, 3));
    K::i(&mut top, Some(lid3), Op::Builtin(Builtin::LocalId));
    let lid = k.v(Ty::I32);
    K::i(&mut top, Some(lid), Op::Extract(lid3, 0));

    // loop (i, acc) = (0, 0) { if i < lid { continue (i+1, acc+i) } else { break acc } }
    let (i, acc) = (k.v(Ty::I32), k.v(Ty::I32));
    let total = k.v(Ty::I32);
    let mut body: Region = vec![];
    let lt = k.v(Ty::BOOL);
    K::i(&mut body, Some(lt), Op::Cmp(CmpOp::ULt, i, lid));
    let mut then: Region = vec![];
    let (i1, a1) = (k.v(Ty::I32), k.v(Ty::I32));
    K::i(&mut then, Some(i1), Op::Bin(BinOp::IAdd, i, one));
    K::i(&mut then, Some(a1), Op::Bin(BinOp::IAdd, acc, i));
    K::i(&mut then, None, Op::Continue(vec![i1, a1]));
    let mut els: Region = vec![];
    K::i(&mut els, None, Op::Break(vec![acc]));
    body.push(Node::If { cond: lt, then, els, results: vec![] });
    body.push(Node::Inst(Inst { result: None, op: Op::Return })); // unreachable: both arms leave
    top.push(Node::Loop { inits: vec![zero, zero], params: vec![i, acc], body, results: vec![total] });

    // r = lid < 6 ? (if-result total) : return
    let small = k.v(Ty::BOOL);
    K::i(&mut top, Some(small), Op::Cmp(CmpOp::ULt, lid, six));
    let r = k.v(Ty::I32);
    let mut t2: Region = vec![];
    K::i(&mut t2, None, Op::Yield(vec![total]));
    let mut e2: Region = vec![];
    K::i(&mut e2, None, Op::Return);
    top.push(Node::If { cond: small, then: t2, els: e2, results: vec![r] });
    let off = k.v(Ty::I32);
    K::i(&mut top, Some(off), Op::Bin(BinOp::IMul, lid, four));
    K::i(&mut top, None, Op::Store(Space::Buffer { set: 0, binding: 0 }, off, r));
    k.f.body = top;
    k.f
}

#[test]
fn loop_params_results_and_divergent_exit() {
    let f = kernel();
    verify::verify(&f).unwrap();
    let mut b: interp::Buffers = HashMap::from([((0, 0), vec![0xffu8; 32])]);
    interp::run(&f, [1, 1, 1], &mut b).unwrap();
    let out: Vec<u32> = b[&(0, 0)].chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
    assert_eq!(out, vec![0, 0, 1, 3, 6, 10, 0xffff_ffff, 0xffff_ffff]);
}

#[test]
fn verifier_rejects_use_outside_scope() {
    let mut f = kernel();
    // Use the loop parameter after the loop.
    let Node::Loop { params, .. } = &f.body[6] else { panic!() };
    let p = params[0];
    f.body.push(Node::Inst(Inst { result: None, op: Op::Store(Space::Buffer { set: 0, binding: 0 }, p, p) }));
    assert!(verify::verify(&f).is_err());
}

#[test]
fn out_of_bounds_is_an_error() {
    let f = kernel();
    let mut b: interp::Buffers = HashMap::from([((0, 0), vec![0u8; 8])]);
    assert!(interp::run(&f, [1, 1, 1], &mut b).is_err());
}

/// A temporary written on every path before it is read in each iteration
/// still went round the loop as a parameter (through the `if` that writes
/// it): `dce` drops it, with the `if` result that fed it, and the kernel
/// computes the same.
#[test]
fn dead_carried_values_are_dropped() {
    let mut k = K::new();
    let mut top: Region = vec![];
    let zero = k.c(&mut top, 0);
    let one = k.c(&mut top, 1);
    let four = k.c(&mut top, 4);
    let lid3 = k.v(Ty::V(Scalar::I32, 3));
    K::i(&mut top, Some(lid3), Op::Builtin(Builtin::LocalId));
    let lid = k.v(Ty::I32);
    K::i(&mut top, Some(lid), Op::Extract(lid3, 0));
    // loop (i, acc, tmp) = (0, 0, 0) {
    //   if i < lid { tmp' = if i & 1 { i * 4 } else { i + 1 }; continue (i+1, acc+tmp', tmp') }
    //   else { break acc } }
    let (i, acc, tmp) = (k.v(Ty::I32), k.v(Ty::I32), k.v(Ty::I32));
    let total = k.v(Ty::I32);
    let mut body: Region = vec![];
    let lt = k.v(Ty::BOOL);
    K::i(&mut body, Some(lt), Op::Cmp(CmpOp::ULt, i, lid));
    let mut then: Region = vec![];
    let odd_bits = k.v(Ty::I32);
    K::i(&mut then, Some(odd_bits), Op::Bin(BinOp::And, i, one));
    let odd = k.v(Ty::BOOL);
    K::i(&mut then, Some(odd), Op::Cmp(CmpOp::INe, odd_bits, zero));
    let mut a: Region = vec![];
    let ta = k.v(Ty::I32);
    K::i(&mut a, Some(ta), Op::Bin(BinOp::IMul, i, four));
    K::i(&mut a, None, Op::Yield(vec![ta, tmp]));
    let mut b: Region = vec![];
    let tb = k.v(Ty::I32);
    K::i(&mut b, Some(tb), Op::Bin(BinOp::IAdd, i, one));
    K::i(&mut b, None, Op::Yield(vec![tb, tmp]));
    let (t1, old) = (k.v(Ty::I32), k.v(Ty::I32));
    then.push(Node::If { cond: odd, then: a, els: b, results: vec![t1, old] });
    let (i1, a1) = (k.v(Ty::I32), k.v(Ty::I32));
    K::i(&mut then, Some(i1), Op::Bin(BinOp::IAdd, i, one));
    K::i(&mut then, Some(a1), Op::Bin(BinOp::IAdd, acc, t1));
    // `old` (the parameter passed through the if) feeds only the parameter.
    K::i(&mut then, None, Op::Continue(vec![i1, a1, old]));
    let mut els: Region = vec![];
    K::i(&mut els, None, Op::Break(vec![acc]));
    body.push(Node::If { cond: lt, then, els, results: vec![] });
    body.push(Node::Inst(Inst { result: None, op: Op::Return }));
    top.push(Node::Loop { inits: vec![zero, zero, zero], params: vec![i, acc, tmp], body, results: vec![total] });
    let off = k.v(Ty::I32);
    K::i(&mut top, Some(off), Op::Bin(BinOp::IMul, lid, four));
    K::i(&mut top, None, Op::Store(Space::Buffer { set: 0, binding: 0 }, off, total));
    k.f.body = top;
    let mut f = k.f;
    verify::verify(&f).unwrap();
    let run = |f: &Function| {
        let mut b: interp::Buffers = HashMap::from([((0, 0), vec![0u8; 32])]);
        interp::run(f, [1, 1, 1], &mut b).unwrap();
        b[&(0, 0)].clone()
    };
    let want = run(&f);
    assert!(passes::dce::dce(&mut f));
    verify::verify(&f).unwrap();
    assert_eq!(run(&f), want);
    let Some(Node::Loop { params, .. }) = f.body.iter().find(|n| matches!(n, Node::Loop { .. })) else { panic!() };
    assert_eq!(params.len(), 2, "tmp is no longer carried:\n{f}");
}
