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
use rspirv::spirv::{Decoration, Dim, Op, StorageClass, Word};

use crate::pixels::RgbaF32;

/// A texture descriptor + sampler bound at a `(set, binding)`
/// slot, for the interpreter's `OpImageSample*` handler.
///
/// Designed to be cheap to construct in test code: hand-
/// pack a byte buffer for `data`, fill in the dims +
/// format, and pass an array of these in
/// [`ShaderInputs::textures`].
#[derive(Debug, Clone)]
pub struct TextureBinding {
    /// SPIR-V `DescriptorSet` decoration value.
    pub set: u32,
    /// SPIR-V `Binding` decoration value.
    pub binding: u32,
    /// Row-major texel data (size `>= height * stride_bytes`).
    pub data: Vec<u8>,
    /// Image width in texels.
    pub width: u32,
    /// Image height in texels.
    pub height: u32,
    /// Bytes per row (≥ `width * bytes_per_texel(format)`).
    pub stride_bytes: u32,
    /// `atrium_spv_runtime::TexFormat` as `u32`.
    pub format: u32,
    /// Sampler configuration (filter + wrap modes).
    pub sampler: atrium_spv_runtime::SamplerDesc,
}

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
    /// Image / sampler descriptors bound at SPIR-V
    /// `(DescriptorSet, Binding)` slots. The interpreter's
    /// `OpImageSample*` handler looks up the texture by
    /// `(set, binding)` and calls
    /// `atrium_spv_runtime::atrium_tex_sample_2d`.
    pub textures: Vec<TextureBinding>,
    /// Per-invocation packed vertex-attribute bytes for
    /// the vertex stage. `run_vertex` walks the function
    /// once per entry. Empty = single invocation with no
    /// attribute reads (the constant-position smoke case).
    pub vertex_attributes_per_invocation: Vec<Vec<u8>>,
}

impl Default for ShaderInputs {
    fn default() -> Self {
        Self {
            uniforms: Vec::new(),
            push_constants: [0u8; 128],
            varyings_per_invocation: Vec::new(),
            textures: Vec::new(),
            vertex_attributes_per_invocation: Vec::new(),
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

/// Vertex-stage outputs. One position per invocation —
/// `gl_Position` written via `OpStore` through an
/// `OpAccessChain` into the gl_PerVertex block. Varyings
/// follow in a later phase.
#[derive(Debug, Clone, Default)]
pub struct VertexOutputs {
    /// One `vec4` position per invocation, in the order
    /// `vertex_attributes_per_invocation` lists them.
    pub positions: Vec<[f32; 4]>,
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
    /// Variable id → (DescriptorSet, Binding) from
    /// OpDecorate annotations. Used by the image-sample
    /// path to find the matching `TextureBinding` in the
    /// caller's `ShaderInputs`.
    var_binding: HashMap<Word, (u32, u32)>,
    /// Entry point declared with ExecutionModel::Fragment.
    fragment_entry: Option<Word>,
    /// Entry point declared with ExecutionModel::Vertex.
    /// `run_vertex` dispatches off this id.
    vertex_entry: Option<Word>,
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
    /// `OpTypeMatrix column_type_id column_count` —
    /// column-major, per SPIR-V. The interpreter stores
    /// matrix values as `ConstantValue::Vec(columns)`
    /// where each column is itself a `ConstantValue::Vec`
    /// of element scalars.
    Matrix { column: Word, count: u32 },
    /// `OpTypePointer storage_class pointee_type_id`.
    Pointer { storage: StorageClass, pointee: Word },
    /// `OpTypeFunction return_type [param_types]`.
    Function { return_ty: Word, params: Vec<Word> },
    /// `OpTypeImage` — only the dim matters for the
    /// interpreter; sampler/format/MS/etc. are baked
    /// into the host-side `TextureBinding`.
    Image { dim: Dim },
    /// `OpTypeSampler` — a sampler is just an opaque
    /// handle in the interpreter; the actual filter/wrap
    /// config lives in the bound `TextureBinding`.
    Sampler,
    /// `OpTypeSampledImage` — same lifetime as the image
    /// type it wraps; carries the dim through for
    /// validation.
    SampledImage { dim: Dim },
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
    /// A texture / sampler / sampled-image handle: the
    /// `(set, binding)` that selects which
    /// `TextureBinding` in `ShaderInputs` to sample from.
    /// Produced by `OpLoad` of an image/sampler variable
    /// or by `OpSampledImage` combining two such loads;
    /// consumed by `OpImageSample*` / `OpImageFetch`.
    Texture { set: u32, binding: u32 },
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
            var_binding: HashMap::new(),
            fragment_entry: None,
            vertex_entry: None,
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
        for i in 0..n {
            let pixel = self.eval_fragment_invocation(entry, inputs, i)?;
            pixels.push(pixel);
        }
        Ok(ShaderOutputs { pixels })
    }

    /// Run the Vertex entry-point once per
    /// `vertex_attributes_per_invocation` entry (or once
    /// with no inputs if the vector is empty), collecting
    /// one `vec4` position per invocation. v1 finds the
    /// position by scanning post-execution `storage` for a
    /// 4-lane vector value (the gl_Position store) — fine
    /// for the constant-position smoke shape; richer
    /// extraction (multiple outputs, location-tagged
    /// varyings, AccessChain → BuiltIn Position lookup)
    /// lands in a later phase.
    pub fn run_vertex(
        &self,
        inputs: &ShaderInputs,
    ) -> Result<VertexOutputs, InterpError> {
        let entry = self.vertex_entry
            .ok_or(InterpError::NoEntryPoint("Vertex"))?;
        let n = inputs.vertex_attributes_per_invocation.len().max(1);
        let mut positions = Vec::with_capacity(n);
        for i in 0..n {
            let pos = self.eval_vertex_invocation(entry, inputs, i)?;
            positions.push(pos);
        }
        Ok(VertexOutputs { positions })
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
                let id = match ep.operands.get(1) {
                    Some(Operand::IdRef(id)) => Some(*id),
                    _ => None,
                };
                match *model {
                    rspirv::spirv::ExecutionModel::Fragment =>
                        self.fragment_entry = id,
                    rspirv::spirv::ExecutionModel::Vertex =>
                        self.vertex_entry = id,
                    _ => {}
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

        // OpDecorate DescriptorSet / Binding → var_binding.
        // Two passes: collect both values per id then commit
        // a pair once both are seen (a SPIR-V image/sampler
        // variable must have both decorations, but they're
        // emitted as separate OpDecorate instructions).
        let mut sets: HashMap<Word, u32> = HashMap::new();
        let mut bindings: HashMap<Word, u32> = HashMap::new();
        for inst in &self.module.annotations.clone() {
            if inst.class.opcode != Op::Decorate { continue; }
            let target = op_id(&inst.operands, 0)?;
            let kind = match inst.operands.get(1) {
                Some(Operand::Decoration(d)) => *d,
                _ => continue,
            };
            let lit = match inst.operands.get(2) {
                Some(Operand::LiteralBit32(v)) => *v,
                _ => continue,
            };
            match kind {
                Decoration::DescriptorSet => { sets.insert(target, lit); }
                Decoration::Binding       => { bindings.insert(target, lit); }
                _ => {}
            }
        }
        for (id, set) in &sets {
            if let Some(b) = bindings.get(id) {
                self.var_binding.insert(*id, (*set, *b));
            }
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
            Op::TypeMatrix => {
                let column = op_id(&inst.operands, 0)?;
                let count = op_lit(&inst.operands, 1)?;
                self.types.insert(result_id, TypeInfo::Matrix { column, count });
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
            Op::TypeImage => {
                let dim = match inst.operands.get(1) {
                    Some(Operand::Dim(d)) => *d,
                    _ => return Err(InterpError::BadType(result_id)),
                };
                self.types.insert(result_id, TypeInfo::Image { dim });
            }
            Op::TypeSampler => {
                self.types.insert(result_id, TypeInfo::Sampler);
            }
            Op::TypeSampledImage => {
                let img_id = op_id(&inst.operands, 0)?;
                let dim = match self.types.get(&img_id) {
                    Some(TypeInfo::Image { dim }) => *dim,
                    _ => return Err(InterpError::BadType(result_id)),
                };
                self.types.insert(result_id, TypeInfo::SampledImage { dim });
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
        inv_idx: usize,
    ) -> Result<RgbaF32, InterpError> {
        let is_vertex = false;
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
                self.eval_inst(inst, &mut values, &mut storage, inputs, inv_idx, is_vertex)?;
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
                Op::Switch => {
                    // Operands: selector, default, (lit, target)+.
                    let sel_id = op_id(&term.operands, 0)?;
                    let default_label = op_id(&term.operands, 1)?;
                    let sel = self.lookup_value(sel_id, &values)?;
                    let sel_v: i64 = match sel {
                        ConstantValue::Int(n) => n,
                        ConstantValue::Bool(b) => if b { 1 } else { 0 },
                        other => return Err(InterpError::UnsupportedOpcode(
                            format!("Switch selector: {other:?}"))),
                    };
                    // Walk pairs.
                    let mut chosen_label = default_label;
                    let mut j = 2;
                    while j + 1 < term.operands.len() {
                        let lit = match &term.operands[j] {
                            Operand::LiteralBit32(v) => *v as i32 as i64,
                            _ => return Err(InterpError::ParseFailed(
                                "Switch case literal".into())),
                        };
                        let tgt = op_id(&term.operands, j + 1)?;
                        if lit == sel_v {
                            chosen_label = tgt;
                            break;
                        }
                        j += 2;
                    }
                    prev_label = current_label;
                    current_idx = self.find_block_index(func, chosen_label)?;
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

    /// Walk a Vertex entry-point function for one
    /// invocation; return the stored `gl_Position` as a
    /// `[f32; 4]`. v1 implementation: same block-walker as
    /// the fragment path, but the post-execution extraction
    /// scans `storage` for a 4-lane `ConstantValue::Vec` (the
    /// gl_Position store via OpAccessChain). The smoke
    /// shader writes exactly one such value.
    fn eval_vertex_invocation(
        &self,
        entry: Word,
        inputs: &ShaderInputs,
        inv_idx: usize,
    ) -> Result<[f32; 4], InterpError> {
        let is_vertex = true;
        let func = self.module.functions.iter()
            .find(|f| f.def.as_ref().and_then(|d| d.result_id) == Some(entry))
            .ok_or(InterpError::NoEntryPoint("Vertex (function body missing)"))?;

        let mut values: HashMap<Word, ConstantValue> = HashMap::new();
        let mut storage: HashMap<Word, ConstantValue> = HashMap::new();
        let mut current_idx: usize = 0;
        let mut prev_label: Option<Word> = None;
        let mut hops: u32 = 0;
        const MAX_HOPS: u32 = 1024;
        loop {
            if hops >= MAX_HOPS {
                return Err(InterpError::UnsupportedControlFlow(format!(
                    "vertex exceeded {MAX_HOPS} block-hops (loop?)")));
            }
            hops += 1;
            let block = func.blocks.get(current_idx).ok_or_else(||
                InterpError::UnsupportedControlFlow(format!(
                    "block index {current_idx} out of range")))?;
            let current_label = block.label.as_ref()
                .and_then(|l| l.result_id);
            let mut phi_count = 0;
            for inst in &block.instructions {
                if inst.class.opcode != Op::Phi { break; }
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
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
                         matching prev block {prev_label:?}")))?;
                values.insert(result_id, chosen);
                phi_count += 1;
            }
            let last_idx = block.instructions.len().saturating_sub(1);
            for (i, inst) in block.instructions.iter().enumerate() {
                if i < phi_count { continue; }
                if i == last_idx { break; }
                self.eval_inst(inst, &mut values, &mut storage, inputs, inv_idx, is_vertex)?;
            }
            let term = block.instructions.last().ok_or_else(||
                InterpError::UnsupportedControlFlow(
                    "empty block".into()))?;
            match term.class.opcode {
                Op::Return => break,
                Op::Branch => {
                    let target = op_id(&term.operands, 0)?;
                    prev_label = current_label;
                    current_idx = self.find_block_index(func, target)?;
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
                other => return Err(InterpError::UnsupportedControlFlow(
                    format!("unsupported terminator {other:?} in vertex"))),
            }
        }

        // Scan `storage` for any stored 4-lane vector value;
        // that's the gl_Position write. (The smoke shader
        // writes exactly one.)
        for (_ptr_id, v) in &storage {
            if let ConstantValue::Vec(lanes) = v {
                if lanes.len() == 4 {
                    let mut out = [0.0f32; 4];
                    for (i, lane) in lanes.iter().enumerate() {
                        out[i] = match lane {
                            ConstantValue::F32(x) => *x,
                            ConstantValue::F64(x) => *x as f32,
                            ConstantValue::Int(n) => *n as f32,
                            other => return Err(InterpError::UnsupportedOutput(
                                format!("non-numeric vertex lane {i}: {other:?}"))),
                        };
                    }
                    return Ok(out);
                }
            }
        }
        Err(InterpError::UnsupportedOutput(
            "vertex shader didn't store a 4-lane vec to any output".into()))
    }

    fn eval_inst(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        storage: &mut HashMap<Word, ConstantValue>,
        inputs: &ShaderInputs,
        inv_idx: usize,
        is_vertex: bool,
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
            // Integer arithmetic.
            Op::IAdd => self.eval_binop_int(inst, values, |a, b| a.wrapping_add(b)),
            Op::ISub => self.eval_binop_int(inst, values, |a, b| a.wrapping_sub(b)),
            Op::IMul => self.eval_binop_int(inst, values, |a, b| a.wrapping_mul(b)),
            Op::SDiv => self.eval_binop_int(inst, values, |a, b|
                if b == 0 { 0 } else { a.wrapping_div(b) }),
            Op::UDiv => self.eval_binop_int(inst, values, |a, b|
                if b == 0 { 0 } else { (a as u64).wrapping_div(b as u64) as i64 }),
            Op::SMod => self.eval_binop_int(inst, values, |a, b|
                if b == 0 { 0 } else { a.wrapping_rem(b) }),
            Op::UMod => self.eval_binop_int(inst, values, |a, b|
                if b == 0 { 0 } else { (a as u64).wrapping_rem(b as u64) as i64 }),
            Op::SNegate => self.eval_unop_int(inst, values, |a| a.wrapping_neg()),
            // Integer comparisons → Bool.
            Op::IEqual => self.eval_icmp(inst, values, |a, b| a == b),
            Op::INotEqual => self.eval_icmp(inst, values, |a, b| a != b),
            Op::SLessThan => self.eval_icmp(inst, values, |a, b| a < b),
            Op::SLessThanEqual => self.eval_icmp(inst, values, |a, b| a <= b),
            Op::SGreaterThan => self.eval_icmp(inst, values, |a, b| a > b),
            Op::SGreaterThanEqual => self.eval_icmp(inst, values, |a, b| a >= b),
            Op::ULessThan => self.eval_icmp(inst, values, |a, b|
                (a as u64) <  (b as u64)),
            Op::ULessThanEqual => self.eval_icmp(inst, values, |a, b|
                (a as u64) <= (b as u64)),
            Op::UGreaterThan => self.eval_icmp(inst, values, |a, b|
                (a as u64) >  (b as u64)),
            Op::UGreaterThanEqual => self.eval_icmp(inst, values, |a, b|
                (a as u64) >= (b as u64)),
            // Bitwise + shifts (i32-width SPIR-V semantics:
            // shift amount mod 32).
            Op::BitwiseAnd => self.eval_binop_int(inst, values, |a, b| a & b),
            Op::BitwiseOr  => self.eval_binop_int(inst, values, |a, b| a | b),
            Op::BitwiseXor => self.eval_binop_int(inst, values, |a, b| a ^ b),
            Op::Not        => self.eval_unop_int(inst, values, |a|
                ((a as i32) ^ -1) as i64),
            Op::ShiftLeftLogical => self.eval_binop_int(inst, values, |a, b|
                ((a as i32).wrapping_shl((b & 31) as u32)) as i64),
            Op::ShiftRightLogical => self.eval_binop_int(inst, values, |a, b|
                ((a as u32).wrapping_shr((b & 31) as u32)) as i64),
            Op::ShiftRightArithmetic => self.eval_binop_int(inst, values, |a, b|
                ((a as i32).wrapping_shr((b & 31) as u32)) as i64),
            // Int↔float conversions.
            Op::ConvertSToF => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let src_id = op_id(&inst.operands, 0)?;
                let src = self.lookup_value(src_id, values)?;
                let v = match src {
                    ConstantValue::Int(n) => (n as i32) as f32,
                    other => return Err(InterpError::UnsupportedOpcode(
                        format!("ConvertSToF on non-int: {other:?}"))),
                };
                values.insert(result_id, ConstantValue::F32(v));
                Ok(())
            }
            Op::ConvertUToF => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let src_id = op_id(&inst.operands, 0)?;
                let src = self.lookup_value(src_id, values)?;
                let v = match src {
                    ConstantValue::Int(n) => (n as u32) as f32,
                    other => return Err(InterpError::UnsupportedOpcode(
                        format!("ConvertUToF on non-int: {other:?}"))),
                };
                values.insert(result_id, ConstantValue::F32(v));
                Ok(())
            }
            Op::ConvertFToS => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let src_id = op_id(&inst.operands, 0)?;
                let src = self.lookup_value(src_id, values)?;
                let v = match src {
                    ConstantValue::F32(f) => {
                        if f.is_nan() { 0 } else { f as i32 as i64 }
                    }
                    ConstantValue::F64(f) => {
                        if f.is_nan() { 0 } else { f as i32 as i64 }
                    }
                    other => return Err(InterpError::UnsupportedOpcode(
                        format!("ConvertFToS on non-float: {other:?}"))),
                };
                values.insert(result_id, ConstantValue::Int(v));
                Ok(())
            }
            Op::ConvertFToU => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let src_id = op_id(&inst.operands, 0)?;
                let src = self.lookup_value(src_id, values)?;
                let v = match src {
                    ConstantValue::F32(f) => {
                        if f.is_nan() || f < 0.0 { 0 }
                        else { f as u32 as i64 }
                    }
                    ConstantValue::F64(f) => {
                        if f.is_nan() || f < 0.0 { 0 }
                        else { f as u32 as i64 }
                    }
                    other => return Err(InterpError::UnsupportedOpcode(
                        format!("ConvertFToU on non-float: {other:?}"))),
                };
                values.insert(result_id, ConstantValue::Int(v));
                Ok(())
            }
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
                // Image / sampler / sampled-image load: not
                // a value-bearing memory access — it's a
                // descriptor handle. Resolve the variable's
                // (set, binding) and produce a Texture
                // value the OpImageSample* path will look up.
                let result_ty = self.types.get(&result_type_id).cloned();
                if matches!(result_ty,
                    Some(TypeInfo::Image { .. })
                    | Some(TypeInfo::Sampler)
                    | Some(TypeInfo::SampledImage { .. }))
                {
                    let (set, binding) = self.var_binding.get(&var_id)
                        .copied()
                        .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                            "Load of image/sampler variable {var_id} has no \
                             DescriptorSet+Binding decorations",
                        )))?;
                    values.insert(result_id,
                        ConstantValue::Texture { set, binding });
                    return Ok(());
                }
                let value = self.load_from_storage(
                    storage_class, off, result_type_id, inputs, inv_idx, is_vertex)?;
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
            // OpCompositeExtract: pull one scalar lane out
            // of a vector. Single-level vector index only
            // (matches the frontend's restriction).
            Op::CompositeExtract => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let composite_id = op_id(&inst.operands, 0)?;
                let composite = self.lookup_value(composite_id, values)?;
                // Remaining operands are literal indices.
                let indices: Vec<u32> = inst.operands[1..].iter()
                    .filter_map(|o| match o {
                        rspirv::dr::Operand::LiteralBit32(v) => Some(*v),
                        _ => None,
                    })
                    .collect();
                if indices.len() != 1 {
                    return Err(InterpError::UnsupportedOpcode(format!(
                        "CompositeExtract with {} indices; only single-level \
                         vector extract supported", indices.len())));
                }
                let lane = match &composite {
                    ConstantValue::Vec(lanes) => lanes
                        .get(indices[0] as usize)
                        .cloned()
                        .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                            "CompositeExtract index {} out of range ({} lanes)",
                            indices[0], lanes.len())))?,
                    other => return Err(InterpError::UnsupportedOpcode(format!(
                        "CompositeExtract of non-vector: {other:?}"))),
                };
                values.insert(result_id, lane);
                Ok(())
            }
            // OpSampledImage: combine an image handle and a
            // sampler handle. The interpreter's v1 model
            // keeps sampler config inside each
            // `TextureBinding`, so we propagate the image's
            // (set, binding) and ignore the sampler operand's
            // — the host-side test pairs them correctly.
            Op::SampledImage => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let img_id = op_id(&inst.operands, 0)?;
                let img = self.lookup_value(img_id, values)?;
                let handle = match img {
                    ConstantValue::Texture { set, binding } =>
                        ConstantValue::Texture { set, binding },
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "SampledImage image operand is not a Texture handle: \
                         {img:?}"))),
                };
                values.insert(result_id, handle);
                Ok(())
            }
            // OpImageSampleImplicitLod / ExplicitLod: filtered
            // texture sample. The sampled-image operand must be
            // a `Texture` handle; the coord operand must be a
            // vec2<f32>. Calls `atrium_spv_runtime::
            // atrium_tex_sample_2d` to keep the *exact same*
            // sampler implementation the production backends
            // will use — the differential tests the pipeline,
            // not the sampler.
            Op::ImageSampleImplicitLod | Op::ImageSampleExplicitLod
            | Op::ImageSampleProjImplicitLod
            | Op::ImageSampleProjExplicitLod
            | Op::ImageSampleDrefImplicitLod
            | Op::ImageSampleDrefExplicitLod
            | Op::ImageSampleProjDrefImplicitLod
            | Op::ImageSampleProjDrefExplicitLod => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let si_id = op_id(&inst.operands, 0)?;
                let coord_id = op_id(&inst.operands, 1)?;
                let handle = self.lookup_value(si_id, values)?;
                let (set, binding) = match handle {
                    ConstantValue::Texture { set, binding } => (set, binding),
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "ImageSample sampled-image operand is not a Texture handle: \
                         {handle:?}"))),
                };
                let coord = self.lookup_value(coord_id, values)?;
                let is_proj = matches!(inst.class.opcode,
                    Op::ImageSampleProjImplicitLod
                    | Op::ImageSampleProjExplicitLod
                    | Op::ImageSampleProjDrefImplicitLod
                    | Op::ImageSampleProjDrefExplicitLod);
                let is_dref = matches!(inst.class.opcode,
                    Op::ImageSampleDrefImplicitLod
                    | Op::ImageSampleDrefExplicitLod
                    | Op::ImageSampleProjDrefImplicitLod
                    | Op::ImageSampleProjDrefExplicitLod);
                let dref_val: Option<f32> = if is_dref {
                    // Dref's reference value is operand 2 (the
                    // operand right after coord); for ProjDref
                    // the same slot too -- SPIR-V puts it before
                    // the optional image-operands mask.
                    let d_id = op_id(&inst.operands, 2)?;
                    match self.lookup_value(d_id, values)? {
                        ConstantValue::F32(x) => Some(x),
                        other => return Err(InterpError::UnsupportedOpcode(
                            format!("Dref operand not f32: {other:?}"))),
                    }
                } else {
                    None
                };
                let (u, v) = match coord {
                    ConstantValue::Vec(ref lanes) if lanes.len() >= 2 => {
                        let f = |i: usize| -> Result<f32, InterpError> {
                            match &lanes[i] {
                                ConstantValue::F32(x) => Ok(*x),
                                other => Err(InterpError::UnsupportedOpcode(
                                    format!("ImageSample coord lane {i} not f32: \
                                             {other:?}"))),
                            }
                        };
                        let s = f(0)?;
                        let t = f(1)?;
                        if is_proj {
                            // Last lane is `q`; divide first N-1 by it.
                            let q = f(lanes.len() - 1)?;
                            (s / q, t / q)
                        } else {
                            (s, t)
                        }
                    }
                    other => return Err(InterpError::UnsupportedOpcode(format!(
                        "ImageSample coord operand not a vec<f32>: {other:?}"))),
                };
                let tex = inputs.textures.iter()
                    .find(|t| t.set == set && t.binding == binding)
                    .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                        "ImageSample: no TextureBinding for (set={set}, binding={binding})")))?;
                // Safe Rust wrapper — keeps the interpreter
                // crate `#![forbid(unsafe_code)]` clean while
                // sharing the *exact* sampler implementation
                // the production backends will FFI-call.
                let tex_desc = atrium_spv_runtime::TexDesc {
                    data:         std::ptr::null(),
                    width:        tex.width,
                    height:       tex.height,
                    stride_bytes: tex.stride_bytes,
                    format:       tex.format,
                    mip_count:    0,
                    mip_descs:    std::ptr::null(),
                    depth:        1,
                    slice_bytes:  0,
                };
                let out = atrium_spv_runtime::sample_2d(
                    &tex.data, &tex_desc, &tex.sampler, u, v);
                if let Some(d) = dref_val {
                    // Shadow compare: R <= dref -> 1.0 else 0.0.
                    let cmp = if out[0] <= d { 1.0_f32 } else { 0.0_f32 };
                    values.insert(result_id, ConstantValue::F32(cmp));
                } else {
                    values.insert(result_id, ConstantValue::Vec(vec![
                        ConstantValue::F32(out[0]),
                        ConstantValue::F32(out[1]),
                        ConstantValue::F32(out[2]),
                        ConstantValue::F32(out[3]),
                    ]));
                }
                Ok(())
            }
            // OpImageFetch: unfiltered integer-coord texel
            // load. Operand 0 is an *image* (not a sampled-
            // image — fetch doesn't use the sampler);
            // operand 1 is an ivec2 coord. Optional
            // Image Operands + Lod after; v1 ignores them
            // (the runtime's atrium_tex_fetch_2d takes lod
            // but doesn't read it yet).
            // OpImageQueryLod (Arc 38): derivative-free Tier-2
            // returns vec2(0, 0).
            Op::ImageQueryLod => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                values.insert(result_id, ConstantValue::Vec(vec![
                    ConstantValue::F32(0.0),
                    ConstantValue::F32(0.0),
                ]));
                return Ok(());
            }
            // OpImageQuerySamples (Arc 38): no MSAA -> 1.
            Op::ImageQuerySamples => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                values.insert(result_id, ConstantValue::Int(1));
                return Ok(());
            }
            Op::ImageFetch => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let img_id = op_id(&inst.operands, 0)?;
                let coord_id = op_id(&inst.operands, 1)?;
                let handle = self.lookup_value(img_id, values)?;
                let (set, binding) = match handle {
                    ConstantValue::Texture { set, binding } => (set, binding),
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "ImageFetch image operand is not a Texture handle: \
                         {handle:?}"))),
                };
                let coord = self.lookup_value(coord_id, values)?;
                let (mut x, mut y) = match coord {
                    ConstantValue::Vec(ref lanes) if lanes.len() >= 2 => {
                        let x = match &lanes[0] {
                            ConstantValue::Int(n) => *n as i32,
                            other => return Err(InterpError::UnsupportedOpcode(
                                format!("ImageFetch coord lane 0 not int: {other:?}"))),
                        };
                        let y = match &lanes[1] {
                            ConstantValue::Int(n) => *n as i32,
                            other => return Err(InterpError::UnsupportedOpcode(
                                format!("ImageFetch coord lane 1 not int: {other:?}"))),
                        };
                        (x, y)
                    }
                    other => return Err(InterpError::UnsupportedOpcode(format!(
                        "ImageFetch coord operand not an ivec2: {other:?}"))),
                };
                // Arc 41: Image-Operands::ConstOffset / Offset.
                // Operand 2 (if present) is the mask; the
                // offset id follows it.  Apply lane-wise.
                if let Some(rspirv::dr::Operand::ImageOperands(mask)) =
                    inst.operands.get(2)
                {
                    use rspirv::spirv::ImageOperands as IO;
                    if mask.contains(IO::CONST_OFFSET) || mask.contains(IO::OFFSET) {
                        // Skip past lower-bit args: Bias(+1), Lod(+1).
                        let mut idx = 3;
                        if mask.contains(IO::BIAS) { idx += 1; }
                        if mask.contains(IO::LOD)  { idx += 1; }
                        let off_id = match inst.operands.get(idx) {
                            Some(rspirv::dr::Operand::IdRef(id)) => *id,
                            other => return Err(InterpError::UnsupportedOpcode(
                                format!("ImageFetch offset operand: {other:?}"))),
                        };
                        let off = self.lookup_value(off_id, values)?;
                        if let ConstantValue::Vec(off_lanes) = off {
                            if off_lanes.len() >= 2 {
                                if let (ConstantValue::Int(ox), ConstantValue::Int(oy)) =
                                    (&off_lanes[0], &off_lanes[1])
                                {
                                    x += *ox as i32;
                                    y += *oy as i32;
                                }
                            }
                        }
                    }
                }
                let tex = inputs.textures.iter()
                    .find(|t| t.set == set && t.binding == binding)
                    .ok_or_else(|| InterpError::UnsupportedOpcode(format!(
                        "ImageFetch: no TextureBinding for (set={set}, binding={binding})")))?;
                let tex_desc = atrium_spv_runtime::TexDesc {
                    data:         std::ptr::null(),
                    width:        tex.width,
                    height:       tex.height,
                    stride_bytes: tex.stride_bytes,
                    format:       tex.format,
                    mip_count:    0,
                    mip_descs:    std::ptr::null(),
                    depth:        1,
                    slice_bytes:  0,
                };
                // v1: lod ignored. If the SPIR-V supplies a
                // Lod via Image Operands we still pass 0 —
                // the runtime helper's signature accepts it
                // but doesn't use it (mip-0 only).
                let out = atrium_spv_runtime::fetch_2d(
                    &tex.data, &tex_desc, x, y, 0);
                values.insert(result_id, ConstantValue::Vec(vec![
                    ConstantValue::F32(out[0]),
                    ConstantValue::F32(out[1]),
                    ConstantValue::F32(out[2]),
                    ConstantValue::F32(out[3]),
                ]));
                Ok(())
            }
            // OpMatrixTimesVector: column-major mat × col
            // vector. Result lane `i = Σ matrix[j][i] *
            // vector[j]`. Matrix stored as Vec-of-Vecs
            // (one column per outer entry, one lane per
            // inner). Vector stored as Vec of lanes. v1
            // assumes f32 throughout.
            Op::MatrixTimesVector => {
                let result_id = inst.result_id.ok_or_else(||
                    InterpError::BadConstant(0))?;
                let mat_id = op_id(&inst.operands, 0)?;
                let vec_id = op_id(&inst.operands, 1)?;
                let mat = self.lookup_value(mat_id, values)?;
                let vec = self.lookup_value(vec_id, values)?;
                let columns: Vec<Vec<f32>> = match mat {
                    ConstantValue::Vec(cols) => cols.iter().map(|c| match c {
                        ConstantValue::Vec(lanes) => lanes.iter().map(|l| match l {
                            ConstantValue::F32(x) => Ok(*x),
                            other => Err(InterpError::UnsupportedOpcode(format!(
                                "MatrixTimesVector: matrix lane not f32: {other:?}"))),
                        }).collect::<Result<Vec<f32>, _>>(),
                        other => Err(InterpError::UnsupportedOpcode(format!(
                            "MatrixTimesVector: matrix column not a Vec: {other:?}"))),
                    }).collect::<Result<Vec<Vec<f32>>, _>>()?,
                    other => return Err(InterpError::UnsupportedOpcode(format!(
                        "MatrixTimesVector: matrix not a Vec-of-Vecs: {other:?}"))),
                };
                let v: Vec<f32> = match vec {
                    ConstantValue::Vec(lanes) => lanes.iter().map(|l| match l {
                        ConstantValue::F32(x) => Ok(*x),
                        other => Err(InterpError::UnsupportedOpcode(format!(
                            "MatrixTimesVector: vector lane not f32: {other:?}"))),
                    }).collect::<Result<Vec<f32>, _>>()?,
                    other => return Err(InterpError::UnsupportedOpcode(format!(
                        "MatrixTimesVector: vector not a Vec: {other:?}"))),
                };
                if columns.len() != v.len() {
                    return Err(InterpError::UnsupportedOpcode(format!(
                        "MatrixTimesVector: {} columns × {}-lane vector mismatch",
                        columns.len(), v.len())));
                }
                let n_lanes = columns[0].len();
                let mut result = vec![0.0f32; n_lanes];
                for j in 0..columns.len() {
                    let vj = v[j];
                    for i in 0..n_lanes {
                        result[i] += columns[j][i] * vj;
                    }
                }
                values.insert(result_id, ConstantValue::Vec(
                    result.into_iter().map(ConstantValue::F32).collect()));
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
    /// Evaluate a scalar integer binary op. Both operands
    /// must be ConstantValue::Int; result is Int.
    fn eval_binop_int(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        op: impl Fn(i64, i64) -> i64,
    ) -> Result<(), InterpError> {
        let result_id = inst.result_id.ok_or_else(||
            InterpError::BadConstant(0))?;
        let lhs_id = op_id(&inst.operands, 0)?;
        let rhs_id = op_id(&inst.operands, 1)?;
        let lhs = self.lookup_value(lhs_id, values)?;
        let rhs = self.lookup_value(rhs_id, values)?;
        let (la, lb) = match (&lhs, &rhs) {
            (ConstantValue::Int(a), ConstantValue::Int(b)) => (*a, *b),
            _ => return Err(InterpError::UnsupportedOpcode(format!(
                "int binop on non-int: {lhs:?}, {rhs:?}",
            ))),
        };
        values.insert(result_id, ConstantValue::Int(op(la, lb)));
        Ok(())
    }

    /// Evaluate a scalar integer unary op (SNegate).
    fn eval_unop_int(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        op: impl Fn(i64) -> i64,
    ) -> Result<(), InterpError> {
        let result_id = inst.result_id.ok_or_else(||
            InterpError::BadConstant(0))?;
        let src_id = op_id(&inst.operands, 0)?;
        let src = self.lookup_value(src_id, values)?;
        let a = match src {
            ConstantValue::Int(v) => v,
            other => return Err(InterpError::UnsupportedOpcode(format!(
                "int unop on non-int: {other:?}"))),
        };
        values.insert(result_id, ConstantValue::Int(op(a)));
        Ok(())
    }

    /// Evaluate a scalar integer comparison; result is Bool.
    fn eval_icmp(
        &self,
        inst: &Instruction,
        values: &mut HashMap<Word, ConstantValue>,
        cmp: impl Fn(i64, i64) -> bool,
    ) -> Result<(), InterpError> {
        let result_id = inst.result_id.ok_or_else(||
            InterpError::BadConstant(0))?;
        let lhs_id = op_id(&inst.operands, 0)?;
        let rhs_id = op_id(&inst.operands, 1)?;
        let lhs = self.lookup_value(lhs_id, values)?;
        let rhs = self.lookup_value(rhs_id, values)?;
        let (la, lb) = match (&lhs, &rhs) {
            (ConstantValue::Int(a), ConstantValue::Int(b)) => (*a, *b),
            _ => return Err(InterpError::UnsupportedOpcode(format!(
                "icmp on non-int: {lhs:?}, {rhs:?}",
            ))),
        };
        values.insert(result_id, ConstantValue::Bool(cmp(la, lb)));
        Ok(())
    }

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
        inv_idx: usize,
        is_vertex: bool,
    ) -> Result<ConstantValue, InterpError> {
        // Pick the source buffer.
        let empty: [u8; 0] = [];
        let buf: &[u8] = match storage_class {
            StorageClass::PushConstant => &inputs.push_constants[..],
            StorageClass::Uniform | StorageClass::UniformConstant
            | StorageClass::StorageBuffer => &inputs.uniforms[..],
            // Input loads dispatch by stage:
            //   * Vertex Input  → per-vertex attribute
            //     bytes (`vertex_attributes_per_invocation`).
            //   * Fragment Input → interpolated varying
            //     bytes (`varyings_per_invocation`) —
            //     produced upstream by the vertex stage,
            //     or packed directly by the test harness
            //     when not running a real rasterizer.
            StorageClass::Input => {
                let src = if is_vertex {
                    &inputs.vertex_attributes_per_invocation
                } else {
                    &inputs.varyings_per_invocation
                };
                src.get(inv_idx).map(|v| v.as_slice()).unwrap_or(&empty)
            }
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
                        storage_class, off, element, inputs, inv_idx, is_vertex)?;
                    lanes.push(v);
                }
                Ok(ConstantValue::Vec(lanes))
            }
            TypeInfo::Matrix { column, count } => {
                // SPIR-V's matrix stride defaults to the
                // column type's natural size (16 bytes for
                // a vec4). v1 assumes the canonical layout —
                // 4 columns of vec4<f32> = 64 bytes total.
                // Each column loaded recursively as a
                // Vector → ConstantValue::Vec; the matrix
                // ends up as a Vec-of-Vecs.
                let col_info = self.types.get(&column)
                    .ok_or(InterpError::BadType(column))?
                    .clone();
                let col_stride: u32 = match col_info {
                    TypeInfo::Vector { count: lanes, .. } => lanes * 4,
                    _ => return Err(InterpError::UnsupportedOpcode(format!(
                        "Load of matrix with non-vector column {col_info:?}"))),
                };
                let mut columns = Vec::with_capacity(count as usize);
                for i in 0..count {
                    let off = byte_offset.saturating_add(i * col_stride);
                    let c = self.load_from_storage(
                        storage_class, off, column, inputs, inv_idx, is_vertex)?;
                    columns.push(c);
                }
                Ok(ConstantValue::Vec(columns))
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
                    ConstantValue::Texture { .. } => return Err(InterpError::UnsupportedOutput(
                        "texture handle in output".to_string(),
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
