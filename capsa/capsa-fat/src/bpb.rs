//! The boot sector: which FAT, and where everything is.

use crate::{le16, le32, Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatType {
    Fat12,
    Fat16,
    Fat32,
}

/// Everything derived from the boot sector, checked against itself and the
/// device size. All offsets are bytes from the start of the volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Geometry {
    pub fat_type: FatType,
    pub bytes_per_sector: u32,
    pub cluster_size: u32,
    /// Data clusters; valid cluster numbers are `2..clusters + 2`.
    pub clusters: u32,
    pub fat_offset: u64,
    /// Bytes per FAT copy.
    pub fat_bytes: u64,
    pub num_fats: u8,
    /// FAT32: when mirroring is off, the one FAT in use.
    pub active_fat: Option<u8>,
    /// FAT12/16: the fixed root directory.
    pub root_offset: u64,
    pub root_entries: u32,
    /// FAT32: the root directory's first cluster.
    pub root_cluster: u32,
    pub data_offset: u64,
    /// FAT32: the FSInfo sector's offset, if the volume has one.
    pub fsinfo_offset: Option<u64>,
    pub volume_bytes: u64,
    pub label: Option<String>,
}

impl Geometry {
    /// Parse and check a boot sector (`bs`: at least 512 bytes) for a
    /// device of `dev_len` bytes.
    pub fn parse(bs: &[u8], dev_len: u64) -> Result<Self> {
        if bs.len() < 512 {
            return Err(Error::NotFat("short boot sector"));
        }
        if bs[510] != 0x55 || bs[511] != 0xAA {
            return Err(Error::NotFat("no boot signature"));
        }
        if bs.get(3..11) == Some(b"EXFAT   ") {
            return Err(Error::NotFat("exFAT"));
        }
        if bs[0] != 0xEB && bs[0] != 0xE9 {
            return Err(Error::NotFat("no jump instruction"));
        }
        let bps = u32::from(le16(bs, 0x0B));
        if !matches!(bps, 512 | 1024 | 2048 | 4096) {
            return Err(Error::NotFat("bytes per sector"));
        }
        let spc = u32::from(bs[0x0D]);
        if spc == 0 || !spc.is_power_of_two() || spc > 128 {
            return Err(Error::NotFat("sectors per cluster"));
        }
        let cluster_size = bps * spc;
        // Larger than 256 KiB is outside every spec revision.
        if cluster_size > 256 * 1024 {
            return Err(Error::NotFat("cluster size"));
        }
        let reserved = u64::from(le16(bs, 0x0E));
        let num_fats = bs[0x10];
        let root_entries = u32::from(le16(bs, 0x11));
        let ts16 = u64::from(le16(bs, 0x13));
        let fs16 = u64::from(le16(bs, 0x16));
        let ts32 = u64::from(le32(bs, 0x20));
        if reserved == 0 || num_fats == 0 || num_fats > 4 {
            return Err(Error::NotFat("reserved sectors or FAT count"));
        }
        let fat_sectors = if fs16 != 0 { fs16 } else { u64::from(le32(bs, 0x24)) };
        let total = if ts16 != 0 { ts16 } else { ts32 };
        if fat_sectors == 0 || total == 0 {
            return Err(Error::NotFat("FAT size or total sectors"));
        }
        let bps64 = u64::from(bps);
        let root_sectors = (u64::from(root_entries) * 32).div_ceil(bps64);
        let meta = reserved + u64::from(num_fats) * fat_sectors + root_sectors;
        let data_sectors = total.checked_sub(meta).ok_or(Error::NotFat("metadata larger than the volume"))?;
        let clusters64 = data_sectors / u64::from(spc);
        let fat_type = if clusters64 < 4085 {
            FatType::Fat12
        } else if clusters64 < 65525 {
            FatType::Fat16
        } else {
            FatType::Fat32
        };
        // FAT32 tops out at 2^28 - 11 clusters.
        if clusters64 == 0 || clusters64 > 0x0FFF_FFF5 {
            return Err(Error::NotFat("cluster count"));
        }
        let clusters = clusters64 as u32;
        let fat_bytes = fat_sectors * bps64;
        let need = match fat_type {
            FatType::Fat12 => (u64::from(clusters) + 2) * 3 / 2 + 1,
            FatType::Fat16 => (u64::from(clusters) + 2) * 2,
            FatType::Fat32 => (u64::from(clusters) + 2) * 4,
        };
        if fat_bytes < need {
            return Err(Error::NotFat("FAT too small for the clusters"));
        }
        let (root_cluster, active_fat, fsinfo_offset, label_at) = if fat_type == FatType::Fat32 {
            if root_entries != 0 || fs16 != 0 {
                return Err(Error::NotFat("FAT32 with FAT16 fields"));
            }
            if le16(bs, 0x2A) != 0 {
                return Err(Error::NotFat("FAT32 version"));
            }
            let flags = le16(bs, 0x28);
            let active = (flags & 0x80 != 0).then_some((flags & 0x0F) as u8);
            if active.is_some_and(|a| a >= num_fats) {
                return Err(Error::NotFat("active FAT"));
            }
            let rc = le32(bs, 0x2C) & 0x0FFF_FFFF;
            if rc < 2 || rc >= clusters + 2 {
                return Err(Error::NotFat("root cluster"));
            }
            let fsi = u64::from(le16(bs, 0x30));
            let fsinfo = (fsi != 0 && fsi != 0xFFFF && fsi < reserved).then_some(fsi * bps64);
            (rc, active, fsinfo, 0x47)
        } else {
            if root_entries == 0 {
                return Err(Error::NotFat("no root directory"));
            }
            (0, None, None, 0x2B)
        };
        let volume_bytes = total * bps64;
        if volume_bytes > dev_len {
            return Err(Error::NotFat("volume larger than the device"));
        }
        let ext_sig = if fat_type == FatType::Fat32 { bs[0x42] } else { bs[0x26] };
        let label = if ext_sig == 0x29 { bs.get(label_at..label_at + 11).and_then(crate::dir::label) } else { None };
        Ok(Geometry {
            fat_type,
            bytes_per_sector: bps,
            cluster_size,
            clusters,
            fat_offset: reserved * bps64,
            fat_bytes,
            num_fats,
            active_fat,
            root_offset: (reserved + u64::from(num_fats) * fat_sectors) * bps64,
            root_entries,
            root_cluster,
            data_offset: meta * bps64,
            fsinfo_offset,
            volume_bytes,
            label,
        })
    }

    /// The byte offset of cluster `c` (2..clusters+2).
    pub fn cluster_offset(&self, c: u32) -> Option<u64> {
        if c < 2 || c >= self.clusters + 2 {
            return None;
        }
        Some(self.data_offset + u64::from(c - 2) * u64::from(self.cluster_size))
    }

    pub fn valid_cluster(&self, c: u32) -> bool {
        c >= 2 && c < self.clusters + 2
    }
}
