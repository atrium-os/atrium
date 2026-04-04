use std::collections::HashMap;
use crate::command::protocol::{Hash256, NULL_HASH};
use crate::cas::store::CasStore;
use crate::scene::graph::RenderItem;
use crate::scene::nodes::NodeData;

pub const SLOT_TYPE_NODE: u16 = 0;
pub const SLOT_TYPE_GROUP: u16 = 1;
pub const SLOT_TYPE_TEXT: u16 = 2;

pub const SLOT_FLAG_VISIBLE: u32 = 0x01;
pub const SLOT_FLAG_CLIP: u32 = 0x08;

pub struct TextData {
    pub size: f32,
    pub color: u32,
    pub font_hash: Hash256,
    pub text: [u8; 80],
    pub text_len: usize,
}

pub struct Slot {
    pub active: bool,
    pub node_type: u16,
    pub flags: u32,
    pub transform: [f32; 16],
    pub renderable_hash: Hash256,
    pub children: Vec<u16>,
    pub clip_rect: [f32; 4],
    pub text: Option<TextData>,
    pub cas_subtree: Hash256,  // if not NULL, traverse this CAS node list as children
}

impl Slot {
    pub fn new(node_type: u16, flags: u32) -> Self {
        Self {
            active: true,
            node_type,
            flags,
            transform: IDENTITY,
            renderable_hash: NULL_HASH,
            children: Vec::new(),
            clip_rect: [0.0; 4],
            text: None,
            cas_subtree: NULL_HASH,
        }
    }
}

// Opcode for setting CAS subtree on a slot
pub const CMD_SLOT_SET_CAS_CHILDREN: u16 = 0x0118;

const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0,
    0.0, 1.0, 0.0, 0.0,
    0.0, 0.0, 1.0, 0.0,
    0.0, 0.0, 0.0, 1.0,
];

pub struct SlotTable {
    slots: HashMap<u16, Slot>,
    pub root_slot: Option<u16>,
    pub camera_hash: Hash256,
    pub dirty: bool,
    pub tess_cache: HashMap<Hash256, Hash256>,
}

impl SlotTable {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
            root_slot: None,
            camera_hash: NULL_HASH,
            dirty: true,
            tess_cache: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.slots.clear();
        self.root_slot = None;
        self.camera_hash = NULL_HASH;
        self.dirty = true;
        self.tess_cache.clear();
    }

    pub fn is_active(&self) -> bool {
        self.root_slot.is_some()
    }

    pub fn alloc(&mut self, slot_id: u16, node_type: u16, flags: u32) {
        self.slots.insert(slot_id, Slot::new(node_type, flags));
        self.dirty = true;
    }

    pub fn free(&mut self, slot_id: u16) {
        self.slots.remove(&slot_id);
        if self.root_slot == Some(slot_id) {
            self.root_slot = None;
        }
        self.dirty = true;
    }

    pub fn set_transform_inline(&mut self, slot_id: u16, matrix: [f32; 16]) {
        if let Some(slot) = self.slots.get_mut(&slot_id) {
            slot.transform = matrix;
            self.dirty = true;
        }
    }

    pub fn set_transform_translate(&mut self, slot_id: u16, x: f32, y: f32, z: f32) {
        if let Some(slot) = self.slots.get_mut(&slot_id) {
            slot.transform = [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                x,   y,   z,   1.0,
            ];
            self.dirty = true;
        }
    }

    pub fn set_content(&mut self, slot_id: u16, renderable_hash: Hash256) {
        if let Some(slot) = self.slots.get_mut(&slot_id) {
            slot.renderable_hash = renderable_hash;
            self.dirty = true;
        }
    }

    pub fn set_children(&mut self, slot_id: u16, children: Vec<u16>) {
        if let Some(slot) = self.slots.get_mut(&slot_id) {
            slot.children = children;
            self.dirty = true;
        }
    }

    pub fn set_flags(&mut self, slot_id: u16, flags: u32, clip_rect: [f32; 4]) {
        if let Some(slot) = self.slots.get_mut(&slot_id) {
            slot.flags = flags;
            slot.clip_rect = clip_rect;
            self.dirty = true;
        }
    }

    pub fn set_root(&mut self, slot_id: u16) {
        self.root_slot = Some(slot_id);
        self.dirty = true;
    }

    pub fn set_cas_subtree(&mut self, slot_id: u16, hash: Hash256) {
        if let Some(slot) = self.slots.get_mut(&slot_id) {
            slot.cas_subtree = hash;
            self.dirty = true;
        }
    }

    pub fn set_text(&mut self, slot_id: u16, text_data: TextData) {
        if let Some(slot) = self.slots.get_mut(&slot_id) {
            slot.text = Some(text_data);
            self.dirty = true;
        }
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Returns (render_list, camera_hash, cas_subtrees).
    /// cas_subtrees: Vec of (list_hash, world_matrix, clip) for CAS bridge nodes.
    pub fn traverse(&self, cas: &mut CasStore) -> (Vec<RenderItem>, Hash256, Vec<(Hash256, [f32; 16], Option<[f32; 4]>)>) {
        let mut render_list = Vec::new();
        let mut cas_subtrees = Vec::new();

        let root_id = match self.root_slot {
            Some(id) => id,
            None => return (render_list, self.camera_hash, cas_subtrees),
        };

        self.traverse_slot(cas, root_id, &IDENTITY, None, &mut render_list, &mut cas_subtrees);
        (render_list, self.camera_hash, cas_subtrees)
    }

    fn traverse_slot(
        &self,
        cas: &mut CasStore,
        slot_id: u16,
        parent_matrix: &[f32; 16],
        parent_clip: Option<[f32; 4]>,
        render_list: &mut Vec<RenderItem>,
        cas_subtrees: &mut Vec<(Hash256, [f32; 16], Option<[f32; 4]>)>,
    ) {
        let slot = match self.slots.get(&slot_id) {
            Some(s) if s.active => s,
            _ => return,
        };

        if slot.flags & SLOT_FLAG_VISIBLE == 0 { return; }

        let world_matrix = mat4_mul(parent_matrix, &slot.transform);

        let clip = if slot.flags & SLOT_FLAG_CLIP != 0 {
            Some(slot.clip_rect)
        } else {
            parent_clip
        };

        if slot.renderable_hash != NULL_HASH {
            self.add_renderable(cas, &slot.renderable_hash, &world_matrix, clip, render_list);
        }

        if let Some(ref text) = slot.text {
            cas.mark_alive(&text.font_hash);
            // Text nodes will be handled by the renderer via font_cache
            // For now, store as a special render item (TODO: integrate with font rendering)
        }

        for &child_id in &slot.children {
            self.traverse_slot(cas, child_id, &world_matrix, clip, render_list, cas_subtrees);
        }

        // Collect CAS subtree for later processing by SceneGraph (with font_cache)
        if slot.cas_subtree != NULL_HASH {
            cas_subtrees.push((slot.cas_subtree, world_matrix, clip));
        }
    }

    fn add_renderable(
        &self,
        cas: &mut CasStore,
        renderable_hash: &Hash256,
        world_matrix: &[f32; 16],
        clip_rect: Option<[f32; 4]>,
        render_list: &mut Vec<RenderItem>,
    ) {
        cas.mark_alive(renderable_hash);
        let rend_data = match cas.load(renderable_hash) {
            Some(d) => d.to_vec(),
            None => return,
        };

        if let Some(NodeData::Renderable(r)) = NodeData::parse(&rend_data) {
            cas.mark_alive(&r.material);
            let (mesh_hash, stencil) = self.resolve_mesh(cas, &r.mesh);
            render_list.push(RenderItem {
                world_matrix: *world_matrix,
                mesh: mesh_hash,
                material: r.material,
                render_order: r.render_order,
                flags: r.flags,
                stencil_fill: stencil,
                clip_rect,
            });
        }
    }

    fn resolve_mesh(&self, cas: &mut CasStore, mesh_hash: &Hash256) -> (Hash256, bool) {
        if *mesh_hash == NULL_HASH { return (NULL_HASH, false); }

        cas.mark_alive(mesh_hash);
        let mesh_data = match cas.load(mesh_hash) {
            Some(d) => d,
            None => return (*mesh_hash, false),
        };

        if mesh_data.len() < 48 || u16::from_le_bytes([mesh_data[0], mesh_data[1]]) != 0x0101 {
            return (*mesh_hash, false);
        }

        let path_header = match NodeData::parse(mesh_data) {
            Some(NodeData::Path(p)) => p,
            _ => return (*mesh_hash, false),
        };

        cas.mark_alive(&path_header.path_data);
        if let Some(&cached) = self.tess_cache.get(&path_header.path_data) {
            if cas.exists(&cached) {
                cas.mark_alive(&cached);
                if let Some(md) = cas.load(&cached) {
                    if md.len() >= 80 {
                        let vh = read_hash(md, 16);
                        let ih = read_hash(md, 48);
                        cas.mark_alive(&vh);
                        cas.mark_alive(&ih);
                    }
                }
                return (cached, true);
            }
        }

        (*mesh_hash, true)
    }
}

fn read_hash(data: &[u8], offset: usize) -> Hash256 {
    let mut h = [0u8; 32];
    h.copy_from_slice(&data[offset..offset + 32]);
    h
}

fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut r = [0f32; 16];
    for row in 0..4 {
        for col in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[row * 4 + k] * b[k * 4 + col];
            }
            r[row * 4 + col] = sum;
        }
    }
    r
}
