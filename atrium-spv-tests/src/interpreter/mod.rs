//! SPIR-V interpreter — the differential-test oracle.
//!
//! Walks SPIR-V bytes via `rspirv::dr::Module` with **no
//! shared frontend code** with the production backends.
//! This independence is the whole point — see the module-
//! level docs in [`crate`] and constraint F1 in
//! `docs/spec/tier2-shader-codegen-constraints.md`.
//!
//! # Scope (phase-0 v0c)
//!
//! Implements enough of SPIR-V to interpret a fragment
//! shader that:
//! - declares a single `vec4 out_color` output (Output
//!   storage class)
//! - writes a constant value to it
//! - returns
//!
//! That's enough to validate the harness plumbing end-to-
//! end and to host the first phase-2 demo (constant-colour
//! fullscreen quad through the Cranelift backend). Opcodes
//! beyond this minimal set return
//! [`InterpError::UnsupportedOpcode`] — the harness
//! interprets that as "skip this runner" rather than a
//! hard failure.
//!
//! # What we don't (yet) handle
//!
//! - Arithmetic, comparisons, vector ops (phase 1)
//! - Control flow beyond a single straight-line block
//!   (phase 1)
//! - Memory ops other than `OpStore` to an Output variable
//!   (phase 1)
//! - Texture sampling, derivatives, atomics (phase 5+)
//! - Vertex / compute stages (phase 6+)
//!
//! Adding opcodes is mostly mechanical — see the `eval`
//! function's dispatch match.

use std::collections::HashMap;

use rspirv::dr::{Instruction, Module, Operand};
use rspirv::spirv::{Op, StorageClass, Word};

use crate::pixels::RgbaF32;

/// Per-shader inputs (uniforms, push constants, varyings).
///
/// Phase-0 v0c uses an empty `ShaderInputs` for the
/// constant-colour case; the type exists so the harness
/// signature matches what later phases will need.
#[derive(Debug, Clone)]
pub struct ShaderInputs {
    /// Flat byte buffer holding all bound uniforms.
    pub uniforms: Vec<u8>,
    /// 128-byte push-constant block.
    pub push_constants: [u8; 128],
    /// Per-pixel-quad interpolated varyings for the
    /// fragment-shader invocations the harness drives.
    /// One entry per invocation.
    pub varyings_per_invocation: Vec<Vec<u8>>,
}

impl Default for ShaderInputs {
    fn default() -> Self {
        Self {
            uniforms: Vec::new(),
            push_constants: [0u8; 128],
            varyings_per_invocation: Vec::new(),
        }
    }
}

/// Per-shader outputs.
///
/// For a fragment shader this is one RGBA pixel per
/// invocation. Other stages will grow this type.
#[derive(Debug, Clone, Default)]
pub struct ShaderOutputs {
    /// One RGBA pixel per `varyings_per_invocation` entry.
    /// Single-element for the phase-0 single-invocation
    /// case.
    pub pixels: Vec<RgbaF32>,
}

/// Interpreter errors.
#[derive(Debug)]
pub enum InterpError {
    /// SPIR-V byte stream didn't parse.
    ParseFailed(String),
    /// No `OpEntryPoint` matches the requested stage.
    NoEntryPoint(&'static str),
    /// SPIR-V opcode we haven't implemented. Harness
    /// treats as "skip this runner".
    UnsupportedOpcode(String),
    /// Type table is malformed or references an unknown id.
    BadType(Word),
    /// Constant references an unknown id or has the wrong
    /// shape for its declared type.
    BadConstant(Word),
    /// Shader stores something other than vec4<f32> to the
    /// output variable. Phase-0 only handles vec4 colour
    /// outputs.
    UnsupportedOutput(String),
    /// Reached an opcode that needs control flow we don't
    /// yet implement.
    UnsupportedControlFlow(String),
}

/// The interpreter.
///
/// Cheap to construct (just parses + indexes). Expensive
/// to call for many invocations; intended for test
/// harness use only, never production.
pub struct Interpreter {
    /// Parsed SPIR-V module.
    module: Module,
    /// Type-id → type info.
    types: HashMap<Word, TypeInfo>,
    /// Constant-id → value.
    constants: HashMap<Word, ConstantValue>,
    /// Variable-id → (storage class, type id).
    variables: HashMap<Word, (StorageClass, Word)>,
    /// Struct id → ordered list of (member_byte_offset,
    /// member_type_id). Populated from OpTypeStruct +
    /// OpMemberDecorate Offset. Used by OpAccessChain to
    /// resolve member-index chains to a byte offset.
    struct_layouts: HashMap<Word, Vec<(u32, Word)>>,
    /// Entry point declared with ExecutionModel::Fragment.
    fragment_entry: Option<Word>,
}

/// Information about a SPIR-V type.
///
/// We index types by id at construction so the eval loop
/// doesn't re-walk types_global_values on every reference.
///
/// Many fields here aren't read by the phase-0 v0c eval
/// loop (it only handles the constant-store case which
/// touches the type table mainly through `decode_constant`
/// and the output-variable hand-off). Phase-1 broadens the
/// eval loop and those fields come alive then. `allow(dead_code)`
/// silences "never read" warnings for now.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum TypeInfo {
    /// `OpTypeVoid`.
    Void,
    /// `OpTypeBool`.
    Bool,
    /// `OpTypeInt width signedness`.
    Int { width: u32, signed: bool },
    /// `OpTypeFloat width`.
    Float { width: u32 },
    /// `OpTypeVector element_type_id element_count`.
    Vector { element: Word, count: u32 },
    /// `OpTypePointer storage_class pointee_type_id`.
    Pointer { storage: StorageClass, pointee: Word },
    /// `OpTypeFunction return_type [param_types]`.
    Function { return_ty: Word, params: Vec<Word> },
    /// Anything else, kept for diagnostic when we error.
    Other(Op),
}

/// A SPIR-V constant value.
#[derive(Debug, Clone)]
enum ConstantValue {
    /// Integer; width + signedness from the declared type.
    Int(i64),
    /// 32-bit float.
    F32(f32),
    /// 64-bit float.
    F64(f64),
    /// Bool.
    Bool(bool),
    /// Vector composite: element values in lane order.
    Vec(Vec<ConstantValue>),
    /// A tagged pointer: SPIR-V variable id + byte offset
    /// into that storage block. Produced by OpAccessChain;
    /// consumed by OpLoad / OpStore.
    Ptr { var_id: Word, byte_offset: u32 },
}

impl Interpreter {
    /// Parse + index a SPIR-V byte stream.
    pub fn new(spirv: &[u8]) -> Result<Self, InterpError> {
        let mut loader = rspirv::dr::Loader::new();
        rspirv::binary::parse_bytes(spirv, &mut loader)
            .map_err(|e| InterpError::ParseFailed(format!("{e:?}")))?;
        let module = loader.module();

        let mut interp = Self {
            module,
            types: HashMap::new(),
            constants: HashMap::new(),
            variables: HashMap::new(),
            struct_layouts: HashMap::new(),
            fragment_entry: None,
        };
        interp.index()?;
        Ok(interp)
    }

    /// Run the fragment entry-point once per
    /// `varyings_per_invocation` entry (or once with no
    /// inputs if the vector is empty), collecting an RGBA
    /// pixel per invocation.
    pub fn run_fragment(
        &self,
        inputs: &ShaderInputs,
    ) -> Result<ShaderOutputs, InterpError> {
        let entry = self.fragment_entry
            .ok_or(InterpError::NoEntryPoint("Fragment"))?;

        let n = inputs.varyings_per_invocation.len().max(1);
        let mut pixels = Vec::with_capacity(n);
        for _ in 0..n {
            let pixel = self.eval_fragment_invocation(entry, inputs)?;
            pixels.push(pixel);
        }
        Ok(ShaderOutputs { pixels })
    }

    /// Build the type / constant / variable / entry-point
    /// indices from `self.module`. Called once at `new()`.
    fn index(&mut self) -> Result<(), InterpError> {
        // Entry points.
        for ep in &self.module.entry_points {
            // OpEntryPoint: ExecutionModel, EntryPoint id,
            // Name string, [interface ids...].
            if ep.class.opcode != Op::EntryPoint { continue; }
            if let Some(Operand::ExecutionModel(model)) = ep.operands.first() {
                if *model == rspirv::spirv::ExecutionModel::Fragment {
                    if let Some(Operand::IdRef(id)) = ep.operands.get(1) {
                        self.fragment_entry = Some(*id);
                    }
                }
            }
        }

        // Types, constants, variables in declaration order
        // (forward references aren't allowed in SPIR-V).
        // Clone into a local Vec so we can borrow &mut self
        // for the per-inst indexing.
        let globals = self.module.types_global_values.clone();
        for inst in &globals {
            self.index_global_inst(inst)?;
        }

        // OpMemberDecorate Offset → struct_layouts.
        // Two-pass: first collect (struct_id, member_idx,
        // offset) from annotations, then merge with each
        // OpTypeStruct's member-type-id sequence.
        let mut member_offsets: HashMap<(Word, u32), u32> = HashMap::new();
        for inst in &self.module.annotations.clone() {
            if inst.class.opcode != Op::MemberDecorate { continue; }
            let struct_id = op_id(&inst.operands, 0)?;
            let member_idx = op_lit(&inst.operands, 1)?;
            let kind = match inst.operands.get(2) {
                Some(Operand::Decoration(d)) => *d,
                _ => continue,
            };
            if kind == rspirv::spirv::Decoration::Offset {
                if let Some(Operand::LiteralBit32(off)) = inst.operands.get(3) {
                    member_offsets.insert((struct_id, member_idx), *off);
                }
            }
        }
        for inst in &globals {
            if inst.class.opcode != Op::TypeStruct { continue; }
            let Some(struct_id) = inst.result_id else { continue };
            let mut members = Vec::with_capacity(inst.operands.len());
            let mut ok = true;
            for (idx, op) in inst.operands.iter().enumerate() {
                let type_id = match op {
                    Operand::IdRef(id) => *id,
                    _ => { ok = false; break; }
                };
                match member_offsets.get(&(struct_id, idx as u32)) {
                    Some(off) => members.push((*off, type_id)),
                    None => { ok = false; break; }
                }
            }
            if ok && !members.is_empty() {
                self.struct_layouts.insert(struct_id, members);
            }
        }
        Ok(())
    }

    fn index_global_inst(&mut self, inst: &Instruction) -> Result<(), InterpError> {
        let result_id = match inst.result_id { Some(id) => id, None => return Ok(()) };
        match inst.class.opcode {
            Op::TypeVoid => { self.types.insert(result_id, TypeInfo::Void); }
            Op::TypeBool => { self.types.insert(result_id, TypeInfo::Bool); }
            Op::TypeInt => {
                let width = op_lit(&inst.operands, 0)?;
                let signed = op_lit(&inst.operands, 1)? != 0;
                self.types.insert(result_id, TypeInfo::Int { width, signed });
            }
            Op::TypeFloat => {
                let width = op_lit(&inst.operands, 0)?;
                self.types.insert(result_id, TypeInfo::Float { width });
            }
            Op::TypeVector => {
                let element = op_id(&inst.operands, 0)?;
                let count = op_lit(&inst.operands, 1)?;
                self.types.insert(result_id, TypeInfo::Vector { element, count });
            }
            Op::TypePointer => {
                let storage = op_storage(&inst.operands, 0)?;
                let pointee = op_id(&inst.operands, 1)?;
                self.types.insert(result_id, TypeInfo::Pointer { storage, pointee });
            }
            Op::TypeFunction => {
                let return_ty = op_id(&inst.operands, 0)?;
                let mut params = Vec::new();
                for i in 1..inst.operands.len() {
                    params.push(op_id(&inst.operands, i)?);
                }
                self.types.insert(result_id, TypeInfo::Function { return_ty, params });
            }
            Op::Constant => {
                let ty = inst.result_type.ok_or(InterpError::BadConstant(result_id))?;
                let value = self.decode_constant(ty, &inst.operands)?;
                self.constants.insert(result_id, value);
            }
            Op::ConstantTrue => {
                self.constants.insert(result_id, ConstantValue::Bool(true));
            }
            Op::ConstantFalse => {
                self.constants.insert(result_id, ConstantValue::Bool(false));
            }
            Op::ConstantComposite => {
                let mut parts = Vec::new();
                for i in 0..inst.operands.len() {
                    let id = op_id(&inst.operands, i)?;
                    let v = self.constants.get(&id)
                        .cloned()
                        .ok_or(InterpError::BadConstant(id))?;
                    parts.push(v);
                }
                self.constants.insert(result_id, ConstantValue::Vec(parts));
            }
            Op::Variable => {
                let storage = op_storage(&inst.operands, 0)?;
                let ty = inst.result_type.ok_or(InterpError::BadType(result_id))?;
                self.variables.insert(result_id, (storage, ty));
            }
            // Capabilities, ext-inst imports, names,
            // decorations, etc. — phase-0 ignores.
            _ => {}
        }
        Ok(())
    }

    fn decode_constant(
        &self,
        ty: Word,
        operands: &[Operand],
    ) -> Result<ConstantValue, InterpError> {
        let info = self.types.get(&ty)
            .ok_or(InterpError::BadType(ty))?;
        match info {
            TypeInfo::Int { width, signed: _ } => {
                let lo = op_lit(operands, 0)?;
                let value = if *width <= 32 {
                    lo as i64
                } else if *width == 64 {
                    let hi = op_lit(operands, 1)?;
                    ((hi as i64) << 32) | (lo as i64 & 0xffff_ffff)
                } else {
                    return Err(InterpError::BadConstant(ty));
                };
                Ok(ConstantValue::Int(value))
            }
            TypeInfo::Float { width } => match *width {
                32 => {
                    let bits = op_lit(operands, 0)?;
                    Ok(ConstantValue::F32(f32::from_bits(bits)))
                }
                64 => {
                    let lo = op_lit(operands, 0)? as u64;
                    let hi = op_lit(operands, 1)? as u64;
                    Ok(ConstantValue::F64(f64::from_bits((hi << 32) | lo)))
                }
                _ => Err(InterpError::BadConstant(ty)),
            }
            _ => Err(InterpError::BadConstant(ty)),
        }
    }

    /// Run the fragment shader function once.
    ///
    /// Phase-0 v0c handles only the constant-output case:
    /// the function body is a single straight-line block
    /// that stores a constant vec4<f32> into the Output
    /// variable and returns.
    fn eval_fragment_invocation(
        &self,
        entry: Word,
        inputs: &ShaderInputs,
    ) -> Result<RgbaF32, InterpError> {
        let func = self.module.functions.iter()
            .find(|f| f.def.as_ref().and_then(|d| d.result_id) == Some(entry))
            .ok_or(InterpError::NoEntryPoint("Fragment (function body missing)"))?;

        // Per-invocation SSA value table.
        let mut values: HashMap<Word, ConstantValue> = HashMap::new();
        // Per-invocation memory: variable-id → current
        // stored value.
        let mut storage: HashMap<Word, ConstantValue> = HashMap::new();

        // Multi-block walk: start at the first block (rspirv
        // keeps SPIR-V function blocks in declaration order;
        // the first one is the entry). Each block ends with a
        // terminator (OpBranch / OpBranchConditional /
        // OpReturn / OpKill etc.); we follow it.
        let mut current_idx: usize = 0;
        let mut prev_label: Option<Word> = None;
        let mut hops: u32 = 0;
        const MAX_HOPS: u32 = 1024;
        loop {
            if hops >= MAX_HOPS {
                return Err(InterpError::UnsupportedControlFlow(format!(
                    "fragment exceeded {MAX_HOPS} block-hops (loop?)"
                )));
            }
            hops += 1;
            let block = func.blocks.get(current_idx).ok_or_else(||
                InterpError::UnsupportedControlFlow(format!(
                    "block index {current_idx} out of range",
                )))?;
            let current_label = block.label.as_ref()
                .and_then(|l| l.result_id);

            // Resolve any leading OpPhi instructions using
            // prev_label to pick the right arm.
            let mut phi_count = 0;
            for inst in &block.instructions {
                if inst.class.opcode != Op::Phi { break; }
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                // Walk arm pairs.
                let mut chosen: Option<ConstantValue> = None;
                let mut j = 0;
                while j + 1 < inst.operands.len() {
                    let val_id = op_id(&inst.operands, j)?;
                    let parent  = op_id(&inst.operands, j+1)?;
                    if Some(parent) == prev_label {
                        chosen = Some(self.lookup_value(val_id, &values)?);
                        break;
                    }
                    j += 2;
                }
                let chosen = chosen.ok_or_else(||
                    InterpError::UnsupportedControlFlow(format!(
                        "OpPhi in block {current_label:?} has no arm \
                         matching prev block {prev_label:?}",
                    )))?;
                values.insert(result_id, chosen);
                phi_count += 1;
            }

            // Run every non-terminator, non-phi inst.
            let last_idx = block.instructions.len().saturating_sub(1);
            for (i, inst) in block.instructions.iter().enumerate() {
                if i < phi_count { continue; }
                if i == last_idx { break; }
                self.eval_inst(inst, &mut values, &mut storage, inputs)?;
            }
            let term = block.instructions.last().ok_or_else(||
                InterpError::UnsupportedControlFlow(
                    "block has no instructions".into()))?;
            match term.class.opcode {
                Op::Return | Op::Kill | Op::TerminateInvocation
                | Op::Unreachable => break,
                Op::Branch => {
                    let label = op_id(&term.operands, 0)?;
                    prev_label = current_label;
                    current_idx = self.find_block_index(func, label)?;
                }
                Op::BranchConditional => {
                    let cond_id = op_id(&term.operands, 0)?;
                    let t_label = op_id(&term.operands, 1)?;
                    let f_label = op_id(&term.operands, 2)?;
                    let cond = self.lookup_value(cond_id, &values)?;
                    let taken = match cond {
                        ConstantValue::Bool(b) => b,
                        ConstantValue::Int(n) => n != 0,
                        other => return Err(InterpError::UnsupportedOpcode(
                            format!("BranchConditional cond: {other:?}"))),
                    };
                    let target = if taken { t_label } else { f_label };
                    prev_label = current_label;
                    current_idx = self.find_block_index(func, target)?;
                }
                Op::SelectionMerge | Op::LoopMerge =>
                    return Err(InterpError::UnsupportedControlFlow(
                        "merge marker as terminator".into())),
                other => return Err(InterpError::UnsupportedControlFlow(
                    format!("unsupported terminator {other:?}"))),
            }
        }

        // Find the Output variable; expect vec4<f32>.
        let output_id = self.variables.iter()
            .find(|(_, (sc, _))| *sc == StorageClass::Output)
            .map(|(id, _)| *id)
            .ok_or_else(|| InterpError::UnsupportedOutput(
                "no Output storage variable found".to_string(),
            ))?;
        let stored = storage.get(&output_id)
            .ok_or_else(|| InterpError::UnsupportedOutput(
                "Output variable was never stored to".to_string(),
            ))?;
        constant_to_rgba(stored)
    }

    fn eval_inst(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        storage: &mut HashMap<Word, ConstantValue>,
        inputs: &ShaderInputs,
    ) -> Result<(), InterpError> {
        match inst.class.opcode {
            Op::Store => {
                // OpStore Pointer Value
                let ptr = op_id(&inst.operands, 0)?;
                let value_id = op_id(&inst.operands, 1)?;
                let v = self.lookup_value(value_id, values)?;
                storage.insert(ptr, v);
                Ok(())
            }
            Op::Return => Ok(()),
            // OpLabel is the block opener; rspirv keeps it
            // as block.label, not in instructions, but
            // older parses might leave it. No-op either way.
            Op::Label | Op::Nop => Ok(()),
            // Structured-CFG merge markers carry no
            // runtime semantics for the interpreter — the
            // multi-block stepper already follows
            // Op{Branch,BranchConditional}.
            Op::SelectionMerge | Op::LoopMerge => Ok(()),
            // ── Float arithmetic ──────────────────────────
            Op::FAdd => self.eval_binop_float(inst, values, |a, b| a + b),
            Op::FSub => self.eval_binop_float(inst, values, |a, b| a - b),
            Op::FMul | Op::VectorTimesScalar =>
                self.eval_binop_float(inst, values, |a, b| a * b),
            Op::FDiv => self.eval_binop_float(inst, values, |a, b| a / b),
            Op::FNegate => self.eval_unop_float(inst, values, |a| -a),
            // OpVectorShuffle: gather lanes from
            // src1 ++ src2 by index.
            Op::VectorShuffle => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let src1_id = op_id(&inst.operands, 0)?;
                let src2_id = op_id(&inst.operands, 1)?;
                let src1 = self.lookup_value(src1_id, values)?;
                let src2 = self.lookup_value(src2_id, values)?;
                let s1_lanes: &[ConstantValue] = match &src1 {
                    ConstantValue::Vec(v) => v,
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "VectorShuffle src1 is not a Vec: {src1:?}",
                    ))),
                };
                let s2_lanes: &[ConstantValue] = match &src2 {
                    ConstantValue::Vec(v) => v,
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "VectorShuffle src2 is not a Vec: {src2:?}",
                    ))),
                };
                let combined_len = s1_lanes.len() + s2_lanes.len();
                let mut out = Vec::with_capacity(inst.operands.len() - 2);
                for op in &inst.operands[2..] {
                    let idx = match op {
                        rspirv::dr::Operand::LiteralBit32(v) => *v,
                        _ => return Err(InterpError::ParseFailed(
                            "VectorShuffle component expected LiteralBit32"
                                .to_string())),
                    };
                    if idx == 0xFFFF_FFFF {
                        // "Undefined" sentinel — match the
                        // backend's choice of 0.0.
                        out.push(ConstantValue::F32(0.0));
                        continue;
                    }
                    let idx = idx as usize;
                    if idx >= combined_len {
                        return Err(InterpError::UnsupportedOpcode(format!(
                            "VectorShuffle component {idx} out of range \
                             (combined source length {combined_len})",
                        )));
                    }
                    let lane = if idx < s1_lanes.len() {
                        s1_lanes[idx].clone()
                    } else {
                        s2_lanes[idx - s1_lanes.len()].clone()
                    };
                    out.push(lane);
                }
                values.insert(result_id, ConstantValue::Vec(out));
                Ok(())
            }

            // Float comparisons → Bool (i32 0/1 per
            // constraint B4). Ordered semantics treat NaN
            // operands as false; unordered treat them as
            // true.
            Op::FOrdEqual => self.eval_fcmp(inst, values, FCmp::OrdEq),
            Op::FOrdNotEqual => self.eval_fcmp(inst, values, FCmp::OrdNe),
            Op::FOrdLessThan => self.eval_fcmp(inst, values, FCmp::OrdLt),
            Op::FOrdLessThanEqual => self.eval_fcmp(inst, values, FCmp::OrdLe),
            Op::FOrdGreaterThan => self.eval_fcmp(inst, values, FCmp::OrdGt),
            Op::FOrdGreaterThanEqual => self.eval_fcmp(inst, values, FCmp::OrdGe),
            Op::FUnordEqual => self.eval_fcmp(inst, values, FCmp::UnordEq),
            Op::FUnordNotEqual => self.eval_fcmp(inst, values, FCmp::UnordNe),
            Op::FUnordLessThan => self.eval_fcmp(inst, values, FCmp::UnordLt),
            Op::FUnordLessThanEqual => self.eval_fcmp(inst, values, FCmp::UnordLe),
            Op::FUnordGreaterThan => self.eval_fcmp(inst, values, FCmp::UnordGt),
            Op::FUnordGreaterThanEqual => self.eval_fcmp(inst, values, FCmp::UnordGe),

            // OpSelect: cond ? t : f. cond is Bool
            // (i32 0/1) per B4.
            Op::Select => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let cond_id = op_id(&inst.operands, 0)?;
                let t_id    = op_id(&inst.operands, 1)?;
                let f_id    = op_id(&inst.operands, 2)?;
                let cond = self.lookup_value(cond_id, values)?;
                let t_val = self.lookup_value(t_id, values)?;
                let f_val = self.lookup_value(f_id, values)?;
                let stored = eval_select_value(&cond, &t_val, &f_val)?;
                values.insert(result_id, stored);
                Ok(())
            }

            // OpAccessChain / OpInBoundsAccessChain:
            // produce a tagged Ptr from base var + a
            // resolved byte offset (struct member walk).
            Op::AccessChain | Op::InBoundsAccessChain => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let base_id = op_id(&inst.operands, 0)?;
                // Base must be a Variable. Look up pointee
                // type id to start the walk.
                let (_storage, base_ptr_ty) = self.variables.get(&base_id)
                    .copied()
                    .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                        "AccessChain base id {base_id} is not a known Variable",
                    )))?;
                // base_ptr_ty is the OpTypePointer id; its
                // pointee is the variable's content type.
                let mut current_type = match self.types.get(&base_ptr_ty) {
                    Some(TypeInfo::Pointer { pointee, .. }) => *pointee,
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "AccessChain base var's type {base_ptr_ty} is not a pointer",
                    ))),
                };
                let mut byte_offset: u32 = 0;
                for op in inst.operands.iter().skip(1) {
                    let idx_id = match op {
                        Operand::IdRef(id) => *id,
                        _ => return Err(InterpError::ParseFailed(
                            "AccessChain index expected IdRef".to_string())),
                    };
                    let idx_val: u32 = match self.constants.get(&idx_id) {
                        Some(ConstantValue::Int(v)) => *v as u32,
                        _ => return Err(InterpError::UnsupportedOpcode(format!(
                            "AccessChain index id {idx_id} is not a known integer constant",
                        ))),
                    };
                    let layout = self.struct_layouts.get(&current_type)
                        .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                            "AccessChain step through non-struct type id {current_type} \
                             not supported",
                        )))?;
                    let (mem_off, mem_ty) = layout.get(idx_val as usize)
                        .copied()
                        .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                            "AccessChain index {idx_val} out of range",
                        )))?;
                    byte_offset = byte_offset.saturating_add(mem_off);
                    current_type = mem_ty;
                }
                values.insert(result_id, ConstantValue::Ptr {
                    var_id: base_id, byte_offset,
                });
                Ok(())
            }
            // OpLoad: read leaf value through a pointer.
            // The pointer is either an AccessChain result
            // (Ptr in values) or a bare Variable.
            Op::Load => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let result_type_id = inst.result_type.ok_or_else(||
                    InterpError::BadType(0))?;
                let ptr_id = op_id(&inst.operands, 0)?;
                let (var_id, off) = match values.get(&ptr_id) {
                    Some(ConstantValue::Ptr { var_id, byte_offset }) =>
                        (*var_id, *byte_offset),
                    _ => (ptr_id, 0u32),
                };
                let (storage_class, _) = self.variables.get(&var_id).copied()
                    .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                        "Load of unknown variable id {var_id}",
                    )))?;
                let value = self.load_from_storage(
                    storage_class, off, result_type_id, inputs)?;
                values.insert(result_id, value);
                Ok(())
            }
            // OpDot: sum of element-wise products.
            Op::Dot => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let lhs_id = op_id(&inst.operands, 0)?;
                let rhs_id = op_id(&inst.operands, 1)?;
                let lhs = self.lookup_value(lhs_id, values)?;
                let rhs = self.lookup_value(rhs_id, values)?;
                let stored = eval_dot_value(&lhs, &rhs)?;
                values.insert(result_id, stored);
                Ok(())
            }
            // OpCompositeConstruct: pack N source values
            // into a Vec.
            Op::CompositeConstruct => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let mut elements = Vec::with_capacity(inst.operands.len());
                for op in &inst.operands {
                    let id = match op {
                        rspirv::dr::Operand::IdRef(id) => *id,
                        _ => return Err(InterpError::ParseFailed(
                            "CompositeConstruct expected IdRef".to_string())),
                    };
                    elements.push(self.lookup_value(id, values)?);
                }
                values.insert(result_id, ConstantValue::Vec(elements));
                Ok(())
            }
            other => Err(InterpError::UnsupportedOpcode(format!("{:?}", other))),
        }
    }

    /// Evaluate a SPIR-V float comparison
    /// (FOrd*/FUnord*). Returns Bool (scalar) or Vec of
    /// Bool (per-lane) depending on operand shape.
    fn eval_fcmp(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        cmp: FCmp,
    ) -> Result<(), InterpError> {
        let result_id = inst.result_id.ok_or_else(||
            InterpError::BadConstant(0))?;
        let lhs_id = op_id(&inst.operands, 0)?;
        let rhs_id = op_id(&inst.operands, 1)?;
        let lhs = self.lookup_value(lhs_id, values)?;
        let rhs = self.lookup_value(rhs_id, values)?;
        let stored = eval_fcmp_value(&lhs, &rhs, cmp)?;
        values.insert(result_id, stored);
        Ok(())
    }

    /// Evaluate a SPIR-V float-arithmetic binop
    /// (FAdd / FSub / FMul / FDiv).
    ///
    /// Polymorphic by operand type: scalar f32/f64 or vec
    /// of same. For Vec operands, walks lanes element-by-
    /// element. Mixed-shape (one scalar, one vec) is
    /// rejected — SPIR-V doesn't allow that anyway.
    fn eval_binop_float(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        op: impl Fn(f64, f64) -> f64,
    ) -> Result<(), InterpError> {
        let result_id = inst.result_id.ok_or_else(||
            InterpError::BadConstant(0))?;
        let lhs_id = op_id(&inst.operands, 0)?;
        let rhs_id = op_id(&inst.operands, 1)?;
        let lhs = self.lookup_value(lhs_id, values)?;
        let rhs = self.lookup_value(rhs_id, values)?;
        let stored = eval_float_binop_value(&lhs, &rhs, &op)?;
        values.insert(result_id, stored);
        Ok(())
    }

    /// Evaluate a SPIR-V float-arithmetic unop
    /// (FNegate). Polymorphic for scalar or vec.
    fn eval_unop_float(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        op: impl Fn(f64) -> f64,
    ) -> Result<(), InterpError> {
        let result_id = inst.result_id.ok_or_else(||
            InterpError::BadConstant(0))?;
        let src_id = op_id(&inst.operands, 0)?;
        let src = self.lookup_value(src_id, values)?;
        let stored = eval_float_unop_value(&src, &op)?;
        values.insert(result_id, stored);
        Ok(())
    }

    /// Find a SPIR-V function's block by its OpLabel id.
    fn find_block_index(
        &self,
        func: &rspirv::dr::Function,
        label_id: Word,
    ) -> Result<usize, InterpError> {
        for (i, b) in func.blocks.iter().enumerate() {
            if b.label.as_ref().and_then(|l| l.result_id) == Some(label_id) {
                return Ok(i);
            }
        }
        Err(InterpError::UnsupportedControlFlow(format!(
            "block label {label_id} not found in function",
        )))
    }

    /// Read a leaf value out of the per-storage-class byte
    /// buffer (push-constants / uniforms) at the given byte
    /// offset. Result type is decoded from the type id.
    fn load_from_storage(
        &self,
        storage_class: StorageClass,
        byte_offset: u32,
        type_id: Word,
        inputs: &ShaderInputs,
    ) -> Result<ConstantValue, InterpError> {
        // Pick the source buffer.
        let buf: &[u8] = match storage_class {
            StorageClass::PushConstant => &inputs.push_constants[..],
            StorageClass::Uniform | StorageClass::UniformConstant
            | StorageClass::StorageBuffer => &inputs.uniforms[..],
            other => return Err(InterpError::UnsupportedOpcode(format!(
                "Load from storage class {other:?} not supported",
            ))),
        };
        let info = self.types.get(&type_id)
            .ok_or(InterpError::BadType(type_id))?
            .clone();
        match info {
            TypeInfo::Float { width: 32 } => {
                Ok(ConstantValue::F32(read_f32_at(buf, byte_offset)?))
            }
            TypeInfo::Int { width: 32, signed } => {
                let v = read_u32_at(buf, byte_offset)? as i64;
                let v = if signed { (v as i32) as i64 } else { v & 0xFFFF_FFFF };
                Ok(ConstantValue::Int(v))
            }
            TypeInfo::Vector { element, count } => {
                let elem_info = self.types.get(&element)
                    .ok_or(InterpError::BadType(element))?
                    .clone();
                let elem_size: u32 = match elem_info {
                    TypeInfo::Float { width } => width / 8,
                    TypeInfo::Int   { width, .. } => width / 8,
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "Load of vector with non-scalar element {elem_info:?}",
                    ))),
                };
                let mut lanes = Vec::with_capacity(count as usize);
                for i in 0..count {
                    let off = byte_offset.saturating_add(i * elem_size);
                    let v = self.load_from_storage(
                        storage_class, off, element, inputs)?;
                    lanes.push(v);
                }
                Ok(ConstantValue::Vec(lanes))
            }
            other => Err(InterpError::UnsupportedOpcode(format!(
                "Load of type {other:?} not supported",
            ))),
        }
    }

    fn lookup_value(
        &self,
        id: Word,
        values: &HashMap<Word, ConstantValue>,
    ) -> Result<ConstantValue, InterpError> {
        if let Some(v) = values.get(&id) { return Ok(v.clone()); }
        if let Some(v) = self.constants.get(&id) { return Ok(v.clone()); }
        Err(InterpError::BadConstant(id))
    }
}

// ── Operand-decoding helpers ───────────────────────────────────

fn op_id(operands: &[Operand], i: usize) -> Result<Word, InterpError> {
    operands.get(i)
        .and_then(|o| if let Operand::IdRef(id) = o { Some(*id) } else { None })
        .ok_or_else(|| InterpError::ParseFailed(
            format!("expected IdRef at operand {i}"),
        ))
}

fn op_lit(operands: &[Operand], i: usize) -> Result<u32, InterpError> {
    operands.get(i)
        .and_then(|o| match o {
            Operand::LiteralBit32(v) => Some(*v),
            Operand::LiteralBit64(_) => None, // caller handles 64-bit case
            _ => None,
        })
        .ok_or_else(|| InterpError::ParseFailed(
            format!("expected LiteralBit32 at operand {i}"),
        ))
}

fn op_storage(operands: &[Operand], i: usize) -> Result<StorageClass, InterpError> {
    operands.get(i)
        .and_then(|o| if let Operand::StorageClass(sc) = o { Some(*sc) } else { None })
        .ok_or_else(|| InterpError::ParseFailed(
            format!("expected StorageClass at operand {i}"),
        ))
}

/// Apply a scalar-f64 binary operator across two
/// ConstantValues, handling both scalar and vector
/// shapes. Recursive: vec lanes are themselves
/// ConstantValues (scalar in practice).
fn eval_float_binop_value(
    lhs: &ConstantValue,
    rhs: &ConstantValue,
    op: &impl Fn(f64, f64) -> f64,
) -> Result<ConstantValue, InterpError> {
    match (lhs, rhs) {
        // Scalar × Scalar.
        (ConstantValue::F32(a), ConstantValue::F32(b)) =>
            Ok(ConstantValue::F32(op(*a as f64, *b as f64) as f32)),
        (ConstantValue::F64(a), ConstantValue::F64(b)) =>
            Ok(ConstantValue::F64(op(*a, *b))),
        (ConstantValue::F32(a), ConstantValue::F64(b)) =>
            Ok(ConstantValue::F64(op(*a as f64, *b))),
        (ConstantValue::F64(a), ConstantValue::F32(b)) =>
            Ok(ConstantValue::F64(op(*a, *b as f64))),

        // Vec × Vec.
        (ConstantValue::Vec(a), ConstantValue::Vec(b)) => {
            if a.len() != b.len() {
                return Err(InterpError::UnsupportedOpcode(format!(
                    "vec binop with mismatched lane counts: {} vs {}",
                    a.len(), b.len(),
                )));
            }
            let mut out = Vec::with_capacity(a.len());
            for (la, lb) in a.iter().zip(b.iter()) {
                out.push(eval_float_binop_value(la, lb, op)?);
            }
            Ok(ConstantValue::Vec(out))
        }

        // Vec × scalar broadcast (and symmetric).
        // OpVectorTimesScalar lowers to the FMul case
        // where one operand is vec and the other is
        // f32 — apply the scalar to every lane.
        (ConstantValue::Vec(a), scalar @ (ConstantValue::F32(_) | ConstantValue::F64(_))) => {
            let mut out = Vec::with_capacity(a.len());
            for la in a {
                out.push(eval_float_binop_value(la, scalar, op)?);
            }
            Ok(ConstantValue::Vec(out))
        }
        (scalar @ (ConstantValue::F32(_) | ConstantValue::F64(_)), ConstantValue::Vec(b)) => {
            let mut out = Vec::with_capacity(b.len());
            for lb in b {
                out.push(eval_float_binop_value(scalar, lb, op)?);
            }
            Ok(ConstantValue::Vec(out))
        }

        _ => Err(InterpError::UnsupportedOpcode(format!(
            "float binop on incompatible operands: {lhs:?}, {rhs:?}",
        ))),
    }
}

/// Float-comparison tag for the interpreter's
/// eval_fcmp dispatch. Ordered comparisons treat NaN
/// operands as false; unordered as true.
#[derive(Copy, Clone, Debug)]
enum FCmp {
    OrdEq, OrdNe, OrdLt, OrdLe, OrdGt, OrdGe,
    UnordEq, UnordNe, UnordLt, UnordLe, UnordGt, UnordGe,
}

fn fcmp_scalar(a: f64, b: f64, cmp: FCmp) -> bool {
    let unordered = a.is_nan() || b.is_nan();
    match cmp {
        FCmp::OrdEq   => !unordered && a == b,
        FCmp::OrdNe   => !unordered && a != b,
        FCmp::OrdLt   => !unordered && a <  b,
        FCmp::OrdLe   => !unordered && a <= b,
        FCmp::OrdGt   => !unordered && a >  b,
        FCmp::OrdGe   => !unordered && a >= b,
        FCmp::UnordEq =>  unordered || a == b,
        FCmp::UnordNe =>  unordered || a != b,
        FCmp::UnordLt =>  unordered || a <  b,
        FCmp::UnordLe =>  unordered || a <= b,
        FCmp::UnordGt =>  unordered || a >  b,
        FCmp::UnordGe =>  unordered || a >= b,
    }
}

fn as_f64(v: &ConstantValue) -> Option<f64> {
    match v {
        ConstantValue::F32(x) => Some(*x as f64),
        ConstantValue::F64(x) => Some(*x),
        _ => None,
    }
}

/// Polymorphic float-compare: scalar×scalar → Bool,
/// vec×vec → Vec<Bool>.
fn eval_fcmp_value(
    lhs: &ConstantValue,
    rhs: &ConstantValue,
    cmp: FCmp,
) -> Result<ConstantValue, InterpError> {
    match (lhs, rhs) {
        (ConstantValue::Vec(a), ConstantValue::Vec(b)) => {
            if a.len() != b.len() {
                return Err(InterpError::UnsupportedOpcode(format!(
                    "fcmp with mismatched lane counts: {} vs {}",
                    a.len(), b.len(),
                )));
            }
            let mut out = Vec::with_capacity(a.len());
            for (la, lb) in a.iter().zip(b.iter()) {
                out.push(eval_fcmp_value(la, lb, cmp)?);
            }
            Ok(ConstantValue::Vec(out))
        }
        _ => {
            let a = as_f64(lhs).ok_or_else(|| InterpError::UnsupportedOpcode(
                format!("fcmp on non-float: {lhs:?}")))?;
            let b = as_f64(rhs).ok_or_else(|| InterpError::UnsupportedOpcode(
                format!("fcmp on non-float: {rhs:?}")))?;
            Ok(ConstantValue::Bool(fcmp_scalar(a, b, cmp)))
        }
    }
}

/// Evaluate OpSelect: cond ? t : f. Supports scalar cond
/// with scalar-or-vec branches, and per-lane vec cond
/// with vec branches.
fn eval_select_value(
    cond: &ConstantValue,
    t_val: &ConstantValue,
    f_val: &ConstantValue,
) -> Result<ConstantValue, InterpError> {
    match cond {
        ConstantValue::Bool(c) => Ok(if *c { t_val.clone() } else { f_val.clone() }),
        // i32 0/1 convention: treat non-zero as true.
        ConstantValue::Int(n) => Ok(if *n != 0 { t_val.clone() } else { f_val.clone() }),
        ConstantValue::Vec(cs) => {
            let ts = match t_val {
                ConstantValue::Vec(v) => v,
                _ => return Err(InterpError::UnsupportedOpcode(
                    "Select with vec cond requires vec branches".into())),
            };
            let fs = match f_val {
                ConstantValue::Vec(v) => v,
                _ => return Err(InterpError::UnsupportedOpcode(
                    "Select with vec cond requires vec branches".into())),
            };
            if cs.len() != ts.len() || cs.len() != fs.len() {
                return Err(InterpError::UnsupportedOpcode(format!(
                    "Select lane-count mismatch: cond={}, t={}, f={}",
                    cs.len(), ts.len(), fs.len())));
            }
            let mut out = Vec::with_capacity(cs.len());
            for ((c, t), f) in cs.iter().zip(ts.iter()).zip(fs.iter()) {
                out.push(eval_select_value(c, t, f)?);
            }
            Ok(ConstantValue::Vec(out))
        }
        other => Err(InterpError::UnsupportedOpcode(format!(
            "Select cond is not Bool/Int/Vec: {other:?}"))),
    }
}

/// Evaluate OpDot: sum of element-wise products. Both
/// operands must be vectors of the same length.
fn eval_dot_value(
    lhs: &ConstantValue,
    rhs: &ConstantValue,
) -> Result<ConstantValue, InterpError> {
    let (a, b) = match (lhs, rhs) {
        (ConstantValue::Vec(a), ConstantValue::Vec(b)) => (a, b),
        _ => return Err(InterpError::UnsupportedOpcode(format!(
            "Dot expects two vectors, got {lhs:?} and {rhs:?}",
        ))),
    };
    if a.len() != b.len() {
        return Err(InterpError::UnsupportedOpcode(format!(
            "Dot with mismatched lane counts: {} vs {}",
            a.len(), b.len(),
        )));
    }
    let mut acc: f64 = 0.0;
    let mut had_f64 = false;
    for (la, lb) in a.iter().zip(b.iter()) {
        let (av, bv, f64_op) = match (la, lb) {
            (ConstantValue::F32(x), ConstantValue::F32(y)) =>
                (*x as f64, *y as f64, false),
            (ConstantValue::F64(x), ConstantValue::F64(y)) =>
                (*x, *y, true),
            (ConstantValue::F32(x), ConstantValue::F64(y)) =>
                (*x as f64, *y, true),
            (ConstantValue::F64(x), ConstantValue::F32(y)) =>
                (*x, *y as f64, true),
            _ => return Err(InterpError::UnsupportedOpcode(format!(
                "Dot lane has non-float elements: {la:?}, {lb:?}",
            ))),
        };
        had_f64 |= f64_op;
        acc += av * bv;
    }
    Ok(if had_f64 { ConstantValue::F64(acc) } else { ConstantValue::F32(acc as f32) })
}

/// Apply a scalar-f64 unary operator across a
/// ConstantValue, handling both scalar and vector shapes.
fn eval_float_unop_value(
    src: &ConstantValue,
    op: &impl Fn(f64) -> f64,
) -> Result<ConstantValue, InterpError> {
    match src {
        ConstantValue::F32(a) =>
            Ok(ConstantValue::F32(op(*a as f64) as f32)),
        ConstantValue::F64(a) =>
            Ok(ConstantValue::F64(op(*a))),
        ConstantValue::Vec(lanes) => {
            let mut out = Vec::with_capacity(lanes.len());
            for l in lanes {
                out.push(eval_float_unop_value(l, op)?);
            }
            Ok(ConstantValue::Vec(out))
        }
        _ => Err(InterpError::UnsupportedOpcode(format!(
            "float unop on incompatible operand: {src:?}",
        ))),
    }
}

fn read_u32_at(buf: &[u8], byte_offset: u32) -> Result<u32, InterpError> {
    let lo = byte_offset as usize;
    let hi = lo.checked_add(4).ok_or_else(|| InterpError::UnsupportedOpcode(
        "Load offset overflow".into()))?;
    if hi > buf.len() {
        return Err(InterpError::UnsupportedOpcode(format!(
            "Load out of bounds: offset {lo}+4 > buf len {}", buf.len())));
    }
    Ok(u32::from_le_bytes([buf[lo], buf[lo+1], buf[lo+2], buf[lo+3]]))
}

fn read_f32_at(buf: &[u8], byte_offset: u32) -> Result<f32, InterpError> {
    Ok(f32::from_bits(read_u32_at(buf, byte_offset)?))
}

fn constant_to_rgba(v: &ConstantValue) -> Result<RgbaF32, InterpError> {
    match v {
        ConstantValue::Vec(elements) if elements.len() == 4 => {
            let mut out = [0.0f32; 4];
            for (i, e) in elements.iter().enumerate() {
                out[i] = match e {
                    ConstantValue::F32(f) => *f,
                    ConstantValue::F64(f) => *f as f32,
                    ConstantValue::Int(n) => *n as f32,
                    ConstantValue::Bool(b) => if *b { 1.0 } else { 0.0 },
                    ConstantValue::Vec(_) => return Err(InterpError::UnsupportedOutput(
                        "nested vector in output".to_string(),
                    )),
                    ConstantValue::Ptr { .. } => return Err(InterpError::UnsupportedOutput(
                        "pointer in output".to_string(),
                    )),
                };
            }
            Ok(out)
        }
        other => Err(InterpError::UnsupportedOutput(
            format!("output is not vec4: {other:?}"),
        )),
    }
}
