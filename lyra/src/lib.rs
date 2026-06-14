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
// The data-plane ring and fd-passing live in lyra-protocol (shared with apps +
// choragusd); re-export so `lyra::ring` / `lyra::fdpass` keep working in-tree.
pub use lyra_protocol::{fdpass, ring};
pub mod passthrough;
pub mod channels;
pub mod spatial;
pub mod convolve;
pub mod biquad;
pub mod gain;
pub mod resampler;
pub mod binaural;
pub mod reverb;
pub mod node_abi;
