//! Journal record checks (core/src/journal.c `read_record`).

use crate::codec::{self, RECORD_HEADER_SIZE, SECTOR, off};
use crate::error::{ECORRUPT, Result};
use crate::le::u32_at;

/// Blocks a record with `body_len` body bytes occupies (header included).
/// Computed in 64 bits: journal.c's 32-bit version wraps for bodies within
/// 4 KiB of 4 GiB, which made such a record's block count look tiny.
pub fn sectors_for(body_len: u32) -> u64 {
    (RECORD_HEADER_SIZE as u64 + u64::from(body_len)).div_ceil(SECTOR as u64)
}

/// Largest record body (the C `MAX_BODY` in core/src/journal.c). Real records
/// are a few hundred bytes; the cap bounds the allocation replay makes.
pub const MAX_BODY: u32 = 1 << 20;

/// Header magic and CRC, then the geometry rule (#126): the body fits in the
/// journal's `cap` data blocks and `block_count` is exactly what the body
/// needs, and the body is at most [`MAX_BODY`]. `blk` is the record's first
/// block.
pub fn record_check(blk: &[u8; SECTOR], cap: u64) -> Result<()> {
    let hdr: &[u8; RECORD_HEADER_SIZE] =
        blk.get(..RECORD_HEADER_SIZE).and_then(|h| h.try_into().ok()).ok_or(ECORRUPT)?;
    codec::record_header(hdr)?;
    let bl = u32_at(hdr, off::RH_BODY_LENGTH).ok_or(ECORRUPT)?;
    let bc = u32_at(hdr, off::RH_BLOCK_COUNT).ok_or(ECORRUPT)?;
    if u64::from(bl) > cap.saturating_mul(SECTOR as u64) || bl > MAX_BODY {
        return Err(ECORRUPT);
    }
    if u64::from(bc) != sectors_for(bl) {
        return Err(ECORRUPT);
    }
    Ok(())
}

/// The block after a record of `block_count` blocks starting at `block`, in a
/// ring of data blocks numbered 1..=cap. `cap` must be nonzero (the journal
/// is at least 4 blocks).
pub fn advance(block: u64, block_count: u32, cap: u64) -> u64 {
    if cap == 0 {
        return 1;
    }
    (block.wrapping_sub(1).wrapping_add(u64::from(block_count)) % cap) + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::MAGIC_TXR;
    use crate::crc::crc32;

    fn block(bl: u32, bc: u32) -> [u8; SECTOR] {
        let mut b = [0u8; SECTOR];
        b[..4].copy_from_slice(MAGIC_TXR);
        b[off::RH_BODY_LENGTH..][..4].copy_from_slice(&bl.to_le_bytes());
        b[off::RH_BLOCK_COUNT..][..4].copy_from_slice(&bc.to_le_bytes());
        let c = crc32(&b[..off::RH_CRC32_HEADER]);
        b[off::RH_CRC32_HEADER..][..4].copy_from_slice(&c.to_le_bytes());
        b
    }

    /// Every body length up to 64 KiB plus a few: the exact block count is
    /// accepted and its neighbours are not. A sweep rather than samples, so a
    /// check that is wrong for one value cannot hide (the differential fuzzer
    /// missed a planted single-value hole here).
    #[test]
    fn block_count_sweep() {
        let cap = 64;
        for bl in 0..=(64 * 1024 + 64) {
            let want = (RECORD_HEADER_SIZE as u32 + bl).div_ceil(SECTOR as u32);
            assert_eq!(sectors_for(bl), u64::from(want));
            if u64::from(bl) > cap * SECTOR as u64 {
                assert!(record_check(&block(bl, want), cap).is_err(), "bl {bl} over cap");
                continue;
            }
            assert!(record_check(&block(bl, want), cap).is_ok(), "bl {bl}");
            assert!(record_check(&block(bl, want + 1), cap).is_err(), "bl {bl} +1");
            assert!(record_check(&block(bl, want - 1), cap).is_err(), "bl {bl} -1");
        }
    }

    #[test]
    fn body_cap() {
        let cap = u64::MAX;
        let ok = |bl: u32| record_check(&block(bl, sectors_for(bl) as u32), cap).is_ok();
        assert!(ok(MAX_BODY));
        assert!(!ok(MAX_BODY + 1));
        assert!(!ok(u32::MAX - 4096));
    }

    #[test]
    fn sectors_for_does_not_wrap() {
        assert_eq!(sectors_for(u32::MAX), (RECORD_HEADER_SIZE as u64 + u64::from(u32::MAX)).div_ceil(SECTOR as u64));
        assert!(record_check(&block(u32::MAX, 0), u64::MAX).is_err());
    }

    #[test]
    fn advance_wraps_in_ring() {
        assert_eq!(advance(1, 1, 4), 2);
        assert_eq!(advance(4, 1, 4), 1);
        assert_eq!(advance(3, 6, 4), 1);
        assert_eq!(advance(9, 1, 0), 1);
    }
}
