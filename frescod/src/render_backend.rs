//! The two renderers frescod can draw a frame with.
//!
//! - `vulkan` (default): `fresco_vulkan::HeadlessRenderer` through whatever
//!   Vulkan ICD the loader finds (lavapipe in the VM today), with the op
//!   bundles.
//! - `compute` (`FRESCOD_RENDERER=compute`): `fresco_compute`'s two compute
//!   kernels run on Tier-2, Atrium's own SPIR-V-to-CPU compiler. No Vulkan,
//!   no Mesa, no bundles. Kernels are compiled through `atrium-spv-compile`
//!   (`ATRIUM_SPV_COMPILE`) into `FRESCO_COMPUTE_CACHE`; built with the
//!   `cranelift` feature, frescod compiles them itself when the compiler
//!   binary isn't there.
//!
//! Both take the same per-frame input (the node types and texture uploads
//! frescod already assembles) and produce BGRA8 sRGB pixels.

use std::collections::HashMap;
use std::error::Error;

use fresco_vulkan::{
    GlyphRunBatch, HeadlessRenderer, PathNode, SceneNode, TextureBatch, UploadRequest,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// One frame's input, as frescod's `render_one_frame` assembles it.
pub struct FrameInput {
    pub rects: Vec<SceneNode>,
    pub paths: Vec<PathNode>,
    pub batches: Vec<TextureBatch>,
    pub glyph_batches: Vec<GlyphRunBatch>,
    pub uploads: Vec<UploadRequest>,
    pub clears: Vec<u32>,
}

pub enum Backend {
    Vulkan(Box<HeadlessRenderer>),
    Compute(Box<Compute>),
}

impl Backend {
    /// Pick the renderer from `FRESCOD_RENDERER` (`vulkan` or `compute`).
    /// `load_bundles` is called only for Vulkan.
    pub fn from_env(
        width: u32,
        height: u32,
        load_bundles: impl FnOnce(&mut HeadlessRenderer) -> Result<()>,
    ) -> Result<Self> {
        match std::env::var("FRESCOD_RENDERER").as_deref() {
            Ok("compute") => Ok(Backend::Compute(Box::new(Compute::new(width, height)?))),
            Ok("vulkan") | Err(_) => {
                let mut r = HeadlessRenderer::new(width, height)
                    .map_err(|e| format!("HeadlessRenderer::new: {e}"))?;
                load_bundles(&mut r)?;
                Ok(Backend::Vulkan(Box::new(r)))
            }
            Ok(other) => Err(format!("FRESCOD_RENDERER={other}: want vulkan or compute").into()),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backend::Vulkan(_) => "vulkan",
            Backend::Compute(_) => "compute (Tier-2)",
        }
    }

    pub fn render(&mut self, f: FrameInput) -> Result<()> {
        match self {
            Backend::Vulkan(r) => {
                r.process_uploads(f.uploads, f.clears)?;
                r.set_rect_nodes(f.rects);
                r.set_path_nodes(f.paths);
                r.set_texture_batches(f.batches);
                r.set_glyph_run_batches(f.glyph_batches);
                r.render_to_buffer()?;
                Ok(())
            }
            Backend::Compute(c) => c.render(f),
        }
    }

    /// The last frame, BGRA8, `width * height * 4` bytes.
    pub fn read_pixels(&self, dst: &mut [u8]) -> Result<()> {
        match self {
            Backend::Vulkan(r) => Ok(r.read_pixels(dst)?),
            Backend::Compute(c) => {
                let n = dst.len().min(c.target.len() * 4);
                for (d, w) in dst[..n].chunks_exact_mut(4).zip(&c.target) {
                    d.copy_from_slice(&w.to_le_bytes());
                }
                Ok(())
            }
        }
    }
}

// vk::Format raw values for the two formats Fresco uploads.
const VK_FORMAT_R8_UNORM: i32 = 9;
const VK_FORMAT_R8G8B8A8_SRGB: i32 = 43;

/// The compute renderer: a slot table of CPU textures plus the Tier-2
/// kernels.
pub struct Compute {
    width: u32,
    height: u32,
    t2: fresco_compute::tier2::Tier2,
    slots: HashMap<u32, fresco_compute::Texture>,
    target: Vec<u32>,
}

impl Compute {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        #[cfg(feature = "cranelift")]
        {
            let have_compiler = std::env::var_os("ATRIUM_SPV_COMPILE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| atrium_spv_loader::LoaderConfig::production().compile_binary)
                .exists();
            if !have_compiler {
                let cache = std::env::var_os("FRESCO_COMPUTE_CACHE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| atrium_spv_loader::LoaderConfig::production().cache_root);
                fresco_compute::tier2::Tier2::seed_cache(&cache)?;
            }
        }
        let t2 = fresco_compute::tier2::Tier2::from_env()?;
        Ok(Self { width, height, t2, slots: HashMap::new(), target: vec![0; (width * height) as usize] })
    }

    fn upload(&mut self, req: UploadRequest) {
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

    fn render(&mut self, f: FrameInput) -> Result<()> {
        for s in f.clears {
            self.slots.remove(&s);
        }
        for u in f.uploads {
            self.upload(u);
        }
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
        let frame = fc::Frame::pack(&scene, self.width, self.height, self.width);
        self.t2.render(&frame, &mut self.target)?;
        Ok(())
    }
}
