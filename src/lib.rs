//! fresco-server library crate. Re-exports the protocol/scene/cas/render
//! modules so other binaries (notably the FreeBSD-native server) can reuse
//! them. The traditional macOS+winit+Metal binary lives in `src/main.rs`
//! and is the lib's first consumer; future binaries (atrium-server) link
//! the lib and supply their own platform layer.

pub mod cas;
pub mod scene;
pub mod command;
pub mod render;
pub mod input;
pub mod platform;
pub mod window;
