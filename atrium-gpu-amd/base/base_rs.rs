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
