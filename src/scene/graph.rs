use crate::command::protocol::{Hash256, NULL_HASH};
use crate::cas::store::CasStore;
use crate::scene::nodes::*;
use crate::render::tessellate;
use std::collections::HashMap;

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
    tess_cache: HashMap<Hash256, Hash256>,
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
            tess_cache: HashMap::new(),
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
            let mesh_hash = self.resolve_mesh(cas, &r.mesh);
            self.render_list.push(RenderItem {
                world_matrix: *world_matrix,
                mesh: mesh_hash,
                material: r.material,
                render_order: r.render_order,
                flags: r.flags,
            });
        }
    }

    fn resolve_mesh(&mut self, cas: &CasStore, mesh_hash: &Hash256) -> Hash256 {
        if *mesh_hash == NULL_HASH { return NULL_HASH; }

        // check if this is a PathHeader (type 0x0D) — needs tessellation
        let mesh_data = match cas.load(mesh_hash) {
            Some(d) => d,
            None => return *mesh_hash,
        };

        if mesh_data.len() < 128 || mesh_data[0] != 0x0D {
            return *mesh_hash; // regular MeshHeader, use as-is
        }

        // PathHeader — check tessellation cache
        let path_header = match NodeData::parse(mesh_data) {
            Some(NodeData::Path(p)) => p,
            _ => return *mesh_hash,
        };

        // check cache: path_data hash → tessellated mesh hash
        if let Some(&cached) = self.tess_cache.get(&path_header.path_data) {
            if cas.exists(&cached) {
                return cached;
            }
        }

        // need to tessellate — but CAS is immutable here
        // store the path_data hash so render_frame can tessellate with mutable CAS
        // for now, return NULL_HASH to signal "needs tessellation"
        // the render will skip items with NULL mesh
        *mesh_hash // return the PathHeader hash; renderer will detect and tessellate
    }

    pub fn tessellate_paths(
        &mut self,
        cas: &mut CasStore,
        display_w: u32,
        display_h: u32,
        mut gpu_tess: Option<&mut dyn FnMut(&[u8], f32, bool) -> Option<(Vec<f32>, Vec<u16>)>>,
    ) {
        // half pixel in NDC space — sub-pixel accuracy, resolution independent
        let pixel_tolerance = 2.0 / display_w.max(display_h).max(1) as f32 * 0.5;
        for item in &mut self.render_list {
            if item.mesh == NULL_HASH { continue; }

            let mesh_data = match cas.load(&item.mesh) {
                Some(d) => d.to_vec(),
                None => continue,
            };

            if mesh_data.len() < 128 || mesh_data[0] != 0x0D { continue; }

            let path_header = match NodeData::parse(&mesh_data) {
                Some(NodeData::Path(p)) => p,
                _ => continue,
            };

            // check cache
            if let Some(&cached) = self.tess_cache.get(&path_header.path_data) {
                if cas.exists(&cached) {
                    item.mesh = cached;
                    continue;
                }
            }

            // load path data and tessellate
            let path_data = match cas.load(&path_header.path_data) {
                Some(d) => d.to_vec(),
                None => continue,
            };

            let tolerance = if path_header.tolerance > 0.0 {
                path_header.tolerance.min(pixel_tolerance)
            } else {
                pixel_tolerance
            };
            let is_fill = path_header.flags & 0x02 == 0;

            // try GPU tessellation first, fall back to CPU
            let result = if let Some(ref mut gpu) = gpu_tess {
                gpu(&path_data, tolerance, is_fill)
            } else {
                None
            };

            let (verts, indices) = if let Some(r) = result {
                r
            } else {
                log::trace!("CPU tessellation fallback for path {:02x}{:02x}..", path_header.path_data[0], path_header.path_data[1]);
                let segments = PathSegment::parse_segments(&path_data);
                if !is_fill {
                    tessellate::tessellate_stroke(&segments, path_header.stroke_width, tolerance)
                } else {
                    tessellate::tessellate_fill(&segments, tolerance)
                }
            };

            if verts.is_empty() { continue; }

            // store vertex data
            let vert_bytes: Vec<u8> = verts.iter().flat_map(|f| f.to_le_bytes()).collect();
            let vert_hash = cas.store(&vert_bytes);

            // store index data
            let idx_bytes: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
            let idx_hash = cas.store(&idx_bytes);

            // create MeshHeader
            let vertex_count = (verts.len() / 3) as u32;
            let index_count = indices.len() as u32;
            let mut mesh_blob = [0u8; 128];
            mesh_blob[0] = 0x08; // MeshHeader
            mesh_blob[1] = 0x01; // INDEXED
            mesh_blob[2..6].copy_from_slice(&vertex_count.to_le_bytes());
            mesh_blob[6..10].copy_from_slice(&index_count.to_le_bytes());
            mesh_blob[10..12].copy_from_slice(&12u16.to_le_bytes()); // stride = 12 (float3)
            mesh_blob[32..64].copy_from_slice(&vert_hash);
            mesh_blob[64..96].copy_from_slice(&idx_hash);
            let mesh_hash = cas.store(&mesh_blob);

            // cache and update render item
            self.tess_cache.insert(path_header.path_data, mesh_hash);
            item.mesh = mesh_hash;
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
