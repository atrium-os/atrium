/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * psp_plan.rs — which pieces of a PSP v2 firmware file go to the PSP
 * bootloader, with which command, in which order (stage 3 of
 * docs/spec/r9700-bare-metal.md). The file is parsed by fw.rs (headers only;
 * the payloads are opaque). Order and command codes are the hardware
 * interface as Linux's MIT-licensed amdgpu_psp.c / amdgpu_psp.h use it for
 * PSP 13/14: KDB, SPL, SYS_DRV, SOC_DRV, INTF_DRV, DBG(HAD)_DRV, RAS_DRV,
 * IPKEYMGR_DRV, SPDM_DRV, then SOS last.
 */

use crate::fw::{FwError, PspFw};

/// Descriptor `fw_type` values in a PSP v2 file (amdgpu_ucode.h psp_fw_type).
pub mod ty {
    pub const SOS: u32 = 1;
    pub const SYS_DRV: u32 = 2;
    pub const KDB: u32 = 3;
    pub const TOC: u32 = 4;
    pub const SPL: u32 = 5;
    pub const RL: u32 = 6;
    pub const SOC_DRV: u32 = 7;
    pub const INTF_DRV: u32 = 8;
    pub const DBG_DRV: u32 = 9;
    pub const RAS_DRV: u32 = 10;
    pub const IPKEYMGR_DRV: u32 = 11;
    pub const SPDM_DRV: u32 = 12;
}

/// Bootloader commands written to C2PMSG_35 (amdgpu_psp.h psp_bootloader_cmd).
pub mod bl {
    pub const LOAD_SYSDRV: u32 = 0x10000;
    pub const LOAD_SOSDRV: u32 = 0x20000;
    pub const LOAD_KEY_DATABASE: u32 = 0x80000;
    pub const LOAD_SOCDRV: u32 = 0xB0000;
    pub const LOAD_DBGDRV: u32 = 0xC0000; /* HAD_DRV on PSP 14 */
    pub const LOAD_INTFDRV: u32 = 0xD0000;
    pub const LOAD_RASDRV: u32 = 0xE0000;
    pub const LOAD_IPKEYMGRDRV: u32 = 0xF0000;
    pub const LOAD_TOS_SPL_TABLE: u32 = 0x1000_0000;
    pub const LOAD_SPDMDRV: u32 = 0x2000_0000;
}

/// The bootloader's staging buffer is 1 MiB; no piece may be larger.
pub const MAX_PIECE: u32 = 1 << 20;

const ORDER: [(u32, u32); 10] = [
    (ty::KDB, bl::LOAD_KEY_DATABASE),
    (ty::SPL, bl::LOAD_TOS_SPL_TABLE),
    (ty::SYS_DRV, bl::LOAD_SYSDRV),
    (ty::SOC_DRV, bl::LOAD_SOCDRV),
    (ty::INTF_DRV, bl::LOAD_INTFDRV),
    (ty::DBG_DRV, bl::LOAD_DBGDRV),
    (ty::RAS_DRV, bl::LOAD_RASDRV),
    (ty::IPKEYMGR_DRV, bl::LOAD_IPKEYMGRDRV),
    (ty::SPDM_DRV, bl::LOAD_SPDMDRV),
    (ty::SOS, bl::LOAD_SOSDRV),
];
pub const MAX_STEPS: usize = ORDER.len();

/// One bootloader step: send `size` bytes at `offset` of the file with `cmd`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Step {
    pub cmd: u32,
    pub fw_type: u32,
    pub fw_version: u32,
    pub offset: u32,
    pub size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    Fw(FwError),
    /// SYS_DRV and SOS are what the bootloader cannot do without.
    Missing(u32),
    Duplicate(u32),
    TooBig(u32),
    Empty(u32),
}

/// Fill `out` with the steps for `file`, in bootloader order; returns how many.
pub fn plan(file: &[u8], out: &mut [Step; MAX_STEPS]) -> Result<usize, PlanError> {
    let psp = PspFw::parse(file).map_err(PlanError::Fw)?;
    let mut n = 0;
    for &(t, cmd) in ORDER.iter() {
        let mut found = None;
        for b in psp.bins() {
            if b.fw_type == t {
                if found.is_some() {
                    return Err(PlanError::Duplicate(t));
                }
                found = Some(*b);
            }
        }
        match found {
            Some(b) => {
                if b.region.size == 0 {
                    return Err(PlanError::Empty(t));
                }
                if b.region.size > MAX_PIECE {
                    return Err(PlanError::TooBig(t));
                }
                out[n] = Step { cmd, fw_type: t, fw_version: b.fw_version, offset: b.region.offset, size: b.region.size };
                n += 1;
            }
            None if t == ty::SYS_DRV || t == ty::SOS => return Err(PlanError::Missing(t)),
            None => {}
        }
    }
    Ok(n)
}

/// The single sub-binary of type `t` (e.g. `ty::TOC`), as (offset, size).
pub fn find(file: &[u8], t: u32) -> Result<Option<(u32, u32)>, PlanError> {
    let psp = PspFw::parse(file).map_err(PlanError::Fw)?;
    let mut found = None;
    for b in psp.bins() {
        if b.fw_type == t {
            if found.is_some() {
                return Err(PlanError::Duplicate(t));
            }
            if b.region.size == 0 {
                return Err(PlanError::Empty(t));
            }
            if b.region.size > MAX_PIECE {
                return Err(PlanError::TooBig(t));
            }
            found = Some((b.region.offset, b.region.size));
        }
    }
    Ok(found)
}
