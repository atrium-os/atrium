/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * ip_fw.rs — which microcode regions go to the PSP with GFX_CMD_ID_LOAD_IP_FW,
 * with which PSP fw type, in which order, for a part whose PSP does RLC
 * autoload (stage 3 of docs/spec/r9700-bare-metal.md).
 *
 * Inputs are the firmware files the driver picked by discovered IP version
 * (smu_<MP1>.bin, sdma_<SDMA0>.bin, gc_<GC>_{pfp,me,mec,uni_mes,imu,rlc}.bin).
 * Only their container headers are read (fw.rs); the payloads are opaque.
 *
 * Order and fw types are the hardware interface as Linux's MIT-licensed
 * amdgpu_psp.c (psp_load_non_psp_fw, psp_get_fw_type), amdgpu_ucode.c
 * (amdgpu_ucode_init_single_fw) and psp_gfx_if.h use it on PSP 14 with
 * autoload: SMU first, then amdgpu's ucode-id order, RLC_G last, after which
 * the driver sends AUTOLOAD_RLC. MES is the unified MES image for both pipes
 * (amdgpu's default, uni_mes=1).
 */

use crate::fw::{self, FwError, ImuFw, MesFw, Region, RlcFw, Rs64Fw, SdmaFw};

/// PSP fw types for LOAD_IP_FW (psp_gfx_if.h enum psp_gfx_fw_type).
pub mod ty {
    pub const RLC_V: u32 = 7;
    pub const RLC_G: u32 = 8;
    pub const SMU: u32 = 18;
    pub const RLC_RESTORE_LIST_GPM_MEM: u32 = 20;
    pub const RLC_RESTORE_LIST_SRM_MEM: u32 = 21;
    pub const RLC_RESTORE_LIST_SRM_CNTL: u32 = 22;
    pub const RLC_P: u32 = 25;
    pub const RLC_IRAM: u32 = 26;
    pub const GLOBAL_TAP_DELAYS: u32 = 27;
    pub const SE0_TAP_DELAYS: u32 = 28;
    pub const SE1_TAP_DELAYS: u32 = 29;
    pub const CP_MES: u32 = 33;
    pub const MES_STACK: u32 = 34;
    pub const RLC_DRAM_BOOT: u32 = 48;
    pub const SE2_TAP_DELAYS: u32 = 65;
    pub const SE3_TAP_DELAYS: u32 = 66;
    pub const IMU_I: u32 = 68;
    pub const IMU_D: u32 = 69;
    pub const SDMA_UCODE_TH0: u32 = 71;
    pub const CP_MES_KIQ: u32 = 81;
    pub const MES_KIQ_STACK: u32 = 82;
    pub const RS64_PFP: u32 = 87;
    pub const RS64_ME: u32 = 88;
    pub const RS64_MEC: u32 = 89;
    pub const RS64_PFP_P0_STACK: u32 = 90;
    pub const RS64_ME_P0_STACK: u32 = 92;
    pub const RS64_MEC_P0_STACK: u32 = 94;
    pub const RS64_MEC_P1_STACK: u32 = 95;
    pub const RLX6_UCODE_CORE1: u32 = 98;
    pub const RLX6_DRAM_BOOT_CORE1: u32 = 99;
}

/// The files, by index into the caller's array.
pub mod file {
    pub const SMU: u32 = 0;
    pub const SDMA: u32 = 1;
    pub const PFP: u32 = 2;
    pub const ME: u32 = 3;
    pub const MEC: u32 = 4;
    pub const MES: u32 = 5;
    pub const IMU: u32 = 6;
    pub const RLC: u32 = 7;
    pub const COUNT: usize = 8;
}

/// One LOAD_IP_FW: `size` bytes at `offset` of file `file`, as PSP type
/// `fw_type`. The last item is RLC_G; AUTOLOAD_RLC follows it.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Item {
    pub file: u32,
    pub fw_type: u32,
    pub offset: u32,
    pub size: u32,
}

/// SMU + SDMA + 7 CP + 4 MES + 2 IMU + 5 tap + 3 lists + 4 RLC images + RLC-P/V + RLC_G.
pub const MAX_ITEMS: usize = 32;

/// No single image the PSP takes is bigger than this (the largest today,
/// MES data, is 512 KiB); a bigger one means a misread header.
pub const MAX_ITEM: u32 = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// File `.0` was refused by the header parser.
    Fw(u32, FwError),
    /// Item of PSP type `.0` is larger than MAX_ITEM.
    TooBig(u32),
    /// Item of PSP type `.0`, which the PSP needs, is empty.
    Empty(u32),
}

struct Out<'a> {
    items: &'a mut [Item; MAX_ITEMS],
    n: usize,
}

impl Out<'_> {
    fn push(&mut self, file: u32, fw_type: u32, r: Region, required: bool) -> Result<(), PlanError> {
        if r.size == 0 {
            return if required { Err(PlanError::Empty(fw_type)) } else { Ok(()) };
        }
        if r.size > MAX_ITEM {
            return Err(PlanError::TooBig(fw_type));
        }
        // MAX_ITEMS covers every push below; cannot overflow.
        self.items[self.n] = Item { file, fw_type, offset: r.offset, size: r.size };
        self.n += 1;
        Ok(())
    }
}

/// Fill `items` in load order from the eight files (indexed by `file::*`);
/// returns the count.
pub fn plan(files: &[&[u8]; file::COUNT], items: &mut [Item; MAX_ITEMS]) -> Result<usize, PlanError> {
    let f = |i: u32| files[i as usize];
    let e = |i: u32| move |x: FwError| PlanError::Fw(i, x);

    let smu = fw::smu_image(f(file::SMU)).map_err(e(file::SMU))?;
    let sdma = SdmaFw::parse(f(file::SDMA)).map_err(e(file::SDMA))?;
    let pfp = Rs64Fw::parse(f(file::PFP)).map_err(e(file::PFP))?;
    let me = Rs64Fw::parse(f(file::ME)).map_err(e(file::ME))?;
    let mec = Rs64Fw::parse(f(file::MEC)).map_err(e(file::MEC))?;
    let mes = MesFw::parse(f(file::MES)).map_err(e(file::MES))?;
    let imu = ImuFw::parse(f(file::IMU)).map_err(e(file::IMU))?;
    let rlc = RlcFw::parse(f(file::RLC)).map_err(e(file::RLC))?;

    let mut o = Out { items, n: 0 };
    // SMU first: autoload without PMFW-centralised C-state management.
    o.push(file::SMU, ty::SMU, smu, true)?;
    // Then amdgpu's ucode-id order.
    o.push(file::SDMA, ty::SDMA_UCODE_TH0, sdma.ucode, true)?;
    o.push(file::PFP, ty::RS64_PFP, pfp.ucode, true)?;
    o.push(file::ME, ty::RS64_ME, me.ucode, true)?;
    o.push(file::MEC, ty::RS64_MEC, mec.ucode, true)?;
    o.push(file::PFP, ty::RS64_PFP_P0_STACK, pfp.data, true)?;
    o.push(file::ME, ty::RS64_ME_P0_STACK, me.data, true)?;
    o.push(file::MEC, ty::RS64_MEC_P0_STACK, mec.data, true)?;
    o.push(file::MEC, ty::RS64_MEC_P1_STACK, mec.data, true)?;
    o.push(file::MES, ty::CP_MES, mes.ucode, true)?;
    o.push(file::MES, ty::MES_STACK, mes.data, true)?;
    o.push(file::MES, ty::CP_MES_KIQ, mes.ucode, true)?;
    o.push(file::MES, ty::MES_KIQ_STACK, mes.data, true)?;
    o.push(file::IMU, ty::IMU_I, imu.iram, true)?;
    o.push(file::IMU, ty::IMU_D, imu.dram, true)?;
    let taps = [
        ty::GLOBAL_TAP_DELAYS,
        ty::SE0_TAP_DELAYS,
        ty::SE1_TAP_DELAYS,
        ty::SE2_TAP_DELAYS,
        ty::SE3_TAP_DELAYS,
    ];
    for (t, r) in taps.iter().zip(rlc.tap.iter()) {
        o.push(file::RLC, *t, *r, false)?;
    }
    o.push(file::RLC, ty::RLC_RESTORE_LIST_SRM_CNTL, rlc.srl_cntl, false)?;
    o.push(file::RLC, ty::RLC_RESTORE_LIST_GPM_MEM, rlc.srl_gpm, false)?;
    o.push(file::RLC, ty::RLC_RESTORE_LIST_SRM_MEM, rlc.srl_srm, false)?;
    o.push(file::RLC, ty::RLC_IRAM, rlc.iram, false)?;
    o.push(file::RLC, ty::RLC_DRAM_BOOT, rlc.dram, false)?;
    o.push(file::RLC, ty::RLX6_UCODE_CORE1, rlc.iram_1, false)?;
    o.push(file::RLC, ty::RLX6_DRAM_BOOT_CORE1, rlc.dram_1, false)?;
    o.push(file::RLC, ty::RLC_P, rlc.rlcp, false)?;
    o.push(file::RLC, ty::RLC_V, rlc.rlcv, false)?;
    o.push(file::RLC, ty::RLC_G, rlc.rlc_g, true)?;
    Ok(o.n)
}
