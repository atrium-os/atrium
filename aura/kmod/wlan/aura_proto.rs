// SPDX-License-Identifier: BSD-2-Clause
// The aura-proto crate, compiled into the kmod by kmod.rust.mk (which builds
// one crate per file). The source stays in aura/aura-proto.
#![no_std]
#[path = "../../aura-proto/src/lib.rs"]
mod inner;
pub use inner::*;
