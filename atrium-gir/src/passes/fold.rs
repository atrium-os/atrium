//! Constant folding and algebraic simplification.
//!
//! Folding uses the interpreter's own arithmetic (`interp::eval`), so a
//! folded program computes exactly what the unfolded one did. The
//! simplifications track known trailing zero bits, which removes the
//! `(x / 4) * 4` round trips the frontend emits for byte-addressed buffers
//! whenever `x` is known to be a multiple of 4.

use crate::interp::eval;
use crate::*;
use std::collections::HashMap;

struct F<'a> {
    f: &'a mut Function,
    k: HashMap<ValueId, u32>,
    tz: HashMap<ValueId, u32>,
    subst: HashMap<ValueId, ValueId>,
    new_consts: HashMap<u32, ValueId>,
    prelude: Region,
    changed: bool,
    defs: HashMap<ValueId, Op>,
}

enum Rewrite {
    Keep,
    Alias(ValueId),
    Const(u32),
    Op(Op),
}

impl<'a> F<'a> {
    fn sub(&self, mut v: ValueId) -> ValueId {
        while let Some(n) = self.subst.get(&v) {
            v = *n;
        }
        v
    }

    fn i32_const(&mut self, bits: u32) -> ValueId {
        if let Some(v) = self.new_consts.get(&bits) {
            return *v;
        }
        let v = self.f.new_value(Ty::I32);
        self.prelude.push(Node::Inst(Inst { result: Some(v), op: Op::Const(bits) }));
        self.k.insert(v, bits);
        self.tz.insert(v, bits.trailing_zeros());
        self.new_consts.insert(bits, v);
        v
    }

    fn tz(&self, v: ValueId) -> u32 {
        self.tz.get(&v).copied().unwrap_or(0)
    }

    fn rewrite(&mut self, op: &Op, res: ValueId) -> Rewrite {
        let ty = self.f.ty(res);
        let kv = |v: &ValueId| self.k.get(v).copied();
        // Full constant evaluation of scalar ops.
        if matches!(op, Op::Un(..) | Op::Bin(..) | Op::Fma(..) | Op::Cmp(..) | Op::Conv(..)) {
            let args: Option<Vec<u32>> = op.operands().iter().map(kv).collect();
            if let Some(a) = args {
                if let Some(r) = eval(op, &a, ty == Ty::BOOL) {
                    return Rewrite::Const(r);
                }
            }
        }
        match op {
            Op::Bitcast(a) if ty.comps() == 1 => match kv(a) {
                Some(b) => Rewrite::Const(b),
                None => Rewrite::Keep,
            },
            Op::Select(c, t, e) => match kv(c) {
                Some(0) => Rewrite::Alias(*e),
                Some(_) => Rewrite::Alias(*t),
                None if t == e => Rewrite::Alias(*t),
                None => Rewrite::Keep,
            },
            Op::Bin(o, a, b) if ty == Ty::I32 => {
                let (ka, kb) = (kv(a), kv(b));
                let pow2 = |x: Option<u32>| x.filter(|v| v.is_power_of_two()).map(|v| v.trailing_zeros());
                match o {
                    BinOp::IAdd if kb == Some(0) => Rewrite::Alias(*a),
                    BinOp::IAdd if ka == Some(0) => Rewrite::Alias(*b),
                    BinOp::ISub if kb == Some(0) => Rewrite::Alias(*a),
                    BinOp::IMul if kb == Some(1) => Rewrite::Alias(*a),
                    BinOp::IMul if ka == Some(1) => Rewrite::Alias(*b),
                    BinOp::IMul if ka == Some(0) || kb == Some(0) => Rewrite::Const(0),
                    BinOp::IMul if pow2(kb).is_some() => {
                        let s = self.i32_const(pow2(kb).unwrap());
                        Rewrite::Op(Op::Bin(BinOp::Shl, *a, s))
                    }
                    BinOp::IMul if pow2(ka).is_some() => {
                        let s = self.i32_const(pow2(ka).unwrap());
                        Rewrite::Op(Op::Bin(BinOp::Shl, *b, s))
                    }
                    BinOp::UDiv if kb == Some(1) => Rewrite::Alias(*a),
                    BinOp::UDiv if pow2(kb).is_some() => {
                        let s = self.i32_const(pow2(kb).unwrap());
                        Rewrite::Op(Op::Bin(BinOp::LShr, *a, s))
                    }
                    BinOp::UMod if pow2(kb).is_some() => {
                        let m = self.i32_const(kb.unwrap() - 1);
                        Rewrite::Op(Op::Bin(BinOp::And, *a, m))
                    }
                    BinOp::Shl | BinOp::LShr | BinOp::AShr if kb.map(|x| x & 31) == Some(0) => Rewrite::Alias(*a),
                    // (x >> k) << k == x when the low k bits of x are known zero.
                    BinOp::Shl => {
                        if let Some(k) = kb {
                            if let Some(Op::Bin(BinOp::LShr, x, k2)) = self.def(*a) {
                                if kv(&k2) == Some(k) && self.tz(x) >= (k & 31) {
                                    return Rewrite::Alias(x);
                                }
                            }
                        }
                        Rewrite::Keep
                    }
                    BinOp::And => {
                        // x & m == x when every bit m clears is known zero in x.
                        for (x, m) in [(a, kb), (b, ka)] {
                            if let Some(m) = m {
                                let low = if self.tz(*x) >= 32 { u32::MAX } else { (1u32 << self.tz(*x)) - 1 };
                                if !m & !low == 0 {
                                    return Rewrite::Alias(*x);
                                }
                            }
                        }
                        Rewrite::Keep
                    }
                    _ => Rewrite::Keep,
                }
            }
            _ => Rewrite::Keep,
        }
    }

    /// Defining op of `v`, if recorded.
    fn def(&self, v: ValueId) -> Option<Op> {
        self.defs.get(&v).cloned()
    }

    fn note_tz(&mut self, res: ValueId, op: &Op) {
        let t = |s: &Self, v: &ValueId| s.tz(*v);
        let k = |s: &Self, v: &ValueId| s.k.get(v).copied();
        let z = match op {
            Op::Const(b) => b.trailing_zeros(),
            Op::Bin(BinOp::IMul, a, b) => (t(self, a) + t(self, b)).min(32),
            Op::Bin(BinOp::Shl, a, b) => match k(self, b) {
                Some(s) => (t(self, a) + (s & 31)).min(32),
                None => t(self, a),
            },
            Op::Bin(BinOp::LShr, a, b) => match k(self, b) {
                Some(s) => t(self, a).saturating_sub(s & 31),
                None => 0,
            },
            Op::Bin(BinOp::IAdd | BinOp::ISub | BinOp::Or | BinOp::Xor, a, b) => t(self, a).min(t(self, b)),
            Op::Bin(BinOp::And, a, b) => t(self, a).max(t(self, b)),
            Op::Select(_, a, b) => t(self, a).min(t(self, b)),
            _ => 0,
        };
        if z > 0 {
            self.tz.insert(res, z);
        }
    }

    fn region(&mut self, r: Region) -> Region {
        let mut out = Region::new();
        let mut iter = r.into_iter();
        while let Some(node) = iter.next() {
            match node {
                Node::Inst(mut inst) => {
                    for o in inst.op.operands_mut() {
                        *o = self.sub(*o);
                    }
                    if let (Some(res), true) = (inst.result, inst.op.is_pure()) {
                        match self.rewrite(&inst.op, res) {
                            Rewrite::Keep => {}
                            Rewrite::Alias(v) => {
                                self.subst.insert(res, v);
                                self.changed = true;
                                continue;
                            }
                            Rewrite::Const(b) => {
                                inst.op = Op::Const(b);
                                self.changed = true;
                            }
                            Rewrite::Op(op) => {
                                inst.op = op;
                                self.changed = true;
                            }
                        }
                        if let Op::Const(b) = inst.op {
                            self.k.insert(res, b);
                        }
                        self.note_tz(res, &inst.op);
                        self.defs.insert(res, inst.op.clone());
                    }
                    let term = inst.op.is_terminator();
                    out.push(Node::Inst(inst));
                    if term {
                        break;
                    }
                }
                Node::If { cond, then, els, results } => {
                    let cond = self.sub(cond);
                    if let Some(c) = self.k.get(&cond).copied() {
                        // Splice the taken arm.
                        self.changed = true;
                        let mut arm = self.region(if c != 0 { then } else { els });
                        let mut ends = false;
                        match arm.last() {
                            Some(Node::Inst(Inst { op: Op::Yield(v), .. })) => {
                                for (r, y) in results.iter().zip(v.clone()) {
                                    self.subst.insert(*r, y);
                                }
                                arm.pop();
                            }
                            _ => ends = never_falls(&arm),
                        }
                        out.extend(arm);
                        if ends {
                            break;
                        }
                        continue;
                    }
                    let then = self.region(then);
                    let els = self.region(els);
                    out.push(Node::If { cond, then, els, results });
                }
                Node::Loop { inits, params, body, results } => {
                    let inits = inits.into_iter().map(|v| self.sub(v)).collect();
                    let body = self.region(body);
                    out.push(Node::Loop { inits, params, body, results });
                }
            }
        }
        out
    }
}

/// Run folding once. Returns true if anything changed.
pub fn fold(f: &mut Function) -> bool {
    let body = std::mem::take(&mut f.body);
    let mut s = F {
        f,
        k: HashMap::new(),
        tz: HashMap::new(),
        subst: HashMap::new(),
        new_consts: HashMap::new(),
        prelude: vec![],
        changed: false,
        defs: HashMap::new(),
    };
    let body = s.region(body);
    let changed = s.changed;
    let mut all = std::mem::take(&mut s.prelude);
    all.extend(body);
    f.body = all;
    changed
}
