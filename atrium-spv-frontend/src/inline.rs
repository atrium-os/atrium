//! Function-call inlining at the SPIR-V level.
//!
//! The rest of the frontend translates one function at a time and the GPU
//! IR builds only the entry point, so `OpFunctionCall` is removed first:
//! every call is replaced by a copy of the callee's body (callees inlined
//! into it first; recursion is rejected, as SPIR-V for shaders forbids it).
//!
//! At a call in block B, B is split: the instructions before the call
//! branch into the copied entry block; each `OpReturn`/`OpReturnValue` in
//! the copy branches to a new block R that holds the rest of B (its
//! terminator and merge marker included). A non-void call's result is an
//! `OpPhi` at the top of R over the returned values. Parameters are
//! replaced by the arguments; the callee's `OpVariable`s move to the top of
//! the caller's entry block. Functions no longer called and not entry
//! points are dropped. The caller re-assembles and re-parses the module so
//! source offsets stay consistent.

use crate::error::FrontendError;
use rspirv::dr::{Block, Function, Instruction, Module, Operand};
use rspirv::spirv::{Op, Word};
use std::collections::{HashMap, HashSet};

fn has_calls(f: &Function) -> bool {
    f.blocks.iter().any(|b| b.instructions.iter().any(|i| i.class.opcode == Op::FunctionCall))
}

fn fid(f: &Function) -> Option<Word> {
    f.def.as_ref().and_then(|d| d.result_id)
}

/// Rename an operand id through `map` (ids not in the map are global).
fn rename(op: &mut Operand, map: &HashMap<Word, Word>) {
    match op {
        Operand::IdRef(id) | Operand::IdScope(id) | Operand::IdMemorySemantics(id) => {
            if let Some(n) = map.get(id) {
                *id = *n;
            }
        }
        _ => {}
    }
}

struct Ids<'a> {
    bound: &'a mut u32,
}

impl Ids<'_> {
    fn fresh(&mut self) -> Word {
        let id = *self.bound;
        *self.bound += 1;
        id
    }
}

/// Inline every call in `caller` (callees must already be call-free).
fn inline_into(caller: &mut Function, callees: &HashMap<Word, Function>, ids: &mut Ids) -> Result<(), FrontendError> {
    let mut hoisted: Vec<Instruction> = vec![];
    loop {
        // First call site.
        let site = caller.blocks.iter().enumerate().find_map(|(bi, b)| {
            b.instructions.iter().position(|i| i.class.opcode == Op::FunctionCall).map(|ii| (bi, ii))
        });
        let Some((bi, ii)) = site else { break };
        let call = caller.blocks[bi].instructions[ii].clone();
        let callee_id = match call.operands.first() {
            Some(Operand::IdRef(id)) => *id,
            other => return Err(FrontendError::Malformed(format!("OpFunctionCall callee operand {other:?}"))),
        };
        let callee = callees.get(&callee_id).ok_or_else(|| FrontendError::Unsupported(format!("call to function {callee_id} (recursive or missing)")))?;
        let args: Vec<Word> = call.operands[1..].iter().map(|o| match o {
            Operand::IdRef(id) => Ok(*id),
            other => Err(FrontendError::Malformed(format!("OpFunctionCall argument {other:?}"))),
        }).collect::<Result<_, _>>()?;

        // Id map for the copy: params → args, every other local id fresh.
        let mut map: HashMap<Word, Word> = HashMap::new();
        if callee.parameters.len() != args.len() {
            return Err(FrontendError::Malformed("OpFunctionCall argument count mismatch".into()));
        }
        for (p, a) in callee.parameters.iter().zip(&args) {
            map.insert(p.result_id.unwrap(), *a);
        }
        for b in &callee.blocks {
            if let Some(l) = b.label.as_ref().and_then(|l| l.result_id) {
                map.insert(l, ids.fresh());
            }
            for i in &b.instructions {
                if let Some(r) = i.result_id {
                    map.insert(r, ids.fresh());
                }
            }
        }
        let ret_label = ids.fresh();

        // Split the caller's block.
        let block = &mut caller.blocks[bi];
        let rest: Vec<Instruction> = block.instructions.drain(ii..).skip(1).collect();
        let entry_label = map[&callee.blocks[0].label.as_ref().unwrap().result_id.unwrap()];
        block.instructions.push(Instruction::new(Op::Branch, None, None, vec![Operand::IdRef(entry_label)]));

        // Copy the callee's blocks.
        let mut returns: Vec<(Word, Word)> = vec![]; // (value, from block)
        let mut copies: Vec<Block> = vec![];
        for b in &callee.blocks {
            let label = map[&b.label.as_ref().unwrap().result_id.unwrap()];
            let mut nb = Block { label: Some(Instruction::new(Op::Label, None, Some(label), vec![])), instructions: vec![] };
            for i in &b.instructions {
                let mut c = i.clone();
                c.result_id = c.result_id.map(|r| map[&r]);
                for o in &mut c.operands {
                    rename(o, &map);
                }
                match c.class.opcode {
                    Op::Variable => hoisted.push(c),
                    Op::Return => nb.instructions.push(Instruction::new(Op::Branch, None, None, vec![Operand::IdRef(ret_label)])),
                    Op::ReturnValue => {
                        let v = match c.operands.first() {
                            Some(Operand::IdRef(v)) => *v,
                            other => return Err(FrontendError::Malformed(format!("OpReturnValue operand {other:?}"))),
                        };
                        returns.push((v, label));
                        nb.instructions.push(Instruction::new(Op::Branch, None, None, vec![Operand::IdRef(ret_label)]));
                    }
                    Op::Kill | Op::Unreachable | Op::TerminateInvocation => {
                        return Err(FrontendError::Unsupported(format!("{:?} in an inlined function", c.class.opcode)));
                    }
                    _ => nb.instructions.push(c),
                }
            }
            copies.push(nb);
        }

        // The return block: phi of the returned values, then the rest.
        let mut rb = Block { label: Some(Instruction::new(Op::Label, None, Some(ret_label), vec![])), instructions: vec![] };
        if let (Some(res), Some(ty)) = (call.result_id, call.result_type) {
            let void = callee.def.as_ref().and_then(|d| d.result_type) == Some(ty) && returns.is_empty();
            if !void {
                if returns.is_empty() {
                    return Err(FrontendError::Malformed("non-void function without OpReturnValue".into()));
                }
                let mut ops = vec![];
                for (v, from) in &returns {
                    ops.push(Operand::IdRef(*v));
                    ops.push(Operand::IdRef(*from));
                }
                rb.instructions.push(Instruction::new(Op::Phi, Some(ty), Some(res), ops));
            }
        }
        rb.instructions.extend(rest);
        // Phis in the old successors named B as a predecessor; B's
        // terminator now lives in R.
        let b_label = caller.blocks[bi].label.as_ref().unwrap().result_id.unwrap();
        let succ: HashSet<Word> = rb.instructions.last().map(|t| t.operands.iter().filter_map(|o| match o {
            Operand::IdRef(id) => Some(*id),
            _ => None,
        }).collect()).unwrap_or_default();
        for b in caller.blocks.iter_mut() {
            let l = b.label.as_ref().unwrap().result_id.unwrap();
            if !succ.contains(&l) {
                continue;
            }
            for i in b.instructions.iter_mut().filter(|i| i.class.opcode == Op::Phi) {
                for k in (1..i.operands.len()).step_by(2) {
                    if i.operands[k] == Operand::IdRef(b_label) {
                        i.operands[k] = Operand::IdRef(ret_label);
                    }
                }
            }
        }
        let at = bi + 1;
        caller.blocks.insert(at, rb);
        for (k, c) in copies.into_iter().enumerate() {
            caller.blocks.insert(at + k, c);
        }
    }
    // OpVariables must come first in the entry block.
    if !hoisted.is_empty() {
        let entry = &mut caller.blocks[0];
        let n = entry.instructions.iter().take_while(|i| i.class.opcode == Op::Variable).count();
        for (k, v) in hoisted.into_iter().enumerate() {
            entry.instructions.insert(n + k, v);
        }
    }
    Ok(())
}

/// Inline all calls in `m`. Returns false if there were none.
pub fn inline_calls(m: &mut Module) -> Result<bool, FrontendError> {
    if !m.functions.iter().any(has_calls) {
        return Ok(false);
    }
    let mut bound = m.header.as_ref().map(|h| h.bound).unwrap_or(0);
    let mut done: HashMap<Word, Function> = HashMap::new();
    let mut pending: Vec<Function> = std::mem::take(&mut m.functions);
    // Inline bottom-up: a function is ready once all its callees are done.
    while !pending.is_empty() {
        let before = pending.len();
        let mut next = vec![];
        for mut f in pending {
            let callees: HashSet<Word> = f.blocks.iter().flat_map(|b| b.instructions.iter())
                .filter(|i| i.class.opcode == Op::FunctionCall)
                .filter_map(|i| match i.operands.first() { Some(Operand::IdRef(id)) => Some(*id), _ => None })
                .collect();
            if callees.iter().all(|c| done.contains_key(c)) {
                let mut ids = Ids { bound: &mut bound };
                inline_into(&mut f, &done, &mut ids)?;
                done.insert(fid(&f).unwrap(), f);
            } else {
                next.push(f);
            }
        }
        if next.len() == before {
            return Err(FrontendError::Unsupported("recursive function calls".into()));
        }
        pending = next;
    }
    let entries: HashSet<Word> = m.entry_points.iter().filter_map(|e| match e.operands.get(1) {
        Some(Operand::IdRef(id)) => Some(*id),
        _ => None,
    }).collect();
    // Keep entry points, in their original order.
    let mut order: Vec<Word> = done.keys().copied().filter(|k| entries.contains(k)).collect();
    order.sort();
    m.functions = order.into_iter().map(|k| done.remove(&k).unwrap()).collect();
    if let Some(h) = m.header.as_mut() {
        h.bound = bound;
    }
    // Names of dropped ids are harmless, but OpName on a removed function
    // could confuse tools; drop debug names for ids no longer defined
    // only where they name functions.
    let live: HashSet<Word> = m.functions.iter().filter_map(fid).collect();
    let dead: HashSet<Word> = done.keys().copied().collect();
    m.debug_names.retain(|n| match n.operands.first() {
        Some(Operand::IdRef(id)) => live.contains(id) || !dead.contains(id),
        _ => true,
    });
    Ok(true)
}
