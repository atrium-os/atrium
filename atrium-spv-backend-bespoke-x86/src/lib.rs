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
//! # Pipeline
//!
//! 1. [`vcode`]: IR → VCode, x86-shaped instructions over virtual registers,
//!    one per vector lane; phis become edge copies.
//! 2. [`regalloc`]: block liveness, live intervals, linear scan with spill
//!    slots over the XMM and GPR files.
//! 3. [`emit`]: VCode + allocation → machine code via iced-x86.
//!
//! # Supported so far (phases 1-3)
//!
//! Fragment shaders with any control flow (branches, switch, loops, phis):
//! f32 math (+ - * /, fma, neg, abs, min, max, sqrt, floor/ceil/trunc, dot),
//! i32/u32 math (+ - *, div/mod with safe zero and overflow cases, bitwise,
//! shifts, neg, not, clz), integer and float compares, select, int↔float
//! conversions (saturating), bitcasts, vector construct/extract/insert/
//! shuffle, loads from varyings, uniforms and push constants, `gl_FragCoord`,
//! and stores to the colour output.

mod emit;
mod regalloc;
mod vcode;

use atrium_spv_ir::{Function, Module, ShaderStage};
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

// ── Function emission ─────────────────────────────────────────────────────

/// Lower one function. Returns its machine code and the PC-map entries
/// `(body offset, SPIR-V offset)`.
fn emit_function(func: &Function) -> Result<(Vec<u8>, Vec<(u32, u32)>), BackendError> {
    if func.stage != ShaderStage::Fragment {
        return unsupported(format!("{:?} stage (fragment only so far)", func.stage));
    }
    let mut vc = vcode::lower(func)?;
    vcode::thread_jumps(&mut vc);
    vcode::dce(&mut vc);
    let al = regalloc::allocate(&vc);
    let (code, pcs) = emit::emit(&vc, &al)?;
    if cfg!(debug_assertions) {
        roundtrip_check(&code)?;
    }
    if std::env::var_os("ATRIUM_SPV_X86_DUMP").is_some() {
        eprintln!("bespoke-x86 {} ({} bytes):\n{}", func.name, code.len(), disassemble(&code));
    }
    Ok((code, pcs))
}

/// Intel-syntax listing of `code`, for `ATRIUM_SPV_X86_DUMP=1`.
pub fn disassemble(code: &[u8]) -> String {
    use iced_x86::Formatter;
    let mut d = iced_x86::Decoder::with_ip(64, code, 0, iced_x86::DecoderOptions::NONE);
    let mut f = iced_x86::IntelFormatter::new();
    let mut out = String::new();
    while d.can_decode() {
        let ins = d.decode();
        let mut s = String::new();
        f.format(&ins, &mut s);
        out.push_str(&format!("  {:4x}  {s}\n", ins.ip()));
    }
    out
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

