//! SPIR-V `OpLoopMerge` patcher — inject `MaxIterations` literals.
//!
//! Why this exists: the `shader_validator` (§Phase 2.2) requires
//! every `OpLoopMerge` to declare a literal iteration bound. slangc
//! 2026.8 silently drops `[MaxIters(N)]` source annotations, leaving
//! the bound unexpressed in the emitted SPIR-V. glslang and dxc are
//! similar. This module is the in-toolchain bridge: it walks a
//! SPIR-V binary and rewrites bare `OpLoopMerge` instructions to
//! include `MaxIterations | N`.
//!
//! ## Strict-mode validator posture (locked 2026-05-12)
//!
//! Atrium's sandbox runs in **strict mode**: only literal-bearing
//! loop-control bits count as a termination promise. The previously-
//! permitted `Unroll`-only loops are no longer accepted directly —
//! the annotate step must add a literal bound for them too. This
//! collapses the trust chain to one rule ("validator demands a
//! literal") and removes the "what if the backend silently falls
//! back from `Unroll` to a runtime loop?" risk. `Unroll` is still
//! preserved in the LoopControl mask so the driver continues to
//! unroll; the validator just no longer trusts it as evidence of
//! bound.
//!
//! ## Idempotency
//!
//! Loops that already carry any literal-bearing bit (`MinIterations`,
//! `MaxIterations`, `IterationMultiple`, `PeelCount`, `PartialCount`,
//! `DependencyLength`) are left untouched. This means:
//! - Re-running annotate is safe.
//! - When a future slangc release fixes `[MaxIters]` emission, the
//!   annotate step gracefully becomes a no-op for those loops.

#![warn(missing_docs)]

use std::fmt;

/// SPIR-V module magic number.
const SPIRV_MAGIC: u32 = 0x07230203;
/// `OpLoopMerge` opcode.
const OP_LOOP_MERGE: u16 = 246;
/// `LoopControl.MaxIterations` bit value (SPIR-V spec §3.23).
const MAX_ITERATIONS_BIT: u32 = 0x020;
/// All `LoopControl` bits that carry a literal operand. Matches
/// `shader_validator`'s strict-mode bounded-bits set.
/// Per SPIR-V spec §3.23: DependencyLength, MinIterations,
/// MaxIterations, IterationMultiple, PeelCount, PartialCount.
const LITERAL_BITS: u32 = 0x008 | 0x010 | 0x020 | 0x040 | 0x080 | 0x100;

/// Annotate errors. The annotate path is structural-only; deep
/// validation is the validator's job.
#[derive(Debug)]
pub enum AnnotateError {
    /// Module too short to contain a header.
    TooShort {
        /// Bytes received.
        bytes: usize,
    },
    /// First word didn't match the SPIR-V magic.
    BadMagic {
        /// Word we saw.
        got: u32,
    },
    /// Byte stream length not a multiple of 4.
    NotWordAligned {
        /// Bytes received.
        bytes: usize,
    },
    /// An instruction's word_count would extend past end-of-module.
    Truncated {
        /// Word offset of the malformed instruction.
        word_offset: usize,
    },
    /// An instruction declared zero word-count (parser would spin).
    ZeroWordCount {
        /// Word offset.
        word_offset: usize,
    },
}

impl fmt::Display for AnnotateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnnotateError::TooShort { bytes } =>
                write!(f, "SPIR-V module too short ({bytes} bytes; need ≥20)"),
            AnnotateError::BadMagic { got } =>
                write!(f, "bad SPIR-V magic {got:#010x} (expected {SPIRV_MAGIC:#010x})"),
            AnnotateError::NotWordAligned { bytes } =>
                write!(f, "SPIR-V module not word-aligned ({bytes} bytes)"),
            AnnotateError::Truncated { word_offset } =>
                write!(f, "instruction at word {word_offset} extends past end of module"),
            AnnotateError::ZeroWordCount { word_offset } =>
                write!(f, "instruction at word {word_offset} has zero word count"),
        }
    }
}
impl std::error::Error for AnnotateError {}

/// Outcome of an annotate pass.
#[derive(Debug, Clone)]
pub struct AnnotateReport {
    /// The patched SPIR-V byte stream. Same length + 4 bytes per
    /// instruction patched.
    pub bytes: Vec<u8>,
    /// How many `OpLoopMerge` instructions were patched.
    pub patched: usize,
    /// How many `OpLoopMerge` instructions were skipped because
    /// they already declared a literal-bearing bound.
    pub already_bounded: usize,
}

/// Inject `MaxIterations | max_iters` into every `OpLoopMerge`
/// instruction whose `LoopControl` mask lacks a literal-bearing
/// bit.
///
/// Preserves: `Unroll`, `DontUnroll`, and any other non-literal
/// bits already set. Appends the new literal at the spec-mandated
/// position (after any lower-numbered set-bit literals; since this
/// function only modifies masks with no literal-bearing bits set,
/// the literal is always inserted at word offset 4 of the
/// instruction).
///
/// SPIR-V `OpLoopMerge` layout (spec §3.32.17):
/// ```text
///   word 0: opcode_header     high 16 = word_count; low 16 = 246
///   word 1: merge_block_id
///   word 2: continue_target_id
///   word 3: loop_control mask
///   word 4+: ordered literals for each set mask bit that carries one
/// ```
pub fn annotate_loop_merges(
    bytes: &[u8],
    max_iters: u32,
) -> Result<AnnotateReport, AnnotateError> {
    if bytes.len() < 20 {
        return Err(AnnotateError::TooShort { bytes: bytes.len() });
    }
    if bytes.len() % 4 != 0 {
        return Err(AnnotateError::NotWordAligned { bytes: bytes.len() });
    }

    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    if words[0] != SPIRV_MAGIC {
        return Err(AnnotateError::BadMagic { got: words[0] });
    }

    let mut out: Vec<u32> = Vec::with_capacity(words.len() + 16);
    out.extend_from_slice(&words[0..5]); // 5-word header

    let mut patched = 0usize;
    let mut already_bounded = 0usize;

    let mut i = 5usize;
    while i < words.len() {
        let w0 = words[i];
        let wc = (w0 >> 16) as usize;
        let opcode = (w0 & 0xFFFF) as u16;
        if wc == 0 {
            return Err(AnnotateError::ZeroWordCount { word_offset: i });
        }
        if i + wc > words.len() {
            return Err(AnnotateError::Truncated { word_offset: i });
        }

        if opcode == OP_LOOP_MERGE {
            if wc < 4 {
                return Err(AnnotateError::Truncated { word_offset: i });
            }
            let mask = words[i + 3];
            if mask & LITERAL_BITS != 0 {
                // Producer (or a prior annotate pass) already
                // declared a bound. Idempotent: leave alone.
                out.extend_from_slice(&words[i..i + wc]);
                already_bounded += 1;
            } else {
                // Set MaxIterations bit, append literal.
                let new_mask = mask | MAX_ITERATIONS_BIT;
                let new_wc = wc + 1;
                let new_header = ((new_wc as u32) << 16) | (opcode as u32);

                out.push(new_header);
                out.push(words[i + 1]);             // merge_block
                out.push(words[i + 2]);             // continue_target
                out.push(new_mask);                 // patched mask
                for j in 4..wc {
                    out.push(words[i + j]);         // any trailing operands
                }
                out.push(max_iters);                // appended literal
                patched += 1;
            }
        } else {
            out.extend_from_slice(&words[i..i + wc]);
        }
        i += wc;
    }

    let mut new_bytes = Vec::with_capacity(out.len() * 4);
    for w in &out {
        new_bytes.extend_from_slice(&w.to_le_bytes());
    }
    Ok(AnnotateReport { bytes: new_bytes, patched, already_bounded })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shader_validator;

    /// Build a minimal SPIR-V header plus one bare OpLoopMerge with
    /// LoopControl = mask. Returns the byte stream.
    fn bare_loop_merge(mask: u32) -> Vec<u8> {
        let mut words = vec![
            SPIRV_MAGIC, 0x0001_0000, 0, 1, 0, // header
        ];
        // OpLoopMerge: word_count=4, opcode=246
        words.push((4u32 << 16) | (OP_LOOP_MERGE as u32));
        words.push(10);    // merge_block_id
        words.push(11);    // continue_target_id
        words.push(mask);  // loop_control
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for w in &words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn patches_bare_loop_merge() {
        let input = bare_loop_merge(0);
        let report = annotate_loop_merges(&input, 1024).unwrap();
        assert_eq!(report.patched, 1);
        assert_eq!(report.already_bounded, 0);
        assert_eq!(report.bytes.len(), input.len() + 4,
                   "instruction gained one word for the literal");

        // Validator should now accept it.
        shader_validator::validate_spirv(&report.bytes)
            .expect("annotated module should validate");
    }

    #[test]
    fn patches_unroll_only_loop_too() {
        // LoopControl = 0x01 (Unroll). Strict mode requires a literal.
        let input = bare_loop_merge(0x01);
        let report = annotate_loop_merges(&input, 256).unwrap();
        assert_eq!(report.patched, 1);
        shader_validator::validate_spirv(&report.bytes)
            .expect("Unroll+MaxIterations should validate in strict mode");
    }

    #[test]
    fn idempotent_on_already_bounded() {
        // Manually craft a 5-word OpLoopMerge with MaxIterations set.
        let mut words = vec![SPIRV_MAGIC, 0x0001_0000, 0, 1, 0];
        words.push((5u32 << 16) | (OP_LOOP_MERGE as u32));
        words.push(10);
        words.push(11);
        words.push(MAX_ITERATIONS_BIT); // already has the bit
        words.push(512);                // literal
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for w in &words { bytes.extend_from_slice(&w.to_le_bytes()); }

        let report = annotate_loop_merges(&bytes, 9999).unwrap();
        assert_eq!(report.patched, 0);
        assert_eq!(report.already_bounded, 1);
        assert_eq!(report.bytes, bytes, "idempotent — no changes");
    }

    #[test]
    fn rejects_too_short() {
        assert!(matches!(annotate_loop_merges(&[0u8; 4], 1),
                         Err(AnnotateError::TooShort { .. })));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut m = bare_loop_merge(0);
        m[0] ^= 0xFF;
        assert!(matches!(annotate_loop_merges(&m, 1),
                         Err(AnnotateError::BadMagic { .. })));
    }

    #[test]
    fn rejects_misaligned() {
        let mut m = bare_loop_merge(0);
        m.push(0);
        assert!(matches!(annotate_loop_merges(&m, 1),
                         Err(AnnotateError::NotWordAligned { .. })));
    }
}
