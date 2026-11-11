//! B+tree node checks (core/src/btree.c `load_node` and `search_leaf`).
//!
//! A node is one 4 KiB sector: a 64-byte header, then `entry_count` entries of
//! `key_size + value_size` bytes (leaf) or `key_size + 8` (internal, the value
//! being the child sector). `node_check` is the gate every node read passes;
//! after it, `search` indexes entries through checked sub-slices as well.

use crate::codec::{self, BTREE_NODE_HEADER_SIZE, off};
use crate::le::{u32_at, u8_at};
use core::cmp::Ordering;

/// `tessera_btree_fail_t`.
pub const FAIL_NONE: i32 = 0;
pub const FAIL_HEADER: i32 = 2;
pub const FAIL_KIND: i32 = 3;

/// What the tree expects of its nodes (`tessera_btree_t` fields).
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    pub tree_kind: u8,
    pub key_size: u32,
    pub value_size: u32,
    pub leaf_fanout: u32,
    pub inner_fanout: u32,
}

/// Why a node was refused: the `FAIL_*` code, and for `FAIL_KIND` the tree
/// kind the node really belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fail {
    pub fail: i32,
    pub found_kind: u8,
}

const fn header_fail() -> Fail {
    Fail { fail: FAIL_HEADER, found_kind: 0 }
}

/// `load_node` after the read: header magic and CRC, tree kind, entry count
/// within the fanout, key and value sizes matching the tree (when nonzero).
pub fn node_check(block: &[u8], s: &Shape) -> Result<(), Fail> {
    let hdr: &[u8; BTREE_NODE_HEADER_SIZE] = block
        .get(..BTREE_NODE_HEADER_SIZE)
        .and_then(|h| h.try_into().ok())
        .ok_or(header_fail())?;
    codec::btree_node_header(hdr).map_err(|_| header_fail())?;
    node_fields(hdr, s)
}

/// The checks after magic and CRC: tree kind, entry count against the
/// fan-out, key/value geometry. For a caller that has already checked the
/// header (the FFI, which also copies it out), so the CRC runs once.
pub fn node_fields(hdr: &[u8; BTREE_NODE_HEADER_SIZE], s: &Shape) -> Result<(), Fail> {
    let node_kind = u8_at(hdr, off::BH_NODE_KIND).ok_or(header_fail())?;
    let tree_kind = u8_at(hdr, off::BH_TREE_KIND).ok_or(header_fail())?;
    if tree_kind != s.tree_kind {
        return Err(Fail { fail: FAIL_KIND, found_kind: tree_kind });
    }
    let entry_count = u32_at(hdr, off::BH_ENTRY_COUNT).ok_or(header_fail())?;
    let cap = if node_kind == 0 { s.leaf_fanout } else { s.inner_fanout };
    if entry_count > cap {
        return Err(header_fail());
    }
    let ks = u32_at(hdr, off::BH_KEY_SIZE).ok_or(header_fail())?;
    let vs = u32_at(hdr, off::BH_VALUE_SIZE).ok_or(header_fail())?;
    let want_vs = if node_kind == 0 { s.value_size } else { 8 };
    if (ks != 0 && ks != s.key_size) || (vs != 0 && vs != want_vs) {
        return Err(header_fail());
    }
    Ok(())
}

/// Byte-wise (memcmp) order, eight bytes at a time: big-endian words
/// compare exactly like their bytes. Same result as `<[u8]>::cmp`, without a
/// memcmp call per probe (the search compares 32-byte keys ~9 times a node).
#[inline]
fn cmp_keys(a: &[u8], b: &[u8]) -> Ordering {
    let n = a.len().min(b.len());
    let mut wa = a[..n].chunks_exact(8);
    let mut wb = b[..n].chunks_exact(8);
    for (x, y) in (&mut wa).zip(&mut wb) {
        let x = <[u8; 8]>::try_from(x).map_or(0, u64::from_be_bytes);
        let y = <[u8; 8]>::try_from(y).map_or(0, u64::from_be_bytes);
        if x != y {
            return x.cmp(&y);
        }
    }
    wa.remainder().cmp(wb.remainder()).then(a.len().cmp(&b.len()))
}

/// `search_leaf`: the largest index whose key is <= `key` (-1 if every key is
/// greater), and whether it matched exactly. Same probe sequence as the C.
/// An entry that would lie outside `block` compares as greater than any key,
/// so a node that slipped past `node_check` still cannot be read past its end.
pub fn search(block: &[u8], entry_count: u32, key_size: u32, entry_size: u32, key: &[u8]) -> (i32, bool) {
    let entries = block.get(BTREE_NODE_HEADER_SIZE..).unwrap_or(&[]);
    let ks = key_size as usize;
    let es = u64::from(entry_size);
    let probe = key.get(..ks).unwrap_or(key);
    // btree.c computes `int hi = (int)entry_count - 1`; take the same starting
    // bound, then do the search in i64 so nothing can wrap.
    let (mut lo, mut hi, mut best) = (0i64, i64::from((entry_count as i32).wrapping_sub(1)), -1i64);
    while lo <= hi {
        // 0 <= lo <= hi here, so the shift is a plain halving.
        let mid = lo + ((hi - lo) >> 1);
        // mid < 2^31 and es < 2^32: the product cannot overflow a u64.
        let at = (mid as u64) * es;
        let e = usize::try_from(at).ok().and_then(|at| entries.get(at..)).and_then(|t| t.get(..ks));
        let ord = match e {
            Some(e) => cmp_keys(e, probe),
            None => Ordering::Greater,
        };
        match ord {
            Ordering::Equal => return (mid as i32, true),
            Ordering::Less => {
                best = mid;
                lo = mid + 1;
            }
            Ordering::Greater => hi = mid - 1,
        }
    }
    (best as i32, false)
}

#[cfg(test)]
mod cmp_tests {
    use super::cmp_keys;

    /// cmp_keys must order exactly like slice (memcmp) order: every pair
    /// from a set that differs at every byte position, both lengths.
    #[test]
    fn matches_slice_order() {
        let mut keys: [[u8; 33]; 70] = [[0x5a; 33]; 70];
        for (i, k) in keys.iter_mut().enumerate() {
            if i < 66 {
                k[i / 2] = if i % 2 == 0 { 0x00 } else { 0xff };
            }
        }
        for a in &keys {
            for b in &keys {
                for la in [0usize, 7, 8, 9, 16, 31, 32, 33] {
                    for lb in [0usize, 8, 32, 33] {
                        assert_eq!(cmp_keys(&a[..la], &b[..lb]), a[..la].cmp(&b[..lb]));
                    }
                }
            }
        }
    }
}
