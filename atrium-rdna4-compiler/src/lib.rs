//! atrium-rdna4-compiler — the RDNA4 (GFX12) machine layer of the shader
//! compiler: GIR → RDNA4 machine code. See docs/spec/atrium-gpu-ir.md §7.
//!
//! Scaffolding for now: instruction emission helpers ([`asm`]) and, under
//! `tests/`, an rspirv kernel builder plus IR probes over the desktop shaders.
//! Code generation lands with GIR milestone G3.

#![warn(missing_docs)]

pub mod asm;
