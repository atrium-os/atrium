//! Scene to kernel input: each window's nodes, as frescod's scene server
//! holds them, packed into a `fresco_compute::Frame`. Needs no GPU, so a
//! jailed window renderer (F4b) builds its frames with the same code the
//! single-process frescod uses.

use std::collections::HashMap;

use fresco_protocol::{PathParams, RectParams, TextureParams};
use fresco_scene_server::command::envelope_frontend::EnvelopeFrontend;
use fresco_vulkan::{
    GlyphInstance, GlyphRunBatch, GlyphRunNode, PathNode, SceneNode, TextureBatch, TextureNode, UploadRequest,
};

/// One window's scene for a frame, in the window's own coordinates.
#[derive(Default)]
pub struct WindowFrame {
    pub id: u32,
    /// Screen position of the window's (0, 0).
    pub pos: (f32, f32),
    /// Its surface size in pixels.
    pub size: (u32, u32),
    /// Already packed (by a window renderer, F4b): the nodes are empty and
    /// this frame, for a surface of its own size, is used as is.
    pub frame: Option<std::sync::Arc<fresco_compute::Frame>>,
    pub rects: Vec<SceneNode>,
    pub paths: Vec<PathNode>,
    pub batches: Vec<TextureBatch>,
    pub glyph_batches: Vec<GlyphRunBatch>,
}

/// One frame's input, as frescod's `render_one_frame` assembles it: the
/// visible windows bottom to top, and the texture slot changes.
pub struct ScreenInput {
    pub windows: Vec<WindowFrame>,
    pub uploads: Vec<UploadRequest>,
    pub clears: Vec<u32>,
}

// vk::Format raw values for the two formats Fresco uploads.
const VK_FORMAT_R8_UNORM: i32 = 9;
const VK_FORMAT_R8G8B8A8_SRGB: i32 = 43;

/// The kernels' scene input across frames: the texture slot table.
#[derive(Default)]
pub struct SceneState {
    slots: HashMap<u32, fresco_compute::Texture>,
}

impl SceneState {
    pub fn upload(&mut self, req: UploadRequest) {
        match req {
            UploadRequest::Texture { slot_id, bytes, width, height, format } => {
                let format = match format.as_raw() {
                    VK_FORMAT_R8_UNORM => fresco_compute::TexFormat::R8,
                    VK_FORMAT_R8G8B8A8_SRGB => fresco_compute::TexFormat::Rgba8Srgb,
                    other => {
                        log::warn!("compute: slot {slot_id}: unsupported format {other}; dropped");
                        return;
                    }
                };
                if bytes.len() != (width * height) as usize * format.bytes_per_texel() {
                    log::warn!("compute: slot {slot_id}: {} bytes for {width}x{height}; dropped", bytes.len());
                    return;
                }
                self.slots.insert(slot_id, fresco_compute::Texture { width, height, format, data: bytes });
            }
            UploadRequest::TextureRegion { slot_id, bytes, dst_x, dst_y, width, height } => {
                let Some(t) = self.slots.get_mut(&slot_id) else {
                    log::warn!("compute: TextureRegion for empty slot {slot_id}; dropped");
                    return;
                };
                let bpp = t.format.bytes_per_texel();
                if dst_x + width > t.width || dst_y + height > t.height
                    || bytes.len() != (width * height) as usize * bpp
                {
                    log::warn!("compute: slot {slot_id}: region out of bounds; dropped");
                    return;
                }
                let row = width as usize * bpp;
                for y in 0..height as usize {
                    let d = ((dst_y as usize + y) * t.width as usize + dst_x as usize) * bpp;
                    t.data[d..d + row].copy_from_slice(&bytes[y * row..(y + 1) * row]);
                }
            }
        }
    }

    /// Apply a frame's texture slot changes.
    pub fn apply(&mut self, uploads: Vec<UploadRequest>, clears: Vec<u32>) {
        for s in clears {
            self.slots.remove(&s);
        }
        for u in uploads {
            self.upload(u);
        }
    }

    /// Pack one window's scene for the kernels, for a `width` x `height`
    /// surface.
    pub fn pack(&self, f: &WindowFrame, width: u32, height: u32) -> fresco_compute::Frame {
        use fresco_compute as fc;
        let mut scene = fc::Scene {
            rects: f.rects.iter().map(|n| fc::Rect {
                position: n.position, size: n.size, color: n.color, radius: n.radius,
            }).collect(),
            paths: f.paths.iter().map(|n| fc::Path {
                model: n.model, angle: n.extra[0], color: n.color,
            }).collect(),
            ..Default::default()
        };
        let mut index: HashMap<u32, usize> = HashMap::new();
        let mut tex_index = |slot: u32, scene: &mut fc::Scene| -> Option<usize> {
            if let Some(&i) = index.get(&slot) {
                return Some(i);
            }
            let t = self.slots.get(&slot)?.clone();
            scene.textures.push(t);
            index.insert(slot, scene.textures.len() - 1);
            Some(scene.textures.len() - 1)
        };
        for b in &f.batches {
            let Some(texture) = tex_index(b.slot_id, &mut scene) else { continue };
            scene.texture_batches.push(fc::TextureBatch {
                texture,
                quads: b.nodes.iter().map(|n| n.model).collect(),
            });
        }
        for b in &f.glyph_batches {
            let Some(atlas) = tex_index(b.atlas_slot_id, &mut scene) else { continue };
            let runs = b.nodes.iter().map(|n| {
                let (count, off) = (n.meta[0].max(0) as usize, n.meta[1].max(0) as usize);
                let glyphs = b.glyphs.get(off..off + count).unwrap_or(&[]);
                fc::GlyphRun {
                    origin: [n.origin[0], n.origin[1]],
                    color: n.color,
                    glyphs: glyphs.iter().map(|g| fc::Glyph {
                        d_offset: g.d_offset, atlas_uv: g.atlas_uv, bearing: g.bearing,
                    }).collect(),
                }
            }).collect();
            scene.glyph_batches.push(fc::GlyphBatch { atlas, runs });
        }
        let mut frame = fc::Frame::pack(&scene, width, height, width);
        // Window 0 is the screen's background; every other window's
        // surface is transparent where the window draws nothing.
        if f.id != 0 {
            frame.header.clear = fc::TRANSPARENT;
        }
        frame
    }
}


/// Window `id`'s scene as the frontend holds it, for a surface of `size`
/// whose (0, 0) is at `pos` on the screen. None if it has drawn nothing.
pub fn window_frame(fe: &EnvelopeFrontend, id: u32, pos: (f32, f32), size: (f32, f32)) -> Option<WindowFrame> {
    let state = fe.window_state(id)?;
    let mut w = WindowFrame {
        id,
        pos,
        size: (size.0.ceil() as u32, size.1.ceil() as u32),
        ..Default::default()
    };
    /* Paint in node_id order (lower id → drawn first → bottom).
     * The per-window node maps are HashMaps, whose iteration
     * order is seeded per-process; iterating `.values()` directly
     * would make the painter's-algorithm z-order nondeterministic
     * across runs (e.g. a full-screen background Rect sometimes
     * painting over the widgets on top of it). Sort by node_id to
     * match the within-type z-order the `extract_*` helpers use. */
    let mut rect_ids: Vec<&u32> = state.rect_nodes.keys().collect();
    rect_ids.sort_unstable();
    for id in rect_ids {
        w.rects.push(translate_rect(&state.rect_nodes[id], 0.0, 0.0));
    }
    let mut path_ids: Vec<&u32> = state.path_nodes.keys().collect();
    path_ids.sort_unstable();
    for id in path_ids {
        w.paths.push(translate_path(&state.path_nodes[id], 0.0, 0.0));
    }
    let mut tex_by_slot: std::collections::BTreeMap<u32, Vec<TextureNode>> =
        std::collections::BTreeMap::new();
    let mut tex_ids: Vec<&u32> = state.texture_nodes.keys().collect();
    tex_ids.sort_unstable();
    for id in tex_ids {
        let p = &state.texture_nodes[id];
        tex_by_slot.entry(p.slot_id).or_default()
            .push(translate_texture(p, 0.0, 0.0));
    }
    w.batches = tex_by_slot.into_iter()
        .map(|(slot_id, nodes)| TextureBatch { slot_id, nodes })
        .collect();
    /* glyph_run nodes grouped by atlas slot. Each batch's `glyphs`
     * vector concatenates all runs in slot, with each node's
     * meta[1] (glyph_offset) pointing at this batch's slice. */
    let mut glyph_by_slot: std::collections::BTreeMap<u32, GlyphRunBatch> =
        std::collections::BTreeMap::new();
    let mut glyph_ids: Vec<&u32> = state.glyph_run_nodes.keys().collect();
    glyph_ids.sort_unstable();
    for id in glyph_ids {
        let p = &state.glyph_run_nodes[id];
        let entry = glyph_by_slot
            .entry(p.atlas_slot_id)
            .or_insert_with(|| GlyphRunBatch {
                atlas_slot_id: p.atlas_slot_id,
                nodes:  Vec::new(),
                glyphs: Vec::new(),
            });
        let glyph_offset = entry.glyphs.len() as i32;
        entry.nodes.push(GlyphRunNode {
            origin: [p.x, p.y, 0.0, 0.0],
            atlas_dim: [p.atlas_width as f32,
                        p.atlas_height as f32, 0.0, 0.0],
            color: [p.r, p.g, p.b, p.a],
            meta: [p.glyphs.len() as i32, glyph_offset, 0, 0],
        });
        for g in &p.glyphs {
            entry.glyphs.push(GlyphInstance {
                d_offset: [g.dx, g.dy],
                atlas_uv: [g.atlas_u as f32, g.atlas_v as f32,
                           g.atlas_w as f32, g.atlas_h as f32],
                bearing:  [g.bearing_x, g.bearing_y],
                ..Default::default()
            });
        }
    }
    w.glyph_batches = glyph_by_slot.into_values().collect();
    Some(w)
}

fn translate_rect(p: &RectParams, ox: f32, oy: f32) -> SceneNode {
    SceneNode {
        position: [p.x + ox, p.y + oy],
        size:     [p.w, p.h],
        color:    [p.r, p.g, p.b, p.a],
        radius:   p.radius,
        _pad:     [0.0; 3],
    }
}

fn translate_texture(p: &TextureParams, ox: f32, oy: f32) -> TextureNode {
    TextureNode { model: [p.x + ox, p.y + oy, p.w, p.h] }
}

fn translate_path(p: &PathParams, ox: f32, oy: f32) -> PathNode {
    PathNode {
        model: [p.cx + ox, p.cy + oy, p.length, p.width],
        extra: [p.angle, 0.0, 0.0, 0.0],
        color: [p.r, p.g, p.b, p.a],
    }
}
