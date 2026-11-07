//! Tactus: Atrium's input layer (docs/spec/atrium-input.md).
//!
//! The library is pure: HID descriptor parsing ([`hid`]), report decoding
//! ([`decode`]), key repeat ([`repeat`]), the pointer stage ([`pointer`])
//! and the wire to clients ([`wire`]). `tactusd` (src/bin) does the I/O:
//! it finds devices, reads them, runs the stages and serves frescod.

pub mod decode;
pub mod hid;
pub mod pointer;
pub mod repeat;
pub mod wire;
