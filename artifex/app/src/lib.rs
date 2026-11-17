//! Artifex, Atrium's code editor (dev-tools.md §3.4, artifex.md;
//! dev-tools-plan.md M11). A regular app with `workspace-exec`: the person
//! picks a workspace (a press), its `work` appears at `/work/<name>`, and
//! Artifex edits files there and runs everything else (builds, git,
//! language servers) in the workspace, as the workspace, with its tools.
//! Artifex itself holds no compilers and reaches nothing the workspace
//! doesn't.

pub mod engine;
pub mod exec;
pub mod git;
pub mod lsp;
pub mod tasks;
pub mod tree;
pub mod ui;

/// How a workspace is named on screen.
pub fn title(name: &str) -> String {
    if name == "scratch" { "Scratch".into() } else { name.to_string() }
}
