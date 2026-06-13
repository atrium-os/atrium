//! Lyra — Atrium's audio subsystem (the kernel-scheduled deadline graph).
//!
//! See `docs/spec/atrium-lyra-architecture.md`. This crate is lyrad, the audio
//! graph **engine** (mechanism): it builds a processing graph, admits each node
//! into the kernel deadline lane, and runs the graph in topological-deadline
//! order. The graph algorithm is a production transcription of the deterministic
//! model proven in the gpusim engine (`engine/src/lyra*.rs`, phases L0–L1).
//!
//! - [`graph`] — admission + topological-deadline assignment (host-portable).
//! - [`lane`] — the `/dev/laminar` deadline-broker shim (the lane wiring).

pub mod graph;
pub mod lane;
pub mod oss;
pub mod ring;
pub mod passthrough;
pub mod channels;
