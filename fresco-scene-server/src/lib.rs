//! fresco-scene-server — library implementing the scene-server roles
//! that aren't tied to a specific GPU stack: IPC dispatch (envelope
//! protocol), scene-state authority, scene-graph traversal, window
//! manager. The Vulkan side (HeadlessRenderer + bundle dispatch) lives
//! in `fresco-vulkan` and is wired together with this crate by
//! `frescod` directly. Platform-independent; consumed by frescod and
//! any other Atrium services that need scene-graph / window
//! machinery.
//!
//! M2.7e (2026-05-05): legacy 128-byte `Command`/`Completion` stack
//! excised — `command::frontend`, `render::tiny_skia_backend`,
//! `render::backend`, `input::*`, `platform::*` are all gone. The new
//! envelope wire is in `fresco-protocol` and dispatched through
//! `command::envelope_frontend::EnvelopeFrontend`.

pub mod cas;
pub mod scene;
pub mod command;
pub mod render;
pub mod window;
