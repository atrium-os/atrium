//! Build GIR from the frontend IR (`atrium-spv-ir`).
//!
//! The frontend's header and merge annotations are not trusted (spec §1):
//! the region tree is rebuilt from the CFG. Loops come from DFS back edges;
//! each conditional branch reconverges at its immediate post-dominator
//! within the enclosing loop. Frontend phis are demoted to private
//! variables (stored in each predecessor, loaded at the phi), and the
//! `mem2reg` pass turns them back into SSA.

use crate::*;
use atrium_spv_ir as spv;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// Why a function could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// A construct GIR (or this builder) does not handle yet.
    Unsupported(String),
    /// The input breaks an assumption the frontend guarantees.
    Malformed(String),
}

fn unsup<T>(s: impl Into<String>) -> Result<T, BuildError> {
    Err(BuildError::Unsupported(s.into()))
}

fn malformed<T>(s: impl Into<String>) -> Result<T, BuildError> {
    Err(BuildError::Malformed(s.into()))
}

type B = spv::BlockId;

#[derive(Clone)]
struct LoopInfo {
    header: B,
    body: BTreeSet<B>,
    exit: Option<B>,
}

#[derive(Clone, Copy)]
enum Val {
    V(ValueId),
    Ptr(Space, ValueId),
}

struct Builder<'a> {
    f: &'a spv::Function,
    out: Function,
    succ: HashMap<B, Vec<B>>,
    loops: BTreeMap<B, LoopInfo>,
    vals: HashMap<spv::ValueId, Val>,
    prelude: Region,
    consts: HashMap<(u32, Ty), ValueId>,
    /// Phi demotion: stores each predecessor performs before its terminator.
    pred_stores: HashMap<B, Vec<(u32, spv::ValueId)>>,
    /// Phi result → its private variable.
    phi_var: HashMap<spv::ValueId, u32>,
    translated: HashSet<B>,
    /// Blocks of a loop's continue construct: reached from every
    /// `continue` path, so they are duplicated per path.
    dup_ok: HashSet<B>,
}

fn terminator(b: &spv::Block) -> Option<&spv::Op> {
    b.insts.last().map(|i| &i.op).filter(|op| {
        matches!(op, spv::Op::Branch(_) | spv::Op::BranchCond { .. } | spv::Op::Switch { .. } | spv::Op::Return | spv::Op::ReturnValue(_) | spv::Op::Discard)
    })
}

fn successors(b: &spv::Block) -> Vec<B> {
    match terminator(b) {
        Some(spv::Op::Branch(t)) => vec![*t],
        Some(spv::Op::BranchCond { t_block, f_block, .. }) => vec![*t_block, *f_block],
        Some(spv::Op::Switch { cases, default, .. }) => {
            let mut v: Vec<B> = cases.iter().map(|c| c.1).collect();
            v.push(*default);
            v
        }
        _ => vec![],
    }
}

/// Map a frontend type to GIR.
fn map_ty(t: &spv::Type) -> Result<Ty, BuildError> {
    use spv::{Type as T, VecElement as E};
    let e = |e: &E| match e {
        E::F32 => Ok(Scalar::F32),
        E::I32 | E::U32 => Ok(Scalar::I32),
        E::F64 => unsup("f64 vectors"),
    };
    Ok(match t {
        T::Bool => Ty::BOOL,
        T::I32 | T::U32 => Ty::I32,
        T::F32 => Ty::F32,
        T::Vec2(x) => Ty::V(e(x)?, 2),
        T::Vec3(x) => Ty::V(e(x)?, 3),
        T::Vec4(x) => Ty::V(e(x)?, 4),
        other => return unsup(format!("type {other:?}")),
    })
}

impl<'a> Builder<'a> {
    // ── CFG analysis ────────────────────────────────────────────

    fn analyse(&mut self) -> Result<(), BuildError> {
        for (id, b) in &self.f.blocks {
            self.succ.insert(*id, successors(b));
        }
        // Iterative DFS for back edges.
        let mut state: HashMap<B, u8> = HashMap::new(); // 1 = on stack, 2 = done
        let mut back: Vec<(B, B)> = vec![];
        let mut stack: Vec<(B, usize)> = vec![(self.f.entry_block, 0)];
        state.insert(self.f.entry_block, 1);
        while let Some((b, i)) = stack.pop() {
            let s = self.succ.get(&b).cloned().unwrap_or_default();
            if i < s.len() {
                stack.push((b, i + 1));
                let t = s[i];
                if !self.f.blocks.contains_key(&t) {
                    return malformed(format!("branch to missing block {t:?}"));
                }
                match state.get(&t) {
                    Some(1) => back.push((b, t)),
                    Some(_) => {}
                    None => {
                        state.insert(t, 1);
                        stack.push((t, 0));
                    }
                }
            } else {
                state.insert(b, 2);
            }
        }
        // Predecessors (reachable blocks only).
        let mut pred: HashMap<B, Vec<B>> = HashMap::new();
        for b in state.keys() {
            for s in &self.succ[b] {
                pred.entry(*s).or_default().push(*b);
            }
        }
        // Natural loops, merged per header.
        for (latch, header) in back {
            let info = self.loops.entry(header).or_insert_with(|| LoopInfo { header, body: BTreeSet::from([header]), exit: None });
            let mut work = vec![latch];
            while let Some(b) = work.pop() {
                if info.body.insert(b) {
                    work.extend(pred.get(&b).cloned().unwrap_or_default());
                }
            }
        }
        // A structured loop's body also holds blocks that leave it by
        // `break` (they don't reach the back edge, so the natural loop
        // misses them): take everything reachable from the header
        // without passing the SPIR-V merge block.
        for info in self.loops.values_mut() {
            let merge = match self.f.blocks.get(&info.header).map(|b| &b.kind) {
                Some(atrium_spv_ir::BlockKind::LoopHeader { merge, .. }) => *merge,
                _ => continue,
            };
            let mut work = vec![info.header];
            let mut seen = BTreeSet::from([info.header]);
            while let Some(b) = work.pop() {
                for s in &self.succ[&b] {
                    if *s != merge && seen.insert(*s) {
                        work.push(*s);
                    }
                }
            }
            info.body.extend(seen);
        }
        for info in self.loops.values() {
            let cont = match self.f.blocks.get(&info.header).map(|b| &b.kind) {
                Some(atrium_spv_ir::BlockKind::LoopHeader { continue_, .. }) if *continue_ != info.header => *continue_,
                _ => continue,
            };
            let mut work = vec![cont];
            while let Some(b) = work.pop() {
                if b != info.header && info.body.contains(&b) && self.dup_ok.insert(b) {
                    work.extend(self.succ[&b].iter().copied());
                }
            }
        }
        for info in self.loops.values_mut() {
            let mut exits = BTreeSet::new();
            for b in &info.body {
                for s in &self.succ[b] {
                    if !info.body.contains(s) {
                        exits.insert(*s);
                    }
                }
            }
            if exits.len() > 1 {
                return unsup(format!("loop at {:?} has {} exits {exits:?}", info.header, exits.len()));
            }
            info.exit = exits.into_iter().next();
        }
        Ok(())
    }

    /// Immediate post-dominator of `b` inside the loop `lp` (or the whole
    /// function), treating the loop's back and exit edges and returns as
    /// edges to a virtual sink. `None` means the sink.
    fn merge_of(&self, b: B, lp: Option<&LoopInfo>) -> Option<B> {
        let nodes: Vec<B> = match lp {
            Some(l) => l.body.iter().copied().collect(),
            None => self.succ.keys().copied().collect(),
        };
        let inside = |x: &B| nodes.contains(x) && lp.map_or(true, |l| *x != l.header);
        let idx: HashMap<B, usize> = nodes.iter().enumerate().map(|(i, b)| (*b, i)).collect();
        let n = nodes.len();
        let sink = n;
        let succs: Vec<Vec<usize>> = nodes
            .iter()
            .map(|x| {
                let s: Vec<usize> = self.succ[x].iter().map(|t| if inside(t) { idx[t] } else { sink }).collect();
                if s.is_empty() { vec![sink] } else { s }
            })
            .collect();
        // pdom sets as bit vectors; start from "everything".
        let full = vec![true; n + 1];
        let mut pdom: Vec<Vec<bool>> = vec![full; n + 1];
        pdom[sink] = (0..=n).map(|i| i == sink).collect();
        let mut changed = true;
        while changed {
            changed = false;
            for i in 0..n {
                let mut s = vec![true; n + 1];
                for &t in &succs[i] {
                    for k in 0..=n {
                        s[k] &= pdom[t][k];
                    }
                }
                s[i] = true;
                if s != pdom[i] {
                    pdom[i] = s;
                    changed = true;
                }
            }
        }
        let me = idx[&b];
        let count = |i: usize| pdom[i].iter().filter(|x| **x).count();
        let mine = count(me);
        let ipdom = (0..=n).find(|&d| d != me && pdom[me][d] && count(d) == mine - 1)?;
        if ipdom == sink { None } else { Some(nodes[ipdom]) }
    }

    // ── Values ──────────────────────────────────────────────────

    fn konst(&mut self, bits: u32, ty: Ty) -> ValueId {
        if let Some(v) = self.consts.get(&(bits, ty)) {
            return *v;
        }
        let v = self.out.new_value(ty);
        self.prelude.push(Node::Inst(Inst { result: Some(v), op: Op::Const(bits) }));
        self.consts.insert((bits, ty), v);
        v
    }

    fn emit(&mut self, r: &mut Region, ty: Ty, op: Op) -> ValueId {
        let v = self.out.new_value(ty);
        r.push(Node::Inst(Inst { result: Some(v), op }));
        v
    }

    fn emit_void(r: &mut Region, op: Op) {
        r.push(Node::Inst(Inst { result: None, op }));
    }

    fn val(&self, v: &spv::Value) -> Result<ValueId, BuildError> {
        match self.vals.get(&v.id) {
            Some(Val::V(x)) => Ok(*x),
            Some(Val::Ptr(..)) => unsup(format!("pointer value {:?} used as data", v.id)),
            None => malformed(format!("value {:?} used before definition", v.id)),
        }
    }

    fn ptr(&self, v: &spv::Value) -> Result<(Space, ValueId), BuildError> {
        match self.vals.get(&v.id) {
            Some(Val::Ptr(s, o)) => Ok((*s, *o)),
            _ => unsup(format!("value {:?} is not a known pointer", v.id)),
        }
    }

    fn comp(&mut self, r: &mut Region, v: ValueId, i: usize) -> ValueId {
        match self.out.ty(v) {
            Ty::S(_) => v,
            Ty::V(s, _) => self.emit(r, Ty::S(s), Op::Extract(v, i as u8)),
        }
    }

    /// Apply `mk` per component when `res` is a vector.
    fn map_n(&mut self, r: &mut Region, res: Ty, args: &[ValueId], mk: &dyn Fn(&[ValueId]) -> Op) -> ValueId {
        if res.comps() == 1 {
            return self.emit(r, res, mk(args));
        }
        let mut parts = vec![];
        for i in 0..res.comps() {
            let a: Vec<ValueId> = args.iter().map(|x| self.comp(r, *x, i)).collect();
            parts.push(self.emit(r, Ty::S(res.scalar()), mk(&a)));
        }
        self.emit(r, res, Op::Construct(parts))
    }

    // ── Instructions ────────────────────────────────────────────

    fn inst(&mut self, r: &mut Region, inst: &spv::Inst) -> Result<(), BuildError> {
        use spv::Op as S;
        let res_ty = match &inst.result {
            Some(v) if !matches!(v.ty, spv::Type::Pointer(..) | spv::Type::Void) => Some(map_ty(&v.ty)?),
            _ => None,
        };
        let rt = || res_ty.ok_or_else(|| BuildError::Malformed(format!("{:?} has no data result", inst.op)));
        let bin = |o: BinOp| move |a: &[ValueId]| Op::Bin(o, a[0], a[1]);
        let un = |o: UnOp| move |a: &[ValueId]| Op::Un(o, a[0]);
        let cmp = |o: CmpOp, swap: bool| move |a: &[ValueId]| if swap { Op::Cmp(o, a[1], a[0]) } else { Op::Cmp(o, a[0], a[1]) };
        let out: Val = match &inst.op {
            S::ConstInt { value, kind } => {
                if matches!(kind, spv::IntKind::I64 | spv::IntKind::U64) {
                    return unsup("64-bit integer constant");
                }
                let t = rt()?;
                let bits = if t == Ty::BOOL { (*value != 0) as u32 } else { *value as u32 };
                Val::V(self.konst(bits, t))
            }
            S::ConstFloat { value, kind } => {
                if *kind != spv::FloatKind::F32 {
                    return unsup("f64 constant");
                }
                Val::V(self.konst((*value as f32).to_bits(), Ty::F32))
            }
            S::ConstNull => {
                let t = rt()?;
                let z = self.konst(0, Ty::S(t.scalar()));
                if t.comps() == 1 { Val::V(z) } else { Val::V(self.emit(r, t, Op::Construct(vec![z; t.comps()]))) }
            }
            S::ConstVec(elems) => {
                let t = rt()?;
                let mut parts = vec![];
                for e in elems {
                    let x = self.val(e)?;
                    for i in 0..self.out.ty(x).comps() {
                        parts.push(self.comp(r, x, i));
                    }
                }
                if parts.len() != t.comps() {
                    return malformed("vector construct size mismatch");
                }
                Val::V(self.emit(r, t, Op::Construct(parts)))
            }
            S::IAdd(a, b) | S::ISub(a, b) | S::IMul(a, b) | S::UDiv(a, b) | S::SDiv(a, b) | S::UMod(a, b)
            | S::FAdd(a, b) | S::FSub(a, b) | S::FMul(a, b) | S::FDiv(a, b) | S::FMin(a, b) | S::FMax(a, b)
            | S::BitAnd(a, b) | S::BitOr(a, b) | S::BitXor(a, b) | S::Shl(a, b) | S::LShr(a, b) | S::AShr(a, b) => {
                let o = match &inst.op {
                    S::IAdd(..) => BinOp::IAdd,
                    S::ISub(..) => BinOp::ISub,
                    S::IMul(..) => BinOp::IMul,
                    S::UDiv(..) => BinOp::UDiv,
                    S::SDiv(..) => BinOp::SDiv,
                    S::UMod(..) => BinOp::UMod,
                    S::FAdd(..) => BinOp::FAdd,
                    S::FSub(..) => BinOp::FSub,
                    S::FMul(..) => BinOp::FMul,
                    S::FDiv(..) => BinOp::FDiv,
                    S::FMin(..) => BinOp::FMin,
                    S::FMax(..) => BinOp::FMax,
                    S::BitAnd(..) => BinOp::And,
                    S::BitOr(..) => BinOp::Or,
                    S::BitXor(..) => BinOp::Xor,
                    S::Shl(..) => BinOp::Shl,
                    S::LShr(..) => BinOp::LShr,
                    _ => BinOp::AShr,
                };
                let (a, b) = (self.val(a)?, self.val(b)?);
                if matches!(o, BinOp::And | BinOp::Or | BinOp::Xor)
                    && self.out.ty(a) == Ty::BOOL && self.out.ty(b) == Ty::BOOL
                {
                    // LogicalAnd/Or arrive as a bit op on bools (then `!= 0`):
                    // combine the masks, then widen.
                    let m = self.emit(r, Ty::BOOL, Op::Bin(o, a, b));
                    Val::V(self.as_data(r, m))
                } else {
                    Val::V(self.map_n(r, rt()?, &[a, b], &bin(o)))
                }
            }
            S::SMod(a, b) => {
                // Floored remainder (sign of the divisor): the truncated
                // remainder, plus the divisor when it is non-zero and its
                // sign differs from the divisor's.
                let (a, b) = (self.val(a)?, self.val(b)?);
                let t = rt()?;
                let z = self.konst(0, Ty::I32);
                let mut parts = vec![];
                for i in 0..t.comps() {
                    let (x, y) = (self.comp(r, a, i), self.comp(r, b, i));
                    let rem = self.emit(r, Ty::I32, Op::Bin(BinOp::SRem, x, y));
                    let sx = self.emit(r, Ty::I32, Op::Bin(BinOp::Xor, rem, y));
                    let neg = self.emit(r, Ty::BOOL, Op::Cmp(CmpOp::SLt, sx, z));
                    let nz = self.emit(r, Ty::BOOL, Op::Cmp(CmpOp::INe, rem, z));
                    let fix = self.emit(r, Ty::BOOL, Op::Bin(BinOp::And, neg, nz));
                    let adj = self.emit(r, Ty::I32, Op::Bin(BinOp::IAdd, rem, y));
                    parts.push(self.emit(r, Ty::I32, Op::Select(fix, adj, rem)));
                }
                Val::V(if parts.len() == 1 { parts[0] } else { self.emit(r, t, Op::Construct(parts)) })
            }
            S::INeg(a) | S::FNeg(a) | S::FAbs(a) | S::FSqrt(a) | S::FFloor(a) | S::FCeil(a) | S::FTrunc(a)
            | S::BitNot(a) | S::Clz(a) | S::Rbit(a) => {
                let o = match &inst.op {
                    S::INeg(_) => UnOp::INeg,
                    S::FNeg(_) => UnOp::FNeg,
                    S::FAbs(_) => UnOp::FAbs,
                    S::FSqrt(_) => UnOp::FSqrt,
                    S::FFloor(_) => UnOp::FFloor,
                    S::FCeil(_) => UnOp::FCeil,
                    S::FTrunc(_) => UnOp::FTrunc,
                    S::BitNot(_) => UnOp::Not,
                    S::Clz(_) => UnOp::Clz,
                    _ => UnOp::BitReverse,
                };
                let a = self.val(a)?;
                Val::V(self.map_n(r, rt()?, &[a], &un(o)))
            }
            S::Fma(a, b, c) => {
                let (a, b, c) = (self.val(a)?, self.val(b)?, self.val(c)?);
                Val::V(self.map_n(r, rt()?, &[a, b, c], &|x| Op::Fma(x[0], x[1], x[2])))
            }
            S::IEq(a, b) | S::INe(a, b) | S::ULt(a, b) | S::ULe(a, b) | S::UGt(a, b) | S::UGe(a, b)
            | S::SLt(a, b) | S::SLe(a, b) | S::SGt(a, b) | S::SGe(a, b)
            | S::FOrdEq(a, b) | S::FOrdNe(a, b) | S::FOrdLt(a, b) | S::FOrdLe(a, b) | S::FOrdGt(a, b) | S::FOrdGe(a, b)
            | S::FUnordEq(a, b) | S::FUnordNe(a, b) | S::FUnordLt(a, b) | S::FUnordLe(a, b) | S::FUnordGt(a, b) | S::FUnordGe(a, b) => {
                let (o, swap) = match &inst.op {
                    S::IEq(..) => (CmpOp::IEq, false),
                    S::INe(..) => (CmpOp::INe, false),
                    S::ULt(..) => (CmpOp::ULt, false),
                    S::ULe(..) => (CmpOp::ULe, false),
                    S::UGt(..) => (CmpOp::ULt, true),
                    S::UGe(..) => (CmpOp::ULe, true),
                    S::SLt(..) => (CmpOp::SLt, false),
                    S::SLe(..) => (CmpOp::SLe, false),
                    S::SGt(..) => (CmpOp::SLt, true),
                    S::SGe(..) => (CmpOp::SLe, true),
                    S::FOrdEq(..) => (CmpOp::FOEq, false),
                    S::FOrdNe(..) => (CmpOp::FONe, false),
                    S::FOrdLt(..) => (CmpOp::FOLt, false),
                    S::FOrdLe(..) => (CmpOp::FOLe, false),
                    S::FOrdGt(..) => (CmpOp::FOLt, true),
                    S::FOrdGe(..) => (CmpOp::FOLe, true),
                    S::FUnordEq(..) => (CmpOp::FUEq, false),
                    S::FUnordNe(..) => (CmpOp::FUNe, false),
                    S::FUnordLt(..) => (CmpOp::FULt, false),
                    S::FUnordLe(..) => (CmpOp::FULe, false),
                    S::FUnordGt(..) => (CmpOp::FULt, true),
                    _ => (CmpOp::FULe, true),
                };
                let (a, b) = (self.val(a)?, self.val(b)?);
                let t = rt()?;
                if t.scalar() == Scalar::Bool {
                    Val::V(self.map_n(r, t, &[a, b], &cmp(o, swap)))
                } else {
                    // The frontend's own expansions (sign, step, …) type
                    // some comparisons as 0/1 integers.
                    let mut parts = vec![];
                    for i in 0..t.comps() {
                        let (ai, bi) = (self.comp(r, a, i), self.comp(r, b, i));
                        let ci = self.emit(r, Ty::BOOL, cmp(o, swap)(&[ai, bi]));
                        let x = self.emit(r, Ty::I32, Op::Conv(ConvOp::BoolToI, ci));
                        parts.push(if t.scalar() == Scalar::F32 { self.emit(r, Ty::F32, Op::Conv(ConvOp::UToF, x)) } else { x });
                    }
                    Val::V(if parts.len() == 1 { parts[0] } else { self.emit(r, t, Op::Construct(parts)) })
                }
            }
            S::Select { cond, t_val, f_val } => {
                let (c, t, f) = (self.val(cond)?, self.val(t_val)?, self.val(f_val)?);
                let res = rt()?;
                if res.comps() > 1 && self.out.ty(c).comps() == 1 {
                    let mut parts = vec![];
                    for i in 0..res.comps() {
                        let (ti, fi) = (self.comp(r, t, i), self.comp(r, f, i));
                        parts.push(self.emit(r, Ty::S(res.scalar()), Op::Select(c, ti, fi)));
                    }
                    Val::V(self.emit(r, res, Op::Construct(parts)))
                } else {
                    Val::V(self.map_n(r, res, &[c, t, f], &|x| Op::Select(x[0], x[1], x[2])))
                }
            }
            S::ConvertSToF(a) | S::ConvertUToF(a) | S::ConvertFToS(a) | S::ConvertFToU(a) => {
                let o = match &inst.op {
                    S::ConvertSToF(_) => ConvOp::SToF,
                    S::ConvertUToF(_) => ConvOp::UToF,
                    S::ConvertFToS(_) => ConvOp::FToS,
                    _ => ConvOp::FToU,
                };
                let a = self.val(a)?;
                // `float(b)` on a bool: 0/1 as an integer first.
                let a = if self.out.ty(a).scalar() == Scalar::Bool && matches!(o, ConvOp::SToF | ConvOp::UToF) {
                    let t = self.out.ty(a);
                    let mut parts = vec![];
                    for i in 0..t.comps() {
                        let c = self.comp(r, a, i);
                        parts.push(self.emit(r, Ty::I32, Op::Conv(ConvOp::BoolToI, c)));
                    }
                    if parts.len() == 1 { parts[0] } else { self.emit(r, Ty::V(Scalar::I32, t.comps() as u8), Op::Construct(parts)) }
                } else {
                    a
                };
                Val::V(self.map_n(r, rt()?, &[a], &|x| Op::Conv(o, x[0])))
            }
            S::SConvert(a, k) | S::UConvert(a, k) => {
                if matches!(k, spv::IntKind::I64 | spv::IntKind::U64) {
                    return unsup("64-bit integer conversion");
                }
                Val::V(self.val(a)?)
            }
            S::FConvert(a, k) => {
                if *k != spv::FloatKind::F32 {
                    return unsup("f64 conversion");
                }
                Val::V(self.val(a)?)
            }
            S::Bitcast(a, _) => {
                let a = self.val(a)?;
                let t = rt()?;
                if self.out.ty(a) == t { Val::V(a) } else { Val::V(self.emit(r, t, Op::Bitcast(a))) }
            }
            S::VectorExtract { vector, index } => {
                let v = self.val(vector)?;
                Val::V(self.comp(r, v, *index as usize))
            }
            S::VectorInsert { vector, scalar, index } => {
                let (v, s) = (self.val(vector)?, self.val(scalar)?);
                let t = rt()?;
                let parts: Vec<ValueId> = (0..t.comps()).map(|i| if i == *index as usize { s } else { self.comp(r, v, i) }).collect();
                Val::V(self.emit(r, t, Op::Construct(parts)))
            }
            S::VectorShuffle { src1, src2, components } => {
                let (a, b) = (self.val(src1)?, self.val(src2)?);
                let na = self.out.ty(a).comps();
                let mut parts = vec![];
                for &c in components {
                    let c = c as usize;
                    parts.push(if c < na { self.comp(r, a, c) } else { self.comp(r, b, c - na) });
                }
                let t = rt()?;
                Val::V(if parts.len() == 1 { parts[0] } else { self.emit(r, t, Op::Construct(parts)) })
            }
            S::Dot(a, b) => {
                let (a, b) = (self.val(a)?, self.val(b)?);
                let n = self.out.ty(a).comps();
                let mut acc = None;
                for i in 0..n {
                    let (x, y) = (self.comp(r, a, i), self.comp(r, b, i));
                    let p = self.emit(r, Ty::F32, Op::Bin(BinOp::FMul, x, y));
                    acc = Some(match acc {
                        None => p,
                        Some(s) => self.emit(r, Ty::F32, Op::Bin(BinOp::FAdd, s, p)),
                    });
                }
                Val::V(acc.unwrap())
            }
            S::LoadBuiltin(k) => {
                let (b, t) = match k {
                    spv::BuiltinKind::GlobalInvocationId => (Builtin::GlobalId, Ty::V(Scalar::I32, 3)),
                    spv::BuiltinKind::LocalInvocationId => (Builtin::LocalId, Ty::V(Scalar::I32, 3)),
                    spv::BuiltinKind::WorkgroupId => (Builtin::WorkgroupId, Ty::V(Scalar::I32, 3)),
                    spv::BuiltinKind::LocalInvocationIndex => (Builtin::LocalIndex, Ty::I32),
                    spv::BuiltinKind::SubgroupSize => (Builtin::SubgroupSize, Ty::I32),
                    spv::BuiltinKind::SubgroupLocalInvocationId => (Builtin::SubgroupLocalId, Ty::I32),
                    spv::BuiltinKind::SubgroupId => (Builtin::SubgroupId, Ty::I32),
                    spv::BuiltinKind::NumSubgroups => (Builtin::NumSubgroups, Ty::I32),
                    other => return unsup(format!("builtin {other:?} in compute")),
                };
                Val::V(self.emit(r, t, Op::Builtin(b)))
            }
            S::AccessChain { base, byte_offset } => {
                let is_push = matches!(&inst.result, Some(spv::Value { ty: spv::Type::Pointer(spv::StorageClass::PushConstant, _), .. }));
                if is_push && !self.vals.contains_key(&base.id) {
                    let z = self.konst(0, Ty::I32);
                    self.vals.insert(base.id, Val::Ptr(Space::Push, z));
                }
                let (s, o) = self.ptr(base)?;
                if *byte_offset == 0 {
                    Val::Ptr(s, o)
                } else {
                    let k = self.konst(*byte_offset, Ty::I32);
                    Val::Ptr(s, self.emit(r, Ty::I32, Op::Bin(BinOp::IAdd, o, k)))
                }
            }
            S::PtrOffsetDynamic { base, index, stride } => {
                let (s, o) = self.ptr(base)?;
                let i = self.val(index)?;
                let k = self.konst(*stride, Ty::I32);
                let m = self.emit(r, Ty::I32, Op::Bin(BinOp::IMul, i, k));
                Val::Ptr(s, self.emit(r, Ty::I32, Op::Bin(BinOp::IAdd, o, m)))
            }
            S::LocalAlloc { bytes, .. } => {
                self.out.privates.push(*bytes);
                let z = self.konst(0, Ty::I32);
                Val::Ptr(Space::Private(self.out.privates.len() as u32 - 1), z)
            }
            S::Load(p) | S::AtomicLoad(p) => {
                let (s, o) = self.ptr(p)?;
                Val::V(self.emit(r, rt()?, Op::Load(s, o)))
            }
            S::Store { ptr, value } | S::AtomicStore { ptr, value } => {
                let (s, o) = self.ptr(ptr)?;
                if s == Space::Push {
                    return malformed("store to push constants");
                }
                let v = self.val(value)?;
                Self::emit_void(r, Op::Store(s, o, v));
                return Ok(());
            }
            S::AtomicIAdd { ptr, value } | S::AtomicAnd { ptr, value } | S::AtomicOr { ptr, value }
            | S::AtomicXor { ptr, value } | S::AtomicSMin { ptr, value } | S::AtomicSMax { ptr, value }
            | S::AtomicUMin { ptr, value } | S::AtomicUMax { ptr, value } | S::AtomicExchange { ptr, value } => {
                let o = match &inst.op {
                    S::AtomicIAdd { .. } => AtomicOp::Add,
                    S::AtomicAnd { .. } => AtomicOp::And,
                    S::AtomicOr { .. } => AtomicOp::Or,
                    S::AtomicXor { .. } => AtomicOp::Xor,
                    S::AtomicSMin { .. } => AtomicOp::SMin,
                    S::AtomicSMax { .. } => AtomicOp::SMax,
                    S::AtomicUMin { .. } => AtomicOp::UMin,
                    S::AtomicUMax { .. } => AtomicOp::UMax,
                    _ => AtomicOp::Exchange,
                };
                let (s, off) = self.ptr(ptr)?;
                let v = self.val(value)?;
                Val::V(self.emit(r, Ty::I32, Op::Atomic(o, s, off, v)))
            }
            S::Barrier => {
                Self::emit_void(r, Op::Barrier);
                return Ok(());
            }
            S::Subgroup { op, args } => Val::V(self.subgroup(r, *op, args, rt()?)?),
            S::Phi(_) => {
                let res = inst.result.as_ref().unwrap();
                let var = self.phi_var[&res.id];
                let z = self.konst(0, Ty::I32);
                Val::V(self.emit(r, rt()?, Op::Load(Space::Private(var), z)))
            }
            other => return unsup(format!("op {other:?}")),
        };
        match &inst.result {
            Some(v) => {
                self.vals.insert(v.id, out);
            }
            None => return malformed(format!("{:?} has no result", inst.op)),
        }
        Ok(())
    }

    /// A frontend subgroup op: vectors are done per component and `bool`
    /// data goes through `i32` (GIR subgroup data is scalar `i32`/`f32`).
    fn subgroup(&mut self, r: &mut Region, op: spv::SubgroupOp, args: &[spv::Value], res: Ty) -> Result<ValueId, BuildError> {
        use spv::{GroupArith as A, SubgroupOp as S};
        let a: Vec<ValueId> = args.iter().map(|v| self.val(v)).collect::<Result<_, _>>()?;
        let g = |x: A, is_bool: bool, is_float: bool| -> Result<GroupArith, BuildError> {
            Ok(match x {
                A::IAdd => GroupArith::IAdd,
                A::FAdd => GroupArith::FAdd,
                A::IMul => GroupArith::IMul,
                A::FMul => GroupArith::FMul,
                A::SMin => GroupArith::SMin,
                A::UMin => GroupArith::UMin,
                A::FMin => GroupArith::FMin,
                A::SMax => GroupArith::SMax,
                A::UMax => GroupArith::UMax,
                A::FMax => GroupArith::FMax,
                // Logical ops on 0/1 values are the bitwise ones.
                A::And => GroupArith::And,
                A::Or => GroupArith::Or,
                A::Xor => GroupArith::Xor,
            })
            .and_then(|o| if o.is_float() != is_float && !is_bool { unsup(format!("{x:?} on this type")) } else { Ok(o) })
        };
        let sop = |is_bool: bool, is_float: bool| -> Result<SubgroupOp, BuildError> {
            Ok(match op {
                S::Elect => SubgroupOp::Elect,
                S::All => SubgroupOp::All,
                S::Any => SubgroupOp::Any,
                S::AllEqual => SubgroupOp::AllEqual,
                S::Ballot => SubgroupOp::Ballot,
                S::Broadcast => SubgroupOp::Broadcast,
                S::BroadcastFirst => SubgroupOp::BroadcastFirst,
                S::Shuffle => SubgroupOp::Shuffle,
                S::ShuffleXor => SubgroupOp::ShuffleXor,
                S::ShuffleUp => SubgroupOp::ShuffleUp,
                S::ShuffleDown => SubgroupOp::ShuffleDown,
                S::Reduce(x) => SubgroupOp::Reduce(g(x, is_bool, is_float)?),
                S::InclusiveScan(x) => SubgroupOp::InclusiveScan(g(x, is_bool, is_float)?),
                S::ExclusiveScan(x) => SubgroupOp::ExclusiveScan(g(x, is_bool, is_float)?),
            })
        };
        match op {
            S::Elect => Ok(self.emit(r, Ty::BOOL, Op::Subgroup(SubgroupOp::Elect, vec![]))),
            S::All | S::Any => Ok(self.emit(r, Ty::BOOL, Op::Subgroup(sop(true, false)?, vec![a[0]]))),
            S::Ballot => {
                let m = self.emit(r, Ty::I32, Op::Subgroup(SubgroupOp::Ballot, vec![a[0]]));
                let z = self.konst(0, Ty::I32);
                Ok(self.emit(r, res, Op::Construct(vec![m, z, z, z])))
            }
            S::AllEqual => {
                // All components equal.
                let t = self.out.ty(a[0]);
                let mut acc: Option<ValueId> = None;
                for i in 0..t.comps() {
                    let x = self.comp(r, a[0], i);
                    let x = self.as_data(r, x);
                    let e = self.emit(r, Ty::BOOL, Op::Subgroup(SubgroupOp::AllEqual, vec![x]));
                    acc = Some(match acc {
                        None => e,
                        Some(p) => self.emit(r, Ty::BOOL, Op::Bin(BinOp::And, p, e)),
                    });
                }
                Ok(acc.unwrap())
            }
            _ => {
                // Data ops: (x [, i32 operand]) -> x, per component.
                let is_bool = res.scalar() == Scalar::Bool;
                let o = sop(is_bool, res.scalar() == Scalar::F32)?;
                let extra = a.get(1).copied();
                let mut parts = vec![];
                for i in 0..res.comps() {
                    let x = self.comp(r, a[0], i);
                    let x = self.as_data(r, x);
                    let mut ops = vec![x];
                    ops.extend(extra);
                    let y = self.emit(r, self.out.ty(x), Op::Subgroup(o, ops));
                    parts.push(if is_bool {
                        let z = self.konst(0, Ty::I32);
                        self.emit(r, Ty::BOOL, Op::Cmp(CmpOp::INe, y, z))
                    } else {
                        y
                    });
                }
                Ok(if parts.len() == 1 { parts[0] } else { self.emit(r, res, Op::Construct(parts)) })
            }
        }
    }

    /// `bool` as `i32` 0/1; other scalars unchanged.
    fn as_data(&mut self, r: &mut Region, x: ValueId) -> ValueId {
        if self.out.ty(x) == Ty::BOOL {
            self.emit(r, Ty::I32, Op::Conv(ConvOp::BoolToI, x))
        } else {
            x
        }
    }

    // ── Structurisation ─────────────────────────────────────────

    fn block_body(&mut self, r: &mut Region, b: B) -> Result<(), BuildError> {
        if !self.translated.insert(b) && !self.dup_ok.contains(&b) {
            return unsup(format!("block {b:?} reached on two structured paths (unstructured control flow)"));
        }
        let block = &self.f.blocks[&b];
        let n = block.insts.len() - terminator(block).is_some() as usize;
        for inst in &block.insts[..n] {
            self.inst(r, inst)?;
        }
        for (var, v) in self.pred_stores.get(&b).cloned().unwrap_or_default() {
            let value = self.val(&spv::Value { id: v, ty: spv::Type::Void })?;
            let z = self.konst(0, Ty::I32);
            Self::emit_void(r, Op::Store(Space::Private(var), z, value));
        }
        Ok(())
    }

    /// Build the region that starts at `start` and ends on reaching `stop`,
    /// a `return`, or (inside `lp`) a back edge or exit edge.
    /// `loop_body` is true when `start` is `lp`'s own header.
    fn region(&mut self, start: B, stop: Option<B>, lp: Option<&LoopInfo>, loop_body: bool) -> Result<Region, BuildError> {
        let mut r: Region = vec![];
        let mut cur = start;
        let mut first = true;
        loop {
            if Some(cur) == stop {
                break;
            }
            if let Some(l) = lp {
                if cur == l.header && !(first && loop_body) {
                    Self::emit_void(&mut r, Op::Continue(vec![]));
                    break;
                }
                if Some(cur) == l.exit {
                    Self::emit_void(&mut r, Op::Break(vec![]));
                    break;
                }
            }
            let entering_own = first && loop_body;
            first = false;
            if !entering_own {
                if let Some(inner) = self.loops.get(&cur).cloned() {
                    let body = self.region(cur, None, Some(&inner), true)?;
                    r.push(Node::Loop { inits: vec![], params: vec![], body, results: vec![] });
                    match inner.exit {
                        Some(e) => {
                            cur = e;
                            continue;
                        }
                        None => break,
                    }
                }
            }
            self.block_body(&mut r, cur)?;
            let block = &self.f.blocks[&cur];
            match terminator(block).cloned() {
                Some(spv::Op::Return) => {
                    Self::emit_void(&mut r, Op::Return);
                    break;
                }
                Some(spv::Op::Branch(t)) => cur = t,
                Some(spv::Op::BranchCond { cond, t_block, f_block }) => {
                    let c = self.val(&cond)?;
                    let m = self.merge_of(cur, lp);
                    let then = self.region(t_block, m, lp, false)?;
                    let els = self.region(f_block, m, lp, false)?;
                    r.push(Node::If { cond: c, then, els, results: vec![] });
                    match m {
                        Some(b) => cur = b,
                        None => break,
                    }
                }
                Some(spv::Op::Switch { selector, cases, default }) => {
                    let sel = self.val(&selector)?;
                    let m = self.merge_of(cur, lp);
                    let tree = self.switch_chain(sel, &cases, default, m, lp)?;
                    r.extend(tree);
                    match m {
                        Some(b) => cur = b,
                        None => break,
                    }
                }
                Some(other) => return unsup(format!("terminator {other:?}")),
                None => return malformed(format!("block {cur:?} has no terminator")),
            }
        }
        Ok(r)
    }

    fn switch_chain(&mut self, sel: ValueId, cases: &[(i64, B)], default: B, m: Option<B>, lp: Option<&LoopInfo>) -> Result<Region, BuildError> {
        let Some(((v, t), rest)) = cases.split_first() else {
            return self.region(default, m, lp, false);
        };
        let mut r = vec![];
        let k = self.konst(*v as u32, Ty::I32);
        let c = self.emit(&mut r, Ty::BOOL, Op::Cmp(CmpOp::IEq, sel, k));
        let then = self.region(*t, m, lp, false)?;
        let els = self.switch_chain(sel, rest, default, m, lp)?;
        r.push(Node::If { cond: c, then, els, results: vec![] });
        Ok(r)
    }
}

/// Build GIR for entry point `entry` of `m`. Compute only, for now.
pub fn build(m: &spv::Module, entry: usize) -> Result<Function, BuildError> {
    let ep = m.entry_points.get(entry).ok_or_else(|| BuildError::Malformed("no such entry point".into()))?;
    if ep.stage != spv::ShaderStage::Compute {
        return unsup(format!("{:?} stage", ep.stage));
    }
    let f = &m.functions[ep.function_index];
    let (x, y, z) = f.local_size.unwrap_or((1, 1, 1));
    let mut b = Builder {
        f,
        out: Function {
            name: ep.name.clone(),
            local_size: [x, y, z],
            types: vec![],
            privates: vec![],
            workgroup_bytes: f.workgroup_size,
            buffers: vec![],
            push_bytes: m.push_constants_size,
            body: vec![],
        },
        succ: HashMap::new(),
        loops: BTreeMap::new(),
        vals: HashMap::new(),
        prelude: vec![],
        consts: HashMap::new(),
        pred_stores: HashMap::new(),
        phi_var: HashMap::new(),
        translated: HashSet::new(),
        dup_ok: HashSet::new(),
    };
    b.analyse()?;

    let zero = b.konst(0, Ty::I32);
    let mut bufs: Vec<(u32, u32)> = f.ssbo_bindings.values().copied().collect();
    bufs.sort();
    bufs.dedup();
    b.out.buffers = bufs;
    for (vid, (set, binding)) in &f.ssbo_bindings {
        b.vals.insert(spv::ValueId(*vid), Val::Ptr(Space::Buffer { set: *set, binding: *binding }, zero));
    }
    for (vid, off) in &f.workgroup_var_offset {
        let o = b.konst(*off, Ty::I32);
        b.vals.insert(*vid, Val::Ptr(Space::Workgroup, o));
    }
    // Demote phis.
    let mut ids: Vec<&B> = f.blocks.keys().collect();
    ids.sort();
    for id in ids {
        for inst in &f.blocks[id].insts {
            if let (spv::Op::Phi(arms), Some(res)) = (&inst.op, &inst.result) {
                let ty = map_ty(&res.ty)?;
                b.out.privates.push(ty.bytes());
                let var = b.out.privates.len() as u32 - 1;
                b.phi_var.insert(res.id, var);
                for arm in arms {
                    b.pred_stores.entry(arm.from).or_default().push((var, arm.value.id));
                }
            }
        }
    }
    let body = b.region(f.entry_block, None, None, false)?;
    let mut all = std::mem::take(&mut b.prelude);
    all.extend(body);
    b.out.body = all;
    Ok(b.out)
}
