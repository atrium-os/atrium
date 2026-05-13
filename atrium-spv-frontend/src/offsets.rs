//! SPIR-V byte-offset table for source-location
//! preservation through the IR.
//!
//! rspirv's `dr::Loader` produces `Instruction`s with no
//! source-position attached. To preserve the SPIR-V byte
//! offset of each instruction through the IR (constraint
//! A2 — the PC-map sidecar reads this for crash triage),
//! we walk the raw word stream in parallel with the
//! Loader's parse and record the byte offset where each
//! instruction starts.
//!
//! The walker is a tiny independent SPIR-V parser:
//!
//! - first 5 words (20 bytes) = module header
//! - each subsequent instruction starts with a header
//!   word: high 16 bits = total word count (including the
//!   header word itself), low 16 bits = opcode
//! - the instruction's byte offset is the sum of all
//!   preceding instruction word counts × 4 + 20
//!
//! The instruction order in our table matches
//! `dr::Module::types_global_values` ++ all
//! `Function::blocks[*].instructions` ++ `Function::end`
//! when walking the module top-to-bottom, but we don't
//! rely on that alignment — instead the frontend
//! function-pass passes an instruction index through to
//! `OffsetTable::get`.
//!
//! Today the function pass passes a per-function counter
//! that increments per instruction it sees. Phase 1 v3
//! could replace this with `rspirv::binary::Consumer`
//! impl-based parsing to get instruction-by-instruction
//! offset attribution directly from the parser — but the
//! simple zip-by-order approach below is enough for
//! crash-triage's needs and avoids a parser rewrite.

use crate::error::FrontendError;

/// Byte offsets of each SPIR-V instruction in source
/// order.
#[derive(Debug, Clone, Default)]
pub struct OffsetTable {
    /// `offsets[i]` = byte offset of the i-th instruction
    /// in the source SPIR-V byte stream (counting from
    /// the start of the file, not from after the header).
    pub offsets: Vec<u32>,
}

impl OffsetTable {
    /// Walk the SPIR-V byte stream and record offsets.
    pub fn build(spirv: &[u8]) -> Result<Self, FrontendError> {
        if spirv.len() < 20 {
            return Err(FrontendError::Malformed(
                "SPIR-V shorter than 5-word header".to_string(),
            ));
        }
        if spirv.len() % 4 != 0 {
            return Err(FrontendError::Malformed(format!(
                "SPIR-V byte length {} is not a multiple of 4",
                spirv.len(),
            )));
        }

        let mut offsets = Vec::new();
        let mut byte_off: usize = 20; // skip header

        while byte_off < spirv.len() {
            // Instruction header word: high 16 = word count
            // (including the header itself), low 16 = opcode.
            let header = u32::from_le_bytes(
                spirv[byte_off..byte_off + 4].try_into().unwrap(),
            );
            let word_count = (header >> 16) as usize;
            if word_count == 0 {
                return Err(FrontendError::Malformed(format!(
                    "instruction at byte offset {byte_off} has word_count=0",
                )));
            }
            offsets.push(byte_off as u32);
            byte_off += word_count * 4;
            if byte_off > spirv.len() {
                return Err(FrontendError::Malformed(format!(
                    "instruction at byte offset {} claims word_count {} \
                     which overruns the {}-byte module",
                    offsets.last().copied().unwrap_or(0), word_count, spirv.len(),
                )));
            }
        }
        Ok(OffsetTable { offsets })
    }

    /// Byte offset of the n-th instruction, or 0 if out
    /// of range. Out-of-range returns are fine: they just
    /// mean the PC-map sidecar attributes the crash to
    /// "start of shader" instead of a specific
    /// instruction.
    pub fn get(&self, index: usize) -> u32 {
        self.offsets.get(index).copied().unwrap_or(0)
    }

    /// Total instruction count (for diagnostic).
    pub fn len(&self) -> usize { self.offsets.len() }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool { self.offsets.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SPIR-V byte stream with a header + three trivial
    /// instructions (Nop, Nop, Nop). Each Nop is a single
    /// word (header word with word_count=1, opcode=0).
    fn three_nops() -> Vec<u8> {
        let mut buf = Vec::new();
        // SPIR-V magic + version + generator + bound +
        // reserved (5 words).
        buf.extend_from_slice(&0x0723_2037u32.to_le_bytes()); // magic
        buf.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // version 1.0
        buf.extend_from_slice(&0x0008_0001u32.to_le_bytes()); // generator
        buf.extend_from_slice(&10u32.to_le_bytes());          // id bound
        buf.extend_from_slice(&0u32.to_le_bytes());           // reserved
        // Three OpNops (word_count=1, opcode=0).
        let nop_header = (1u32 << 16) | 0;
        buf.extend_from_slice(&nop_header.to_le_bytes());
        buf.extend_from_slice(&nop_header.to_le_bytes());
        buf.extend_from_slice(&nop_header.to_le_bytes());
        buf
    }

    #[test]
    fn build_walks_three_nops() {
        let bytes = three_nops();
        let table = OffsetTable::build(&bytes).unwrap();
        assert_eq!(table.len(), 3);
        assert_eq!(table.offsets, vec![20, 24, 28]);
    }

    #[test]
    fn rejects_truncated_header() {
        let err = OffsetTable::build(&[0u8; 10]).unwrap_err();
        assert!(matches!(err, FrontendError::Malformed(_)));
    }

    #[test]
    fn rejects_non_word_aligned_size() {
        let bytes = vec![0u8; 25];
        let err = OffsetTable::build(&bytes).unwrap_err();
        assert!(matches!(err, FrontendError::Malformed(_)));
    }

    #[test]
    fn rejects_zero_word_count() {
        let mut bytes = three_nops();
        // Corrupt the first Nop's header to have word_count = 0.
        bytes[20] = 0;
        bytes[21] = 0;
        bytes[22] = 0;
        bytes[23] = 0;
        let err = OffsetTable::build(&bytes).unwrap_err();
        assert!(matches!(err, FrontendError::Malformed(_)));
    }

    #[test]
    fn rejects_overrunning_instruction() {
        let mut bytes = three_nops();
        // Last Nop claims word_count = 999, far past end.
        let huge = ((999u32) << 16) | 0;
        bytes[28..32].copy_from_slice(&huge.to_le_bytes());
        let err = OffsetTable::build(&bytes).unwrap_err();
        assert!(matches!(err, FrontendError::Malformed(_)));
    }

    #[test]
    fn get_out_of_range_returns_zero() {
        let bytes = three_nops();
        let table = OffsetTable::build(&bytes).unwrap();
        assert_eq!(table.get(0), 20);
        assert_eq!(table.get(2), 28);
        assert_eq!(table.get(99), 0);
    }
}
