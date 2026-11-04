//! Structural and type checks for GIR. Run after building and after every
//! pass in tests.

use crate::*;
use std::collections::HashSet;

/// A verifier failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError(pub String);

#[derive(Clone, Copy)]
enum Ctx<'a> {
    /// Inside an `if` arm with these results.
    Arm(&'a [ValueId]),
    /// Directly in a loop body or the function body.
    Plain,
}

struct V<'a> {
    f: &'a Function,
    defined: HashSet<ValueId>,
    /// Visible values, as a stack of scopes.
    scopes: Vec<Vec<ValueId>>,
    /// Innermost loop's (params, results), if any.
    loops: Vec<(&'a [ValueId], &'a [ValueId])>,
}

fn err<T>(s: String) -> Result<T, VerifyError> {
    Err(VerifyError(s))
}

impl<'a> V<'a> {
    fn visible(&self, v: ValueId) -> bool {
        self.scopes.iter().any(|s| s.contains(&v))
    }

    fn define(&mut self, v: ValueId) -> Result<(), VerifyError> {
        if v.0 as usize >= self.f.types.len() {
            return err(format!("{v} has no type"));
        }
        if !self.defined.insert(v) {
            return err(format!("{v} defined twice"));
        }
        self.scopes.last_mut().unwrap().push(v);
        Ok(())
    }

    fn use_(&self, v: ValueId) -> Result<(), VerifyError> {
        if !self.visible(v) {
            return err(format!("{v} used where it is not defined"));
        }
        Ok(())
    }

    fn same_types(&self, what: &str, a: &[ValueId], b: &[ValueId]) -> Result<(), VerifyError> {
        if a.len() != b.len() {
            return err(format!("{what}: {} values for {} slots", a.len(), b.len()));
        }
        for (x, y) in a.iter().zip(b) {
            if self.f.ty(*x) != self.f.ty(*y) {
                return err(format!("{what}: {x} is {} but slot {y} is {}", self.f.ty(*x), self.f.ty(*y)));
            }
        }
        Ok(())
    }

    fn region(&mut self, r: &'a Region, ctx: Ctx<'a>) -> Result<(), VerifyError> {
        self.scopes.push(vec![]);
        for (i, n) in r.iter().enumerate() {
            let last = i + 1 == r.len();
            match n {
                Node::Inst(inst) => {
                    for o in inst.op.operands() {
                        self.use_(o)?;
                    }
                    if inst.op.is_terminator() && !last {
                        return err(format!("{:?} is not last in its region", inst.op));
                    }
                    match &inst.op {
                        Op::Yield(v) => match ctx {
                            Ctx::Arm(res) => self.same_types("yield", v, res)?,
                            Ctx::Plain => return err("yield outside an if arm".into()),
                        },
                        Op::Continue(v) => match self.loops.last() {
                            Some((params, _)) => self.same_types("continue", v, params)?,
                            None => return err("continue outside a loop".into()),
                        },
                        Op::Break(v) => match self.loops.last() {
                            Some((_, res)) => self.same_types("break", v, res)?,
                            None => return err("break outside a loop".into()),
                        },
                        _ => {}
                    }
                    self.check_types(inst)?;
                    if let Some(v) = inst.result {
                        self.define(v)?;
                    }
                }
                Node::If { cond, then, els, results } => {
                    self.use_(*cond)?;
                    if self.f.ty(*cond) != Ty::BOOL {
                        return err(format!("if condition {cond} is not bool"));
                    }
                    for arm in [then, els] {
                        if !results.is_empty() && !ends_with_terminator(arm) {
                            return err("if arm with results must end in a terminator".into());
                        }
                        self.region(arm, Ctx::Arm(results))?;
                    }
                    for v in results {
                        self.define(*v)?;
                    }
                }
                Node::Loop { inits, params, body, results } => {
                    for v in inits {
                        self.use_(*v)?;
                    }
                    self.same_types("loop inits", inits, params)?;
                    if !params.is_empty() && !ends_with_terminator(body) && !never_falls(body) {
                        return err("loop body with parameters must end in a terminator".into());
                    }
                    self.scopes.push(vec![]);
                    for v in params {
                        self.define(*v)?;
                    }
                    self.loops.push((params, results));
                    self.region(body, Ctx::Plain)?;
                    self.loops.pop();
                    self.scopes.pop();
                    for v in results {
                        self.define(*v)?;
                    }
                }
            }
        }
        self.scopes.pop();
        Ok(())
    }

    fn check_types(&self, inst: &Inst) -> Result<(), VerifyError> {
        let ty = |v: ValueId| self.f.ty(v);
        let res = inst.result.map(|v| ty(v));
        let bad = || err(format!("type error in {:?} -> {:?}", inst.op, res));
        let needs_result = !matches!(inst.op, Op::Store(..) | Op::Barrier | Op::Yield(_) | Op::Continue(_) | Op::Break(_) | Op::Return);
        if needs_result != res.is_some() {
            return bad();
        }
        let ok = match &inst.op {
            Op::Const(_) | Op::Builtin(_) => matches!(res, Some(Ty::S(_))) || matches!(inst.op, Op::Builtin(_)),
            Op::Un(o, a) => {
                let t = ty(*a);
                Some(t) == res
                    && match o {
                        UnOp::Not => t == Ty::I32 || t == Ty::BOOL,
                        UnOp::INeg | UnOp::Clz | UnOp::BitReverse => t == Ty::I32,
                        _ => t == Ty::F32,
                    }
            }
            Op::Bin(o, a, b) => {
                let t = ty(*a);
                t == ty(*b)
                    && Some(t) == res
                    && match o {
                        BinOp::And | BinOp::Or | BinOp::Xor => t == Ty::I32 || t == Ty::BOOL,
                        BinOp::FAdd | BinOp::FSub | BinOp::FMul | BinOp::FDiv | BinOp::FMin | BinOp::FMax => t == Ty::F32,
                        _ => t == Ty::I32,
                    }
            }
            Op::Fma(a, b, c) => [*a, *b, *c].iter().all(|v| ty(*v) == Ty::F32) && res == Some(Ty::F32),
            Op::Cmp(o, a, b) => {
                let t = ty(*a);
                t == ty(*b)
                    && res == Some(Ty::BOOL)
                    && match o {
                        CmpOp::IEq | CmpOp::INe => t == Ty::I32 || t == Ty::BOOL,
                        CmpOp::ULt | CmpOp::ULe | CmpOp::SLt | CmpOp::SLe => t == Ty::I32,
                        _ => t == Ty::F32,
                    }
            }
            Op::Select(c, a, b) => ty(*c) == Ty::BOOL && ty(*a) == ty(*b) && res == Some(ty(*a)),
            Op::Conv(o, a) => match o {
                ConvOp::SToF | ConvOp::UToF => ty(*a) == Ty::I32 && res == Some(Ty::F32),
                ConvOp::FToS | ConvOp::FToU => ty(*a) == Ty::F32 && res == Some(Ty::I32),
                ConvOp::BoolToI => ty(*a) == Ty::BOOL && res == Some(Ty::I32),
            },
            Op::Bitcast(a) => res.map(|r| r.comps()) == Some(ty(*a).comps()),
            Op::Construct(v) => match res {
                Some(Ty::V(s, n)) => v.len() == n as usize && v.iter().all(|x| ty(*x) == Ty::S(s)),
                _ => false,
            },
            Op::Extract(a, i) => match ty(*a) {
                Ty::V(s, n) => *i < n && res == Some(Ty::S(s)),
                _ => false,
            },
            Op::Load(_, o) => ty(*o) == Ty::I32,
            Op::Store(_, o, _) => ty(*o) == Ty::I32,
            Op::Atomic(_, _, o, v) => ty(*o) == Ty::I32 && ty(*v) == Ty::I32 && res == Some(Ty::I32),
            Op::Barrier | Op::Yield(_) | Op::Continue(_) | Op::Break(_) | Op::Return => true,
            Op::Subgroup(o, v) => {
                let data = |t: Ty| t == Ty::I32 || t == Ty::F32;
                match (o, v.as_slice()) {
                    (SubgroupOp::Elect, []) => res == Some(Ty::BOOL),
                    (SubgroupOp::All | SubgroupOp::Any, [p]) => ty(*p) == Ty::BOOL && res == Some(Ty::BOOL),
                    (SubgroupOp::AllEqual, [x]) => data(ty(*x)) && res == Some(Ty::BOOL),
                    (SubgroupOp::Ballot, [p]) => ty(*p) == Ty::BOOL && res == Some(Ty::I32),
                    (SubgroupOp::BroadcastFirst, [x]) => data(ty(*x)) && res == Some(ty(*x)),
                    (SubgroupOp::Broadcast | SubgroupOp::Shuffle | SubgroupOp::ShuffleXor | SubgroupOp::ShuffleUp | SubgroupOp::ShuffleDown, [x, i]) => {
                        data(ty(*x)) && ty(*i) == Ty::I32 && res == Some(ty(*x))
                    }
                    (SubgroupOp::Reduce(a) | SubgroupOp::InclusiveScan(a) | SubgroupOp::ExclusiveScan(a), [x]) => {
                        ty(*x) == if a.is_float() { Ty::F32 } else { Ty::I32 } && res == Some(ty(*x))
                    }
                    _ => false,
                }
            }
        };
        if ok { Ok(()) } else { bad() }
    }
}

fn ends_with_terminator(r: &Region) -> bool {
    match r.last() {
        Some(Node::Inst(i)) => i.op.is_terminator(),
        _ => false,
    }
}

/// Check `f`: single definition, uses only where defined (structured
/// dominance), terminators in place, and op types.
pub fn verify(f: &Function) -> Result<(), VerifyError> {
    let mut v = V { f, defined: HashSet::new(), scopes: vec![], loops: vec![] };
    v.region(&f.body, Ctx::Plain)
}
