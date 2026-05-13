//! Structured CFG recovery from SPIR-V's
//! `OpSelectionMerge` / `OpLoopMerge` markers.
//!
//! # Phase status
//!
//! **Phase 1 v1 stub.** Real CFG recovery lands in v2.
//! For v1 we only handle single-block functions: any
//! function with multiple blocks returns
//! `FrontendError::Unsupported` per constraint A1's
//! "reject early, don't try to recover" rule.

use rspirv::dr::Function;

use crate::error::FrontendError;

/// Reject any function with more than one block.
///
/// Real CFG recovery will replace this with structured-
/// block detection in v2.
pub fn reject_unstructured(func: &Function) -> Result<(), FrontendError> {
    if func.blocks.len() > 1 {
        return Err(FrontendError::Unsupported(format!(
            "function has {} blocks; phase 1 v1 supports exactly 1 (no control flow yet)",
            func.blocks.len(),
        )));
    }
    Ok(())
}
