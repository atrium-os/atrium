use crate::command::protocol::{Hash256, NULL_HASH};
use sha2::{Sha256, Digest};
use std::collections::HashMap;

struct Blob {
    data: Vec<u8>,
    ref_count: u32,
    last_seen: u64,
}

struct StagedUpload {
    data: Vec<u8>,
    total_size: usize,
}

pub struct CasStore {
    blobs: HashMap<Hash256, Blob>,
    upload_staging: HashMap<u32, StagedUpload>,
    pub dedup_hits: u64,
    pub dedup_bytes_saved: u64,
    pub gc_freed_blobs: u64,
    pub gc_freed_bytes: u64,
    pub last_tree_size: u32,
    pub last_tree_shared: u32,
    pub generation: u64,
}

impl CasStore {
    pub fn new() -> Self {
        Self {
            blobs: HashMap::new(),
            upload_staging: HashMap::new(),
            dedup_hits: 0,
            dedup_bytes_saved: 0,
            gc_freed_blobs: 0,
            gc_freed_bytes: 0,
            last_tree_size: 0,
            last_tree_shared: 0,
            generation: 0,
        }
    }

    pub fn clear(&mut self) {
        self.blobs.clear();
        self.upload_staging.clear();
        self.dedup_hits = 0;
        self.dedup_bytes_saved = 0;
        self.gc_freed_blobs = 0;
        self.gc_freed_bytes = 0;
        self.last_tree_size = 0;
        self.last_tree_shared = 0;
    }

    pub fn hash(data: &[u8]) -> Hash256 {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        let mut h = [0u8; 32];
        h.copy_from_slice(&result);
        h
    }

    pub fn store(&mut self, data: &[u8]) -> Hash256 {
        let hash = Self::hash(data);
        let gen = self.generation;
        if let Some(blob) = self.blobs.get_mut(&hash) {
            blob.last_seen = gen;
            self.dedup_hits += 1;
            self.dedup_bytes_saved += data.len() as u64;
        } else {
            self.blobs.insert(hash, Blob {
                data: data.to_vec(),
                ref_count: 0,
                last_seen: gen,
            });
        }
        hash
    }

    pub fn store_pinned(&mut self, data: &[u8]) -> Hash256 {
        let hash = Self::hash(data);
        if !self.blobs.contains_key(&hash) {
            self.blobs.insert(hash, Blob {
                data: data.to_vec(),
                ref_count: u32::MAX / 2,
                last_seen: u64::MAX,
            });
        }
        hash
    }

    pub fn load(&self, hash: &Hash256) -> Option<&[u8]> {
        self.blobs.get(hash).map(|b| b.data.as_slice())
    }

    pub fn exists(&self, hash: &Hash256) -> bool {
        self.blobs.contains_key(hash)
    }

    pub fn mark_alive(&mut self, hash: &Hash256) {
        if let Some(blob) = self.blobs.get_mut(hash) {
            blob.last_seen = self.generation;
        }
    }

    pub fn sweep(&mut self, keep_generations: u64) {
        let threshold = self.generation.saturating_sub(keep_generations);
        let before = self.blobs.len();
        self.blobs.retain(|_, blob| blob.last_seen >= threshold);
        let freed = before - self.blobs.len();
        if freed > 0 {
            self.gc_freed_blobs += freed as u64;
            log::debug!("GC sweep: freed {} blobs (kept gen >= {})", freed, threshold);
        }
    }

    pub fn advance_generation(&mut self) {
        self.generation += 1;
    }

    pub fn ref_count(&self, hash: &Hash256) -> u32 {
        self.blobs.get(hash).map(|b| b.ref_count).unwrap_or(0)
    }

    /// Increment refcount for a hash and all hashes it references (recursive).
    /// Also counts tree size and shared nodes (piggybacks on the walk — zero extra cost).
    pub fn inc_ref_tree(&mut self, hash: &Hash256) {
        self.last_tree_size = 0;
        self.last_tree_shared = 0;
        self.inc_ref_tree_inner(hash);
    }

    fn inc_ref_tree_inner(&mut self, hash: &Hash256) {
        if *hash == NULL_HASH { return; }
        let was_referenced = if let Some(blob) = self.blobs.get_mut(hash) {
            let was = blob.ref_count > 0;
            blob.ref_count += 1;
            was
        } else {
            return;
        };
        self.last_tree_size += 1;
        if was_referenced {
            self.last_tree_shared += 1;
        }
        let refs = self.extract_refs(hash);
        for r in refs {
            self.inc_ref_tree_inner(&r);
        }
    }

    /// Decrement refcount for a hash. If it reaches 0, cascade dec_ref to all
    /// referenced hashes and free the blob. Arc-style recursive cleanup.
    pub fn dec_ref_tree(&mut self, hash: &Hash256) {
        if *hash == NULL_HASH { return; }

        let should_free = if let Some(blob) = self.blobs.get_mut(hash) {
            blob.ref_count = blob.ref_count.saturating_sub(1);
            blob.ref_count == 0
        } else {
            false
        };

        if should_free {
            let refs = self.extract_refs(hash);
            let freed = self.blobs.remove(hash);
            if let Some(blob) = freed {
                self.gc_freed_blobs += 1;
                self.gc_freed_bytes += blob.data.len() as u64;
            }
            for r in refs {
                self.dec_ref_tree(&r);
            }
        }
    }

    /// Extract all hash references embedded in a blob, based on node type.
    /// Returns references that need inc/dec when the parent is retained/freed.
    fn extract_refs(&self, hash: &Hash256) -> Vec<Hash256> {
        let data = match self.blobs.get(hash) {
            Some(b) => &b.data,
            None => return Vec::new(),
        };

        if data.len() < 8 { return Vec::new(); }

        let type_id = u16::from_le_bytes([data[0], data[1]]);
        let p = &data[8..]; // payload after v1 header

        let mut refs = Vec::new();
        match type_id {
            0x0001 if p.len() >= 64 => {
                // SceneRoot: children[0], camera[32]
                refs.push(read_hash(p, 0));
                refs.push(read_hash(p, 32));
            }
            0x0002 if p.len() >= 96 => {
                // SceneNode: transform[0], renderable[32], children[64]
                refs.push(read_hash(p, 0));
                refs.push(read_hash(p, 32));
                refs.push(read_hash(p, 64));
            }
            0x0004 => {
                // Transform: no refs
            }
            0x0005 if p.len() >= 64 => {
                // Renderable: geometry[0], material[32]
                refs.push(read_hash(p, 0));
                refs.push(read_hash(p, 32));
            }
            0x0200 | 0x0201 | 0x0202 => {
                // Materials: no hash refs in v1 solid/gradient (PBR has texture hashes)
                if type_id == 0x0202 && p.len() >= 148 {
                    refs.push(read_hash(p, 16));  // base color texture
                    refs.push(read_hash(p, 48));  // normal map
                    refs.push(read_hash(p, 80));  // metallic-roughness
                    refs.push(read_hash(p, 112)); // emissive
                }
            }
            0x0003 if p.len() >= 48 => {
                // Camera: view_transform[16]
                refs.push(read_hash(p, 16));
            }
            0x0100 if p.len() >= 72 => {
                // Mesh: vertex_data[8], index_data[40]
                refs.push(read_hash(p, 8));
                refs.push(read_hash(p, 40));
            }
            0x0101 if p.len() >= 40 => {
                // Path: segment_data[8]
                refs.push(read_hash(p, 8));
            }
            0x0009 if p.len() >= 4 => {
                // NodeList: count[0], then N×hash[4..]
                let count = u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize;
                for i in 0..count {
                    let offset = 4 + i * 32;
                    if offset + 32 > p.len() { break; }
                    refs.push(read_hash(p, offset));
                }
            }
            0x0300 if p.len() >= 40 => {
                // TextNode: font_hash[8]
                refs.push(read_hash(p, 8));
            }
            _ => {}
        }

        refs.retain(|h| *h != NULL_HASH);
        refs
    }

    // ── Upload protocol ─────────────────────────────────────────

    pub fn begin_upload(&mut self, upload_id: u32, total_size: usize, initial_data: &[u8]) {
        let take = initial_data.len().min(total_size);
        self.upload_staging.insert(upload_id, StagedUpload {
            data: initial_data[..take].to_vec(),
            total_size,
        });
    }

    pub fn append_upload(&mut self, upload_id: u32, data: &[u8]) {
        if let Some(staged) = self.upload_staging.get_mut(&upload_id) {
            let remaining = staged.total_size.saturating_sub(staged.data.len());
            let take = data.len().min(remaining);
            if take > 0 {
                staged.data.extend_from_slice(&data[..take]);
            }
        }
    }

    pub fn finish_upload(&mut self, upload_id: u32) -> Option<Hash256> {
        if let Some(staged) = self.upload_staging.remove(&upload_id) {
            Some(self.store(&staged.data))
        } else {
            None
        }
    }

    // ── Stats ───────────────────────────────────────────────────

    pub fn blob_count(&self) -> usize {
        self.blobs.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.blobs.values().map(|b| b.data.len()).sum()
    }
}

fn read_hash(data: &[u8], offset: usize) -> Hash256 {
    let mut h = [0u8; 32];
    h.copy_from_slice(&data[offset..offset + 32]);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_transform() -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[0] = 0x03;
        b
    }

    fn make_node(xform: &Hash256, rend: &Hash256) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[0] = 0x02;
        b[1] = 0x01;
        b[32..64].copy_from_slice(xform);
        b[64..96].copy_from_slice(rend);
        b
    }

    fn make_list(entries: &[Hash256]) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[0] = 0x10;
        b[1] = entries.len() as u8;
        for (i, h) in entries.iter().enumerate() {
            let off = 36 + i * 32;
            b[off..off+32].copy_from_slice(h);
        }
        b
    }

    fn make_root(list: &Hash256) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[0] = 0x01;
        b[32..64].copy_from_slice(list);
        b
    }

    #[test]
    fn test_gc_frees_old_scene() {
        let mut cas = CasStore::new();

        // build scene v1
        let xform = cas.store(&make_transform());
        let node = cas.store(&make_node(&xform, &NULL_HASH));
        let list = cas.store(&make_list(&[node]));
        let root1 = cas.store(&make_root(&list));

        // SET_ROOT(root1): claim the tree
        cas.inc_ref_tree(&root1);
        assert_eq!(cas.blob_count(), 4);
        assert_eq!(cas.ref_count(&root1), 1);
        assert_eq!(cas.ref_count(&xform), 1);

        // build scene v2 (different transform)
        let xform2 = cas.store(&{
            let mut b = make_transform();
            b[64] = 0x42;
            b
        });
        let node2 = cas.store(&make_node(&xform2, &NULL_HASH));
        let list2 = cas.store(&make_list(&[node2]));
        let root2 = cas.store(&make_root(&list2));

        assert_eq!(cas.blob_count(), 8); // 4 old + 4 new (new at refcount=0)

        // SET_ROOT(root2): inc new tree, dec old tree
        cas.inc_ref_tree(&root2);
        cas.dec_ref_tree(&root1);

        // old tree freed (refcount 1→0 cascades), new tree alive (refcount=1)
        assert_eq!(cas.blob_count(), 4);
        assert!(cas.exists(&root2));
        assert!(cas.exists(&xform2));
        assert!(!cas.exists(&root1));
        assert!(!cas.exists(&xform));
    }

    #[test]
    fn test_gc_preserves_shared_subtrees() {
        let mut cas = CasStore::new();

        // shared mesh data (used by both scenes)
        let mesh_data = cas.store(b"shared_mesh_vertices");

        // scene v1: xform_a + shared mesh
        let xform_a = cas.store(&make_transform());
        let node_a = cas.store(&make_node(&xform_a, &mesh_data));
        let list = cas.store(&make_list(&[node_a]));
        let root1 = cas.store(&make_root(&list));
        cas.inc_ref_tree(&root1);

        // mesh_data refcount = 1 (from inc_ref_tree via node_a → renderable slot)
        assert_eq!(cas.ref_count(&mesh_data), 1);

        // scene v2: xform_b + SAME mesh
        let xform_b = cas.store(&{
            let mut b = make_transform();
            b[64] = 0x99;
            b
        });
        let node_b = cas.store(&make_node(&xform_b, &mesh_data));
        let list2 = cas.store(&make_list(&[node_b]));
        let root2 = cas.store(&make_root(&list2));

        // SET_ROOT(root2)
        cas.inc_ref_tree(&root2);
        // mesh_data: +1 from root2's tree = 2
        assert_eq!(cas.ref_count(&mesh_data), 2);

        cas.dec_ref_tree(&root1);
        // mesh_data: -1 from root1's tree = 1
        // old-only blobs (xform_a, node_a, list, root1): 1→0, freed

        assert!(!cas.exists(&root1));
        assert!(!cas.exists(&xform_a));
        assert!(!cas.exists(&node_a));
        assert!(cas.exists(&root2));
        assert!(cas.exists(&mesh_data)); // shared — survived GC
        assert_eq!(cas.ref_count(&mesh_data), 1); // one ref from root2's tree
    }
}
