//! Bespoke x86_64 backend for atrium-spv tier-2.
//!
//! The x86 twin of `atrium-spv-backend-bespoke` (arm64): atrium-spv-ir
//! straight to x86-64-v3 machine code, with no Cranelift in the middle. The
//! plan, and the reasons for each choice below, are in
//! `docs/spec/tier2-bespoke-x86.md`.
//!
//! * **Encoder.** iced-x86's `code_asm`. It is a full x86-64 assembler with
//!   labels and branch relaxation, so this crate does instruction selection
//!   and register allocation only. Every emitted function is decoded again
//!   in debug builds (see [`roundtrip_check`]).
//! * **ISA.** x86-64-v3: scalar float uses the 3-operand VEX forms (`vaddss`,
//!   `vfmadd213ss`, `vroundss`), so no copies are needed to keep a source
//!   alive, and there are no SSE/AVX transition stalls. All XMM use is
//!   128-bit, so the upper halves are never dirty and no `vzeroupper` is
//!   needed.
//! * **ABI.** SysV AMD64 with the loader's signatures
//!   (`atrium_spv_loader::{FsMain, ...}`).
//! * **Never silently wrong.** Anything outside the supported subset returns
//!   [`BackendError::Unsupported`], and `atrium-spv-compile` then uses
//!   Cranelift for that shader.
//!
//! # Phase 1 (this file)
//!
//! Fragment shaders that are a single straight-line block of f32 math:
//! constants, + - * /, fma, neg, abs, min, max, sqrt, floor/ceil/trunc, dot,
//! vector construct/extract/insert/shuffle, loads from varyings, uniforms and
//! push constants, `gl_FragCoord`, and stores to the colour output. Vectors are
//! held one lane per XMM register. Registers are freed at each value's last
//! use, and a register can be shared by several values (an extracted lane, a
//! shuffled vector). Running out of XMM registers is `Unsupported` until the
//! phase-2 spiller lands.

use std::collections::HashMap;

use atrium_spv_ir::{
    BuiltinKind, FloatKind, Function, Inst, Module, Op, ShaderStage, StorageClass, Type, Value,
    ValueId, VecElement,
};
use iced_x86::code_asm::*;
use iced_x86::BlockEncoderOptions;
use thiserror::Error;

/// What went wrong compiling a module.
#[derive(Debug, Error)]
pub enum BackendError {
    /// The IR uses an opcode or shape this backend doesn't support yet.
    /// The driver falls back to Cranelift.
    #[error("bespoke-x86 backend doesn't support: {0}")]
    Unsupported(String),
    /// Anything else (encoder failure, internal invariant violation).
    #[error("internal error: {0}")]
    Internal(String),
}

fn unsupported<T>(msg: impl Into<String>) -> Result<T, BackendError> {
    Err(BackendError::Unsupported(msg.into()))
}

/// Target the produced object file is for. The machine code is identical
/// for both; only the object format's OS ABI differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// FreeBSD/amd64.
    X86_64FreeBSD,
    /// Linux/x86_64 (the R9700 box, CI).
    X86_64Linux,
}

impl Target {
    /// The host target. Non-FreeBSD hosts get `X86_64Linux`.
    pub fn host() -> Self {
        if cfg!(target_os = "freebsd") { Target::X86_64FreeBSD } else { Target::X86_64Linux }
    }
}

/// Result of [`compile`], field-for-field like the other backends'.
#[derive(Debug, Clone)]
pub struct CompileOutput {
    /// ELF object bytes, ready for `cc -shared`.
    pub object: Vec<u8>,
    /// PC-map sidecar (atrium-spv-pcmap v1).
    pub pcmap: Vec<u8>,
}

/// Result of [`compile_blob`].
#[derive(Debug, Clone)]
pub struct BlobOutput {
    /// Serialised `atrium-spv-blob` container (flat PIC code + entries).
    pub blob: Vec<u8>,
    /// PC-map sidecar (atrium-spv-pcmap v1).
    pub pcmap: Vec<u8>,
}

/// Compile a module to an ELF object with exported `atrium_*_main` symbols.
pub fn compile(module: &Module, _target: Target) -> Result<CompileOutput, BackendError> {
    let mut obj = object::write::Object::new(
        object::BinaryFormat::Elf,
        object::Architecture::X86_64,
        object::Endianness::Little,
    );
    let text = obj.section_id(object::write::StandardSection::Text);
    // Non-executable stack marker, so linking the object doesn't
    // make the whole shared library ask for an executable stack.
    obj.add_section(Vec::new(), b".note.GNU-stack".to_vec(), object::SectionKind::Elf(object::elf::SHT_PROGBITS));
    let mut pcmap = atrium_spv_pcmap::Builder::new();
    for func in &module.functions {
        let (body, pcs) = emit_function(func)?;
        let sym = obj.add_symbol(object::write::Symbol {
            name: exported_symbol_name(func).into_bytes(),
            value: 0,
            size: body.len() as u64,
            kind: object::SymbolKind::Text,
            scope: object::SymbolScope::Dynamic,
            weak: false,
            section: object::write::SymbolSection::Section(text),
            flags: object::SymbolFlags::None,
        });
        let off = obj.add_symbol_data(sym, text, &body, 16);
        for (rel, spirv) in pcs {
            pcmap.push(off as u32 + rel, spirv);
        }
    }
    let object = obj
        .write()
        .map_err(|e| BackendError::Internal(format!("object write: {e}")))?;
    Ok(CompileOutput { object, pcmap: pcmap.finish_to_bytes() })
}

/// Compile a module straight to a flat executable blob (no ELF, no linker).
pub fn compile_blob(module: &Module, _target: Target) -> Result<BlobOutput, BackendError> {
    let mut code: Vec<u8> = Vec::new();
    let mut pcmap = atrium_spv_pcmap::Builder::new();
    let mut entries = atrium_spv_blob::EntryOffsets::default();
    for func in &module.functions {
        // 16-byte align each entry (int3 padding).
        while code.len() % 16 != 0 {
            code.push(0xCC);
        }
        let (body, pcs) = emit_function(func)?;
        let off = code.len() as u32;
        for (rel, spirv) in pcs {
            pcmap.push(off + rel, spirv);
        }
        match func.stage {
            ShaderStage::Vertex => entries.vs = Some(off),
            ShaderStage::Fragment => entries.fs = Some(off),
            ShaderStage::Compute => entries.cs = Some(off),
        }
        code.extend_from_slice(&body);
    }
    let blob = atrium_spv_blob::ShaderBlob { arch: atrium_spv_blob::ARCH_X86_64, code, entries };
    Ok(BlobOutput { blob: blob.to_bytes(), pcmap: pcmap.finish_to_bytes() })
}

fn exported_symbol_name(func: &Function) -> String {
    match func.stage {
        ShaderStage::Vertex => "atrium_vs_main".to_string(),
        ShaderStage::Fragment => "atrium_fs_main".to_string(),
        ShaderStage::Compute => "atrium_cs_main".to_string(),
    }
}

// ── Registers ─────────────────────────────────────────────────────────────

const XMM: [AsmRegisterXmm; 16] = [
    xmm0, xmm1, xmm2, xmm3, xmm4, xmm5, xmm6, xmm7, xmm8, xmm9, xmm10, xmm11, xmm12, xmm13,
    xmm14, xmm15,
];

/// A pointer: a base GPR holding an ABI pointer argument, plus a constant
/// byte offset folded in from access chains.
#[derive(Clone, Copy, Debug)]
struct Ptr {
    base: AsmRegister64,
    off: i32,
}

/// XMM register file with sharing: `refs[r]` counts how many live values
/// hold register `r` (an extracted lane shares its vector's register).
struct XmmFile {
    refs: [u32; 16],
}

impl XmmFile {
    fn alloc(&mut self) -> Result<u8, BackendError> {
        // Prefer the high registers so xmm0..3 (frag coord on entry) are
        // taken last.
        for r in (0..16u8).rev() {
            if self.refs[r as usize] == 0 {
                self.refs[r as usize] = 1;
                return Ok(r);
            }
        }
        unsupported("ran out of XMM registers (spilling lands in phase 2)")
    }
    fn retain(&mut self, r: u8) {
        self.refs[r as usize] += 1;
    }
    fn release(&mut self, r: u8) {
        debug_assert!(self.refs[r as usize] > 0);
        self.refs[r as usize] -= 1;
    }
}

// ── Function emission ─────────────────────────────────────────────────────

struct Emitter<'f> {
    func: &'f Function,
    a: CodeAssembler,
    xmm: XmmFile,
    /// f32 scalars and f32 vectors: one XMM register per lane.
    lanes: HashMap<ValueId, Vec<u8>>,
    ptrs: HashMap<ValueId, Ptr>,
    /// Instruction index (within the single block) of each value's last use.
    last_use: HashMap<ValueId, usize>,
}

/// Lower one function. Returns its machine code and the PC-map entries
/// `(body offset, SPIR-V offset)`, one per IR instruction.
fn emit_function(func: &Function) -> Result<(Vec<u8>, Vec<(u32, u32)>), BackendError> {
    if func.stage != ShaderStage::Fragment {
        return unsupported(format!("{:?} stage (phase 1 is fragment only)", func.stage));
    }
    if func.blocks.len() != 1 {
        return unsupported("control flow (more than one block; phase 3)");
    }
    let block = func
        .blocks
        .get(&func.entry_block)
        .ok_or_else(|| BackendError::Internal("entry block missing".into()))?;

    let mut last_use = HashMap::new();
    for (i, inst) in block.insts.iter().enumerate() {
        for v in operands(&inst.op) {
            last_use.insert(v, i);
        }
    }

    let mut e = Emitter {
        func,
        a: CodeAssembler::new(64).map_err(enc)?,
        // xmm0..3 hold frag_coord.{x,y,z,w} on entry; they are reserved only
        // if the shader reads gl_FragCoord (released at its last read).
        xmm: XmmFile { refs: [0; 16] },
        lanes: HashMap::new(),
        ptrs: HashMap::new(),
        last_use,
    };
    let frag_coord_used = block
        .insts
        .iter()
        .any(|i| matches!(i.op, Op::LoadBuiltin(BuiltinKind::FragCoord)));
    if frag_coord_used {
        for r in 0..4 {
            e.xmm.retain(r);
        }
    }
    let last_frag_coord = block
        .insts
        .iter()
        .rposition(|i| matches!(i.op, Op::LoadBuiltin(BuiltinKind::FragCoord)));

    let mut marks: Vec<(usize, u32)> = Vec::with_capacity(block.insts.len());
    for (i, inst) in block.insts.iter().enumerate() {
        marks.push((e.a.instructions().len(), inst.source_spirv_offset));
        e.lower(inst)?;
        if Some(i) == last_frag_coord {
            // The ABI's own hold on xmm0..3 ends here; values loaded from
            // gl_FragCoord keep their own references.
            for r in 0..4 {
                e.xmm.release(r);
            }
        }
        // Free everything whose last use was this instruction.
        for v in operands(&inst.op) {
            if e.last_use.get(&v) == Some(&i) {
                e.kill(v);
            }
        }
        // A result nobody reads dies immediately.
        if let Some(r) = &inst.result {
            if !e.last_use.contains_key(&r.id) {
                e.kill(r.id);
            }
        }
    }

    let result = e
        .a
        .assemble_options(0, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS)
        .map_err(enc)?;
    let offsets = result.inner.new_instruction_offsets;
    let code = result.inner.code_buffer;
    let pcs = marks
        .into_iter()
        .map(|(idx, spirv)| {
            let off = offsets.get(idx).copied().unwrap_or(code.len() as u32);
            (off, spirv)
        })
        .collect();
    if cfg!(debug_assertions) {
        roundtrip_check(&code)?;
    }
    Ok((code, pcs))
}

fn enc(e: IcedError) -> BackendError {
    BackendError::Internal(format!("iced-x86: {e}"))
}

/// Decode `code` again and require every byte to form a valid instruction.
/// Catches encoder misuse (an operand combination iced accepted but that
/// means something else) before the code ever runs.
pub fn roundtrip_check(code: &[u8]) -> Result<(), BackendError> {
    let mut d = iced_x86::Decoder::new(64, code, iced_x86::DecoderOptions::NONE);
    while d.can_decode() {
        let ins = d.decode();
        if ins.is_invalid() {
            return Err(BackendError::Internal(format!(
                "round-trip decode failed at offset {:#x}",
                d.position() - ins.len()
            )));
        }
    }
    Ok(())
}

fn is_f32(ty: &Type) -> bool {
    matches!(ty, Type::F32)
}

fn f32_lanes(ty: &Type) -> Option<usize> {
    match ty {
        Type::F32 => Some(1),
        Type::Vec2(VecElement::F32) => Some(2),
        Type::Vec3(VecElement::F32) => Some(3),
        Type::Vec4(VecElement::F32) => Some(4),
        _ => None,
    }
}

impl<'f> Emitter<'f> {
    fn kill(&mut self, v: ValueId) {
        if let Some(regs) = self.lanes.remove(&v) {
            for r in regs {
                self.xmm.release(r);
            }
        }
        self.ptrs.remove(&v);
    }

    fn lanes_of(&self, v: &Value) -> Result<Vec<u8>, BackendError> {
        self.lanes
            .get(&v.id)
            .cloned()
            .ok_or_else(|| BackendError::Internal(format!("value {:?} has no register", v.id)))
    }

    /// Bind `result` to existing registers (aliasing: extract/shuffle/construct).
    fn bind_alias(&mut self, result: &Value, regs: Vec<u8>) {
        for &r in &regs {
            self.xmm.retain(r);
        }
        self.lanes.insert(result.id, regs);
    }

    fn fresh(&mut self, n: usize) -> Result<Vec<u8>, BackendError> {
        (0..n).map(|_| self.xmm.alloc()).collect()
    }

    fn ptr_of(&mut self, v: &Value) -> Result<Ptr, BackendError> {
        if let Some(p) = self.ptrs.get(&v.id) {
            return Ok(*p);
        }
        // A bare Variable: map its storage class to the ABI argument.
        let sc = match &v.ty {
            Type::Pointer(sc, _) => *sc,
            other => return unsupported(format!("pointer of type {other:?}")),
        };
        // FsMain (SysV): rdi in_varyings, rsi uniforms, rdx push_constants,
        // xmm0-3 frag_coord, ecx samples_mask, r8 out_color, r9 out_depth,
        // [rsp+8] front_facing, [rsp+16] primitive_id.
        let base = match sc {
            StorageClass::Input => rdi,
            StorageClass::Uniform => rsi,
            StorageClass::PushConstant => rdx,
            StorageClass::Output => r8,
            other => return unsupported(format!("fragment storage class {other:?}")),
        };
        let p = Ptr { base, off: 0 };
        self.ptrs.insert(v.id, p);
        Ok(p)
    }

    fn result<'i>(&self, inst: &'i Inst) -> Result<&'i Value, BackendError> {
        inst.result
            .as_ref()
            .ok_or_else(|| BackendError::Internal(format!("{:?} without result", inst.op)))
    }

    /// Lane-wise binary op on f32 scalars/vectors.
    fn binop(
        &mut self,
        inst: &Inst,
        a: &Value,
        b: &Value,
        f: fn(&mut CodeAssembler, AsmRegisterXmm, AsmRegisterXmm, AsmRegisterXmm) -> Result<(), IcedError>,
    ) -> Result<(), BackendError> {
        let res = self.result(inst)?.clone();
        let n = f32_lanes(&res.ty).ok_or_else(|| BackendError::Unsupported(format!("{:?} on {:?}", inst.op, res.ty)))?;
        let mut la = self.lanes_of(a)?;
        let mut lb = self.lanes_of(b)?;
        // VectorTimesScalar and friends: broadcast the scalar side.
        if la.len() == 1 && n > 1 { la = vec![la[0]; n]; }
        if lb.len() == 1 && n > 1 { lb = vec![lb[0]; n]; }
        if la.len() != n || lb.len() != n {
            return unsupported(format!("{:?}: mixed lane counts", inst.op));
        }
        let d = self.fresh(n)?;
        for i in 0..n {
            f(&mut self.a, XMM[d[i] as usize], XMM[la[i] as usize], XMM[lb[i] as usize]).map_err(enc)?;
        }
        self.lanes.insert(res.id, d);
        Ok(())
    }

    fn unop(
        &mut self,
        inst: &Inst,
        a: &Value,
        f: &dyn Fn(&mut CodeAssembler, AsmRegisterXmm, AsmRegisterXmm) -> Result<(), IcedError>,
    ) -> Result<(), BackendError> {
        let res = self.result(inst)?.clone();
        let n = f32_lanes(&res.ty).ok_or_else(|| BackendError::Unsupported(format!("{:?} on {:?}", inst.op, res.ty)))?;
        let la = self.lanes_of(a)?;
        if la.len() != n {
            return unsupported(format!("{:?}: lane count", inst.op));
        }
        let d = self.fresh(n)?;
        for i in 0..n {
            f(&mut self.a, XMM[d[i] as usize], XMM[la[i] as usize]).map_err(enc)?;
        }
        self.lanes.insert(res.id, d);
        Ok(())
    }

    /// Materialise a 32-bit pattern in an XMM register (via eax).
    fn const_bits(&mut self, dst: u8, bits: u32) -> Result<(), BackendError> {
        let x = XMM[dst as usize];
        if bits == 0 {
            self.a.vxorps(x, x, x).map_err(enc)?;
        } else {
            self.a.mov(eax, bits as i32).map_err(enc)?;
            self.a.vmovd(x, eax).map_err(enc)?;
        }
        Ok(())
    }

    /// `dst = src OP mask` with a broadcast-free scalar mask in a temp.
    fn mask_op(&mut self, inst: &Inst, a: &Value, bits: u32, and: bool) -> Result<(), BackendError> {
        let res = self.result(inst)?.clone();
        let n = f32_lanes(&res.ty).ok_or_else(|| BackendError::Unsupported(format!("{:?} on {:?}", inst.op, res.ty)))?;
        let la = self.lanes_of(a)?;
        let t = self.xmm.alloc()?;
        self.const_bits(t, bits)?;
        let d = self.fresh(n)?;
        for i in 0..n {
            let (xd, xs, xt) = (XMM[d[i] as usize], XMM[la[i] as usize], XMM[t as usize]);
            if and {
                self.a.vandps(xd, xs, xt).map_err(enc)?;
            } else {
                self.a.vxorps(xd, xs, xt).map_err(enc)?;
            }
        }
        self.xmm.release(t);
        self.lanes.insert(res.id, d);
        Ok(())
    }

    fn lower(&mut self, inst: &Inst) -> Result<(), BackendError> {
        match &inst.op {
            Op::ConstFloat { value, kind: FloatKind::F32 } => {
                let res = self.result(inst)?.clone();
                let d = self.xmm.alloc()?;
                self.const_bits(d, (*value as f32).to_bits())?;
                self.lanes.insert(res.id, vec![d]);
            }
            Op::ConstVec(parts) => {
                let res = self.result(inst)?.clone();
                if f32_lanes(&res.ty).is_none() {
                    return unsupported(format!("ConstVec of {:?}", res.ty));
                }
                let mut regs = Vec::new();
                for p in parts {
                    regs.extend(self.lanes_of(p)?);
                }
                self.bind_alias(&res, regs);
            }
            Op::FAdd(a, b) => self.binop(inst, a, b, |c, d, x, y| c.vaddss(d, x, y))?,
            Op::FSub(a, b) => self.binop(inst, a, b, |c, d, x, y| c.vsubss(d, x, y))?,
            Op::FMul(a, b) => self.binop(inst, a, b, |c, d, x, y| c.vmulss(d, x, y))?,
            Op::FDiv(a, b) => self.binop(inst, a, b, |c, d, x, y| c.vdivss(d, x, y))?,
            Op::FMin(a, b) => self.binop(inst, a, b, |c, d, x, y| c.vminss(d, x, y))?,
            Op::FMax(a, b) => self.binop(inst, a, b, |c, d, x, y| c.vmaxss(d, x, y))?,
            Op::Fma(a, b, c) => {
                // d = a*b + c: copy a into d, then vfmadd213ss d, b, c.
                let res = self.result(inst)?.clone();
                let n = f32_lanes(&res.ty).ok_or_else(|| BackendError::Unsupported(format!("Fma on {:?}", res.ty)))?;
                let (la, lb, lc) = (self.lanes_of(a)?, self.lanes_of(b)?, self.lanes_of(c)?);
                if la.len() != n || lb.len() != n || lc.len() != n {
                    return unsupported("Fma: lane counts");
                }
                let d = self.fresh(n)?;
                for i in 0..n {
                    let xd = XMM[d[i] as usize];
                    self.a.vmovaps(xd, XMM[la[i] as usize]).map_err(enc)?;
                    self.a
                        .vfmadd213ss(xd, XMM[lb[i] as usize], XMM[lc[i] as usize])
                        .map_err(enc)?;
                }
                self.lanes.insert(res.id, d);
            }
            Op::FNeg(a) => self.mask_op(inst, a, 0x8000_0000, false)?,
            Op::FAbs(a) => self.mask_op(inst, a, 0x7fff_ffff, true)?,
            Op::FSqrt(a) => self.unop(inst, a, &|c, d, x| c.vsqrtss(d, x, x))?,
            // vroundss imm: 8 = suppress precision exception, | 1 floor,
            // 2 ceil, 3 trunc.
            Op::FFloor(a) => self.unop(inst, a, &|c, d, x| c.vroundss(d, x, x, 9))?,
            Op::FCeil(a) => self.unop(inst, a, &|c, d, x| c.vroundss(d, x, x, 10))?,
            Op::FTrunc(a) => self.unop(inst, a, &|c, d, x| c.vroundss(d, x, x, 11))?,
            Op::Dot(a, b) => {
                let res = self.result(inst)?.clone();
                if !is_f32(&res.ty) {
                    return unsupported(format!("Dot -> {:?}", res.ty));
                }
                let (la, lb) = (self.lanes_of(a)?, self.lanes_of(b)?);
                if la.len() != lb.len() || la.is_empty() {
                    return unsupported("Dot: lane counts");
                }
                let d = self.xmm.alloc()?;
                let xd = XMM[d as usize];
                self.a.vmulss(xd, XMM[la[0] as usize], XMM[lb[0] as usize]).map_err(enc)?;
                // Separate multiply and add, not FMA: the interpreter and
                // Cranelift round after each step and the tests compare
                // bit-exactly.
                if la.len() > 1 {
                    let t = self.xmm.alloc()?;
                    let xt = XMM[t as usize];
                    for i in 1..la.len() {
                        self.a.vmulss(xt, XMM[la[i] as usize], XMM[lb[i] as usize]).map_err(enc)?;
                        self.a.vaddss(xd, xd, xt).map_err(enc)?;
                    }
                    self.xmm.release(t);
                }
                self.lanes.insert(res.id, vec![d]);
            }
            Op::VectorExtract { vector, index } => {
                let res = self.result(inst)?.clone();
                let l = self.lanes_of(vector)?;
                let r = *l
                    .get(*index as usize)
                    .ok_or_else(|| BackendError::Internal("VectorExtract index".into()))?;
                self.bind_alias(&res, vec![r]);
            }
            Op::VectorInsert { vector, scalar, index } => {
                let res = self.result(inst)?.clone();
                let mut l = self.lanes_of(vector)?;
                let s = self.lanes_of(scalar)?;
                let slot = l
                    .get_mut(*index as usize)
                    .ok_or_else(|| BackendError::Internal("VectorInsert index".into()))?;
                *slot = s[0];
                self.bind_alias(&res, l);
            }
            Op::VectorShuffle { src1, src2, components } => {
                let res = self.result(inst)?.clone();
                let (l1, l2) = (self.lanes_of(src1)?, self.lanes_of(src2)?);
                let mut regs = Vec::new();
                for &c in components {
                    let c = c as usize;
                    let r = if c < l1.len() {
                        l1[c]
                    } else if c - l1.len() < l2.len() {
                        l2[c - l1.len()]
                    } else {
                        return unsupported("VectorShuffle undefined component");
                    };
                    regs.push(r);
                }
                self.bind_alias(&res, regs);
            }
            Op::LoadBuiltin(BuiltinKind::FragCoord) => {
                let res = self.result(inst)?.clone();
                self.bind_alias(&res, vec![0, 1, 2, 3]);
            }
            Op::AccessChain { base, byte_offset } => {
                let res = self.result(inst)?.clone();
                let p = self.ptr_of(base)?;
                let off = p
                    .off
                    .checked_add(*byte_offset as i32)
                    .ok_or_else(|| BackendError::Unsupported("AccessChain offset overflow".into()))?;
                self.ptrs.insert(res.id, Ptr { base: p.base, off });
            }
            Op::Load(ptr) => {
                let res = self.result(inst)?.clone();
                let n = f32_lanes(&res.ty)
                    .ok_or_else(|| BackendError::Unsupported(format!("Load of {:?}", res.ty)))?;
                let p = self.ptr_of(ptr)?;
                let d = self.fresh(n)?;
                for (i, &r) in d.iter().enumerate() {
                    self.a
                        .vmovss(XMM[r as usize], dword_ptr(p.base + p.off + 4 * i as i32))
                        .map_err(enc)?;
                }
                self.lanes.insert(res.id, d);
            }
            Op::Store { ptr, value } => {
                if f32_lanes(&value.ty).is_none() {
                    return unsupported(format!("Store of {:?}", value.ty));
                }
                let p = self.ptr_of(ptr)?;
                if p.base != r8 {
                    return unsupported("fragment store to a non-colour output");
                }
                let l = self.lanes_of(value)?;
                for (i, &r) in l.iter().enumerate() {
                    self.a
                        .vmovss(dword_ptr(p.base + p.off + 4 * i as i32), XMM[r as usize])
                        .map_err(enc)?;
                }
            }
            Op::Return => {
                self.a.ret().map_err(enc)?;
            }
            other => return unsupported(format!("{}", op_name(other))),
        }
        // Any result that isn't a pointer or f32 lanes was not produced.
        let _ = self.func;
        Ok(())
    }
}

/// The IR values an op reads (pointers included).
fn operands(op: &Op) -> Vec<ValueId> {
    let mut v: Vec<ValueId> = Vec::new();
    let mut p = |x: &Value| v.push(x.id);
    match op {
        Op::ConstVec(parts) => parts.iter().for_each(&mut p),
        Op::FAdd(a, b) | Op::FSub(a, b) | Op::FMul(a, b) | Op::FDiv(a, b) | Op::FMin(a, b)
        | Op::FMax(a, b) | Op::Dot(a, b) => {
            p(a);
            p(b);
        }
        Op::Fma(a, b, c) => {
            p(a);
            p(b);
            p(c);
        }
        Op::FNeg(a) | Op::FAbs(a) | Op::FSqrt(a) | Op::FFloor(a) | Op::FCeil(a)
        | Op::FTrunc(a) | Op::Load(a) => p(a),
        Op::VectorExtract { vector, .. } => p(vector),
        Op::VectorInsert { vector, scalar, .. } => {
            p(vector);
            p(scalar);
        }
        Op::VectorShuffle { src1, src2, .. } => {
            p(src1);
            p(src2);
        }
        Op::AccessChain { base, .. } => p(base),
        Op::Store { ptr, value } => {
            p(ptr);
            p(value);
        }
        _ => {}
    }
    v
}

fn op_name(op: &Op) -> String {
    let s = format!("{op:?}");
    s.split(|c: char| c == '(' || c == ' ' || c == '{').next().unwrap_or("?").to_string()
}
