//! Uniformity (divergence) analysis.
//!
//! A value is *uniform* when every invocation of a subgroup that computes it
//! gets the same result; otherwise it is *divergent*. Targets put uniform
//! values in scalar registers (SGPRs on AMD) and branch on uniform
//! conditions with scalar branches.
//!
//! Divergent sources: local/global invocation ids and the local index,
//! atomic results, loads from private memory, loads at a divergent offset,
//! and loads from a storage buffer the kernel also writes (a scalar load of
//! such a buffer may not see the kernel's own vector stores, so it must stay
//! a per-lane load). Propagation: an op is divergent if an operand is; an
//! `if`'s results are divergent if its condition or a yielded value is; a
//! loop's parameters and results are divergent if an incoming value is or if
//! any invocation can leave or skip an iteration under a divergent condition.
//! Conservative: unknown means divergent.

use crate::*;
use std::collections::HashSet;

/// Result of the analysis.
#[derive(Debug, Clone)]
pub struct Uniformity {
    /// `divergent[v]` for every value.
    pub divergent: Vec<bool>,
}

impl Uniformity {
    /// True if `v` is the same for every invocation in a subgroup.
    pub fn is_uniform(&self, v: ValueId) -> bool {
        !self.divergent[v.0 as usize]
    }
}

fn written_buffers(r: &Region, out: &mut HashSet<Space>) {
    for n in r {
        match n {
            Node::Inst(i) => match &i.op {
                Op::Store(s @ Space::Buffer { .. }, ..) | Op::Atomic(_, s @ Space::Buffer { .. }, ..) => {
                    out.insert(*s);
                }
                _ => {}
            },
            Node::If { then, els, .. } => {
                written_buffers(then, out);
                written_buffers(els, out);
            }
            Node::Loop { body, .. } => written_buffers(body, out),
        }
    }
}

struct U<'a> {
    d: &'a mut Vec<bool>,
    written: HashSet<Space>,
    changed: bool,
}

impl<'a> U<'a> {
    fn mark(&mut self, v: ValueId) {
        if !self.d[v.0 as usize] {
            self.d[v.0 as usize] = true;
            self.changed = true;
        }
    }
    fn div(&self, v: ValueId) -> bool {
        self.d[v.0 as usize]
    }

    /// Walk `r`. `divergent_ctl` says some enclosing condition (inside the
    /// innermost loop) is divergent. Returns true if a `break`, `continue`
    /// or `return` happens under divergent control in this loop level.
    fn region(&mut self, r: &Region, divergent_ctl: bool, loop_vals: Option<(&[ValueId], &[ValueId])>) -> bool {
        let mut divergent_exit = false;
        for n in r {
            match n {
                Node::Inst(i) => {
                    let ops = i.op.operands();
                    let any = ops.iter().any(|v| self.div(*v));
                    match &i.op {
                        Op::Continue(v) | Op::Break(v) => {
                            if divergent_ctl {
                                divergent_exit = true;
                            }
                            if let Some((params, results)) = loop_vals {
                                let slots = if matches!(i.op, Op::Continue(_)) { params } else { results };
                                for (s, x) in slots.iter().zip(v) {
                                    if self.div(*x) {
                                        self.mark(*s);
                                    }
                                }
                            }
                        }
                        Op::Return => {
                            if divergent_ctl {
                                divergent_exit = true;
                            }
                        }
                        _ => {}
                    }
                    let Some(res) = i.result else { continue };
                    let source = match &i.op {
                        Op::Builtin(b) => !matches!(b, Builtin::WorkgroupId),
                        Op::Atomic(..) => true,
                        Op::Load(Space::Private(_), _) => true,
                        Op::Load(s @ Space::Buffer { .. }, _) => self.written.contains(s),
                        _ => false,
                    };
                    if source || any {
                        self.mark(res);
                    }
                }
                Node::If { cond, then, els, results } => {
                    let dc = divergent_ctl || self.div(*cond);
                    divergent_exit |= self.region(then, dc, loop_vals);
                    divergent_exit |= self.region(els, dc, loop_vals);
                    let yielded = |arm: &Region| match arm.last() {
                        Some(Node::Inst(Inst { op: Op::Yield(v), .. })) => v.clone(),
                        _ => vec![],
                    };
                    let (yt, ye) = (yielded(then), yielded(els));
                    for (k, r) in results.iter().enumerate() {
                        let d = self.div(*cond) || yt.get(k).map_or(false, |v| self.div(*v)) || ye.get(k).map_or(false, |v| self.div(*v));
                        if d {
                            self.mark(*r);
                        }
                    }
                }
                Node::Loop { inits, params, body, results } => {
                    for (p, i) in params.iter().zip(inits) {
                        if self.div(*i) {
                            self.mark(*p);
                        }
                    }
                    // Inside the body, control starts uniform relative to
                    // the loop; the enclosing state still counts for exits
                    // of outer loops but not for this loop's own exits.
                    let inner_div_exit = self.region(body, false, Some((params, results)));
                    if inner_div_exit {
                        // Invocations iterate different numbers of times.
                        for v in params.iter().chain(results) {
                            self.mark(*v);
                        }
                    }
                    if divergent_ctl && contains_return(body) {
                        divergent_exit = true;
                    }
                }
            }
        }
        divergent_exit
    }
}

fn contains_return(r: &Region) -> bool {
    r.iter().any(|n| match n {
        Node::Inst(i) => matches!(i.op, Op::Return),
        Node::If { then, els, .. } => contains_return(then) || contains_return(els),
        Node::Loop { body, .. } => contains_return(body),
    })
}

/// Compute uniformity for `f`.
pub fn analyse(f: &Function) -> Uniformity {
    let mut d = vec![false; f.types.len()];
    let mut written = HashSet::new();
    written_buffers(&f.body, &mut written);
    loop {
        let mut u = U { d: &mut d, written: written.clone(), changed: false };
        u.region(&f.body, false, None);
        if !u.changed {
            break;
        }
    }
    Uniformity { divergent: d }
}
