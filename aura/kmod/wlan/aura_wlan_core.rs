// SPDX-License-Identifier: BSD-2-Clause
// The aura-wlan-core crate, compiled into the kmod (see aura_proto.rs).
#![no_std]
#[path = "../../aura-wlan-core/src/lib.rs"]
mod inner;
pub use inner::*;
