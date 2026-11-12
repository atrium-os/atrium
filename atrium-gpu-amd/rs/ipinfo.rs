/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * ipinfo.rs — the discovery table reduced to what the driver uses, in the
 * C layout of `struct amd_ip_info` (atrium_gpu_amd.h). identify.c hands the
 * raw table (untrusted bytes copied out of VRAM) to
 * atrium_amd_discovery_parse (base/base_rs.rs), which calls `fill`.
 * Host-tested against the real R9700 table in rs-host-tests.
 */

use crate::discovery::{hwid, Discovery, DiscoveryError, IpInstance};

pub const NBASE: usize = 8;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IpVer {
    pub present: u8,
    pub major: u8,
    pub minor: u8,
    pub rev: u8,
    pub nbase: u8,
    pub base: [u32; NBASE],
}

/// Same field order as `struct amd_ip_info`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IpInfo {
    pub gc: IpVer,
    pub mp0: IpVer,
    pub mp1: IpVer,
    pub sdma0: IpVer,
    pub mmhub: IpVer,
    pub osssys: IpVer,
    pub hdp: IpVer,
    pub nbif: IpVer,
    pub dmu: IpVer,
    pub vcn: IpVer,
    pub nips: u32,
}

/// Error codes returned through the C entry point (negative, errno-free).
pub const E_TRUNCATED: i32 = -1;
pub const E_SIGNATURE: i32 = -2;
pub const E_CHECKSUM: i32 = -3;
pub const E_LAYOUT: i32 = -4;
pub const E_BASE_RANGE: i32 = -5;

fn code(e: DiscoveryError) -> i32 {
    match e {
        DiscoveryError::Truncated => E_TRUNCATED,
        DiscoveryError::BadBinarySignature(_) | DiscoveryError::BadIpSignature(_) => E_SIGNATURE,
        DiscoveryError::BadBinaryChecksum { .. } | DiscoveryError::BadIpChecksum { .. } => E_CHECKSUM,
        DiscoveryError::TooManyDies(_) | DiscoveryError::OutOfBounds => E_LAYOUT,
    }
}

fn ver(ip: &IpInstance) -> Result<IpVer, i32> {
    let mut v = IpVer { present: 1, major: ip.major, minor: ip.minor, rev: ip.revision, ..IpVer::default() };
    for (i, &b) in ip.bases().iter().enumerate().take(NBASE) {
        // Segment bases are dword addresses inside the register BAR; a value
        // that does not fit 32 bits is not one we could ever program.
        v.base[i] = u32::try_from(b).map_err(|_| E_BASE_RANGE)?;
        v.nbase = (i + 1) as u8;
    }
    Ok(v)
}

/// Parse `bin` and fill `out` (die 0, instance 0 of each block). 0 or E_*.
pub fn fill(bin: &[u8], out: &mut IpInfo) -> i32 {
    *out = IpInfo::default();
    let d = match Discovery::parse(bin) {
        Ok(d) => d,
        Err(e) => return code(e),
    };
    let mut err = 0;
    let mut n = 0u32;
    let r = d.for_each_ip(|ip| {
        n += 1;
        if ip.die != 0 || ip.instance != 0 || err != 0 {
            return;
        }
        let slot = match ip.hw_id {
            hwid::GC => &mut out.gc,
            hwid::MP0 => &mut out.mp0,
            hwid::MP1 => &mut out.mp1,
            hwid::SDMA0 => &mut out.sdma0,
            hwid::MMHUB => &mut out.mmhub,
            hwid::OSSSYS => &mut out.osssys,
            hwid::HDP => &mut out.hdp,
            hwid::NBIF => &mut out.nbif,
            hwid::DMU => &mut out.dmu,
            hwid::VCN => &mut out.vcn,
            _ => return,
        };
        if slot.present == 0 {
            match ver(ip) {
                Ok(v) => *slot = v,
                Err(e) => err = e,
            }
        }
    });
    if let Err(e) = r {
        *out = IpInfo::default();
        return code(e);
    }
    if err != 0 {
        *out = IpInfo::default();
        return err;
    }
    out.nips = n;
    0
}
