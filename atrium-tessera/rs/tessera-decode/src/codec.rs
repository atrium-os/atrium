//! Fixed-layout on-disk structs (core/src/codec.c).
//!
//! The C structs in `tessera/format.h` are packed and little-endian, so the
//! decoded struct is byte-for-byte the on-disk bytes: decoding is "validate,
//! then copy". These functions do the validation; the copy is the caller's
//! (`ffi.rs` for C callers). Sizes and offsets are pinned to format.h by the
//! `layout` table, which `core/fuzz/fuzz_diff.c` checks against `offsetof`.

use crate::crc::stored_matches;
use crate::error::{EBADCRC, EBADMAGIC, ECORRUPT, Result};
use crate::le::{u32_at, u64_at};

pub const SECTOR: usize = 4096;
pub const RECORD_HEADER_SIZE: usize = 64;
pub const BTREE_NODE_HEADER_SIZE: usize = 64;
pub const MANIFEST_HEADER_SIZE: usize = 32;
pub const BLOB_DESCRIPTOR_SIZE: usize = 16;
pub const PACK_INDEX_ENTRY_SIZE: usize = 48;
pub const INODE_RECORD_SIZE: usize = 144;
pub const QUOTA_DOMAIN_SIZE: usize = 128;
pub const REGISTRY_ENTRY_SIZE: usize = 64;
pub const EXTENT_ENTRY_SIZE: usize = 16;
pub const HASH_SIZE: usize = 32;

pub const MAGIC_SUPERBLOCK: &[u8] = b"TESSERA1";
pub const MAGIC_JOURNAL: &[u8] = b"TJOURNAL";
pub const MAGIC_TXR: &[u8] = b"TXR\0";
pub const MAGIC_PACK: &[u8] = b"TPACK\0\0\0";
pub const MAGIC_PACK_END: &[u8] = b"TPACKEND";
pub const MAGIC_BLOB: &[u8] = b"TBLB";
pub const MAGIC_MANIFEST: &[u8] = b"TMFT";
pub const MAGIC_BTREE_NODE: &[u8] = b"TBTR";
pub const PEL_MAGIC: u64 = 0x3150_5645_4C45_5054; // "TPELEV01"
pub const PEL_MAX_EXTENTS: u32 = 253;

/// Byte offsets of the fields the decoders read (format.h).
pub mod off {
    pub const SB_CRC32: usize = 4060;
    pub const SB_TOTAL_SECTORS: usize = 48;
    pub const SB_JOURNAL_START: usize = 64;
    pub const SB_JOURNAL_LENGTH: usize = 72;
    pub const SB_PACK_ZONE_START: usize = 128;
    pub const SB_PACK_ZONE_LENGTH: usize = 136;
    pub const SB_META_RESERVE_START: usize = 184;
    pub const SB_META_RESERVE_LENGTH: usize = 192;
    pub const SB_META_RESERVE_BUMP: usize = 200;
    pub const JH_CRC32: usize = 48;
    pub const JH_HEAD_BLOCK: usize = 32;
    pub const JH_TAIL_BLOCK: usize = 40;
    pub const RH_RECORD_TYPE: usize = 4;
    pub const RH_BODY_LENGTH: usize = 16;
    pub const RH_BLOCK_COUNT: usize = 20;
    pub const RH_CRC32_BODY: usize = 24;
    pub const RH_CRC32_HEADER: usize = 28;
    pub const PH_PACK_ID: usize = 16;
    pub const PH_BLOB_COUNT: usize = 48;
    pub const PH_INDEX_BLOCKS: usize = 52;
    pub const PH_BLOOM_BYTES: usize = 56;
    pub const PH_BLOOM_HASH_COUNT: usize = 60;
    pub const PH_DATA_OFFSET: usize = 64;
    pub const PH_DATA_LENGTH: usize = 72;
    pub const PH_TOTAL_PACK_BYTES: usize = 80;
    pub const PH_CRC32: usize = 88;
    pub const PF_BLOB_COUNT_CHECK: usize = 8;
    pub const PF_CRC32_PACK: usize = 12;
    pub const IE_DATA_OFFSET: usize = 32;
    pub const IE_DATA_SIZE: usize = 40;
    pub const MH_KIND: usize = 6;
    pub const MH_LOGICAL_SIZE: usize = 8;
    pub const MH_ENTRY_COUNT: usize = 24;
    pub const BH_NODE_KIND: usize = 6;
    pub const BH_TREE_KIND: usize = 7;
    pub const BH_ENTRY_COUNT: usize = 8;
    pub const BH_KEY_SIZE: usize = 12;
    pub const BH_VALUE_SIZE: usize = 16;
    pub const BH_CRC32: usize = 24;
    pub const PEL_EXTENT_COUNT: usize = 12;
    pub const PEL_TOTAL_LENGTH: usize = 16;
    pub const PEL_EXTENTS: usize = 32;
    pub const PEL_CRC32: usize = 4092;
}

fn magic(b: &[u8], m: &[u8]) -> bool {
    b.get(..m.len()) == Some(m)
}

/// Magic and header CRC, both mapped to ECORRUPT (codec.c's convention for
/// every struct except the pack extent list).
fn magic_crc(b: &[u8], m: &[u8], crc_off: usize) -> Result<()> {
    if !magic(b, m) || !stored_matches(b, crc_off) {
        return Err(ECORRUPT);
    }
    Ok(())
}

fn magic_only(b: &[u8], m: &[u8]) -> Result<()> {
    if magic(b, m) { Ok(()) } else { Err(ECORRUPT) }
}

pub fn superblock(b: &[u8; SECTOR]) -> Result<()> {
    magic_crc(b, MAGIC_SUPERBLOCK, off::SB_CRC32)
}
pub fn journal_header(b: &[u8; SECTOR]) -> Result<()> {
    magic_crc(b, MAGIC_JOURNAL, off::JH_CRC32)
}
pub fn record_header(b: &[u8; RECORD_HEADER_SIZE]) -> Result<()> {
    magic_crc(b, MAGIC_TXR, off::RH_CRC32_HEADER)
}
pub fn pack_header(b: &[u8; SECTOR]) -> Result<()> {
    magic_crc(b, MAGIC_PACK, off::PH_CRC32)
}
pub fn pack_footer(b: &[u8; SECTOR]) -> Result<()> {
    magic_only(b, MAGIC_PACK_END)
}
pub fn blob_descriptor(b: &[u8; BLOB_DESCRIPTOR_SIZE]) -> Result<()> {
    magic_only(b, MAGIC_BLOB)
}
pub fn manifest_header(b: &[u8; MANIFEST_HEADER_SIZE]) -> Result<()> {
    magic_only(b, MAGIC_MANIFEST)
}
pub fn btree_node_header(b: &[u8; BTREE_NODE_HEADER_SIZE]) -> Result<()> {
    magic_crc(b, MAGIC_BTREE_NODE, off::BH_CRC32)
}

/// Pack extent list (a 4 KiB sector). codec.c copied the whole sector onto the
/// stack before checking it, the largest kernel stack frame in Tessera (4160
/// bytes, kmod audit T2); this reads the fields in place.
pub fn pack_extent_list(b: &[u8; SECTOR]) -> Result<()> {
    if u64_at(b, 0) != Some(PEL_MAGIC) {
        return Err(EBADMAGIC);
    }
    if !stored_matches(b, off::PEL_CRC32) {
        return Err(EBADCRC);
    }
    let count = u32_at(b, off::PEL_EXTENT_COUNT).ok_or(ECORRUPT)?;
    if count == 0 || count > PEL_MAX_EXTENTS {
        return Err(ECORRUPT);
    }
    let mut sum: u64 = 0;
    let mut pos = off::PEL_EXTENTS;
    for _ in 0..count {
        // tessera_pack_extent_t: start_sector, length_sectors
        let len = u64_at(b, pos + 8).ok_or(ECORRUPT)?;
        if len == 0 {
            return Err(ECORRUPT);
        }
        sum = sum.wrapping_add(len); // C: uint64 sum, wraps
        pos += 16;
    }
    if Some(sum) != u64_at(b, off::PEL_TOTAL_LENGTH) {
        return Err(ECORRUPT);
    }
    Ok(())
}

/// Layout facts the C side checks against `offsetof`/`sizeof` (fuzz_diff.c).
/// Order is part of that contract.
pub const LAYOUT: &[usize] = &[
    off::SB_CRC32, off::JH_CRC32, off::JH_HEAD_BLOCK, off::JH_TAIL_BLOCK,
    off::RH_RECORD_TYPE, off::RH_BODY_LENGTH, off::RH_BLOCK_COUNT,
    off::RH_CRC32_BODY, off::RH_CRC32_HEADER,
    off::PH_PACK_ID, off::PH_BLOB_COUNT, off::PH_INDEX_BLOCKS, off::PH_BLOOM_BYTES,
    off::PH_BLOOM_HASH_COUNT, off::PH_DATA_OFFSET, off::PH_DATA_LENGTH,
    off::PH_TOTAL_PACK_BYTES, off::PH_CRC32,
    off::PF_BLOB_COUNT_CHECK, off::PF_CRC32_PACK,
    off::IE_DATA_OFFSET, off::IE_DATA_SIZE,
    off::MH_KIND, off::MH_LOGICAL_SIZE, off::MH_ENTRY_COUNT,
    off::BH_NODE_KIND, off::BH_TREE_KIND, off::BH_ENTRY_COUNT, off::BH_KEY_SIZE,
    off::BH_VALUE_SIZE, off::BH_CRC32,
    off::PEL_EXTENT_COUNT, off::PEL_TOTAL_LENGTH, off::PEL_EXTENTS, off::PEL_CRC32,
    RECORD_HEADER_SIZE, BTREE_NODE_HEADER_SIZE, MANIFEST_HEADER_SIZE,
    BLOB_DESCRIPTOR_SIZE, PACK_INDEX_ENTRY_SIZE, INODE_RECORD_SIZE,
    QUOTA_DOMAIN_SIZE, REGISTRY_ENTRY_SIZE, EXTENT_ENTRY_SIZE,
    off::SB_TOTAL_SECTORS, off::SB_JOURNAL_START, off::SB_JOURNAL_LENGTH,
    off::SB_PACK_ZONE_START, off::SB_PACK_ZONE_LENGTH, off::SB_META_RESERVE_START,
    off::SB_META_RESERVE_LENGTH, off::SB_META_RESERVE_BUMP,
];

/// A superblock's regions, in mkfs order: the two superblock copies (sectors 0
/// and 1), the journal, the metadata reserve, the pack zone, all inside
/// `total_sectors`, which is inside the device (`device_sectors`, 0 if not
/// known). Returns the pack zone `[start, end)`: the only sectors the
/// free-extent allocator may hand out. The CRC proves the superblock is the
/// one written, not that it is sane; without this a free list or pack entry
/// over the journal or a superblock is allocated and overwritten.
///
/// A volume with no metadata reserve (length 0) or no recorded pack zone
/// (length 0) is accepted; the zone is then everything after the journal and
/// reserve.
pub fn superblock_layout(sb: &[u8; SECTOR], device_sectors: u64) -> Result<(u64, u64)> {
    let f = |o| u64_at(sb, o).ok_or(ECORRUPT);
    let end = |a: u64, n: u64| a.checked_add(n).ok_or(ECORRUPT);
    let total = f(off::SB_TOTAL_SECTORS)?;
    if device_sectors != 0 && total > device_sectors {
        return Err(ECORRUPT);
    }
    let (js, jl) = (f(off::SB_JOURNAL_START)?, f(off::SB_JOURNAL_LENGTH)?);
    let je = end(js, jl)?;
    if js < 2 || jl < 2 || je > total {
        return Err(ECORRUPT);
    }
    let (ms, ml) = (f(off::SB_META_RESERVE_START)?, f(off::SB_META_RESERVE_LENGTH)?);
    let mut floor = je;
    if ml != 0 {
        let me = end(ms, ml)?;
        let bump = f(off::SB_META_RESERVE_BUMP)?;
        if ms < je || me > total || bump < ms || bump > me {
            return Err(ECORRUPT);
        }
        floor = me;
    }
    let (ps, pl) = (f(off::SB_PACK_ZONE_START)?, f(off::SB_PACK_ZONE_LENGTH)?);
    if pl == 0 {
        return if floor < total { Ok((floor, total)) } else { Err(ECORRUPT) };
    }
    let pe = end(ps, pl)?;
    if ps < floor || pe > total {
        return Err(ECORRUPT);
    }
    Ok((ps, pe))
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    /// mkfs's layout for a 100 000-sector volume: journal 4..1028, reserve
    /// 1028..9220, pack zone 9220..100000.
    fn sb() -> [u8; SECTOR] {
        let mut b = [0u8; SECTOR];
        for (o, v) in [
            (off::SB_TOTAL_SECTORS, 100_000u64),
            (off::SB_JOURNAL_START, 4),
            (off::SB_JOURNAL_LENGTH, 1024),
            (off::SB_META_RESERVE_START, 1028),
            (off::SB_META_RESERVE_LENGTH, 8192),
            (off::SB_META_RESERVE_BUMP, 1100),
            (off::SB_PACK_ZONE_START, 9220),
            (off::SB_PACK_ZONE_LENGTH, 90_780),
        ] {
            b[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    fn with(o: usize, v: u64) -> [u8; SECTOR] {
        let mut b = sb();
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
        b
    }

    #[test]
    fn good_and_boundaries() {
        assert_eq!(superblock_layout(&sb(), 0), Ok((9220, 100_000)));
        assert_eq!(superblock_layout(&sb(), 100_000), Ok((9220, 100_000)));
        assert_eq!(superblock_layout(&sb(), 99_999), Err(ECORRUPT));
        // bump at both ends of the reserve
        assert!(superblock_layout(&with(off::SB_META_RESERVE_BUMP, 1028), 0).is_ok());
        assert!(superblock_layout(&with(off::SB_META_RESERVE_BUMP, 9220), 0).is_ok());
        assert_eq!(superblock_layout(&with(off::SB_META_RESERVE_BUMP, 9221), 0), Err(ECORRUPT));
        assert_eq!(superblock_layout(&with(off::SB_META_RESERVE_BUMP, 1027), 0), Err(ECORRUPT));
        // no reserve, no recorded zone
        let mut b = with(off::SB_META_RESERVE_LENGTH, 0);
        b[off::SB_PACK_ZONE_LENGTH..off::SB_PACK_ZONE_LENGTH + 8].fill(0);
        assert_eq!(superblock_layout(&b, 0), Ok((1028, 100_000)));
    }

    #[test]
    fn overlaps_and_wraps() {
        for (o, v) in [
            (off::SB_JOURNAL_START, 1),             // over SB-B
            (off::SB_JOURNAL_LENGTH, 1),
            (off::SB_JOURNAL_LENGTH, u64::MAX),     // wraps
            (off::SB_META_RESERVE_START, 1027),     // into the journal
            (off::SB_META_RESERVE_LENGTH, u64::MAX - 1000),
            (off::SB_PACK_ZONE_START, 9219),        // into the reserve
            (off::SB_PACK_ZONE_START, 0),           // over the superblocks
            (off::SB_PACK_ZONE_LENGTH, 90_781),     // past the end
            (off::SB_PACK_ZONE_LENGTH, u64::MAX),
            (off::SB_TOTAL_SECTORS, 99_999),
            (off::SB_TOTAL_SECTORS, 0),
        ] {
            assert_eq!(superblock_layout(&with(o, v), 0), Err(ECORRUPT), "off {o} = {v}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc::crc32;

    fn stamp(b: &mut [u8], crc_off: usize) {
        let c = crc32(&b[..crc_off]);
        b[crc_off..crc_off + 4].copy_from_slice(&c.to_le_bytes());
    }

    #[test]
    fn superblock_magic_and_crc() {
        let mut b = [0u8; SECTOR];
        assert_eq!(superblock(&b), Err(ECORRUPT));
        b[..8].copy_from_slice(MAGIC_SUPERBLOCK);
        assert_eq!(superblock(&b), Err(ECORRUPT));
        stamp(&mut b, off::SB_CRC32);
        assert_eq!(superblock(&b), Ok(()));
        b[100] ^= 1;
        assert_eq!(superblock(&b), Err(ECORRUPT));
    }

    #[test]
    fn pel_rules() {
        let mut b = [0u8; SECTOR];
        assert_eq!(pack_extent_list(&b), Err(EBADMAGIC));
        b[..8].copy_from_slice(&PEL_MAGIC.to_le_bytes());
        assert_eq!(pack_extent_list(&b), Err(EBADCRC));
        stamp(&mut b, off::PEL_CRC32);
        assert_eq!(pack_extent_list(&b), Err(ECORRUPT)); // count 0
        b[12..16].copy_from_slice(&2u32.to_le_bytes());
        b[40..48].copy_from_slice(&3u64.to_le_bytes());
        b[56..64].copy_from_slice(&4u64.to_le_bytes());
        b[16..24].copy_from_slice(&7u64.to_le_bytes());
        stamp(&mut b, off::PEL_CRC32);
        assert_eq!(pack_extent_list(&b), Ok(()));
        b[12..16].copy_from_slice(&254u32.to_le_bytes());
        stamp(&mut b, off::PEL_CRC32);
        assert_eq!(pack_extent_list(&b), Err(ECORRUPT));
    }
}
