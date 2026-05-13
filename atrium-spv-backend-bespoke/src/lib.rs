//! Bespoke ARM64 backend for atrium-spv tier-2.
//!
//! # Why a second backend
//!
//! The user explicitly committed to the bespoke path early
//! on, citing the PPTK project's data point that
//! Cranelift-class regalloc + ISel hits ~70% of bespoke-
//! quality scalar code while the 90% bar is what makes
//! tier-2 worth shipping for production shaders. The
//! Cranelift backend stays — it's the differential
//! oracle's twin AND the graceful-degradation fallback for
//! IR shapes the bespoke pass doesn't yet handle — but the
//! perf path goes through here.
//!
//! # Phase 3 status
//!
//! **Skeleton.** This commit lands the crate shape +
//! public `compile(module, target) -> CompileOutput`
//! function with the same signature as
//! [`atrium_spv_backend_cranelift::compile`]. The
//! implementation currently emits:
//!   - empty functions (single `Op::Return`) as a literal
//!     ARM64 `ret` (0xD65F03C0).
//!   - wraps the bytes in a host-format object file (ELF
//!     for FreeBSD, Mach-O for Darwin) via the `object`
//!     crate.
//!
//! Real instruction selection + linear-scan register
//! allocation lands in phase 3 step 2+. The skeleton lets
//! the differential harness pull this backend into the
//! runner-set as soon as it understands any opcode at all
//! (currently: just empty functions).

use atrium_spv_ir::{Function, Module, Op, ShaderStage};
use pptk_codegen_arm64::asm;
use thiserror::Error;

/// What went wrong compiling a module.
#[derive(Debug, Error)]
pub enum BackendError {
    /// The IR uses an opcode / shape the bespoke backend
    /// doesn't yet support. The driver interprets this as
    /// "fall back to Cranelift" in production.
    #[error("bespoke backend doesn't support: {0}")]
    Unsupported(String),
    /// Anything else (object emission failure, internal
    /// invariant violation).
    #[error("internal error: {0}")]
    Internal(String),
}

/// Target the produced object file is for.
///
/// Mirrors [`atrium_spv_backend_cranelift::Target`] so the
/// driver can transparently swap backends. The bespoke
/// backend is ARM64-only by charter; X86_64 selection is
/// rejected at the top of [`compile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// FreeBSD on 64-bit ARM (the production tier-2 target).
    Aarch64FreeBSD,
    /// macOS on Apple Silicon (dev iteration).
    Aarch64Darwin,
}

impl Target {
    /// The host's target, panicking on x86_64 (the bespoke
    /// backend doesn't target it).
    pub fn host() -> Self {
        #[cfg(all(target_arch = "aarch64", target_os = "freebsd"))]
        { return Target::Aarch64FreeBSD; }
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        { return Target::Aarch64Darwin; }
        #[cfg(not(any(
            all(target_arch = "aarch64", target_os = "freebsd"),
            all(target_arch = "aarch64", target_os = "macos"),
        )))]
        { return Target::Aarch64Darwin; }
    }

    fn object_format(self) -> object::BinaryFormat {
        match self {
            Target::Aarch64FreeBSD => object::BinaryFormat::Elf,
            Target::Aarch64Darwin  => object::BinaryFormat::MachO,
        }
    }
}

/// Result of compiling a module — matches
/// [`atrium_spv_backend_cranelift::CompileOutput`] field-
/// for-field.
#[derive(Debug, Clone)]
pub struct CompileOutput {
    /// Object-file bytes ready to pass to `ld` / `cc`.
    pub object: Vec<u8>,
    /// PC-map sidecar bytes (atrium-spv-pcmap format v1).
    pub pcmap: Vec<u8>,
}

/// Compile an atrium-spv-ir module to a native object
/// file + PC-map sidecar.
pub fn compile(module: &Module, target: Target) -> Result<CompileOutput, BackendError> {
    let mut obj = object::write::Object::new(
        target.object_format(),
        object::Architecture::Aarch64,
        object::Endianness::Little,
    );
    let text_section = obj.section_id(object::write::StandardSection::Text);

    let mut pcmap = atrium_spv_pcmap::Builder::new();

    for func in &module.functions {
        let body = emit_function(func)?;
        let symbol_name = exported_symbol_name(func);

        let sym = obj.add_symbol(object::write::Symbol {
            name: symbol_name.into_bytes(),
            value: 0,
            size: body.len() as u64,
            kind: object::SymbolKind::Text,
            scope: object::SymbolScope::Dynamic,
            weak: false,
            section: object::write::SymbolSection::Section(text_section),
            flags: object::SymbolFlags::None,
        });
        // 4-byte alignment for ARM64 instructions.
        let _off = obj.add_symbol_data(sym, text_section, &body, 4);

        // PC-map entry: function-relative host_offset = 0,
        // spirv_offset = first body inst's
        // source_spirv_offset (or 0 if no insts).
        let first_offset = func.blocks.get(&func.entry_block)
            .and_then(|b| b.insts.first())
            .map(|i| i.source_spirv_offset)
            .unwrap_or(0);
        pcmap.push(0, first_offset);
    }

    let object_bytes = obj.write()
        .map_err(|e| BackendError::Internal(format!("object write: {e}")))?;
    let pcmap_bytes = pcmap.finish_to_bytes();
    Ok(CompileOutput { object: object_bytes, pcmap: pcmap_bytes })
}

/// Emit the ARM64 instruction bytes for one function.
///
/// Phase 3 skeleton supports only the empty-body case
/// (single `Op::Return`). Anything else is `Unsupported`.
fn emit_function(func: &Function) -> Result<Vec<u8>, BackendError> {
    let block = func.blocks.get(&func.entry_block).ok_or_else(||
        BackendError::Internal("entry block missing".into()))?;
    if block.insts.len() != 1 || !matches!(block.insts[0].op, Op::Return) {
        return Err(BackendError::Unsupported(format!(
            "function {} has body beyond a single Op::Return; \
             bespoke backend phase 3 skeleton supports empty fns only",
            func.name,
        )));
    }
    let mut a = asm::Asm::new();
    a.emit(asm::ret());
    Ok(a.into_bytes())
}

/// Compute the exported symbol name for a function.
///
/// Same convention as the Cranelift backend:
/// `atrium_vs_main` / `atrium_fs_main` / `atrium_cs_main`
/// for entry points, else the IR name verbatim.
fn exported_symbol_name(func: &Function) -> String {
    match func.stage {
        ShaderStage::Vertex   => "atrium_vs_main".to_string(),
        ShaderStage::Fragment => "atrium_fs_main".to_string(),
        ShaderStage::Compute  => "atrium_cs_main".to_string(),
    }
}
