/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * fw.rs — parse the headers of AMD GPU firmware files (gc_12_0_1_*.bin,
 * psp_14_0_3_*.bin, smu_14_0_3.bin, sdma_7_0_1.bin, dcn_4_0_1_dmcub.bin) for
 * real-silicon bring-up.
 *
 * Only the container format is read: the headers say where the microcode and
 * data regions start and how big they are. The microcode itself is opaque and
 * is handed to the PSP / engine as-is. Nothing here looks inside it (the
 * firmware licence forbids disassembly; memory amdgpu-firmware-license).
 *
 * The layouts are the documented on-disk structs (Linux's MIT-licensed
 * amdgpu_ucode.h describes the same files); every field is little-endian.
 *
 * The file came from disk via firmware(9), so it is trusted only as far as
 * its signature check on the PSP goes. Every offset and size here is checked
 * against the file before a region is handed out, so a corrupt or truncated
 * file gives an error, never an out-of-bounds slice. Pure, no_std, no unsafe:
 * host-tested by atrium-gpu-amd/rs-host-tests.
 */

/// Why a firmware file was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwError {
    /// Shorter than the header it claims to have.
    Truncated,
    /// `size_bytes` in the header is not the file's length.
    SizeMismatch,
    /// A region (offset, size) lies outside the file or overflows.
    RegionOutOfBounds,
    /// Header version this parser does not know.
    UnknownVersion { major: u16, minor: u16 },
    /// Too many PSP sub-binaries for the table we read.
    TooManyBins,
}

#[inline]
fn le16(b: &[u8], off: usize) -> Result<u16, FwError> {
    let s = b.get(off..off + 2).ok_or(FwError::Truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

#[inline]
fn le32(b: &[u8], off: usize) -> Result<u32, FwError> {
    let s = b.get(off..off + 4).ok_or(FwError::Truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// A byte range inside a firmware file, checked to lie within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub offset: u32,
    pub size: u32,
}

impl Region {
    fn checked(file_len: usize, offset: u32, size: u32) -> Result<Region, FwError> {
        let end = (offset as u64) + (size as u64);
        if end > file_len as u64 {
            return Err(FwError::RegionOutOfBounds);
        }
        Ok(Region { offset, size })
    }

    /// The region's bytes. Cannot fail: the region was checked against this
    /// file when it was made (pass the same file).
    pub fn bytes<'a>(&self, file: &'a [u8]) -> Option<&'a [u8]> {
        file.get(self.offset as usize..(self.offset as usize + self.size as usize))
    }
}

/// The 32-byte header every amdgpu firmware file starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommonHeader {
    pub size_bytes: u32,
    pub header_size_bytes: u32,
    pub header_version_major: u16,
    pub header_version_minor: u16,
    pub ip_version_major: u16,
    pub ip_version_minor: u16,
    pub ucode_version: u32,
    pub ucode_size_bytes: u32,
    pub ucode_array_offset_bytes: u32,
    pub crc32: u32,
}

pub const COMMON_HEADER_LEN: usize = 32;

impl CommonHeader {
    pub fn parse(file: &[u8]) -> Result<CommonHeader, FwError> {
        if file.len() < COMMON_HEADER_LEN {
            return Err(FwError::Truncated);
        }
        let h = CommonHeader {
            size_bytes: le32(file, 0)?,
            header_size_bytes: le32(file, 4)?,
            header_version_major: le16(file, 8)?,
            header_version_minor: le16(file, 10)?,
            ip_version_major: le16(file, 12)?,
            ip_version_minor: le16(file, 14)?,
            ucode_version: le32(file, 16)?,
            ucode_size_bytes: le32(file, 20)?,
            ucode_array_offset_bytes: le32(file, 24)?,
            crc32: le32(file, 28)?,
        };
        if h.size_bytes as usize != file.len() {
            return Err(FwError::SizeMismatch);
        }
        if (h.header_size_bytes as usize) < COMMON_HEADER_LEN
            || h.header_size_bytes as usize > file.len()
        {
            return Err(FwError::Truncated);
        }
        Ok(h)
    }

    /// The main microcode array the common header names.
    pub fn ucode(&self, file: &[u8]) -> Result<Region, FwError> {
        Region::checked(file.len(), self.ucode_array_offset_bytes, self.ucode_size_bytes)
    }

    fn version(&self) -> (u16, u16) {
        (self.header_version_major, self.header_version_minor)
    }
}

/// RS64 CP microcode (PFP, ME, MEC on GFX11/12): `gfx_firmware_header_v2_0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rs64Fw {
    pub common: CommonHeader,
    pub feature_version: u32,
    pub ucode: Region,
    pub data: Region,
    pub ucode_start_addr: u64,
}

impl Rs64Fw {
    pub fn parse(file: &[u8]) -> Result<Rs64Fw, FwError> {
        let common = CommonHeader::parse(file)?;
        if common.version().0 != 2 {
            let (major, minor) = common.version();
            return Err(FwError::UnknownVersion { major, minor });
        }
        let b = COMMON_HEADER_LEN;
        need_header(&common, b + 28)?;
        Ok(Rs64Fw {
            common,
            feature_version: le32(file, b)?,
            ucode: Region::checked(file.len(), le32(file, b + 8)?, le32(file, b + 4)?)?,
            data: Region::checked(file.len(), le32(file, b + 16)?, le32(file, b + 12)?)?,
            ucode_start_addr: (le32(file, b + 20)? as u64) | ((le32(file, b + 24)? as u64) << 32),
        })
    }
}

/// MES scheduler firmware: `mes_firmware_header_v1_0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MesFw {
    pub common: CommonHeader,
    pub ucode_version: u32,
    pub ucode: Region,
    pub data_version: u32,
    pub data: Region,
    pub ucode_start_addr: u64,
    pub data_start_addr: u64,
}

impl MesFw {
    pub fn parse(file: &[u8]) -> Result<MesFw, FwError> {
        let common = CommonHeader::parse(file)?;
        if common.version().0 != 1 {
            let (major, minor) = common.version();
            return Err(FwError::UnknownVersion { major, minor });
        }
        let b = COMMON_HEADER_LEN;
        need_header(&common, b + 40)?;
        Ok(MesFw {
            common,
            ucode_version: le32(file, b)?,
            ucode: Region::checked(file.len(), le32(file, b + 8)?, le32(file, b + 4)?)?,
            data_version: le32(file, b + 12)?,
            data: Region::checked(file.len(), le32(file, b + 20)?, le32(file, b + 16)?)?,
            ucode_start_addr: (le32(file, b + 24)? as u64) | ((le32(file, b + 28)? as u64) << 32),
            data_start_addr: (le32(file, b + 32)? as u64) | ((le32(file, b + 36)? as u64) << 32),
        })
    }
}

/// IMU firmware (GFX11/12 power-up microcontroller): `imu_firmware_header_v1_0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImuFw {
    pub common: CommonHeader,
    pub iram: Region,
    pub dram: Region,
}

impl ImuFw {
    pub fn parse(file: &[u8]) -> Result<ImuFw, FwError> {
        let common = CommonHeader::parse(file)?;
        if common.version().0 != 1 {
            let (major, minor) = common.version();
            return Err(FwError::UnknownVersion { major, minor });
        }
        let b = COMMON_HEADER_LEN;
        need_header(&common, b + 16)?;
        // IRAM follows the ucode array start; DRAM follows IRAM. The header
        // gives sizes only for v1.0, with the offsets relative to the array.
        let base = common.ucode_array_offset_bytes;
        let iram_size = le32(file, b)?;
        let dram_size = le32(file, b + 8)?;
        let iram = Region::checked(file.len(), base, iram_size)?;
        let dram_off = base.checked_add(iram_size).ok_or(FwError::RegionOutOfBounds)?;
        let dram = Region::checked(file.len(), dram_off, dram_size)?;
        Ok(ImuFw { common, iram, dram })
    }
}

/// SDMA firmware on SDMA 6/7: `sdma_firmware_header_v3_0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdmaFw {
    pub common: CommonHeader,
    pub feature_version: u32,
    pub ucode: Region,
}

impl SdmaFw {
    pub fn parse(file: &[u8]) -> Result<SdmaFw, FwError> {
        let common = CommonHeader::parse(file)?;
        if common.version().0 != 3 {
            let (major, minor) = common.version();
            return Err(FwError::UnknownVersion { major, minor });
        }
        let b = COMMON_HEADER_LEN;
        need_header(&common, b + 12)?;
        Ok(SdmaFw {
            common,
            feature_version: le32(file, b)?,
            ucode: Region::checked(file.len(), le32(file, b + 4)?, le32(file, b + 8)?)?,
        })
    }
}

/// One sub-binary of a PSP v2 file (SOS, sys driver, KDB, SPL, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PspBin {
    pub fw_type: u32,
    pub fw_version: u32,
    pub region: Region,
}

pub const PSP_MAX_BINS: usize = 16;

/// PSP bootloader/SOS file (`psp_firmware_header_v2_0` / `_v2_1`): a table
/// of typed sub-binaries the driver hands to the PSP bootloader one by one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PspFw {
    pub common: CommonHeader,
    /// v2.1 only: index of the auxiliary binary, if any.
    pub aux_index: Option<u32>,
    pub count: usize,
    pub bins: [PspBin; PSP_MAX_BINS],
}

impl PspFw {
    pub fn parse(file: &[u8]) -> Result<PspFw, FwError> {
        let common = CommonHeader::parse(file)?;
        let (major, minor) = common.version();
        let b = COMMON_HEADER_LEN;
        let (count, aux_index, table) = match (major, minor) {
            (2, 0) => (le32(file, b)?, None, b + 4),
            (2, 1) => (le32(file, b)?, Some(le32(file, b + 4)?), b + 8),
            _ => return Err(FwError::UnknownVersion { major, minor }),
        };
        let count = count as usize;
        if count > PSP_MAX_BINS {
            return Err(FwError::TooManyBins);
        }
        // The descriptor table is a flexible array: header_size_bytes covers
        // only the fixed part (36 bytes for v2.0 on psp_14_0_3_sos.bin), so
        // the table is checked against the file, not the header size.
        if table + count * 16 > file.len() {
            return Err(FwError::Truncated);
        }
        let empty = PspBin { fw_type: 0, fw_version: 0, region: Region { offset: 0, size: 0 } };
        let mut bins = [empty; PSP_MAX_BINS];
        for (i, slot) in bins.iter_mut().enumerate().take(count) {
            let e = table + i * 16;
            // Sub-binary offsets are relative to the ucode array.
            let off = common
                .ucode_array_offset_bytes
                .checked_add(le32(file, e + 8)?)
                .ok_or(FwError::RegionOutOfBounds)?;
            *slot = PspBin {
                fw_type: le32(file, e)?,
                fw_version: le32(file, e + 4)?,
                region: Region::checked(file.len(), off, le32(file, e + 12)?)?,
            };
        }
        if let Some(a) = aux_index {
            if a as usize >= count && a != u32::MAX {
                return Err(FwError::RegionOutOfBounds);
            }
        }
        Ok(PspFw { common, aux_index, count, bins })
    }

    pub fn bins(&self) -> &[PspBin] {
        &self.bins[..self.count]
    }
}

fn need_header(common: &CommonHeader, len: usize) -> Result<(), FwError> {
    if (common.header_size_bytes as usize) < len || (common.size_bytes as usize) < len {
        return Err(FwError::Truncated);
    }
    Ok(())
}
