//! atrium-player's supervisor side (docs/spec/atrium-player.md §2).
//!
//! The supervisor holds the parent's channel, the window, the video layer
//! and the audio stream, and never parses media: it forwards bytes to the
//! decode worker and reads back only fixed-size [`wire::Record`]s.
//!
//! - [`wire`]: the supervisor ⟷ worker protocol.
//! - [`sys`]: descriptors, shared memory, the sandbox (the crate's only `unsafe`).
//! - [`ring`]: the PCM ring.
//! - [`supervisor`]: the worker's lifecycle and the player state.
#![deny(unsafe_code)]

pub mod app;
pub mod audio_out;
pub mod cdm;
pub mod controls;
pub mod fetch;
pub mod fresco_sink;
pub mod ring;
pub mod supervisor;
pub mod sys;
pub mod wire;
pub mod wsola;
