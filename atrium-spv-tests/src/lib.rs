//! atrium-spv-tests — differential-test infrastructure for
//! tier-2 software Vulkan.
//!
//! This crate contains two pieces:
//!
//! 1. **The SPIR-V interpreter** ([`interpreter`]) — a
//!    correctness oracle that walks SPIR-V bytes directly
//!    via `rspirv::dr`, with **zero shared frontend code**
//!    with the production backends. It exists so that
//!    when the production backends (bespoke + Cranelift)
//!    disagree with each other but BOTH disagree with the
//!    interpreter, we know the bug is in the production
//!    frontend (which the two backends share). Without
//!    the interpreter, frontend bugs would silently produce
//!    wrong-but-consistent output across both backends.
//!
//! 2. **The differential test harness** ([`harness`]) — a
//!    set of helpers (most importantly
//!    [`harness::assert_shader_agrees`]) that compile a
//!    SPIR-V module through every available backend
//!    (bespoke / Cranelift / interpreter), execute each
//!    with the same inputs, and assert pixel-equivalent
//!    output.
//!
//! # Spec references
//!
//! - [`docs/spec/tier2-renderer.md`] §10.2 — differential
//!   test harness design
//! - [`docs/spec/tier2-shader-codegen-constraints.md`] §F —
//!   cross-backend parity rules
//!
//! # Phase status
//!
//! **Phase 0 v0c skeleton.** The interpreter handles a
//! narrow opcode subset — enough to interpret a constant-
//! colour fragment shader end-to-end. Unsupported opcodes
//! return [`interpreter::InterpError::UnsupportedOpcode`].
//! The harness has a [`harness::ShaderRunner`] trait that
//! the production backends will implement once they land
//! (phases 2 + 3); for phase 0 the only registered runner
//! is the interpreter itself, so harness tests assert
//! "interpreter agrees with itself" (a trivial pass, but
//! exercises the harness plumbing).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod interpreter;
pub mod harness;
pub mod pixels;
