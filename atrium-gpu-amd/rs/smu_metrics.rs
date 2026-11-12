/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * smu_metrics.rs — read the SMU 14 metrics table (TABLE_SMU_METRICS) the
 * PMFW writes into the driver table: temperatures, power, fan, clocks,
 * throttling. docs/spec/gpu-power-management.md §4.
 *
 * The bytes come from device VRAM, written by firmware, so they are
 * untrusted: every value is range-checked and the whole read is refused if
 * one is implausible. The layout is `SmuMetrics_t` from Linux's MIT-licensed
 * smu14_driver_if_v14_0.h at driver interface 0x2E. A PMFW reporting another
 * interface version is parsed with the same layout but marked unverified;
 * the plausibility checks then carry the weight, and the caller says so.
 */

/// Driver interface version whose layout this file encodes.
pub const IF_VERSION_KNOWN: u32 = 0x2e;
/// sizeof(SmuMetrics_t) at 0x2E.
pub const METRICS_LEN: usize = 260;

pub const TEMP_COUNT: usize = 12;
pub const THROTTLER_COUNT: usize = 21;
pub const PPCLK_COUNT: usize = 11;

/// TEMP_e order.
pub mod temp {
    pub const EDGE: usize = 0;
    pub const HOTSPOT: usize = 1;
    pub const HOTSPOT_GFX: usize = 2;
    pub const HOTSPOT_SOC: usize = 3;
    pub const MEM: usize = 4;
    pub const VR_GFX: usize = 5;
    pub const VR_SOC: usize = 6;
    pub const VR_MEM0: usize = 7;
    pub const VR_MEM1: usize = 8;
}

/// What the driver exposes, C layout (atrium_gpu_amd.h struct amd_smu_metrics).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Metrics {
    /// 1 if the PMFW's interface version is the one this layout is from.
    pub verified: u32,
    pub counter: u32,
    /// °C, TEMP_e order.
    pub temp_c: [u16; TEMP_COUNT],
    pub socket_power_w: u16,
    pub board_power_w: u16,
    pub fan_rpm: u16,
    pub fan_pwm: u8,
    pub gfx_activity: u8,
    pub gfxclk_mhz: u16,
    pub memclk_mhz: u16,
    /// MHz, PPCLK_e order.
    pub clk_mhz: [u32; PPCLK_COUNT],
    /// Percent of time throttled, per throttler.
    pub throttling: [u8; THROTTLER_COUNT],
    pub _pad: [u8; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricsError {
    Short,
    /// Field (byte offset) out of its plausible range.
    Implausible(usize),
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

// SmuMetrics_t offsets at 0x2E (checked with offsetof against the header).
const OFF_CURR_CLOCK: usize = 0;
const OFF_GFXCLK_POSTDS: usize = 48;
const OFF_MEMCLK_POSTDS: usize = 56;
const OFF_COUNTER: usize = 104;
const OFF_GFX_ACTIVITY: usize = 124;
const OFF_SOCKET_POWER: usize = 136;
const OFF_BOARD_POWER: usize = 138;
const OFF_TEMP: usize = 140;
const OFF_FAN_PWM: usize = 168;
const OFF_FAN_RPM: usize = 170;
const OFF_THROTTLING: usize = 172;

/// Plausible ranges: a reading outside them means a wrong layout or garbage.
const TEMP_MAX_C: u16 = 150;
const POWER_MAX_W: u16 = 1500;
const FAN_MAX_RPM: u16 = 10_000;
const CLK_MAX_MHZ: u32 = 10_000;

pub fn parse(b: &[u8], if_version: u32) -> Result<Metrics, MetricsError> {
    if b.len() < METRICS_LEN {
        return Err(MetricsError::Short);
    }
    let mut m = Metrics {
        verified: (if_version == IF_VERSION_KNOWN) as u32,
        counter: le32(b, OFF_COUNTER),
        socket_power_w: le16(b, OFF_SOCKET_POWER),
        board_power_w: le16(b, OFF_BOARD_POWER),
        fan_pwm: b[OFF_FAN_PWM],
        fan_rpm: le16(b, OFF_FAN_RPM),
        gfxclk_mhz: le16(b, OFF_GFXCLK_POSTDS),
        memclk_mhz: le16(b, OFF_MEMCLK_POSTDS),
        ..Metrics::default()
    };
    let act = le16(b, OFF_GFX_ACTIVITY);
    if act > 100 {
        return Err(MetricsError::Implausible(OFF_GFX_ACTIVITY));
    }
    m.gfx_activity = act as u8;
    for (i, t) in m.temp_c.iter_mut().enumerate() {
        *t = le16(b, OFF_TEMP + 2 * i);
    }
    for (i, c) in m.clk_mhz.iter_mut().enumerate() {
        *c = le32(b, OFF_CURR_CLOCK + 4 * i);
        if *c > CLK_MAX_MHZ {
            return Err(MetricsError::Implausible(OFF_CURR_CLOCK + 4 * i));
        }
    }
    m.throttling.copy_from_slice(&b[OFF_THROTTLING..OFF_THROTTLING + THROTTLER_COUNT]);
    // Edge, hotspot and memory are always sensed on a dGPU; a 0 or a
    // too-hot value means we are not reading what we think.
    for i in [temp::EDGE, temp::HOTSPOT, temp::MEM] {
        let t = m.temp_c[i];
        if t == 0 || t > TEMP_MAX_C {
            return Err(MetricsError::Implausible(OFF_TEMP + 2 * i));
        }
    }
    if m.temp_c.iter().any(|&t| t > TEMP_MAX_C) {
        return Err(MetricsError::Implausible(OFF_TEMP));
    }
    if m.temp_c[temp::HOTSPOT] < m.temp_c[temp::EDGE] {
        return Err(MetricsError::Implausible(OFF_TEMP + 2 * temp::HOTSPOT));
    }
    if m.socket_power_w > POWER_MAX_W || m.board_power_w > POWER_MAX_W {
        return Err(MetricsError::Implausible(OFF_SOCKET_POWER));
    }
    if m.fan_rpm > FAN_MAX_RPM || m.fan_pwm > 100 {
        return Err(MetricsError::Implausible(OFF_FAN_RPM));
    }
    if m.throttling.iter().any(|&p| p > 100) {
        return Err(MetricsError::Implausible(OFF_THROTTLING));
    }
    Ok(m)
}
