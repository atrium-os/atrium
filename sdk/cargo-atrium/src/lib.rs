//! `cargo atrium` (dev-tools.md §5.5, app-composition.md §6): the engine
//! under Artifex's New project, composition panel and fuzz panel; the CLI
//! is the same engine for people who prefer it.

pub mod check;
pub mod new;
pub mod cross;
pub mod registry;
pub mod fuzz;
pub mod worker;
pub mod bundle;
