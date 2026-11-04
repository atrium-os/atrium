//! Reference interpreter for GIR.
//!
//! Runs a whole workgroup in lockstep: every instruction executes for all
//! active invocations before the next one starts, `if` arms run one after
//! the other under narrowed masks, and loops iterate until every invocation
//! has left. This is how a GPU runs the region tree, so the interpreter also
//! gives subgroup ops and barriers their real meaning once they are added.
//!
//! Memory accesses are bounds-checked: an out-of-range access is an error,
//! not undefined behaviour. Atomics and conflicting stores apply in
//! invocation order.

use crate::*;
use std::collections::HashMap;

/// An interpreter failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpError(pub String);

fn err<T>(s: String) -> Result<T, InterpError> {
    Err(InterpError(s))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Running,
    Returned,
    Broke,
    Continued,
}

type Lane = [u32; 4];

/// Storage buffers by (set, binding); the push-constant block under [`PUSH`].
pub type Buffers = HashMap<(u32, u32), Vec<u8>>;

/// Key of the push-constant block in [`Buffers`].
pub const PUSH: (u32, u32) = (u32::MAX, u32::MAX);

struct Wg<'a> {
    f: &'a Function,
    n: usize,
    wg_id: [u32; 3],
    vals: Vec<Vec<Lane>>,
    status: Vec<Status>,
    private: Vec<Vec<Vec<u8>>>,
    shared: Vec<u8>,
    buffers: &'a mut Buffers,
    /// Staged continue values for the innermost loop, per lane.
    cont: Vec<Vec<Option<Vec<Lane>>>>,
    /// Targets of `yield`, innermost last.
    yields: Vec<&'a [ValueId]>,
    /// (params, results) of loops, innermost last.
    loops: Vec<(&'a [ValueId], &'a [ValueId])>,
}

fn f(b: u32) -> f32 {
    f32::from_bits(b)
}
fn b(x: f32) -> u32 {
    x.to_bits()
}

impl<'a> Wg<'a> {
    fn get(&self, v: ValueId, l: usize) -> Lane {
        self.vals[v.0 as usize][l]
    }
    fn s(&self, v: ValueId, l: usize) -> u32 {
        self.vals[v.0 as usize][l][0]
    }
    fn set(&mut self, v: ValueId, l: usize, x: Lane) {
        self.vals[v.0 as usize][l] = x;
    }

    fn local_id(&self, l: usize) -> [u32; 3] {
        let [x, y, _] = self.f.local_size;
        let l = l as u32;
        [l % x, (l / x) % y, l / (x * y)]
    }

    fn mem(&mut self, s: Space, l: usize) -> Result<&mut Vec<u8>, InterpError> {
        Ok(match s {
            Space::Buffer { set, binding } => match self.buffers.get_mut(&(set, binding)) {
                Some(b) => b,
                None => return err(format!("buffer ({set}, {binding}) not bound")),
            },
            Space::Push => match self.buffers.get_mut(&PUSH) {
                Some(b) => b,
                None => return err("push constants not bound".into()),
            },
            Space::Private(i) => &mut self.private[l][i as usize],
            Space::Workgroup => &mut self.shared,
        })
    }

    fn load(&mut self, s: Space, off: u32, comps: usize, l: usize) -> Result<Lane, InterpError> {
        let m = self.mem(s, l)?;
        let end = off as usize + 4 * comps;
        if off % 4 != 0 || end > m.len() {
            return err(format!("load of {} bytes at {off} out of range in {s:?} (size {})", 4 * comps, m.len()));
        }
        let mut x = [0u32; 4];
        for (i, c) in x.iter_mut().enumerate().take(comps) {
            let o = off as usize + 4 * i;
            *c = u32::from_le_bytes(m[o..o + 4].try_into().unwrap());
        }
        Ok(x)
    }

    fn store(&mut self, s: Space, off: u32, comps: usize, x: Lane, l: usize) -> Result<(), InterpError> {
        let m = self.mem(s, l)?;
        let end = off as usize + 4 * comps;
        if off % 4 != 0 || end > m.len() {
            return err(format!("store of {} bytes at {off} out of range in {s:?} (size {})", 4 * comps, m.len()));
        }
        for (i, c) in x.iter().enumerate().take(comps) {
            let o = off as usize + 4 * i;
            m[o..o + 4].copy_from_slice(&c.to_le_bytes());
        }
        Ok(())
    }

    fn eff(&self, mask: &[bool]) -> Vec<bool> {
        mask.iter().zip(&self.status).map(|(m, s)| *m && *s == Status::Running).collect()
    }

    fn inst(&mut self, inst: &'a Inst, mask: &[bool]) -> Result<(), InterpError> {
        let lanes: Vec<usize> = (0..self.n).filter(|&l| mask[l]).collect();
        let fty = self.f;
        for &l in &lanes {
            let s1 = |w: &Self, v: ValueId| w.s(v, l);
            let x: Option<Lane> = match &inst.op {
                Op::Const(c) => Some([*c, 0, 0, 0]),
                Op::Un(o, a) => {
                    let a = s1(self, *a);
                    let is_bool = fty.ty(inst.result.unwrap()) == Ty::BOOL;
                    let r = match o {
                        UnOp::INeg => a.wrapping_neg(),
                        UnOp::Not => if is_bool { (a == 0) as u32 } else { !a },
                        UnOp::FNeg => a ^ 0x8000_0000,
                        UnOp::FAbs => a & 0x7fff_ffff,
                        UnOp::FSqrt => b(f(a).sqrt()),
                        UnOp::FFloor => b(f(a).floor()),
                        UnOp::FCeil => b(f(a).ceil()),
                        UnOp::FTrunc => b(f(a).trunc()),
                        UnOp::Clz => a.leading_zeros(),
                        UnOp::BitReverse => a.reverse_bits(),
                    };
                    Some([r, 0, 0, 0])
                }
                Op::Bin(o, a, c) => {
                    let (a, c) = (s1(self, *a), s1(self, *c));
                    let r = match o {
                        BinOp::IAdd => a.wrapping_add(c),
                        BinOp::ISub => a.wrapping_sub(c),
                        BinOp::IMul => a.wrapping_mul(c),
                        BinOp::UDiv => if c == 0 { u32::MAX } else { a / c },
                        BinOp::SDiv => if c == 0 { u32::MAX } else { (a as i32).wrapping_div(c as i32) as u32 },
                        BinOp::UMod => if c == 0 { u32::MAX } else { a % c },
                        BinOp::SRem => if c == 0 { u32::MAX } else { (a as i32).wrapping_rem(c as i32) as u32 },
                        BinOp::And => a & c,
                        BinOp::Or => a | c,
                        BinOp::Xor => a ^ c,
                        BinOp::Shl => a << (c & 31),
                        BinOp::LShr => a >> (c & 31),
                        BinOp::AShr => ((a as i32) >> (c & 31)) as u32,
                        BinOp::FAdd => b(f(a) + f(c)),
                        BinOp::FSub => b(f(a) - f(c)),
                        BinOp::FMul => b(f(a) * f(c)),
                        BinOp::FDiv => b(f(a) / f(c)),
                        BinOp::FMin => b(f(a).min(f(c))),
                        BinOp::FMax => b(f(a).max(f(c))),
                    };
                    Some([r, 0, 0, 0])
                }
                Op::Fma(a, c, d) => Some([b(f(s1(self, *a)).mul_add(f(s1(self, *c)), f(s1(self, *d)))), 0, 0, 0]),
                Op::Cmp(o, a, c) => {
                    let (a, c) = (s1(self, *a), s1(self, *c));
                    let (x, y) = (f(a), f(c));
                    let un = x.is_nan() || y.is_nan();
                    let r = match o {
                        CmpOp::IEq => a == c,
                        CmpOp::INe => a != c,
                        CmpOp::ULt => a < c,
                        CmpOp::ULe => a <= c,
                        CmpOp::SLt => (a as i32) < (c as i32),
                        CmpOp::SLe => (a as i32) <= (c as i32),
                        CmpOp::FOEq => x == y,
                        CmpOp::FONe => !un && x != y,
                        CmpOp::FOLt => x < y,
                        CmpOp::FOLe => x <= y,
                        CmpOp::FUEq => un || x == y,
                        CmpOp::FUNe => x != y,
                        CmpOp::FULt => un || x < y,
                        CmpOp::FULe => un || x <= y,
                    };
                    Some([r as u32, 0, 0, 0])
                }
                Op::Select(c, t, e) => Some(if s1(self, *c) != 0 { self.get(*t, l) } else { self.get(*e, l) }),
                Op::Conv(o, a) => {
                    let a = s1(self, *a);
                    let r = match o {
                        ConvOp::SToF => b(a as i32 as f32),
                        ConvOp::UToF => b(a as f32),
                        ConvOp::FToS => f(a) as i32 as u32,
                        ConvOp::FToU => f(a) as u32,
                        ConvOp::BoolToI => (a != 0) as u32,
                    };
                    Some([r, 0, 0, 0])
                }
                Op::Bitcast(a) => Some(self.get(*a, l)),
                Op::Construct(v) => {
                    let mut x = [0; 4];
                    for (i, c) in v.iter().enumerate() {
                        x[i] = s1(self, *c);
                    }
                    Some(x)
                }
                Op::Extract(a, i) => Some([self.get(*a, l)[*i as usize], 0, 0, 0]),
                Op::Builtin(k) => {
                    let lid = self.local_id(l);
                    let ls = self.f.local_size;
                    Some(match k {
                        Builtin::LocalId => [lid[0], lid[1], lid[2], 0],
                        Builtin::WorkgroupId => [self.wg_id[0], self.wg_id[1], self.wg_id[2], 0],
                        Builtin::GlobalId => {
                            let g = |i: usize| self.wg_id[i] * ls[i] + lid[i];
                            [g(0), g(1), g(2), 0]
                        }
                        Builtin::LocalIndex => [l as u32, 0, 0, 0],
                    })
                }
                Op::Load(s, o) => {
                    let comps = fty.ty(inst.result.unwrap()).comps();
                    let off = s1(self, *o);
                    Some(self.load(*s, off, comps, l)?)
                }
                Op::Store(s, o, v) => {
                    let comps = fty.ty(*v).comps();
                    let (off, x) = (s1(self, *o), self.get(*v, l));
                    self.store(*s, off, comps, x, l)?;
                    None
                }
                Op::Atomic(o, s, off, v) => {
                    let (off, v) = (s1(self, *off), s1(self, *v));
                    let old = self.load(*s, off, 1, l)?[0];
                    let new = match o {
                        AtomicOp::Add => old.wrapping_add(v),
                        AtomicOp::And => old & v,
                        AtomicOp::Or => old | v,
                        AtomicOp::Xor => old ^ v,
                        AtomicOp::SMin => (old as i32).min(v as i32) as u32,
                        AtomicOp::SMax => (old as i32).max(v as i32) as u32,
                        AtomicOp::UMin => old.min(v),
                        AtomicOp::UMax => old.max(v),
                        AtomicOp::Exchange => v,
                    };
                    self.store(*s, off, 1, [new, 0, 0, 0], l)?;
                    Some([old, 0, 0, 0])
                }
                Op::Barrier => None,
                Op::Yield(v) => {
                    let targets = *self.yields.last().unwrap();
                    for (t, s) in targets.iter().zip(v) {
                        let x = self.get(*s, l);
                        self.set(*t, l, x);
                    }
                    None
                }
                Op::Continue(v) => {
                    let vals: Vec<Lane> = v.iter().map(|s| self.get(*s, l)).collect();
                    self.cont.last_mut().unwrap()[l] = Some(vals);
                    self.status[l] = Status::Continued;
                    None
                }
                Op::Break(v) => {
                    let results = self.loops.last().unwrap().1;
                    for (t, s) in results.iter().zip(v) {
                        let x = self.get(*s, l);
                        self.set(*t, l, x);
                    }
                    self.status[l] = Status::Broke;
                    None
                }
                Op::Return => {
                    self.status[l] = Status::Returned;
                    None
                }
            };
            if let (Some(r), Some(x)) = (inst.result, x) {
                self.set(r, l, x);
            }
        }
        Ok(())
    }

    fn region(&mut self, r: &'a Region, mask: &[bool]) -> Result<(), InterpError> {
        for node in r {
            let eff = self.eff(mask);
            if !eff.iter().any(|x| *x) {
                return Ok(());
            }
            match node {
                Node::Inst(i) => self.inst(i, &eff)?,
                Node::If { cond, then, els, results } => {
                    let t: Vec<bool> = (0..self.n).map(|l| eff[l] && self.s(*cond, l) != 0).collect();
                    let e: Vec<bool> = (0..self.n).map(|l| eff[l] && self.s(*cond, l) == 0).collect();
                    self.yields.push(results);
                    self.region(then, &t)?;
                    self.region(els, &e)?;
                    self.yields.pop();
                }
                Node::Loop { inits, params, body, results } => {
                    for l in 0..self.n {
                        if eff[l] {
                            for (p, i) in params.iter().zip(inits) {
                                let x = self.get(*i, l);
                                self.set(*p, l, x);
                            }
                        }
                    }
                    let mut members = eff.clone();
                    self.loops.push((params, results));
                    self.cont.push(vec![None; self.n]);
                    let mut iters = 0u64;
                    while members.iter().any(|x| *x) {
                        iters += 1;
                        if iters > 1 << 24 {
                            return err("loop did not terminate after 2^24 iterations".into());
                        }
                        self.region(body, &members)?;
                        for l in 0..self.n {
                            if !members[l] {
                                continue;
                            }
                            match self.status[l] {
                                Status::Continued | Status::Running => {
                                    self.status[l] = Status::Running;
                                    if let Some(v) = self.cont.last_mut().unwrap()[l].take() {
                                        for (p, x) in params.iter().zip(v) {
                                            self.set(*p, l, x);
                                        }
                                    }
                                }
                                Status::Broke => {
                                    self.status[l] = Status::Running;
                                    members[l] = false;
                                }
                                Status::Returned => members[l] = false,
                            }
                        }
                    }
                    self.cont.pop();
                    self.loops.pop();
                }
            }
        }
        Ok(())
    }
}

/// Run `f` over `groups` workgroups against `buffers`.
pub fn run(f: &Function, groups: [u32; 3], buffers: &mut Buffers) -> Result<(), InterpError> {
    let n = (f.local_size[0] * f.local_size[1] * f.local_size[2]) as usize;
    for (set, binding) in &f.buffers {
        if !buffers.contains_key(&(*set, *binding)) {
            return err(format!("buffer ({set}, {binding}) not bound"));
        }
    }
    for z in 0..groups[2] {
        for y in 0..groups[1] {
            for x in 0..groups[0] {
                let mut wg = Wg {
                    f,
                    n,
                    wg_id: [x, y, z],
                    vals: vec![vec![[0; 4]; n]; f.types.len()],
                    status: vec![Status::Running; n],
                    private: vec![f.privates.iter().map(|b| vec![0u8; *b as usize]).collect(); n],
                    shared: vec![0u8; f.workgroup_bytes as usize],
                    buffers,
                    cont: vec![],
                    yields: vec![],
                    loops: vec![],
                };
                let mask = vec![true; n];
                wg.region(&f.body, &mask)?;
            }
        }
    }
    Ok(())
}
