//! Scene to kernel input: each window's nodes, as frescod's scene server
//! holds them, packed into a `fresco_compute::Frame`. Needs no GPU, so a
//! jailed window renderer (F4b) builds its frames with the same code the
//! single-process frescod uses.

use std::collections::HashMap;

use fresco_vulkan::{GlyphRunBatch, PathNode, SceneNode, TextureBatch, UploadRequest};

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
    pub frame: Option<fresco_compute::Frame>,
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

