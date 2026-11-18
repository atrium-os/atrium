//! Opifex, Atrium's package system (docs/spec/opifex.md). The library behind
//! the `opifex` CLI, `opifexd` and the `opifex-check` worker.

pub mod app;
pub mod archive;
pub mod content;
pub mod daemon;
pub mod fetch;
pub mod font;
pub mod installed;
pub mod layer;
pub mod proto;
pub mod runtime;
pub mod updates;
