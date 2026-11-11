//! B+tree node checks (core/src/btree.c `load_node` and `search_leaf`).
//!
//! A node is one 4 KiB sector: a 64-byte header, then `entry_count` entries of
//! `key_size + value_size` bytes (leaf) or `key_size + 8` (internal, the value
//! being the child sector). `node_check` is the gate every node read passes;
//! after it, `search` indexes entries through checked sub-slices as well.

use crate::codec::{self, BTREE_NODE_HEADER_SIZE, off};
use crate::error::{EINVAL, Result as EResult};
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

// ---- node edits (put, delete, sorted batch) ---------------------------------
//
// btree.c keeps the I/O, allocation and copy-on-write order; these move the
// entry bytes. Every count and index here has been bounded by `node_check`
// (count <= fanout) and the put/delete logic, so an `Err(EINVAL)` means a bug in
// the caller, not a bad disk; it is still an error return, never a write
// outside the block.

/// `n` entries of `es` bytes, as a byte length.
#[inline]
fn span(n: u32, es: u32) -> Option<usize> {
    usize::try_from(u64::from(n) * u64::from(es)).ok()
}

/// Copy `src` into `dst`; both must be the same length. The explicit check
/// lets the compiler drop `copy_from_slice`'s length panic.
#[inline]
fn copy(dst: &mut [u8], src: &[u8]) -> EResult<()> {
    if dst.len() != src.len() {
        return Err(EINVAL);
    }
    dst.copy_from_slice(src);
    Ok(())
}

/// Write one entry, `key` then `val`, into `slot` (exactly their total size).
#[inline]
fn put_entry(slot: &mut [u8], key: &[u8], val: &[u8]) -> EResult<()> {
    if slot.len() != key.len() + val.len() {
        return Err(EINVAL);
    }
    let (k, v) = slot.split_at_mut(key.len());
    copy(k, key)?;
    copy(v, val)
}

fn area_mut(block: &mut [u8]) -> EResult<&mut [u8]> {
    block.get_mut(BTREE_NODE_HEADER_SIZE..).ok_or(EINVAL)
}

fn area(block: &[u8]) -> EResult<&[u8]> {
    block.get(BTREE_NODE_HEADER_SIZE..).ok_or(EINVAL)
}

/// Overwrite `bytes.len()` bytes at offset `at` of entry `idx`: a leaf value
/// replaced in place, an internal child pointer, a lowered routing key.
pub fn entry_write(block: &mut [u8], es: u32, idx: u32, at: u32, bytes: &[u8]) -> EResult<()> {
    let start = span(idx, es).ok_or(EINVAL)?;
    let at = at as usize;
    if at.checked_add(bytes.len()).is_none_or(|end| end > es as usize) {
        return Err(EINVAL);
    }
    let off = start.checked_add(at).ok_or(EINVAL)?;
    let end = off.checked_add(bytes.len()).ok_or(EINVAL)?;
    copy(area_mut(block)?.get_mut(off..end).ok_or(EINVAL)?, bytes)
}

/// Insert the entry `key`‖`val` at index `ins` of a node holding `count`,
/// shifting entries `ins..count` up one. The node must have room for one more.
pub fn insert(block: &mut [u8], count: u32, es: u32, ins: u32, key: &[u8], val: &[u8]) -> EResult<()> {
    if ins > count {
        return Err(EINVAL);
    }
    let from = span(ins, es).ok_or(EINVAL)?;
    let to = span(count.checked_add(1).ok_or(EINVAL)?, es).ok_or(EINVAL)?;
    let es = es as usize;
    let tail = area_mut(block)?.get_mut(from..to).ok_or(EINVAL)?;
    if tail.len() < es {
        return Err(EINVAL);
    }
    let moved = tail.len() - es;
    tail.copy_within(..moved, es);
    put_entry(tail.get_mut(..es).ok_or(EINVAL)?, key, val)
}

/// Remove entry `idx` of a node holding `count`, shifting the rest down. The
/// last slot keeps its old bytes, as the C memmove left it.
pub fn remove(block: &mut [u8], count: u32, es: u32, idx: u32) -> EResult<()> {
    if idx >= count {
        return Err(EINVAL);
    }
    let from = span(idx, es).ok_or(EINVAL)?;
    let to = span(count, es).ok_or(EINVAL)?;
    let tail = area_mut(block)?.get_mut(from..to).ok_or(EINVAL)?;
    let es = es as usize;
    if tail.len() < es {
        return Err(EINVAL);
    }
    tail.copy_within(es.., 0);
    Ok(())
}

/// Entries `a..b` of the node's `count` entries with `key`‖`val` inserted at
/// `ins`, written to `dst` (exactly `(b - a) * es` bytes).
#[allow(clippy::too_many_arguments)]
fn merged(old: &[u8], es: u32, ins: u32, key: &[u8], val: &[u8], a: u32, b: u32, dst: &mut [u8]) -> EResult<()> {
    // old entries [a, min(b, ins)) land first ...
    let o1 = (a, b.min(ins));
    let mut w = 0usize;
    if o1.0 < o1.1 {
        let src = old.get(span(o1.0, es).ok_or(EINVAL)?..span(o1.1, es).ok_or(EINVAL)?).ok_or(EINVAL)?;
        copy(dst.get_mut(..src.len()).ok_or(EINVAL)?, src)?;
        w = src.len();
    }
    // ... then the new entry, if it falls in range ...
    if a <= ins && ins < b {
        let e = es as usize;
        put_entry(dst.get_mut(w..w + e).ok_or(EINVAL)?, key, val)?;
        w += e;
    }
    // ... then old entries from max(a, ins + 1) - 1 up to b - 1.
    let lo = a.max(ins + 1);
    if lo < b {
        let src = old.get(span(lo - 1, es).ok_or(EINVAL)?..span(b - 1, es).ok_or(EINVAL)?).ok_or(EINVAL)?;
        copy(dst.get_mut(w..).ok_or(EINVAL)?, src)?;
        w += src.len();
    }
    if w != dst.len() {
        return Err(EINVAL);
    }
    Ok(())
}

/// Split a full node on insert: the `count + 1` entries (the node's with
/// `key`‖`val` at `ins`) go `total / 2` to `left` and the rest to `right`,
/// after each block's header. Returns `(left_n, right_n)`; the split key is
/// `right`'s first key. Headers are the caller's.
#[allow(clippy::too_many_arguments)]
pub fn split_insert(
    block: &[u8], count: u32, es: u32, ins: u32, key: &[u8], val: &[u8], left: &mut [u8], right: &mut [u8],
) -> EResult<(u32, u32)> {
    if ins > count || key.len() + val.len() != es as usize {
        return Err(EINVAL);
    }
    let total = count.checked_add(1).ok_or(EINVAL)?;
    let (ln, rn) = (total / 2, total - total / 2);
    let old = area(block)?.get(..span(count, es).ok_or(EINVAL)?).ok_or(EINVAL)?;
    let l = area_mut(left)?.get_mut(..span(ln, es).ok_or(EINVAL)?).ok_or(EINVAL)?;
    merged(old, es, ins, key, val, 0, ln, l)?;
    let r = area_mut(right)?.get_mut(..span(rn, es).ok_or(EINVAL)?).ok_or(EINVAL)?;
    merged(old, es, ins, key, val, ln, total, r)?;
    Ok((ln, rn))
}

/// The sorted batch's leaf merge: the node's `count` entries and the batch's
/// `n` (key, value) pairs (keys strictly ascending, `ks` and `vs` bytes each)
/// merged by key into `out`, a batch entry replacing an equal node entry.
/// `displaced(key, old_value, new_value)` is called for each replacement.
/// Returns the number of entries written.
#[allow(clippy::too_many_arguments)]
pub fn leaf_merge(
    block: &[u8], count: u32, ks: u32, vs: u32, keys: &[u8], vals: &[u8], n: u32, out: &mut [u8],
    displaced: &mut dyn FnMut(&[u8], &[u8], &[u8]),
) -> EResult<u32> {
    if ks == 0 {
        return Err(EINVAL);
    }
    let es = ks.checked_add(vs).ok_or(EINVAL)?;
    let (ksz, esz) = (ks as usize, es as usize);
    let old = area(block)?.get(..span(count, es).ok_or(EINVAL)?).ok_or(EINVAL)?;
    let keys = keys.get(..span(n, ks).ok_or(EINVAL)?).ok_or(EINVAL)?;
    let vals = vals.get(..span(n, vs).ok_or(EINVAL)?).ok_or(EINVAL)?;
    let mut olds = old.chunks_exact(esz.max(1)).peekable();
    // By index, not chunks_exact, so a zero value size still yields n pairs.
    let vsz = vs as usize;
    let mut news = (0..n as usize)
        .map(|j| (keys.get(j * ksz..(j + 1) * ksz).unwrap_or(&[]), vals.get(j * vsz..(j + 1) * vsz).unwrap_or(&[])))
        .peekable();
    let mut slots = out.chunks_exact_mut(esz.max(1));
    let mut total = 0u32;
    loop {
        let ord = match (olds.peek(), news.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(o), Some((k, _))) => cmp_keys(o.get(..ksz).unwrap_or(o), k),
        };
        let slot = slots.next().ok_or(EINVAL)?;
        if ord == Ordering::Less {
            copy(slot, olds.next().ok_or(EINVAL)?)?;
        } else {
            let (k, v) = news.next().ok_or(EINVAL)?;
            put_entry(slot, k, v)?;
            if ord == Ordering::Equal {
                let o = olds.next().ok_or(EINVAL)?;
                let (ok, ov) = o.split_at(ksz.min(o.len()));
                displaced(ok, ov, v);
            }
        }
        total += 1;
    }
    Ok(total)
}

#[cfg(test)]
mod edit_tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const B: usize = 4096;
    const H: usize = BTREE_NODE_HEADER_SIZE;

    fn node(count: u32, es: u32) -> Vec<u8> {
        let mut b = std::vec![0xeeu8; B];
        for i in 0..count as usize {
            for j in 0..es as usize {
                b[H + i * es as usize + j] = (i as u8).wrapping_mul(7) ^ j as u8;
            }
        }
        b
    }

    /// The reference: build the merged entry list with Vec, compare.
    fn entries_with(b: &[u8], count: u32, es: u32, ins: u32, e: &[u8]) -> Vec<u8> {
        let es = es as usize;
        let mut v = b[H..H + count as usize * es].to_vec();
        let at = ins as usize * es;
        v.splice(at..at, e.iter().copied());
        v
    }

    #[test]
    fn insert_and_split_every_position() {
        for es in [5u32, 12, 40, 1008] {
            let cap = ((B - H) / es as usize) as u32;
            for count in [0u32, 1, 2, 3, cap.saturating_sub(1), cap] {
                for ins in 0..=count {
                    let key: Vec<u8> = (0..3).map(|i| 0xa0 + i).collect();
                    let val: Vec<u8> = (0..es - 3).map(|i| 0x50 ^ i as u8).collect();
                    let e: Vec<u8> = key.iter().chain(val.iter()).copied().collect();
                    let b = node(count, es);
                    let want = entries_with(&b, count, es, ins, &e);
                    if count < cap {
                        let mut m = b.clone();
                        insert(&mut m, count, es, ins, &key, &val).unwrap();
                        assert_eq!(&m[H..H + want.len()], &want[..]);
                        assert_eq!(&m[H + want.len()..], &b[H + want.len()..], "past the entries");
                    } else {
                        let mut m = b.clone();
                        assert_eq!(insert(&mut m, count, es, ins, &key, &val), Err(EINVAL));
                    }
                    let (mut l, mut r) = (std::vec![0u8; B], std::vec![0u8; B]);
                    let (ln, rn) = split_insert(&b, count, es, ins, &key, &val, &mut l, &mut r).unwrap();
                    assert_eq!(ln + rn, count + 1);
                    assert_eq!(ln, (count + 1) / 2);
                    let cut = ln as usize * es as usize;
                    assert_eq!(&l[H..H + cut], &want[..cut]);
                    assert_eq!(&r[H..H + want.len() - cut], &want[cut..]);
                    assert!(l[H + cut..].iter().all(|&x| x == 0) && r[H + want.len() - cut..].iter().all(|&x| x == 0));
                }
            }
        }
    }

    #[test]
    fn remove_every_position() {
        for es in [5u32, 40] {
            for count in [1u32, 2, 7] {
                for idx in 0..count {
                    let b = node(count, es);
                    let mut m = b.clone();
                    remove(&mut m, count, es, idx).unwrap();
                    let e = es as usize;
                    let mut want = b[H..H + count as usize * e].to_vec();
                    want.drain(idx as usize * e..(idx as usize + 1) * e);
                    assert_eq!(&m[H..H + want.len()], &want[..]);
                    // the vacated last slot keeps its old bytes
                    let last = H + want.len();
                    assert_eq!(&m[last..last + e], &b[last..last + e]);
                }
                let mut m = node(count, es);
                assert_eq!(remove(&mut m, count, es, count), Err(EINVAL));
            }
        }
    }

    #[test]
    fn bounds() {
        let mut b = std::vec![0u8; B];
        assert_eq!(entry_write(&mut b, 12, 0, 8, &[1; 4]), Ok(()));
        assert_eq!(entry_write(&mut b, 12, 0, 9, &[1; 4]), Err(EINVAL));
        assert_eq!(entry_write(&mut b, 12, 335, 0, &[1; 12]), Ok(()));
        assert_eq!(entry_write(&mut b, 12, 336, 0, &[1; 12]), Err(EINVAL));
        assert_eq!(insert(&mut b, 0, 12, 1, &[0; 4], &[0; 8]), Err(EINVAL));
        assert_eq!(insert(&mut b, 0, 12, 0, &[0; 4], &[0; 7]), Err(EINVAL));
        assert_eq!(insert(&mut b, u32::MAX, 12, 0, &[0; 4], &[0; 8]), Err(EINVAL));
        let (mut l, mut r) = (std::vec![0u8; B], std::vec![0u8; B]);
        // 2 * 336 entries cannot come from one block
        assert_eq!(split_insert(&b, 670, 12, 0, &[0; 4], &[0; 8], &mut l, &mut r), Err(EINVAL));
    }

    #[test]
    fn merge() {
        let (ks, vs) = (2u32, 3u32);
        let mut b = std::vec![0u8; B];
        for (i, k) in [2u8, 4, 6].iter().enumerate() {
            b[H + i * 5..H + i * 5 + 5].copy_from_slice(&[0, *k, 1, 1, 1]);
        }
        let keys = [0u8, 1, 0, 4, 0, 9];
        let vals = [9u8, 9, 9, 8, 8, 8, 7, 7, 7];
        let mut out = std::vec![0u8; 6 * 5];
        let mut seen = Vec::new();
        let n = leaf_merge(&b, 3, ks, vs, &keys, &vals, 3, &mut out, &mut |k, o, v| {
            seen.push((k.to_vec(), o.to_vec(), v.to_vec()));
        })
        .unwrap();
        assert_eq!(n, 5);
        assert_eq!(
            &out[..25],
            &[0, 1, 9, 9, 9, 0, 2, 1, 1, 1, 0, 4, 8, 8, 8, 0, 6, 1, 1, 1, 0, 9, 7, 7, 7]
        );
        assert_eq!(seen, [(std::vec![0, 4], std::vec![1, 1, 1], std::vec![8, 8, 8])]);
        let mut small = std::vec![0u8; 4 * 5];
        assert_eq!(leaf_merge(&b, 3, ks, vs, &keys, &vals, 3, &mut small, &mut |_, _, _| {}), Err(EINVAL));
    }
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
