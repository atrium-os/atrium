//! jaild — Atrium privileged jail-creation broker.
//!
//! The smallest-TCB tier of Atrium userspace. Sole caller of
//! `jail_set(2)` at runtime; portcullisd talks to jaild over a unix
//! socket. Spec: `docs/spec/portcullis.md` §0.5 and
//! `docs/spec/login-handoff.md`.
//!
//! ## Discipline (per `docs/LANGUAGE-POLICY.md` smallest-TCB carve-out)
//!
//! - `#![deny(unsafe_code)]` at the crate root. The only module
//!   permitted to relax it is `mod ffi`, with an explicit
//!   `#![allow(unsafe_code)]` at its top — that's the *single*
//!   place in jaild where libc + libjail entrypoints get wrapped
//!   behind safe Rust APIs. (forbid would be stricter but
//!   irrelaxable; we use deny so the audit point is one
//!   well-named module rather than spread across the tree.)
//! - No async runtime. Single-threaded blocking accept loop;
//!   future per-request fork (Phase 0b/V1) is via plain `fork(2)`,
//!   not tokio-anything.
//! - Minimal external deps: `libc`, `serde`, `serde_json`, `toml`,
//!   `thiserror`, `log`, `env_logger`. New deps require a written
//!   case in this crate's `CONTRIBUTING.md` (TODO: add when the
//!   first PR proposing one arrives).
//! - "C with safety" reading style. A FreeBSD developer should be
//!   able to follow control flow on first read.

#![deny(unsafe_code)]

pub mod error;
pub mod ffi;
pub mod protocol;
pub mod server;
pub mod state;
pub mod validator;

pub use error::JaildError;
pub use protocol::{CreateJailRequest, CreateJailResponse, Request, Response};
