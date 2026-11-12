/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * power.rs — the vendor-neutral half of GPU power management
 * (docs/spec/gpu-power-management.md §2–§4). Nothing here knows about AMD,
 * Intel or any firmware: a backend (AMD: the SMU generation tables in
 * smu.c + smu_metrics.rs) turns its controller's readings into `Telemetry`
 * and carries out `Levers`; this file owns what every backend must obey:
 *
 * - the telemetry record and its units, and the plausibility rules every
 *   backend's readings pass before anyone sees them;
 * - the posture/grant → lever mapping, clamped so a request can only ever
 *   lower a limit the firmware reported (rule 3);
 * - the idle check: a card that reads busy and hot while no work is queued
 *   is reported, because the 2026-11-12 R9700 run left one at its power
 *   limit with nothing running.
 */

/// A reading the backend's controller does not provide.
pub const UNKNOWN: u16 = 0xffff;

/// Throttle classes, vendor-neutral. A backend maps its own throttler ids
/// onto these; `Telemetry::vendor_throttle` keeps the raw ids for logs.
pub mod throttle {
    /// A temperature limit (die, hotspot, memory, VR, board).
    pub const THERMAL: u32 = 1 << 0;
    /// A power limit (PPT / PL1 / PL2 / TDP).
    pub const POWER: u32 = 1 << 1;
    /// A current limit (TDC / EDC / Iccmax).
    pub const CURRENT: u32 = 1 << 2;
    /// Anything else the firmware reports (reliability, voltage, ...).
    pub const OTHER: u32 = 1 << 3;
}

/// One telemetry sample, C layout (atrium_gpu_amd.h struct
/// atrium_gpu_power_telemetry). Units are in the names; UNKNOWN where the
/// controller has no such sensor.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Telemetry {
    pub edge_c: u16,
    pub hotspot_c: u16,
    pub mem_c: u16,
    pub vr_c: u16,
    /// Package (socket / GPU chip) power, averaged by the controller.
    pub power_w: u16,
    pub board_power_w: u16,
    /// The power limit the controller is enforcing now, UNKNOWN if not
    /// reported.
    pub power_limit_w: u16,
    pub fan_rpm: u16,
    pub fan_pct: u8,
    pub gfx_busy_pct: u8,
    pub _pad0: u16,
    pub gfx_mhz: u16,
    pub mem_mhz: u16,
    /// `throttle::*` classes active in this sample.
    pub throttle: u32,
    /// The backend's own throttler ids that are active (bit n = id n).
    pub vendor_throttle: u32,
    /// The controller's sample counter, informational only (not every
    /// controller advances it per read).
    pub sample: u32,
}

impl Default for Telemetry {
    fn default() -> Self {
        Telemetry {
            edge_c: UNKNOWN,
            hotspot_c: UNKNOWN,
            mem_c: UNKNOWN,
            vr_c: UNKNOWN,
            power_w: UNKNOWN,
            board_power_w: UNKNOWN,
            power_limit_w: UNKNOWN,
            fan_rpm: UNKNOWN,
            fan_pct: 0xff,
            gfx_busy_pct: 0xff,
            _pad0: 0,
            gfx_mhz: UNKNOWN,
            mem_mhz: UNKNOWN,
            throttle: 0,
            vendor_throttle: 0,
            sample: 0,
        }
    }
}

/// Which field failed the plausibility check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Edge,
    Hotspot,
    Mem,
    Vr,
    Power,
    PowerLimit,
    Fan,
    Busy,
    Clock,
}

pub const TEMP_MAX_C: u16 = 150;
pub const POWER_MAX_W: u16 = 1500;
pub const FAN_MAX_RPM: u16 = 10_000;
pub const CLK_MAX_MHZ: u16 = 10_000;

fn known(v: u16) -> bool {
    v != UNKNOWN
}

/// The rules every backend's sample passes before it is shown or used
/// (spec §4). A failure means a wrong layout or garbage: the sample is
/// dropped, never guessed at. Edge temperature is required: every discrete
/// GPU senses it, so a missing one means we are not reading the controller.
pub fn check(t: &Telemetry) -> Result<(), Field> {
    if !known(t.edge_c) || t.edge_c == 0 || t.edge_c > TEMP_MAX_C {
        return Err(Field::Edge);
    }
    if known(t.hotspot_c) && (t.hotspot_c == 0 || t.hotspot_c > TEMP_MAX_C || t.hotspot_c < t.edge_c) {
        return Err(Field::Hotspot);
    }
    if known(t.mem_c) && (t.mem_c == 0 || t.mem_c > TEMP_MAX_C) {
        return Err(Field::Mem);
    }
    if known(t.vr_c) && t.vr_c > TEMP_MAX_C {
        return Err(Field::Vr);
    }
    for p in [t.power_w, t.board_power_w] {
        if known(p) && p > POWER_MAX_W {
            return Err(Field::Power);
        }
    }
    if known(t.power_limit_w) && (t.power_limit_w == 0 || t.power_limit_w > POWER_MAX_W) {
        return Err(Field::PowerLimit);
    }
    if known(t.fan_rpm) && t.fan_rpm > FAN_MAX_RPM || t.fan_pct != 0xff && t.fan_pct > 100 {
        return Err(Field::Fan);
    }
    if t.gfx_busy_pct != 0xff && t.gfx_busy_pct > 100 {
        return Err(Field::Busy);
    }
    for c in [t.gfx_mhz, t.mem_mhz] {
        if known(c) && c > CLK_MAX_MHZ {
            return Err(Field::Clock);
        }
    }
    Ok(())
}

/// Busy above this with no work queued means the card is not idling.
pub const IDLE_BUSY_MAX_PCT: u8 = 20;
/// Package power above this share of the enforced limit with no work queued
/// means the same (an idle discrete GPU sits far below a quarter of it).
pub const IDLE_POWER_MAX_PCT: u32 = 25;

/// Is the card working hard while the driver has no work queued? The driver
/// logs it and raises a note; it does not try to fight the firmware.
pub fn idle_suspicious(t: &Telemetry) -> bool {
    let busy = t.gfx_busy_pct != 0xff && t.gfx_busy_pct > IDLE_BUSY_MAX_PCT;
    let power = known(t.power_w)
        && known(t.power_limit_w)
        && (t.power_w as u32) * 100 > (t.power_limit_w as u32) * IDLE_POWER_MAX_PCT;
    busy || power || t.throttle & (throttle::POWER | throttle::CURRENT) != 0
}

/// What the controller reported as the envelope, read once at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub cap_min_w: u32,
    pub cap_default_w: u32,
    /// The default maximum gfx clock (the controller's top DPM level).
    pub gfx_max_mhz: u32,
}

/// Idle policy request.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idle {
    /// The firmware may power the graphics core off when idle (default).
    Allowed = 0,
    /// Held on only while work is queued (posture performance).
    HeldWhileQueued = 1,
}

/// Workload profile, vendor-neutral; a backend maps it to its own
/// (AMD: SetWorkloadMask bits).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    PowerSaving = 0,
    Default = 1,
}

/// The requests a backend carries out. Every value is inside `Limits`, and
/// never above the controller's default (rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Levers {
    pub cap_w: u32,
    pub gfx_soft_max_mhz: u32,
    pub idle: Idle,
    pub profile: Profile,
}

/// Map a posture (0 powersave .. 10 performance, atrium-power-posture.md)
/// and the energy federation's grant (W, 0 = none given) to levers.
pub fn plan(posture: u32, grant_w: u32, l: &Limits) -> Levers {
    let floor = l.cap_min_w.min(l.cap_default_w);
    let want = if grant_w == 0 { l.cap_default_w } else { grant_w };
    let cap_w = want.clamp(floor, l.cap_default_w);
    let (soft, idle, profile) = match posture {
        0..=2 => (l.gfx_max_mhz * 7 / 10, Idle::Allowed, Profile::PowerSaving),
        3..=7 => (l.gfx_max_mhz, Idle::Allowed, Profile::Default),
        _ => (l.gfx_max_mhz, Idle::HeldWhileQueued, Profile::Default),
    };
    Levers { cap_w, gfx_soft_max_mhz: soft, idle, profile }
}

/// The levers a backend restores on stop, detach or a thermal event: the
/// controller's own defaults (rule 6).
pub fn defaults(l: &Limits) -> Levers {
    Levers { cap_w: l.cap_default_w, gfx_soft_max_mhz: l.gfx_max_mhz, idle: Idle::Allowed, profile: Profile::Default }
}

/// Minimum time between two cap changes, and the smallest step worth one.
pub const CAP_MIN_INTERVAL_MS: u64 = 1000;
pub const CAP_HYSTERESIS_W: u32 = 5;

/// Should the cap move from `cur` to `want` now? Defaults always go through
/// (a restore is never delayed).
pub fn cap_change(cur: u32, want: u32, since_last_ms: u64, l: &Limits) -> bool {
    if want == cur {
        return false;
    }
    if want == l.cap_default_w {
        return true;
    }
    since_last_ms >= CAP_MIN_INTERVAL_MS && want.abs_diff(cur) >= CAP_HYSTERESIS_W
}
