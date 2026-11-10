//! Pack reader (core/src/pack.c, "Reader" half).
//!
//! `open` validates the header, the region geometry and the footer once and
//! returns [`Geometry`]: offsets, not pointers. Every later access re-slices
//! the pack through `get()`, so even a `Geometry` that was corrupted after
//! `open` (it lives in a C struct) cannot read outside the pack buffer.

use crate::codec::{self, BLOB_DESCRIPTOR_SIZE, HASH_SIZE, PACK_INDEX_ENTRY_SIZE, SECTOR, off};
use crate::crc::crc32;
use crate::error::{ECORRUPT, EINVAL, ENOENT, Result};
use crate::le::{range, u32_at, u64_at, usize_of};
use core::cmp::Ordering;

/// Bloom probes per lookup are capped here. The builder uses 7; the count comes
/// off disk and the C reader looped up to 2^32 times on a hostile pack. Testing
/// fewer bits can only turn a "definitely absent" into "might contain", which
/// the caller confirms with `lookup` anyway, so the cap never hides a blob.
pub const MAX_BLOOM_PROBES: u32 = 64;

/// Validated pack layout. `#[repr(C)]`: C keeps it in its reader struct
/// (`tessera_rs_pack_geom_t` in core/src/tessera_rs.h).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Geometry {
    pub index_off: u64,
    pub bloom_off: u64,
    pub bloom_bytes: u64,
    pub data_off: u64,
    pub data_len: u64,
    pub blob_count: u32,
    pub k_hashes: u32,
}

/// `tessera_pack_open`.
pub fn open(data: &[u8]) -> Result<Geometry> {
    let len = data.len();
    if len < SECTOR * 2 {
        return Err(EINVAL);
    }
    let hdr: &[u8; SECTOR] = data.get(..SECTOR).and_then(|h| h.try_into().ok()).ok_or(EINVAL)?;
    codec::pack_header(hdr)?;
    let rd32 = |o| u32_at(hdr, o).ok_or(ECORRUPT);
    let rd64 = |o| u64_at(hdr, o).ok_or(ECORRUPT);
    if rd64(off::PH_TOTAL_PACK_BYTES)? != len as u64 {
        return Err(ECORRUPT);
    }
    let blob_count = rd32(off::PH_BLOB_COUNT)?;
    let index_blocks = rd32(off::PH_INDEX_BLOCKS)? as usize;
    let bloom_bytes = rd32(off::PH_BLOOM_BYTES)? as usize;
    let data_off = rd64(off::PH_DATA_OFFSET)?;
    let data_len = rd64(off::PH_DATA_LENGTH)?;

    // Same containment rules as pack.c (#124), written so nothing can wrap:
    // every region lies between the header and the footer.
    let index_off = SECTOR;
    let foot_off = len - SECTOR; // len >= 2 sectors
    if index_blocks > (foot_off - index_off) / SECTOR {
        return Err(ECORRUPT);
    }
    let index_bytes = index_blocks * SECTOR;
    if blob_count as usize > index_bytes / PACK_INDEX_ENTRY_SIZE {
        return Err(ECORRUPT);
    }
    let bloom_off = index_off + index_bytes; // <= foot_off
    if bloom_bytes > foot_off - bloom_off {
        return Err(ECORRUPT);
    }
    let foot = foot_off as u64;
    if data_off > foot || data_len > foot - data_off {
        return Err(ECORRUPT);
    }

    // Footer: magic, blob count, CRC over the data region.
    let footer: &[u8; SECTOR] = data.get(foot_off..).and_then(|f| f.try_into().ok()).ok_or(ECORRUPT)?;
    codec::pack_footer(footer)?;
    let region = range(data, usize_of(data_off).ok_or(ECORRUPT)?, usize_of(data_len).ok_or(ECORRUPT)?)
        .ok_or(ECORRUPT)?;
    if u32_at(footer, off::PF_BLOB_COUNT_CHECK) != Some(blob_count)
        || u32_at(footer, off::PF_CRC32_PACK) != Some(crc32(region))
    {
        return Err(ECORRUPT);
    }

    Ok(Geometry {
        index_off: index_off as u64,
        bloom_off: bloom_off as u64,
        bloom_bytes: bloom_bytes as u64,
        data_off,
        data_len,
        blob_count,
        k_hashes: rd32(off::PH_BLOOM_HASH_COUNT)?,
    })
}

fn sub(data: &[u8], off: u64, len: u64) -> Option<&[u8]> {
    range(data, usize_of(off)?, usize_of(len)?)
}

fn index_entry<'a>(data: &'a [u8], g: &Geometry, i: u32) -> Option<&'a [u8]> {
    let at = g.index_off.checked_add(u64::from(i).checked_mul(PACK_INDEX_ENTRY_SIZE as u64)?)?;
    sub(data, at, PACK_INDEX_ENTRY_SIZE as u64)
}

/// `tessera_pack_bloom_might_contain`: true = might be present.
pub fn bloom_might_contain(data: &[u8], g: &Geometry, hash: &[u8; HASH_SIZE]) -> bool {
    let bits = g.bloom_bytes.wrapping_mul(8);
    if bits == 0 {
        return true;
    }
    let Some(bloom) = sub(data, g.bloom_off, g.bloom_bytes) else {
        return true; // geometry no longer fits: answer conservatively
    };
    let h1 = u64_at(hash, 0).unwrap_or(0);
    let h2 = match u64_at(hash, 8).unwrap_or(0) {
        0 => 1,
        v => v,
    };
    for i in 0..g.k_hashes.min(MAX_BLOOM_PROBES) {
        let bit = h1.wrapping_add(u64::from(i).wrapping_mul(h2)) % bits;
        let byte = usize_of(bit >> 3).and_then(|b| bloom.get(b)).copied().unwrap_or(0xff);
        if byte & (1u8 << (bit & 7)) == 0 {
            return false;
        }
    }
    true
}

/// `tessera_pack_lookup`: (offset of the blob bytes within `data`, length).
pub fn lookup(data: &[u8], g: &Geometry, hash: &[u8; HASH_SIZE]) -> Result<(usize, u32)> {
    // The same probe sequence as pack.c (inclusive bounds, lower midpoint), so
    // an index that is not sorted, or holds a hash twice, resolves to the same
    // entry in both implementations.
    let (mut lo, mut hi) = (0i64, i64::from(g.blob_count) - 1);
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let e = u32::try_from(mid).ok().and_then(|m| index_entry(data, g, m)).ok_or(ECORRUPT)?;
        let key = e.get(..HASH_SIZE).ok_or(ECORRUPT)?;
        match key.cmp(&hash[..]) {
            Ordering::Less => lo = mid + 1,
            Ordering::Greater => hi = mid - 1,
            Ordering::Equal => return found(data, g, e),
        }
    }
    Err(ENOENT)
}

fn found(data: &[u8], g: &Geometry, e: &[u8]) -> Result<(usize, u32)> {
    let doff = u64_at(e, off::IE_DATA_OFFSET).ok_or(ECORRUPT)?;
    let dsize = u32_at(e, off::IE_DATA_SIZE).ok_or(ECORRUPT)?;
    if doff > g.data_len {
        return Err(ECORRUPT);
    }
    let avail = g.data_len - doff;
    let desc = BLOB_DESCRIPTOR_SIZE as u64;
    if avail < desc || u64::from(dsize) > avail - desc {
        return Err(ECORRUPT);
    }
    let at = g.data_off.checked_add(doff).ok_or(ECORRUPT)?;
    let bd: &[u8; BLOB_DESCRIPTOR_SIZE] =
        sub(data, at, desc).and_then(|b| b.try_into().ok()).ok_or(ECORRUPT)?;
    codec::blob_descriptor(bd).map_err(|_| ECORRUPT)?;
    let start = at.checked_add(desc).ok_or(ECORRUPT)?;
    // Make sure the bytes handed back really are inside the pack.
    sub(data, start, u64::from(dsize)).ok_or(ECORRUPT)?;
    Ok((usize_of(start).ok_or(ECORRUPT)?, dsize))
}

/// `tessera_pack_blob_hash_at`.
pub fn blob_hash_at<'a>(data: &'a [u8], g: &Geometry, index: u32) -> Result<&'a [u8]> {
    if index >= g.blob_count {
        return Err(EINVAL);
    }
    index_entry(data, g, index).and_then(|e| e.get(..HASH_SIZE)).ok_or(ECORRUPT)
}

#[cfg(test)]
mod boundary_tests {
    extern crate std;
    use super::*;
    use crate::codec::MAGIC_BLOB;
    use std::{vec, vec::Vec};

    /// A data region holding one blob descriptor at `doff` and `dsize`
    /// payload bytes after it, `slack` bytes past that; the index entry
    /// points at it.
    fn case(doff: u64, dsize: u32, slack: u64) -> (Vec<u8>, Geometry, [u8; PACK_INDEX_ENTRY_SIZE]) {
        let data_len = doff + BLOB_DESCRIPTOR_SIZE as u64 + u64::from(dsize) + slack;
        let mut data = vec![0u8; data_len as usize];
        data[doff as usize..][..4].copy_from_slice(MAGIC_BLOB);
        let g = Geometry { index_off: 0, bloom_off: 0, bloom_bytes: 0, data_off: 0, data_len, blob_count: 1, k_hashes: 0 };
        let mut e = [0u8; PACK_INDEX_ENTRY_SIZE];
        e[off::IE_DATA_OFFSET..][..8].copy_from_slice(&doff.to_le_bytes());
        e[off::IE_DATA_SIZE..][..4].copy_from_slice(&dsize.to_le_bytes());
        (data, g, e)
    }

    /// The blob ends exactly at the end of the data region: accepted, for
    /// every small size including an empty blob. One byte more: refused.
    #[test]
    fn blob_at_end_of_data() {
        for dsize in 0..64u32 {
            for doff in [0u64, 1, 4096] {
                let (data, g, e) = case(doff, dsize, 0);
                assert_eq!(found(&data, &g, &e), Ok((doff as usize + BLOB_DESCRIPTOR_SIZE, dsize)), "doff {doff} dsize {dsize}");
                let mut e2 = e;
                e2[off::IE_DATA_SIZE..][..4].copy_from_slice(&(dsize + 1).to_le_bytes());
                assert!(found(&data, &g, &e2).is_err(), "doff {doff} dsize {dsize}+1");
            }
        }
        // Descriptor itself cut short.
        let (data, mut g, e) = case(0, 0, 0);
        g.data_len -= 1;
        assert!(found(&data, &g, &e).is_err());
    }
}
