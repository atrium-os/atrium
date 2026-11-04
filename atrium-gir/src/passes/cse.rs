//! Common subexpression elimination over the region tree.
//!
//! A scoped table of pure ops: an op is replaced by an identical one computed
//! earlier in the same or an enclosing region (which dominates it). Loads
//! from the read-only push-constant block count as pure.

use crate::*;
use std::collections::HashMap;

struct C {
    scopes: Vec<HashMap<(Op, Ty), ValueId>>,
    subst: HashMap<ValueId, ValueId>,
    changed: bool,
}

impl C {
    fn sub(&self, mut v: ValueId) -> ValueId {
        while let Some(n) = self.subst.get(&v) {
            v = *n;
        }
        v
    }

    fn lookup(&self, k: &(Op, Ty)) -> Option<ValueId> {
        self.scopes.iter().rev().find_map(|s| s.get(k).copied())
    }

    fn region(&mut self, f: &Function, r: Region) -> Region {
        self.scopes.push(HashMap::new());
        let mut out = Region::new();
        for node in r {
            match node {
                Node::Inst(mut inst) => {
                    for o in inst.op.operands_mut() {
                        *o = self.sub(*o);
                    }
                    let cseable = inst.op.is_pure() || matches!(inst.op, Op::Load(Space::Push, _));
                    if let (Some(res), true) = (inst.result, cseable) {
                        let key = (inst.op.clone(), f.ty(res));
                        if let Some(prev) = self.lookup(&key) {
                            self.subst.insert(res, prev);
                            self.changed = true;
                            continue;
                        }
                        self.scopes.last_mut().unwrap().insert(key, res);
                    }
                    out.push(Node::Inst(inst));
                }
                Node::If { cond, then, els, results } => {
                    let cond = self.sub(cond);
                    let then = self.region(f, then);
                    let els = self.region(f, els);
                    out.push(Node::If { cond, then, els, results });
                }
                Node::Loop { inits, params, body, results } => {
                    let inits = inits.into_iter().map(|v| self.sub(v)).collect();
                    let body = self.region(f, body);
                    out.push(Node::Loop { inits, params, body, results });
                }
            }
        }
        self.scopes.pop();
        out
    }
}

/// Run CSE once. Returns true if anything changed.
pub fn cse(f: &mut Function) -> bool {
    let body = std::mem::take(&mut f.body);
    let mut c = C { scopes: vec![], subst: HashMap::new(), changed: false };
    f.body = c.region(f, body);
    c.changed
}
