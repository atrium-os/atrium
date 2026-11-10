//! Manifest parser (core/src/manifest.c, "Parser" half).
//!
//! A manifest is a 32-byte header followed by a body whose encoding depends on
//! the kind. Every accessor below walks the body with checked sub-slices, so a
//! count, name length or value length from disk cannot reach past the body.

use crate::codec::{self, HASH_SIZE, MANIFEST_HEADER_SIZE, off};
use crate::error::{ECORRUPT, EINVAL, ENOENT, Result};
use crate::le::{range, u16_at, u32_at, u64_at, u8_at};

pub const INLINE: u8 = 1;
pub const CHUNK_LIST: u8 = 2;
pub const CHUNK_TREE: u8 = 3;
pub const DIRECTORY: u8 = 4;
pub const SYMLINK: u8 = 5;
pub const XATTR_STORE: u8 = 6;
pub const DIRECTORY_2L: u8 = 8;
pub const DIRECTORY_BTREE: u8 = 9;

pub const CHUNK_RECORD_SIZE: usize = 48;
pub const TREE_RECORD_SIZE: usize = 40;
pub const DIR_BUCKET_RECORD_SIZE: usize = 40;

#[derive(Clone, Copy, Debug)]
pub struct Manifest<'a> {
    pub kind: u8,
    pub logical_size: u64,
    pub entry_count: u32,
    pub body: &'a [u8],
}

/// A named entry: name and value (xattr) or name and inode (dirent).
pub struct Named<'a, T> {
    pub name: &'a [u8],
    pub val: T,
}

impl<'a> Manifest<'a> {
    /// `tessera_manifest_parse`: at least a header, with the right magic.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let hdr = data.get(..MANIFEST_HEADER_SIZE).ok_or(EINVAL)?;
        let hdr: &[u8; MANIFEST_HEADER_SIZE] = hdr.try_into().map_err(|_| EINVAL)?;
        codec::manifest_header(hdr)?;
        let body = data.get(MANIFEST_HEADER_SIZE..).ok_or(EINVAL)?;
        Self::from_parts(hdr, body)
    }

    /// Rebuild from a header that was already decoded and a body (how the C
    /// parser object holds them).
    pub fn from_parts(hdr: &[u8; MANIFEST_HEADER_SIZE], body: &'a [u8]) -> Result<Self> {
        Ok(Manifest {
            kind: u8_at(hdr, off::MH_KIND).ok_or(EINVAL)?,
            logical_size: u64_at(hdr, off::MH_LOGICAL_SIZE).ok_or(EINVAL)?,
            entry_count: u32_at(hdr, off::MH_ENTRY_COUNT).ok_or(EINVAL)?,
            body,
        })
    }

    /// Fixed-size record `index` of a kind whose body is an array of them.
    fn record(&self, kind: u8, size: usize, index: u32) -> Result<&'a [u8]> {
        if self.kind != kind {
            return Err(EINVAL);
        }
        if index >= self.entry_count {
            return Err(ENOENT);
        }
        let at = (index as usize).checked_mul(size).ok_or(ECORRUPT)?;
        range(self.body, at, size).ok_or(ECORRUPT)
    }

    pub fn chunk_at(&self, index: u32) -> Result<&'a [u8]> {
        self.record(CHUNK_LIST, CHUNK_RECORD_SIZE, index)
    }
    pub fn tree_at(&self, index: u32) -> Result<&'a [u8]> {
        self.record(CHUNK_TREE, TREE_RECORD_SIZE, index)
    }
    pub fn dir_bucket_at(&self, index: u32) -> Result<&'a [u8]> {
        self.record(DIRECTORY_2L, DIR_BUCKET_RECORD_SIZE, index)
    }

    pub fn inline_data(&self) -> Result<&'a [u8]> {
        if self.kind != INLINE && self.kind != SYMLINK {
            return Err(EINVAL);
        }
        Ok(self.body)
    }

    /// XATTR_STORE entry `idx`: `[u16 name_len][u16 value_len][name][value]`.
    pub fn xattr_at(&self, idx: u32) -> Result<Named<'a, &'a [u8]>> {
        if self.kind != XATTR_STORE {
            return Err(EINVAL);
        }
        let b = self.body;
        let mut pos = 0usize;
        let mut i = 0u32;
        while pos < b.len() {
            let nl = u16_at(b, pos).ok_or(ECORRUPT)? as usize;
            let vl = u16_at(b, pos + 2).ok_or(ECORRUPT)? as usize;
            let name = range(b, pos + 4, nl).ok_or(ECORRUPT)?;
            let val = range(b, pos + 4 + nl, vl).ok_or(ECORRUPT)?;
            if i == idx {
                return Ok(Named { name, val });
            }
            pos += 4 + nl + vl;
            i = i.wrapping_add(1);
        }
        Err(ENOENT)
    }

    /// Flat DIRECTORY entry `idx`: `[u64 child_inode][u16 name_len][name]`.
    pub fn dirent_at(&self, idx: u32) -> Result<Named<'a, u64>> {
        if self.kind != DIRECTORY {
            return Err(EINVAL);
        }
        walk_dirents(self.body, 0, 0, idx)
    }

    /// DIRECTORY_BTREE: Some(true) leaf, Some(false) inner, None if this is
    /// not a DIRECTORY_BTREE or its body is shorter than its 8-byte header.
    pub fn dir_btree_is_leaf(&self) -> Option<bool> {
        if self.kind != DIRECTORY_BTREE || self.body.len() < 8 {
            return None;
        }
        Some(u8_at(self.body, 0)? != 0)
    }

    /// Child manifest hash `idx` of an inner DIRECTORY_BTREE node:
    /// `[u64 max_name_hash][hash]` records after the 8-byte body header.
    pub fn dir_btree_inner_at(&self, idx: u32) -> Result<&'a [u8]> {
        if self.kind != DIRECTORY_BTREE || self.body.len() < 8 || u8_at(self.body, 0) != Some(0) {
            return Err(EINVAL);
        }
        let at = (idx as usize)
            .checked_mul(8 + HASH_SIZE)
            .and_then(|o| o.checked_add(8 + 8))
            .ok_or(ENOENT)?;
        range(self.body, at, HASH_SIZE).ok_or(ENOENT)
    }

    /// Entry `idx` of a DIRECTORY_BTREE leaf:
    /// `[u64 name_hash][u64 inode_no][u16 name_len][name]` after the header.
    pub fn dir_btree_leaf_at(&self, idx: u32) -> Result<Named<'a, u64>> {
        if self.kind != DIRECTORY_BTREE || self.body.len() < 8 || u8_at(self.body, 0) != Some(1) {
            return Err(EINVAL);
        }
        walk_dirents(self.body, 8, 8, idx)
    }
}

/// A DIRECTORY_BTREE node whose entry count has been checked against its
/// body: `count` entries really are there, so a caller may walk them without
/// further bounds checks. (tessera_fs.c's insert, remove and lookup paths do,
/// in C.)
#[derive(Clone, Copy, Debug)]
pub struct DirBtreeNode<'a> {
    pub leaf: bool,
    /// The body after its 8-byte node header: the entries.
    pub entries: &'a [u8],
    pub count: u32,
}

/// Leaf entry: `[u64 name_hash][u64 inode_no][u16 name_len][name]`.
pub const DIR_BTREE_LEAF_FIXED: usize = 18;
/// Inner entry: `[u64 max_name_hash][child hash]`.
pub const DIR_BTREE_INNER_SIZE: usize = 8 + HASH_SIZE;

/// Validate a whole DIRECTORY_BTREE manifest blob (kmod
/// `tessera_fs_dir_btree_decode`). Before this check existed, the kernel
/// trusted the header's entry_count: leaf insert and remove walked that many
/// entries with no bounds check, sized arrays as `count + 1` (0 when count is
/// 2^32-1), and an inner node with count 0 indexed child `count - 1`.
/// Now: a leaf's `count` entries must all lie inside the body, an inner node
/// must have at least one child and all of them inside the body.
pub fn dir_btree_node(blob: &[u8]) -> Result<DirBtreeNode<'_>> {
    let m = Manifest::parse(blob).map_err(|_| ECORRUPT)?;
    if m.kind != DIRECTORY_BTREE || m.body.len() < 8 {
        return Err(ECORRUPT);
    }
    let leaf = u8_at(m.body, 0).ok_or(ECORRUPT)? != 0;
    let entries = m.body.get(8..).ok_or(ECORRUPT)?;
    let count = m.entry_count;
    if leaf {
        let mut pos = 0usize;
        for _ in 0..count {
            let nl = u16_at(entries, pos + 16).ok_or(ECORRUPT)? as usize;
            range(entries, pos + DIR_BTREE_LEAF_FIXED, nl).ok_or(ECORRUPT)?;
            pos += DIR_BTREE_LEAF_FIXED + nl;
        }
    } else {
        let need = (count as usize).checked_mul(DIR_BTREE_INNER_SIZE).ok_or(ECORRUPT)?;
        if count == 0 || need > entries.len() {
            return Err(ECORRUPT);
        }
    }
    Ok(DirBtreeNode { leaf, entries, count })
}

/// Walk `[skip bytes][u64 inode][u16 name_len][name]` entries from `start`
/// (`skip` = 0 for flat dirents, 8 for the leaf's name_hash). Like the C
/// loop, a trailing fragment shorter than a fixed part ends the walk (ENOENT)
/// and a name running past the body is ECORRUPT.
fn walk_dirents(b: &[u8], start: usize, skip: usize, idx: u32) -> Result<Named<'_, u64>> {
    let fixed = skip + 10;
    let mut pos = start;
    let mut i = 0u32;
    while range(b, pos, fixed).is_some() {
        let ino = u64_at(b, pos + skip).ok_or(ECORRUPT)?;
        let nl = u16_at(b, pos + skip + 8).ok_or(ECORRUPT)? as usize;
        let name = range(b, pos + fixed, nl).ok_or(ECORRUPT)?;
        if i == idx {
            return Ok(Named { name, val: ino });
        }
        pos += fixed + nl;
        i = i.wrapping_add(1);
    }
    Err(ENOENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mft(kind: u8, count: u32, body: &[u8]) -> ([u8; 32], &[u8]) {
        let mut h = [0u8; 32];
        h[..4].copy_from_slice(b"TMFT");
        h[6] = kind;
        h[24..28].copy_from_slice(&count.to_le_bytes());
        (h, body)
    }

    #[test]
    fn dirents() {
        let mut body = [0u8; 64];
        body[0..8].copy_from_slice(&7u64.to_le_bytes());
        body[8..10].copy_from_slice(&3u16.to_le_bytes());
        body[10..13].copy_from_slice(b"abc");
        body[13..21].copy_from_slice(&9u64.to_le_bytes());
        body[21..23].copy_from_slice(&200u16.to_le_bytes()); // runs past the body
        let (h, b) = mft(DIRECTORY, 2, &body);
        let m = Manifest::from_parts(&h, b).unwrap();
        let e = m.dirent_at(0).ok().unwrap();
        assert_eq!((e.val, e.name), (7, &b"abc"[..]));
        assert_eq!(m.dirent_at(1).err(), Some(ECORRUPT));
        assert_eq!(m.chunk_at(0).err(), Some(EINVAL));
    }

    #[test]
    fn dir_btree_counts_must_fit() {
        let mut blob = [0u8; 32 + 8 + 18 + 2];
        blob[..4].copy_from_slice(b"TMFT");
        blob[6] = DIRECTORY_BTREE;
        blob[32] = 1; // leaf
        blob[24..28].copy_from_slice(&1u32.to_le_bytes());
        blob[40 + 16..40 + 18].copy_from_slice(&2u16.to_le_bytes());
        let n = dir_btree_node(&blob).unwrap();
        assert!(n.leaf && n.count == 1);
        blob[24..28].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(dir_btree_node(&blob).err(), Some(ECORRUPT));
        blob[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(dir_btree_node(&blob).err(), Some(ECORRUPT));
        blob[32] = 0; // inner, 20 bytes of body: no room for one child
        blob[24..28].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(dir_btree_node(&blob).err(), Some(ECORRUPT));
        blob[24..28].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(dir_btree_node(&blob).err(), Some(ECORRUPT));
    }

    #[test]
    fn chunk_records_bounded_by_body() {
        let body = [0u8; 50];
        let (h, b) = mft(CHUNK_LIST, 3, &body);
        let m = Manifest::from_parts(&h, b).unwrap();
        assert!(m.chunk_at(0).is_ok());
        assert_eq!(m.chunk_at(1), Err(ECORRUPT));
        assert_eq!(m.chunk_at(3), Err(ENOENT));
        assert_eq!(m.chunk_at(u32::MAX), Err(ENOENT));
    }
}
