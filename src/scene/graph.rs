use crate::command::protocol::{Hash256, NULL_HASH};
use crate::cas::store::CasStore;
use crate::scene::nodes::*;
use crate::render::tessellate;
use crate::render::font::FontData;
use std::collections::HashMap;

pub struct RenderItem {
    pub world_matrix: [f32; 16],
    pub mesh: Hash256,
    pub material: Hash256,
    pub render_order: u16,
    pub flags: u8,
    pub stencil_fill: bool,
    pub clip_rect: Option<[f32; 4]>, // world-space [x, y, w, h] if clipped
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
    font_cache: HashMap<Hash256, FontData>,
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
            font_cache: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.root_hash = NULL_HASH;
        self.camera_hash = NULL_HASH;
        self.prev_root = NULL_HASH;
        self.render_list.clear();
        self.light_list.clear();
        self.dirty = true;
        self.tess_cache.clear();
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

    pub fn traverse(&mut self, cas: &mut CasStore) {
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
            self.traverse_node_list(cas, &root.child_list, &identity, None);
        } else {
            log::trace!("traverse: root has NULL child_list");
        }

        self.dirty = false;
    }

    fn traverse_node_list(
        &mut self,
        cas: &mut CasStore,
        list_hash: &Hash256,
        parent_matrix: &[f32; 16],
        clip: Option<[f32; 4]>,
    ) {
        let list_data = match cas.load(list_hash) {
            Some(d) => d.to_vec(),
            None => {
                log::warn!("traverse_node_list: {:02x}{:02x}.. not in CAS", list_hash[0], list_hash[1]);
                return;
            }
        };

        if list_data.len() < 12 {
            log::warn!("traverse_node_list: blob too short ({})", list_data.len());
            return;
        }
        let p = &list_data[8..];
        let count = u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize;
        log::trace!("traverse_node_list: v1 count={} len={}", count, list_data.len());

        for i in 0..count {
            let offset = 4 + i * 32;
            if offset + 32 > p.len() { break; }
            let node_hash = read_hash_from(p, offset);
            let exists = cas.exists(&node_hash);
            log::trace!("  node[{}]: {:02x}{:02x}.. exists={}", i, node_hash[0], node_hash[1], exists);
            if exists {
                self.traverse_node(cas, &node_hash, parent_matrix, clip);
            }
        }
    }

    fn traverse_node(
        &mut self,
        cas: &mut CasStore,
        node_hash: &Hash256,
        parent_matrix: &[f32; 16],
        parent_clip: Option<[f32; 4]>,
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

                // Check clip flag (bit 3) — clip children to screen-pixel rect
                let clip = if node.flags & 0x08 != 0 {
                    if node_data.len() >= 8 + 112 {
                        let p = &node_data[8..];
                        let cx = f32::from_le_bytes([p[96], p[97], p[98], p[99]]);
                        let cy = f32::from_le_bytes([p[100], p[101], p[102], p[103]]);
                        let cw = f32::from_le_bytes([p[104], p[105], p[106], p[107]]);
                        let ch = f32::from_le_bytes([p[108], p[109], p[110], p[111]]);
                        Some([cx, cy, cw, ch]) // screen pixels from WM
                    } else {
                        parent_clip
                    }
                } else {
                    parent_clip
                };

                if node.renderable != NULL_HASH {
                    self.add_renderable(cas, &node.renderable, &world_matrix, clip);
                }

                if node.children != NULL_HASH {
                    self.traverse_node_list(cas, &node.children, &world_matrix, clip);
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

            NodeData::Text(text_node) => {
                let font_hash = text_node.font_hash;
                if !self.font_cache.contains_key(&font_hash) {
                    if let Some(font_data) = cas.load(&font_hash) {
                        if let Some(fd) = FontData::load(font_data) {
                            self.font_cache.insert(font_hash, fd);
                        } else {
                            log::warn!("traverse: font {:02x}{:02x}.. failed to parse", font_hash[0], font_hash[1]);
                        }
                    } else {
                        log::warn!("traverse: font {:02x}{:02x}.. not in CAS", font_hash[0], font_hash[1]);
                    }
                }
                if let Some(font) = self.font_cache.get_mut(&font_hash) {
                    let glyphs = font.layout_text(cas, &text_node.text, text_node.size, 0.0, 0.0);
                    let color = text_node.color;
                    let mut mat_blob = [0u8; 16];
                    mat_blob[0..2].copy_from_slice(&0x0200u16.to_le_bytes());
                    mat_blob[2..4].copy_from_slice(&1u16.to_le_bytes());
                    mat_blob[8..12].copy_from_slice(&color.to_le_bytes());
                    let mat_hash = cas.store(&mat_blob);

                    for (glyph_hash, gx, gy) in &glyphs {
                        let (mesh_hash, _stencil) = self.resolve_mesh(cas, glyph_hash);
                        if mesh_hash != NULL_HASH {
                            let mut glyph_matrix = *parent_matrix;
                            glyph_matrix[12] += parent_matrix[0] * gx + parent_matrix[4] * gy;
                            glyph_matrix[13] += parent_matrix[1] * gx + parent_matrix[5] * gy;
                            glyph_matrix[14] += parent_matrix[2] * gx + parent_matrix[6] * gy;
                            self.render_list.push(RenderItem {
                                world_matrix: glyph_matrix,
                                mesh: mesh_hash,
                                material: mat_hash,
                                render_order: 0,
                                flags: 0x01,
                                stencil_fill: true,
                                clip_rect: parent_clip,
                            });
                        }
                    }
                }
            }

            _ => {}
        }
    }

    fn add_renderable(
        &mut self,
        cas: &mut CasStore,
        renderable_hash: &Hash256,
        world_matrix: &[f32; 16],
        clip_rect: Option<[f32; 4]>,
    ) {
        let rend_data = match cas.load(renderable_hash) {
            Some(d) => d.to_vec(),
            None => return,
        };

        if let Some(NodeData::Renderable(r)) = NodeData::parse(&rend_data) {
            let (mesh_hash, stencil) = self.resolve_mesh(cas, &r.mesh);
            self.render_list.push(RenderItem {
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

    fn resolve_mesh(&mut self, cas: &mut CasStore, mesh_hash: &Hash256) -> (Hash256, bool) {
        if *mesh_hash == NULL_HASH { return (NULL_HASH, false); }

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

        if let Some(&cached) = self.tess_cache.get(&path_header.path_data) {
            if cas.exists(&cached) {
                return (cached, true); // all paths use stencil even-odd
            }
        }

        (*mesh_hash, true) // all paths use stencil even-odd
    }

    pub fn tessellate_paths(
        &mut self,
        cas: &mut CasStore,
        display_w: u32,
        display_h: u32,
        mut gpu_tess: Option<&mut dyn FnMut(&[u8], f32, bool) -> Option<(Vec<f32>, Vec<u16>)>>,
    ) {
        let pixel_tolerance = 2.0 / display_w.max(display_h).max(1) as f32 * 0.5;
        for item in &mut self.render_list {
            if item.mesh == NULL_HASH { continue; }

            let mesh_data = match cas.load(&item.mesh) {
                Some(d) => d.to_vec(),
                None => continue,
            };

            if mesh_data.len() < 48 || u16::from_le_bytes([mesh_data[0], mesh_data[1]]) != 0x0101 { continue; }

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

            // load path segment data (skip v1 header)
            let path_data = match cas.load(&path_header.path_data) {
                Some(d) if d.len() > 8 => d[8..].to_vec(),
                _ => continue,
            };

            let tolerance = if path_header.tolerance > 0.0 {
                path_header.tolerance.min(pixel_tolerance)
            } else {
                pixel_tolerance
            };
            let is_fill = path_header.flags & 0x03 == 0; // draw_mode: 0=fill, 1=stroke, 2=both

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

            if verts.is_empty() {
                continue;
            }

            // store vertex data (v1 0x0110)
            let raw_verts: Vec<u8> = verts.iter().flat_map(|f| f.to_le_bytes()).collect();
            let mut vert_blob = Vec::with_capacity(8 + raw_verts.len());
            vert_blob.extend_from_slice(&0x0110u16.to_le_bytes());
            vert_blob.extend_from_slice(&1u16.to_le_bytes());
            vert_blob.extend_from_slice(&0u32.to_le_bytes());
            vert_blob.extend_from_slice(&raw_verts);
            let vert_hash = cas.store(&vert_blob);

            // store index data (v1 0x0111)
            let raw_idx: Vec<u8> = indices.iter().flat_map(|i| i.to_le_bytes()).collect();
            let mut idx_blob = Vec::with_capacity(8 + raw_idx.len());
            idx_blob.extend_from_slice(&0x0111u16.to_le_bytes());
            idx_blob.extend_from_slice(&1u16.to_le_bytes());
            idx_blob.extend_from_slice(&0u32.to_le_bytes());
            idx_blob.extend_from_slice(&raw_idx);
            let idx_hash = cas.store(&idx_blob);

            // create v1 Mesh blob (0x0100)
            let vertex_count = (verts.len() / 3) as u32;
            let index_count = indices.len() as u32;
            let mut mesh_blob = [0u8; 80]; // 8 header + 72 payload
            mesh_blob[0..2].copy_from_slice(&0x0100u16.to_le_bytes()); // Mesh type
            mesh_blob[2..4].copy_from_slice(&1u16.to_le_bytes()); // version
            // flags: triangles(0) | u16 indices(0) | POSITION bit(0x0100)
            mesh_blob[4..8].copy_from_slice(&0x0100u32.to_le_bytes());
            mesh_blob[8..12].copy_from_slice(&vertex_count.to_le_bytes());
            mesh_blob[12..16].copy_from_slice(&index_count.to_le_bytes());
            mesh_blob[16..48].copy_from_slice(&vert_hash);
            mesh_blob[48..80].copy_from_slice(&idx_hash);
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
