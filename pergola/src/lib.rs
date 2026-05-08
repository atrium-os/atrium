//! # Pergola — Atrium UI toolkit
//!
//! Sits above Fresco. Emits scene-graph deltas via `fresco-client`. Sync
//! API with a signal-based reactive bridge for asynchronous data flow.
//!
//! See:
//! - `docs/spec/pergola.md` — toolkit architecture
//! - `docs/design/atrium-visual-language.md` — typography, color,
//!   spacing, shape, motion. Widgets reference its tokens; never raw
//!   values.
//!
//! ## Phase 0 scope
//!
//! This crate ships the foundational abstractions: geometry, color,
//! theme tokens, and the `View` trait + commit cycle. It does **not**
//! yet ship widgets, layout, input routing, or window management —
//! those land in subsequent phases on the roadmap (`docs/spec/pergola.md`
//! §9 + the Pergola track in the bsd repo).
//!
//! The example `examples/hello.rs` exercises the commit cycle by
//! producing a `Node` tree and dumping the resulting render plan to
//! stdout. Wire-protocol emission (the actual `fresco-client` calls)
//! lands in phase 4 alongside the `Window` API.

pub mod app;
pub mod color;
pub mod event;
pub mod geom;
pub mod input;
pub mod interaction;
pub mod layout;
pub mod node;
pub mod reactive;
pub mod surface;
pub mod theme;
pub mod view;
pub mod widgets;

pub use app::App;
pub use color::Color;
pub use event::{hit_test, Event, Key, KeyEventKind, Modifiers};
pub use geom::{Point, Rect, Size};
pub use interaction::{Handlers, Interactions};
pub use node::{diff, Node, NodeDelta, NodeId, NodeTree, TextStyle};
pub use reactive::Mutable;
pub use surface::{commit, FrescoSurface, LogSurface, Surface};
pub use view::{render, Ctx, View};
pub use widgets::{Button, TextField};
