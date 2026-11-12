/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * base_rs.rs — crate root of atrium_gpu_amd_pci.ko's Rust part: the IP
 * discovery parser (../rs/discovery.rs) and its reduction to the driver's
 * view (../rs/ipinfo.rs), behind one C entry point for identify.c. The
 * table comes out of device VRAM, so it is parsed in Rust (D2: new parsers
 * of untrusted bytes in kernel are Rust).
 */
#![no_std]

#[allow(dead_code)]
#[path = "../rs/discovery.rs"]
pub mod discovery;
#[path = "../rs/ipinfo.rs"]
pub mod ipinfo;
#[allow(dead_code)]
#[path = "../rs/fw.rs"]
pub mod fw;
#[path = "../rs/psp_plan.rs"]
pub mod psp_plan;
#[path = "../rs/ip_fw.rs"]
pub mod ip_fw;
#[allow(dead_code)]
#[path = "../rs/power.rs"]
pub mod power;
#[path = "../rs/smu_metrics.rs"]
pub mod smu_metrics;

/// # Safety
/// `bin` points to `len` readable bytes and `out` to a writable
/// `struct amd_ip_info` (same layout as `ipinfo::IpInfo`), both for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_amd_discovery_parse(
    bin: *const u8,
    len: usize,
    out: *mut ipinfo::IpInfo,
) -> i32 {
    if bin.is_null() || out.is_null() {
        return ipinfo::E_TRUNCATED;
    }
    // SAFETY: per the contract above.
    let (b, o) = unsafe { (core::slice::from_raw_parts(bin, len), &mut *out) };
    ipinfo::fill(b, o)
}

/// Fill `out[..max]` with the PSP bootloader steps for the firmware file
/// `bin`; returns the step count, or a negative code if the file is refused.
///
/// # Safety
/// `bin` points to `len` readable bytes and `out` to `max` writable steps.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_amd_psp_plan(
    bin: *const u8,
    len: usize,
    out: *mut psp_plan::Step,
    max: usize,
) -> i32 {
    if bin.is_null() || out.is_null() || max < psp_plan::MAX_STEPS {
        return -1;
    }
    // SAFETY: per the contract above.
    let (b, o) = unsafe { (core::slice::from_raw_parts(bin, len), &mut *(out as *mut [psp_plan::Step; psp_plan::MAX_STEPS])) };
    match psp_plan::plan(b, o) {
        Ok(n) => n as i32,
        Err(psp_plan::PlanError::Fw(_)) => -2,
        Err(psp_plan::PlanError::Missing(_)) => -3,
        Err(psp_plan::PlanError::Duplicate(_)) => -4,
        Err(psp_plan::PlanError::TooBig(_)) => -5,
        Err(psp_plan::PlanError::Empty(_)) => -6,
    }
}

/// Find the sub-binary of type `t`: 1 and (offset, size) if present, 0 if
/// absent, negative if the file is refused.
///
/// # Safety
/// `bin` points to `len` readable bytes; `off` and `size` are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_amd_psp_find(
    bin: *const u8,
    len: usize,
    t: u32,
    off: *mut u32,
    size: *mut u32,
) -> i32 {
    if bin.is_null() || off.is_null() || size.is_null() {
        return -1;
    }
    // SAFETY: per the contract above.
    let b = unsafe { core::slice::from_raw_parts(bin, len) };
    match psp_plan::find(b, t) {
        Ok(Some((o, s))) => {
            // SAFETY: per the contract above.
            unsafe { (*off, *size) = (o, s) };
            1
        }
        Ok(None) => 0,
        Err(_) => -2,
    }
}

/// Plan the LOAD_IP_FW sequence from the eight firmware files (order of
/// `ip_fw::file`): fill `out[..max]`, return the count, or a negative code
/// -(100 * (file + 1) + 1) if file `file` is refused, -2 for an oversized
/// image, -3 for a required empty one.
///
/// # Safety
/// `bins[i]` points to `lens[i]` readable bytes for i < 8; `out` to `max`
/// writable items.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_amd_ip_fw_plan(
    bins: *const *const u8,
    lens: *const usize,
    out: *mut ip_fw::Item,
    max: usize,
) -> i32 {
    if bins.is_null() || lens.is_null() || out.is_null() || max < ip_fw::MAX_ITEMS {
        return -1;
    }
    let mut files: [&[u8]; ip_fw::file::COUNT] = [&[]; ip_fw::file::COUNT];
    for (i, f) in files.iter_mut().enumerate() {
        // SAFETY: per the contract above.
        let (p, n) = unsafe { (*bins.add(i), *lens.add(i)) };
        if p.is_null() {
            return -1;
        }
        // SAFETY: per the contract above.
        *f = unsafe { core::slice::from_raw_parts(p, n) };
    }
    // SAFETY: per the contract above.
    let o = unsafe { &mut *(out as *mut [ip_fw::Item; ip_fw::MAX_ITEMS]) };
    match ip_fw::plan(&files, o) {
        Ok(n) => n as i32,
        Err(ip_fw::PlanError::Fw(i, _)) => -(100 * (i as i32 + 1) + 1),
        Err(ip_fw::PlanError::TooBig(_)) => -2,
        Err(ip_fw::PlanError::Empty(_)) => -3,
    }
}

/// Bytes of the SMU metrics table for this driver interface version, 0 if
/// no layout is known (then nothing is read or decoded).
#[unsafe(no_mangle)]
pub extern "C" fn atrium_amd_smu_metrics_len(if_version: u32) -> usize {
    smu_metrics::len(if_version).unwrap_or(0)
}

/// Decode the SMU metrics table into the vendor-neutral telemetry record:
/// 0 ok, -1 no layout for this interface version, -2 too short,
/// -(10 + power::Field) for an implausible field.
///
/// # Safety
/// `b` points to `len` readable bytes and `out` to a writable
/// `struct atrium_gpu_power_telemetry` (same layout as `power::Telemetry`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_amd_smu_metrics(
    b: *const u8,
    len: usize,
    if_version: u32,
    out: *mut power::Telemetry,
) -> i32 {
    if b.is_null() || out.is_null() {
        return -2;
    }
    // SAFETY: per the contract above.
    let (bytes, o) = unsafe { (core::slice::from_raw_parts(b, len), &mut *out) };
    match smu_metrics::parse(bytes, if_version) {
        Ok(t) => {
            *o = t;
            0
        }
        Err(smu_metrics::MetricsError::Unsupported) => -1,
        Err(smu_metrics::MetricsError::Short) => -2,
        Err(smu_metrics::MetricsError::Implausible(f)) => -(10 + f as i32),
    }
}

/// Is the card busy or near its power limit with no work queued?
///
/// # Safety
/// `t` points to a readable `struct atrium_gpu_power_telemetry`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_gpu_power_idle_suspicious(t: *const power::Telemetry) -> i32 {
    if t.is_null() {
        return 0;
    }
    // SAFETY: per the contract above.
    power::idle_suspicious(unsafe { &*t }) as i32
}

/// The RS64 program start address (`ucode_start_addr`, 64-bit) of a PFP, ME
/// or MEC image (`gfx_firmware_header_v2_0`). 0 ok, -1 refused.
///
/// # Safety
/// `bin` points to `len` readable bytes and `out` to a writable u64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_amd_rs64_start(bin: *const u8, len: usize, out: *mut u64) -> i32 {
    if bin.is_null() || out.is_null() {
        return -1;
    }
    // SAFETY: per the contract above.
    let b = unsafe { core::slice::from_raw_parts(bin, len) };
    match fw::Rs64Fw::parse(b) {
        Ok(f) => {
            // SAFETY: per the contract above.
            unsafe { *out = f.ucode_start_addr };
            0
        }
        Err(_) => -1,
    }
}

/// The MES program start address (`mes_uc_start_addr`, 64-bit) of a MES
/// image (`mes_firmware_header_v1_0`), for the CP_MES_PRGRM_CNTR_START
/// registers (mes12.c, stage 4d). 0 on success, -1 if the header is refused.
///
/// # Safety
/// `bin` points to `len` readable bytes and `out` to a writable u64.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atrium_amd_mes_start(bin: *const u8, len: usize, out: *mut u64) -> i32 {
    if bin.is_null() || out.is_null() {
        return -1;
    }
    // SAFETY: per the contract above.
    let b = unsafe { core::slice::from_raw_parts(bin, len) };
    match fw::MesFw::parse(b) {
        Ok(f) => {
            // SAFETY: per the contract above.
            unsafe { *out = f.ucode_start_addr };
            0
        }
        Err(_) => -1,
    }
}
