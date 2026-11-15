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

static OUTPUTS: RwLock<Vec<fresco_protocol::DisplayOutput>> = RwLock::new(Vec::new());

/// The monitors as apps may know them (`OP_DISPLAY_OUTPUTS`), primary
/// first, for split mode's broker to tell renderers.
pub fn outputs() -> Vec<fresco_protocol::DisplayOutput> {
    OUTPUTS.read().unwrap().clone()
}

pub fn set_outputs(o: Vec<fresco_protocol::DisplayOutput>) {
    *OUTPUTS.write().unwrap() = o;
}

/// Split records telling a renderer the monitors.
pub fn output_records() -> Vec<fresco_split::Msg> {
    let o = outputs();
    let count = o.len() as u32;
    o.iter().enumerate().map(|(i, d)| fresco_split::Msg::Output {
        index: i as u32,
        count,
        identity: d.identity,
        rect: [d.rect.x, d.rect.y, d.rect.w, d.rect.h],
        scale_milli: d.scale_milli.clamp(fresco_split::SCALE_MIN, fresco_split::SCALE_MAX),
        refresh_mhz: d.refresh_mhz,
        internal: d.internal,
    }).collect()
}
