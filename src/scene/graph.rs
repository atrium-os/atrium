use crate::command::protocol::{Hash256, NULL_HASH};
use crate::cas::store::CasStore;
use crate::scene::nodes::*;

pub struct RenderItem {
    pub world_matrix: [f32; 16],
    pub mesh: Hash256,
    pub material: Hash256,
    pub render_order: u16,
    pub flags: u8,
}

pub struct LightItem {
    pub world_matrix: [f32; 16],
    pub light: Light,
}

pub struct SceneGraph {
    pub root_hash: Hash256,
    pub camera_hash: Hash256,
    prev_root: Hash256,
    render_list: Vec<RenderItem>,
    light_list: Vec<LightItem>,
    dirty: bool,
}

impl SceneGraph {
    pub fn new() -> Self {
        Self {
            root_hash: NULL_HASH,
            camera_hash: NULL_HASH,
            prev_root: NULL_HASH,
            render_list: Vec::new(),
            light_list: Vec::new(),
            dirty: true,
        }
    }

    pub fn set_root(&mut self, hash: Hash256) {
        self.prev_root = self.root_hash;
        self.root_hash = hash;
        self.dirty = true;
    }

    pub fn prev_root(&self) -> Hash256 {
        self.prev_root
    }

    pub fn set_camera(&mut self, hash: Hash256) {
        self.camera_hash = hash;
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn render_list(&self) -> &[RenderItem] {
        &self.render_list
    }

    pub fn light_list(&self) -> &[LightItem] {
        &self.light_list
    }

    pub fn traverse(&mut self, cas: &CasStore) {
        self.render_list.clear();
        self.light_list.clear();

        if self.root_hash == NULL_HASH { return; }

        let root_data = match cas.load(&self.root_hash) {
            Some(d) => d.to_vec(),
            None => {
                log::warn!("traverse: root {:02x}{:02x}.. not in CAS", self.root_hash[0], self.root_hash[1]);
                return;
            }
        };

        let root = match NodeData::parse(&root_data) {
            Some(NodeData::Root(r)) => r,
            other => {
                log::warn!("traverse: root blob type={} len={} parsed={:?}",
                    root_data[0], root_data.len(),
                    other.as_ref().map(|n| std::mem::discriminant(n)));
                return;
            }
        };

        log::trace!("traverse: root child_list={:02x}{:02x}.. camera={:02x}{:02x}.. (blob[0]={} len={})",
            root.child_list[0], root.child_list[1],
            root.camera[0], root.camera[1],
            root_data[0], root_data.len());

        if root.camera != NULL_HASH {
            self.camera_hash = root.camera;
        }

        if root.child_list != NULL_HASH {
            if !cas.exists(&root.child_list) {
                log::warn!("traverse: child_list {:02x}{:02x}.. NOT in CAS!", root.child_list[0], root.child_list[1]);
            }
            let identity = [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, 0.0, 1.0f32,
            ];
            self.traverse_node_list(cas, &root.child_list, &identity);
        } else {
            log::trace!("traverse: root has NULL child_list");
        }

        self.dirty = false;
    }

    fn traverse_node_list(
        &mut self,
        cas: &CasStore,
        list_hash: &Hash256,
        parent_matrix: &[f32; 16],
    ) {
        let list_data = match cas.load(list_hash) {
            Some(d) => d.to_vec(),
            None => {
                log::warn!("traverse_node_list: {:02x}{:02x}.. not in CAS", list_hash[0], list_hash[1]);
                return;
            }
        };

        if list_data.len() < 36 {
            log::warn!("traverse_node_list: blob too short ({})", list_data.len());
            return;
        }
        let count = list_data[1] as usize;
        let next_hash = read_hash_from(&list_data, 4);
        log::trace!("traverse_node_list: type=0x{:02x} count={} len={}", list_data[0], count, list_data.len());

        for i in 0..count {
            let offset = 36 + i * 32;
            if offset + 32 > list_data.len() { break; }
            let node_hash = read_hash_from(&list_data, offset);
            let exists = cas.exists(&node_hash);
            log::trace!("  node[{}]: {:02x}{:02x}.. exists={}", i, node_hash[0], node_hash[1], exists);
            if exists {
                self.traverse_node(cas, &node_hash, parent_matrix);
            }
        }

        if next_hash != NULL_HASH {
            self.traverse_node_list(cas, &next_hash, parent_matrix);
        }
    }

    fn traverse_node(
        &mut self,
        cas: &CasStore,
        node_hash: &Hash256,
        parent_matrix: &[f32; 16],
    ) {
        let node_data = match cas.load(node_hash) {
            Some(d) => d.to_vec(),
            None => return,
        };

        let parsed = match NodeData::parse(&node_data) {
            Some(p) => p,
            None => return,
        };

        match parsed {
            NodeData::Node(node) => {
                if node.flags & 0x01 == 0 { return; } // !VISIBLE

                let world_matrix = if node.transform != NULL_HASH {
                    match load_transform(cas, &node.transform) {
                        Some(t) => mat4_mul(parent_matrix, &t.matrix),
                        None => *parent_matrix,
                    }
                } else {
                    *parent_matrix
                };

                if node.renderable != NULL_HASH {
                    self.add_renderable(cas, &node.renderable, &world_matrix);
                }

                if node.children != NULL_HASH {
                    self.traverse_node_list(cas, &node.children, &world_matrix);
                }
            }

            NodeData::Light(light) => {
                let world_matrix = if light.transform != NULL_HASH {
                    match load_transform(cas, &light.transform) {
                        Some(t) => mat4_mul(parent_matrix, &t.matrix),
                        None => *parent_matrix,
                    }
                } else {
                    *parent_matrix
                };

                self.light_list.push(LightItem { world_matrix, light });
            }

            _ => {}
        }
    }

    fn add_renderable(
        &mut self,
        cas: &CasStore,
        renderable_hash: &Hash256,
        world_matrix: &[f32; 16],
    ) {
        let rend_data = match cas.load(renderable_hash) {
            Some(d) => d.to_vec(),
            None => return,
        };

        if let Some(NodeData::Renderable(r)) = NodeData::parse(&rend_data) {
            self.render_list.push(RenderItem {
                world_matrix: *world_matrix,
                mesh: r.mesh,
                material: r.material,
                render_order: r.render_order,
                flags: r.flags,
            });
        }
    }

    pub fn camera(&self, cas: &CasStore) -> Option<(Camera, Transform)> {
        if self.camera_hash == NULL_HASH { return None; }
        let cam_data = cas.load(&self.camera_hash)?.to_vec();
        let cam = match NodeData::parse(&cam_data)? {
            NodeData::Camera(c) => c,
            _ => return None,
        };
        let xform = load_transform(cas, &cam.view_transform)?;
        Some((cam, xform))
    }
}

fn load_transform(cas: &CasStore, hash: &Hash256) -> Option<Transform> {
    let data = cas.load(hash)?.to_vec();
    match NodeData::parse(&data)? {
        NodeData::Transform(t) => Some(t),
        _ => None,
    }
}

fn read_hash_from(data: &[u8], offset: usize) -> Hash256 {
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
