//! The portable core of a Fresco window renderer (docs/spec/fenestra.md §14.1).
//!
//! Everything here builds and tests on any host (Linux, macOS, FreeBSD): it
//! turns a window's validated scene, as `fresco_scene_server` holds it, into
//! the compute kernels' packed `fresco_compute::Frame` ([`pack`]), and draws
//! windows with sprites on the CPU ([`sprites`]). frescod's renderer jail and
//! Fenestra's viewers both use it, so a remote viewer draws a scene with
//! exactly the code the host would.

pub mod pack;
pub mod sprites;
pub mod record;
pub mod stream;
