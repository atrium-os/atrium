//! Promote private variables to SSA values.
//!
//! A variable is promoted when every access is a whole-variable load or
//! store at constant offset 0 with one type, and no atomic touches it. On
//! the region tree this is a single walk: the current value of each variable
//! flows forward; an `if` gets a result for every variable an arm changes;
//! a loop gets a parameter and a result for every variable its body stores,
//! fed by `continue` and `break`.

use crate::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

type Env = BTreeMap<u32, ValueId>;

struct M<'a> {
    f: &'a mut Function,
    vars: BTreeMap<u32, Ty>,
    subst: HashMap<ValueId, ValueId>,
    /// Promoted variables carried by each enclosing loop, innermost last.
    loops: Vec<Vec<u32>>,
    undef: HashMap<Ty, ValueId>,
    prelude: Region,
}

fn consts(r: &Region, out: &mut HashMap<ValueId, u32>) {
    for n in r {
        match n {
            Node::Inst(Inst { result: Some(v), op: Op::Const(b) }) => {
                out.insert(*v, *b);
            }
            Node::Inst(_) => {}
            Node::If { then, els, .. } => {
                consts(then, out);
                consts(els, out);
            }
            Node::Loop { body, .. } => consts(body, out),
        }
    }
}

/// Find promotable variables: (index → type).
fn candidates(f: &Function) -> BTreeMap<u32, Ty> {
    let mut k = HashMap::new();
    consts(&f.body, &mut k);
    let mut ok: BTreeMap<u32, Option<Ty>> = (0..f.privates.len() as u32).map(|i| (i, None)).collect();
    let mut bad = BTreeSet::new();
    fn walk(r: &Region, f: &Function, k: &HashMap<ValueId, u32>, ok: &mut BTreeMap<u32, Option<Ty>>, bad: &mut BTreeSet<u32>) {
        for n in r {
            match n {
                Node::Inst(i) => {
                    let (var, off, ty) = match &i.op {
                        Op::Load(Space::Private(v), o) => (*v, *o, f.ty(i.result.unwrap())),
                        Op::Store(Space::Private(v), o, x) => (*v, *o, f.ty(*x)),
                        Op::Atomic(_, Space::Private(v), _, _) => {
                            bad.insert(*v);
                            continue;
                        }
                        _ => continue,
                    };
                    let size_ok = ty.bytes() == f.privates[var as usize];
                    if k.get(&off) != Some(&0) || !size_ok {
                        bad.insert(var);
                    }
                    match ok.get_mut(&var).unwrap() {
                        Some(t) if *t != ty => {
                            bad.insert(var);
                        }
                        slot => *slot = Some(ty),
                    }
                }
                Node::If { then, els, .. } => {
                    walk(then, f, k, ok, bad);
                    walk(els, f, k, ok, bad);
                }
                Node::Loop { body, .. } => walk(body, f, k, ok, bad),
            }
        }
    }
    walk(&f.body, f, &k, &mut ok, &mut bad);
    ok.into_iter().filter(|(v, t)| t.is_some() && !bad.contains(v)).map(|(v, t)| (v, t.unwrap())).collect()
}

/// Promoted variables stored anywhere in `r`.
fn stored(r: &Region, vars: &BTreeMap<u32, Ty>, out: &mut BTreeSet<u32>) {
    for n in r {
        match n {
            Node::Inst(Inst { op: Op::Store(Space::Private(v), ..), .. }) if vars.contains_key(v) => {
                out.insert(*v);
            }
            Node::Inst(_) => {}
            Node::If { then, els, .. } => {
                stored(then, vars, out);
                stored(els, vars, out);
            }
            Node::Loop { body, .. } => stored(body, vars, out),
        }
    }
}

impl<'a> M<'a> {
    fn sub(&self, mut v: ValueId) -> ValueId {
        while let Some(n) = self.subst.get(&v) {
            v = *n;
        }
        v
    }

    fn undef(&mut self, ty: Ty) -> ValueId {
        if let Some(v) = self.undef.get(&ty) {
            return *v;
        }
        let zero = |m: &mut Self, s: Scalar| {
            let v = m.f.new_value(Ty::S(s));
            m.prelude.push(Node::Inst(Inst { result: Some(v), op: Op::Const(0) }));
            v
        };
        let v = match ty {
            Ty::S(s) => zero(self, s),
            Ty::V(s, n) => {
                let z = zero(self, s);
                let v = self.f.new_value(ty);
                self.prelude.push(Node::Inst(Inst { result: Some(v), op: Op::Construct(vec![z; n as usize]) }));
                v
            }
        };
        self.undef.insert(ty, v);
        v
    }

    fn value(&mut self, env: &Env, var: u32) -> ValueId {
        match env.get(&var) {
            Some(v) => *v,
            None => {
                let t = self.vars[&var];
                self.undef(t)
            }
        }
    }

    fn region(&mut self, r: Region, env: &mut Env) -> Region {
        let mut out = Region::new();
        for node in r {
            match node {
                Node::Inst(mut inst) => {
                    for o in inst.op.operands_mut() {
                        *o = self.sub(*o);
                    }
                    match &mut inst.op {
                        Op::Load(Space::Private(v), _) if self.vars.contains_key(v) => {
                            let x = self.value(env, *v);
                            self.subst.insert(inst.result.unwrap(), x);
                            continue;
                        }
                        Op::Store(Space::Private(v), _, x) if self.vars.contains_key(v) => {
                            env.insert(*v, *x);
                            continue;
                        }
                        Op::Continue(vals) | Op::Break(vals) => {
                            let carried = self.loops.last().cloned().unwrap_or_default();
                            for var in carried {
                                let x = self.value(env, var);
                                vals.push(x);
                            }
                        }
                        _ => {}
                    }
                    out.push(Node::Inst(inst));
                }
                Node::If { cond, then, els, mut results } => {
                    let cond = self.sub(cond);
                    let mut et = env.clone();
                    let mut ee = env.clone();
                    let mut then = self.region(then, &mut et);
                    let mut els = self.region(els, &mut ee);
                    let ft = !never_falls(&then);
                    let fe = !never_falls(&els);
                    let mut changed = BTreeSet::new();
                    for (arm_env, flows) in [(&et, ft), (&ee, fe)] {
                        if flows {
                            for (v, x) in arm_env {
                                if env.get(v) != Some(x) {
                                    changed.insert(*v);
                                }
                            }
                        }
                    }
                    if !changed.is_empty() {
                        for (arm, arm_env, flows) in [(&mut then, &et, ft), (&mut els, &ee, fe)] {
                            if !flows {
                                continue;
                            }
                            let has_yield = matches!(arm.last(), Some(Node::Inst(Inst { op: Op::Yield(_), .. })));
                            if !has_yield {
                                arm.push(Node::Inst(Inst { result: None, op: Op::Yield(vec![]) }));
                            }
                            let vals: Vec<ValueId> = changed.iter().map(|v| self.value(arm_env, *v)).collect();
                            if let Some(Node::Inst(Inst { op: Op::Yield(y), .. })) = arm.last_mut() {
                                y.extend(vals);
                            }
                        }
                        for v in &changed {
                            let r = self.f.new_value(self.vars[v]);
                            results.push(r);
                            env.insert(*v, r);
                        }
                    }
                    out.push(Node::If { cond, then, els, results });
                }
                Node::Loop { mut inits, mut params, body, mut results } => {
                    for i in inits.iter_mut() {
                        *i = self.sub(*i);
                    }
                    let mut carried = BTreeSet::new();
                    stored(&body, &self.vars, &mut carried);
                    let carried: Vec<u32> = carried.into_iter().collect();
                    let mut benv = env.clone();
                    for v in &carried {
                        let init = self.value(env, *v);
                        inits.push(init);
                        let p = self.f.new_value(self.vars[v]);
                        params.push(p);
                        benv.insert(*v, p);
                    }
                    self.loops.push(carried.clone());
                    let mut body = self.region(body, &mut benv);
                    if !carried.is_empty() && !never_falls(&body) {
                        // Falling off the end is `continue`; make it explicit.
                        let vals: Vec<ValueId> = carried.iter().map(|v| self.value(&benv, *v)).collect();
                        body.push(Node::Inst(Inst { result: None, op: Op::Continue(vals) }));
                    }
                    self.loops.pop();
                    for v in &carried {
                        let r = self.f.new_value(self.vars[v]);
                        results.push(r);
                        env.insert(*v, r);
                    }
                    out.push(Node::Loop { inits, params, body, results });
                }
            }
        }
        out
    }
}

/// Renumber `Private(i)` after removing the promoted variables.
fn renumber(r: &mut Region, map: &HashMap<u32, u32>) {
    for n in r {
        match n {
            Node::Inst(i) => match &mut i.op {
                Op::Load(Space::Private(v), _) | Op::Store(Space::Private(v), ..) | Op::Atomic(_, Space::Private(v), ..) => {
                    *v = map[v];
                }
                _ => {}
            },
            Node::If { then, els, .. } => {
                renumber(then, map);
                renumber(els, map);
            }
            Node::Loop { body, .. } => renumber(body, map),
        }
    }
}

/// Run mem2reg. Returns the number of variables promoted.
pub fn mem2reg(f: &mut Function) -> usize {
    let vars = candidates(f);
    if vars.is_empty() {
        return 0;
    }
    let body = std::mem::take(&mut f.body);
    let mut m = M { f, vars: vars.clone(), subst: HashMap::new(), loops: vec![], undef: HashMap::new(), prelude: vec![] };
    let mut env = Env::new();
    let body = m.region(body, &mut env);
    let mut all = std::mem::take(&mut m.prelude);
    all.extend(body);
    f.body = all;
    let mut map = HashMap::new();
    let mut kept = vec![];
    for (i, b) in f.privates.iter().enumerate() {
        if !vars.contains_key(&(i as u32)) {
            map.insert(i as u32, kept.len() as u32);
            kept.push(*b);
        }
    }
    f.privates = kept;
    renumber(&mut f.body, &map);
    vars.len()
}
