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
            let pixel = self.eval_fragment_invocation(entry)?;
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
    fn eval_fragment_invocation(&self, entry: Word) -> Result<RgbaF32, InterpError> {
        let func = self.module.functions.iter()
            .find(|f| f.def.as_ref().and_then(|d| d.result_id) == Some(entry))
            .ok_or(InterpError::NoEntryPoint("Fragment (function body missing)"))?;

        // Per-invocation SSA value table.
        let mut values: HashMap<Word, ConstantValue> = HashMap::new();
        // Per-invocation memory: variable-id → current
        // stored value.
        let mut storage: HashMap<Word, ConstantValue> = HashMap::new();

        if func.blocks.len() != 1 {
            return Err(InterpError::UnsupportedControlFlow(
                format!("fragment has {} blocks; v0c supports exactly 1",
                        func.blocks.len()),
            ));
        }
        let block = &func.blocks[0];
        for inst in &block.instructions {
            self.eval_inst(inst, &mut values, &mut storage)?;
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
            // ── Float arithmetic ──────────────────────────
            Op::FAdd => self.eval_binop_float(inst, values, |a, b| a + b),
            Op::FSub => self.eval_binop_float(inst, values, |a, b| a - b),
            Op::FMul => self.eval_binop_float(inst, values, |a, b| a * b),
            Op::FDiv => self.eval_binop_float(inst, values, |a, b| a / b),
            Op::FNegate => self.eval_unop_float(inst, values, |a| -a),
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

        _ => Err(InterpError::UnsupportedOpcode(format!(
            "float binop on incompatible operands: {lhs:?}, {rhs:?}",
        ))),
    }
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
                };
            }
            Ok(out)
        }
        other => Err(InterpError::UnsupportedOutput(
            format!("output is not vec4: {other:?}"),
        )),
    }
}
