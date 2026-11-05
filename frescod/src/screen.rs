//! The mode the display scans out now, for the threads that need the
//! screen's size (pointer and injected input clamp the cursor to it; split
//! mode's broker tells new renderers). The frame loop sets it at start-up
//! and on every mode switch.

use std::sync::RwLock;

static SCREEN: RwLock<(u32, u32, u32)> = RwLock::new((0, 0, 0));

/// (width, height, refresh in milli-Hz).
pub fn get() -> (u32, u32, u32) {
    *SCREEN.read().unwrap()
}

pub fn set(width: u32, height: u32, refresh_mhz: u32) {
    *SCREEN.write().unwrap() = (width, height, refresh_mhz);
}
