//! fresco-scene-server — library implementing every scene-server role
//! (IPC, scene-state authority, scene-graph processor, window manager,
//! bundle host, GPU resource broker, GPU command driver) per
//! `docs/spec/fresco-rendering-stack.md`. Platform-independent;
//! consumed by `frescod` (the FreeBSD daemon binary) and other
//! Atrium services that need any of the underlying machinery.
//!
//! Earlier macOS-only binaries (winit + Metal backend) were retired
//! 2026-05-04 — they were superseded by the
//! `scratch/fresco-arch-validation` POC and now by frescod itself.

pub mod cas;
pub mod scene;
pub mod command;
pub mod render;
pub mod input;
pub mod platform;
pub mod window;
