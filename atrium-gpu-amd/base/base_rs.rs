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
