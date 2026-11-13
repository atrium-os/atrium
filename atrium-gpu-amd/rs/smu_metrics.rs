/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * smu_metrics.rs — AMD backend: decode the SMU metrics table
 * (TABLE_SMU_METRICS) the PMFW writes into the driver table, into the
 * vendor-neutral `power::Telemetry` (docs/spec/gpu-power-management.md §4,
 * §8). The bytes come from device VRAM, written by firmware: untrusted.
 *
 * The layout is chosen by the driver interface version the PMFW reports
 * (GetDriverIfVersion), from a table of layouts this file knows. An
 * interface version that is not in the table is refused (Unsupported):
 * nothing is decoded or shown, since a guessed layout reads garbage as
 * fact. Adding a row needs evidence, recorded next to it.
 */

use crate::power::{self, Telemetry};

/// AMD throttler ids (THROTTLER_*_BIT, smu14_driver_if_v14_0.h) → class.
const THROTTLER_COUNT: usize = 21;

/// One `SmuMetrics_t` layout: byte offsets of the fields we read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub len: usize,
    gfxclk_postds: usize,
    memclk_postds: usize,
    power_limit: usize,
    counter: usize,
    gfx_activity: usize,
    socket_power: usize,
    board_power: usize,
    temp: usize,
    fan_pwm: usize,
    fan_rpm: usize,
    throttling: usize,
    /// THROTTLER bit → power::throttle class.
    classes: [u32; THROTTLER_COUNT],
}

/// TEMP_e order within AvgTemperature[].
const T_EDGE: usize = 0;
const T_HOTSPOT: usize = 1;
const T_MEM: usize = 4;
const T_VR_GFX: usize = 5;

const TH: u32 = power::throttle::THERMAL;
const PW: u32 = power::throttle::POWER;
const CU: u32 = power::throttle::CURRENT;
const OT: u32 = power::throttle::OTHER;

/// SMU 14.0.2/3 `SmuMetrics_t` (smu14_driver_if_v14_0.h; offsets checked
/// with offsetof against the header). Throttlers 0–11 temperatures, 12–13
/// TDC, 14–17 PPT, 18 FIT, 19–20 GFX APCC+/DVO.
const SMU14_0_2: Layout = Layout {
    len: 260,
    gfxclk_postds: 48,
    memclk_postds: 56,
    power_limit: 68, // dGPU_W_MAX
    counter: 104,
    gfx_activity: 124,
    socket_power: 136,
    board_power: 138,
    temp: 140,
    fan_pwm: 168,
    fan_rpm: 170,
    throttling: 172,
    classes: [
        TH, TH, TH, TH, TH, TH, TH, TH, TH, TH, TH, TH, // temperatures
        CU, CU, // TDC gfx, soc
        PW, PW, PW, PW, // PPT0..3
        OT, OT, OT, // FIT, APCC+, DVO
    ],
};

/// Driver interface versions with a known layout, and the evidence.
pub const LAYOUTS: &[(u32, Layout)] = &[
    // The Linux header's own version.
    (0x2e, SMU14_0_2),
    // R9700 (SMU 14.0.3, PMFW 0x685000), 2026-11-12: raw table read on the
    // box decodes self-consistently with the 0x2E offsets: dGPU_W_MAX 300 W
    // (the board's TBP), socket = board power 299 W under PPT0 throttling,
    // moving-average power 303 W, nine sensed temperatures 40–98 °C,
    // hotspot ≥ edge, fan 3470 rpm at 73 %, gfxclk target/pre/post 3207/
    // 3191/3191 MHz, MetricsCounter not advancing per read
    // (docs/spec/r9700-bare-metal.md §5).
    (0x33, SMU14_0_2),
];

pub fn layout(if_version: u32) -> Option<&'static Layout> {
    LAYOUTS.iter().find(|(v, _)| *v == if_version).map(|(_, l)| l)
}

/// Bytes the caller must read for this interface version.
pub fn len(if_version: u32) -> Option<usize> {
    layout(if_version).map(|l| l.len)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsError {
    /// No layout for this driver interface version.
    Unsupported,
    Short,
    Implausible(power::Field),
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// Decode and check one metrics table.
pub fn parse(b: &[u8], if_version: u32) -> Result<Telemetry, MetricsError> {
    let l = layout(if_version).ok_or(MetricsError::Unsupported)?;
    if b.len() < l.len {
        return Err(MetricsError::Short);
    }
    let temp = |i: usize| match le16(b, l.temp + 2 * i) {
        0 if i == T_VR_GFX => power::UNKNOWN,
        v => v,
    };
    let pct = |v: u16| if v > 100 { 0xfe } else { v as u8 }; // 0xfe fails check()
    let mut t = Telemetry {
        edge_c: temp(T_EDGE),
        hotspot_c: temp(T_HOTSPOT),
        mem_c: temp(T_MEM),
        vr_c: temp(T_VR_GFX),
        power_w: le16(b, l.socket_power),
        board_power_w: le16(b, l.board_power),
        power_limit_w: match le16(b, l.power_limit) {
            0 => power::UNKNOWN,
            v => v,
        },
        fan_rpm: le16(b, l.fan_rpm),
        fan_pct: b[l.fan_pwm],
        gfx_busy_pct: pct(le16(b, l.gfx_activity)),
        gfx_mhz: le16(b, l.gfxclk_postds),
        mem_mhz: le16(b, l.memclk_postds),
        sample: le32(b, l.counter),
        ..Telemetry::default()
    };
    for (i, &p) in b[l.throttling..l.throttling + THROTTLER_COUNT].iter().enumerate() {
        if p > 100 {
            return Err(MetricsError::Implausible(power::Field::Busy));
        }
        if p != 0 {
            t.vendor_throttle |= 1 << i;
            t.throttle |= l.classes[i];
        }
    }
    power::check(&t).map_err(MetricsError::Implausible)?;
    Ok(t)
}
