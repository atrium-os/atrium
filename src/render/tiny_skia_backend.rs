//! Software rasterizer `GpuBackend` over [tiny-skia].
//!
//! Translates fresco-server's `SceneGraph` (which produces flattened
//! triangle-mesh `RenderItem`s after tessellation) into tiny-skia paths
//! and fills, rendered into an internal `Pixmap`. Consumers (notably
//! `atrium-compositor` on FreeBSD) call `render_frame` and then read
//! `pixels()` to copy/swap into the scanout BO.
//!
//! Scope (v0.1):
//! - Solid-color materials: yes
//! - Linear gradient materials: yes
//! - Radial gradient: TODO (v0.2)
//! - Textured materials: TODO (v0.2 — needs a texture cache + Pattern shader)
//! - Stencil-fill paths (text glyphs): rendered via the same triangle-list
//!   path the Metal backend uses; tiny-skia rasterizes correctly without a
//!   separate stencil pass.
//! - Per-window FBOs: TODO — `sync_fbos` / `render_window_to_fbo` are no-ops
//!   for now; the screen pass is the only render target.

use crate::cas::store::CasStore;
use crate::command::protocol::NULL_HASH;
use crate::render::backend::{GpuBackend, WindowOverlay};
use crate::scene::graph::SceneGraph;
use crate::scene::nodes::{Material, NodeData};

use std::collections::HashMap;

use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, IntRect, LinearGradient, Paint, Pattern,
    PathBuilder, Pixmap, PixmapMut, PixmapPaint, PixmapRef, Point, Rect, Shader, SpreadMode,
    Transform,
};

pub struct TinySkiaBackend {
    pixmap: Pixmap,
    width: u32,
    height: u32,
    scale: f64,
    /// CAS-hash → decoded Pixmap. Populated lazily from textured
    /// materials encountered during rasterization. Same lifetime as
    /// the backend; cleared on `resize` only because the textures
    /// themselves are dimension-independent.
    tex_cache: HashMap<crate::command::protocol::Hash256, Pixmap>,
    /// Per-window offscreen pixmap (FBO). Each window's scene
    /// rasterizes into its own pixmap via `render_window_to_fbo`,
    /// then `render_screen_with_windows` blits them onto the screen
    /// pixmap at the WM's reported window rects.
    window_fbos: HashMap<u16, Pixmap>,
}

impl TinySkiaBackend {
    pub fn new(width: u32, height: u32) -> Self {
        let pixmap = Pixmap::new(width, height)
            .expect("tiny-skia Pixmap allocation");
        Self {
            pixmap, width, height, scale: 1.0,
            tex_cache:   HashMap::new(),
            window_fbos: HashMap::new(),
        }
    }

    /// Reconcile the per-window FBO map against `live`: ensure a
    /// pixmap exists at the right size for each (id, w, h), drop any
    /// FBO whose window is gone or whose dimensions changed past
    /// recovery. Called once per frame from the compositor.
    pub fn sync_fbos(&mut self, live: &HashMap<u16, (u32, u32)>) {
        // Drop stale (window destroyed or resized).
        let stale: Vec<u16> = self.window_fbos.iter()
            .filter_map(|(id, pm)| match live.get(id) {
                Some(&(w, h)) if pm.width() == w && pm.height() == h => None,
                _ => Some(*id),
            })
            .collect();
        for id in stale {
            self.window_fbos.remove(&id);
        }
        // Allocate any missing.
        for (&id, &(w, h)) in live {
            if self.window_fbos.contains_key(&id) { continue; }
            if w == 0 || h == 0 { continue; }
            if let Some(pm) = Pixmap::new(w, h) {
                self.window_fbos.insert(id, pm);
            }
        }
    }

    /// Rasterize one window's scene into its FBO. Caller must have
    /// already invoked `sync_fbos` so the pixmap exists.
    pub fn render_window_to_fbo(&mut self, id: u16, scene: &SceneGraph, cas: &CasStore) {
        let Some(fbo) = self.window_fbos.get_mut(&id) else { return; };
        let (fw, fh) = (fbo.width(), fbo.height());
        // Window background — slightly translucent dark so the screen
        // bg / lower-z windows tint through faintly. Apps draw their
        // content on top (glyph alpha composes correctly over this).
        // Use 0xE0 alpha (~88% opaque): mostly solid, hint of show-
        // through. Apps that want full opacity can fill themselves.
        fbo.fill(Color::from_rgba8(0x14, 0x18, 0x22, 0xE0));
        Self::rasterize_items_into(
            fbo, &mut self.tex_cache,
            scene.render_list(), cas,
            fw, fh,
        );
    }

    /// Full-screen render: clear, rasterize the screen scene, then
    /// for each window in z-order (lowest first): blit its FBO,
    /// stroke its border, rasterize *that window's* decorations
    /// (titlebar/close-button/title text). Per-window interleave so
    /// a higher-z window's content covers lower-z windows' titlebars
    /// in the overlap region.
    pub fn render_screen_with_windows(
        &mut self,
        scene: &SceneGraph,
        cas: &CasStore,
        layered: &[(crate::render::backend::WindowOverlay, Vec<crate::scene::graph::RenderItem>)],
    ) {
        self.clear_to(Color::from_rgba8(0x14, 0x18, 0x22, 0xff));
        // Screen scene (window 0). Apps that don't create their own
        // window draw here too — backwards-compatible with single-
        // window mode.
        Self::rasterize_items_into(
            &mut self.pixmap, &mut self.tex_cache,
            scene.render_list(), cas,
            self.width, self.height,
        );
        for (ov, deco) in layered {
            if let Some(fbo) = self.window_fbos.get(&ov.id) {
                blit_pixmap(&mut self.pixmap, fbo, ov.x, ov.y, ov.w, ov.h);
            }
            stroke_window_border(&mut self.pixmap, ov.x, ov.y, ov.w, ov.h);
            if !deco.is_empty() {
                Self::rasterize_items_into(
                    &mut self.pixmap, &mut self.tex_cache,
                    deco, cas,
                    self.width, self.height,
                );
            }
        }
    }

    /// Decode a CAS-stored NODE_TEXTURE blob into a tiny-skia Pixmap
    /// and stash it in the cache. No-op if already cached, or if the
    /// blob can't be parsed / pixel data isn't loaded.
    fn ensure_texture(&mut self, cas: &CasStore, hash: &crate::command::protocol::Hash256) {
        Self::ensure_texture_into(&mut self.tex_cache, cas, hash);
    }

    fn ensure_texture_into(
        tex_cache: &mut HashMap<crate::command::protocol::Hash256, Pixmap>,
        cas: &CasStore,
        hash: &crate::command::protocol::Hash256,
    ) {
        if tex_cache.contains_key(hash) {
            return;
        }
        let Some(tex_data) = cas.load(hash) else { return; };
        let Some(NodeData::Texture(th)) = NodeData::parse(tex_data) else { return; };
        let Some(pixel_blob) = cas.load(&th.pixel_data) else { return; };
        let pixels = if pixel_blob.len() > 8 { &pixel_blob[8..] } else { pixel_blob };
        let need = (th.width as usize)
            .saturating_mul(th.height as usize)
            .saturating_mul(4);
        if pixels.len() < need || th.width == 0 || th.height == 0 {
            return;
        }
        let Some(mut pm) = Pixmap::new(th.width, th.height) else { return; };
        pm.data_mut()[..need].copy_from_slice(&pixels[..need]);
        tex_cache.insert(*hash, pm);
    }

    /// Raw RGBA pixels, top-down, premultiplied. Length = w*h*4.
    pub fn pixels(&self) -> &[u8] {
        self.pixmap.data()
    }

    /// Mutable view of the internal pixmap. Lets the consumer draw
    /// directly with tiny-skia while still letting the backend manage
    /// allocation + lifecycle (resize, scale tracking). Used during
    /// the bring-up phase before the SceneGraph rendering path is
    /// fully wired.
    pub fn pixmap_mut(&mut self) -> PixmapMut<'_> {
        self.pixmap.as_mut()
    }

    pub fn width(&self) -> u32 { self.width }
    pub fn height(&self) -> u32 { self.height }

    /// Copy pixels into `dst` after byte-swapping channels 0 and 2 so
    /// the result is BGRA (virtio-gpu's hardcoded scanout format).
    /// `dst` must be `w * h * 4` bytes.
    pub fn copy_to_bgra(&self, dst: &mut [u8]) {
        debug_assert_eq!(dst.len(), self.pixels().len());
        let src = self.pixels();
        // Process 4 bytes at a time.
        let n = src.len() / 4;
        for i in 0..n {
            let o = i * 4;
            dst[o]     = src[o + 2];   // B  ← R
            dst[o + 1] = src[o + 1];   // G
            dst[o + 2] = src[o];       // R  ← B
            dst[o + 3] = src[o + 3];   // A
        }
    }

    fn clear_to(&mut self, c: Color) {
        self.pixmap.fill(c);
    }

    /// Walk the scene's render_list and rasterize each item via tiny-skia.
    fn rasterize_scene(&mut self, scene: &SceneGraph, cas: &CasStore) {
        Self::rasterize_items_into(
            &mut self.pixmap, &mut self.tex_cache,
            scene.render_list(), cas,
            self.width, self.height,
        );
    }

    /// Rasterize a slice of `RenderItem`s into `pixmap`. Pulled out
    /// so the caller can render the screen scene, per-window FBOs,
    /// and WM decoration overlays through the same path.
    fn rasterize_items_into(
        pixmap: &mut Pixmap,
        tex_cache: &mut HashMap<crate::command::protocol::Hash256, Pixmap>,
        items: &[crate::scene::graph::RenderItem],
        cas: &CasStore,
        width: u32,
        height: u32,
    ) {
        let vp_w = width as f32;
        let vp_h = height as f32;

        // Texture pre-pass: ensure_texture borrows &mut, while the
        // render loop wants to hold &tex_cache while &mut pixmap.
        // Decoding upfront keeps the borrow split clean.
        for item in items.iter() {
            if item.material == NULL_HASH { continue; }
            if let Some(data) = cas.load(&item.material) {
                if let Some(NodeData::Material(m)) = NodeData::parse(data) {
                    if m.albedo_tex != NULL_HASH {
                        Self::ensure_texture_into(tex_cache, cas, &m.albedo_tex);
                    }
                }
            }
        }

        for (idx, item) in items.iter().enumerate() {
            if item.mesh == NULL_HASH {
                continue;
            }

            let mesh_data = match cas.load(&item.mesh) {
                Some(d) => d,
                None => {
                    log::warn!(
                        "tiny-skia render[{}]: mesh {:02x}{:02x}.. not in CAS",
                        idx, item.mesh[0], item.mesh[1]);
                    continue;
                }
            };
            let mesh = match NodeData::parse(mesh_data) {
                Some(NodeData::Mesh(m)) => m,
                _ => continue,
            };

            // Vertex blob — first 8 bytes are a header in fresco's CAS
            // layout (matches metal_backend's handling).
            let verts_raw = match cas.load(&mesh.vertex_data) {
                Some(d) => d,
                None => continue,
            };
            let verts_bytes = if verts_raw.len() > 8 { &verts_raw[8..] } else { verts_raw };

            let stride = mesh.vertex_stride.max(8) as usize;
            let positions = read_vertex_positions(verts_bytes, stride, mesh.vertex_count as usize);

            // Indices.
            let indices: Vec<u32> = if mesh.index_count > 0 && mesh.index_data != NULL_HASH {
                let idx_raw = match cas.load(&mesh.index_data) {
                    Some(d) => d,
                    None => continue,
                };
                let idx_bytes = if idx_raw.len() > 8 { &idx_raw[8..] } else { idx_raw };
                if mesh.index_format == 4 {
                    idx_bytes
                        .chunks_exact(4)
                        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect()
                } else {
                    idx_bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]) as u32)
                        .collect()
                }
            } else {
                (0..mesh.vertex_count).collect()
            };

            // Build a Path from the triangle list.
            let path = match build_triangle_path(&positions, &indices) {
                Some(p) => p,
                None => continue,
            };

            // Resolve material (color / gradient).
            let mat = if item.material != NULL_HASH {
                cas.load(&item.material)
                    .and_then(|d| match NodeData::parse(d) {
                        Some(NodeData::Material(m)) => Some(m),
                        _ => None,
                    })
            } else {
                None
            };

            // World transform: the world_matrix is a 4x4. For 2D
            // rendering we extract the affine 2D part (cols 0,1,3
            // → tx, ty, sx, sy, ...).
            // matrix layout (column-major):
            //   m[0..3]   col0  (sx, ., ., 0)
            //   m[4..7]   col1  (., sy, ., 0)
            //   m[8..11]  col2  z-related
            //   m[12..15] col3  (tx, ty, ., 1)
            let w = item.world_matrix;
            let xform = Transform::from_row(
                w[0], w[1],   // sx, shy
                w[4], w[5],   // shx, sy
                w[12], w[13], // tx, ty
            );

            let _ = (vp_w, vp_h);  // keep params for future viewport use.
            let _ = item.clip_rect; // clip-rect → tiny-skia Mask, deferred

            // Build paint, possibly textured.
            let textured_pixmap = mat.as_ref()
                .filter(|m| m.albedo_tex != NULL_HASH)
                .and_then(|m| tex_cache.get(&m.albedo_tex));

            let mut paint = Paint::default();
            paint.anti_alias = true;
            if let Some(tex) = textured_pixmap {
                // Pattern.transform maps pattern-pixel-space (texture
                // pixels) to path-local space (the unit-rect 0..1 the
                // mesh is built in). `fill_path` post-concats its
                // own xform onto the shader transform.
                //
                // No UV region (full texture, default 0,0,1,1):
                //   scale(1/tex_w, 1/tex_h) → texture fills unit rect.
                //
                // With UV region (u0,v0,u1,v1) — used to sub-rect a
                // shared atlas (e.g. one cell of a glyph atlas):
                //   path (0,0)             ↔ tex (u0*W, v0*H)
                //   path (1,1)             ↔ tex (u1*W, v1*H)
                // → scale(1/((u1-u0)*W), 1/((v1-v0)*H)),
                //   translate(-u0/(u1-u0), -v0/(v1-v0)).
                // tiny-skia's Pattern.transform is the mapping from
                // pattern-source (texture pixel) to user-space (path-
                // local). painter.rs post-concats fill_path's xform
                // onto it, so the effective transform inside the
                // shader pipeline is texel → pixmap, then inverted
                // to sample. For a unit-rect path with sub-region
                // (u0..u1, v0..v1) intended to fill it:
                //   src texel (px, py) → path ((px/W − u0)/du, (py/H − v0)/dv)
                // With uv = (0, 0, 1, 1) this reduces to
                // scale(1/W, 1/H), matching the pre-atlas baseline.
                let uv = mat.as_ref().map(|m| m.uv_region).unwrap_or([0.0, 0.0, 1.0, 1.0]);
                let tex_w = tex.width()  as f32;
                let tex_h = tex.height() as f32;
                let du = (uv[2] - uv[0]).max(1e-6);
                let dv = (uv[3] - uv[1]).max(1e-6);
                let sx = 1.0 / (du * tex_w);
                let sy = 1.0 / (dv * tex_h);
                let tx = -uv[0] / du;
                let ty = -uv[1] / dv;
                let pat_t = Transform::from_row(sx, 0.0, 0.0, sy, tx, ty);
                paint.shader = Pattern::new(
                    tex.as_ref(),
                    SpreadMode::Pad,
                    FilterQuality::Bilinear,
                    1.0,
                    pat_t,
                );
            } else if let Some(m) = mat.as_ref() {
                paint = build_paint_from_material(Some(m));
            } else {
                paint.shader = Shader::SolidColor(Color::from_rgba8(0xcc, 0xcc, 0xcc, 0xff));
            }

            pixmap.fill_path(&path, &paint, FillRule::Winding, xform, None);
        }
    }
}

/// Draw a 1-pixel border around the rect (x, y, w, h). Uses a pale
/// grey so it's visible against any window content.
fn stroke_window_border(dst: &mut Pixmap, x: f32, y: f32, w: f32, h: f32) {
    let Some(rect) = Rect::from_xywh(x, y, w, h) else { return; };
    let path = PathBuilder::from_rect(rect);
    let mut paint = Paint::default();
    paint.shader = Shader::SolidColor(Color::from_rgba8(0xa0, 0xa0, 0xa8, 0xff));
    paint.anti_alias = false;
    let mut stroke = tiny_skia::Stroke::default();
    stroke.width = 1.0;
    dst.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// Blit `src` into `dst` at the screen rect (x, y, w, h). If the
/// FBO's native size matches (w, h), a fast `draw_pixmap` does an
/// integer-pixel copy. If the rects differ, we fall back to a
/// scaled draw via `Pattern` shader so windows stay correct after
/// resize before the FBO has been reallocated.
fn blit_pixmap(dst: &mut Pixmap, src: &Pixmap, x: f32, y: f32, w: f32, h: f32) {
    let src_ref: PixmapRef<'_> = src.as_ref();
    let same_size = (src.width() as f32 - w).abs() < 0.5
                  && (src.height() as f32 - h).abs() < 0.5;
    if same_size {
        let mut paint = PixmapPaint::default();
        paint.quality = FilterQuality::Nearest;
        dst.draw_pixmap(
            x.round() as i32,
            y.round() as i32,
            src_ref,
            &paint,
            Transform::identity(),
            None,
        );
    } else {
        // Stretched: build a unit-rect path covering (x..x+w, y..y+h)
        // and fill it with a Pattern that maps src texels into that
        // rect. Pattern.transform is "src texel → user (path-local)";
        // post-concat with fill_path xform makes the effective
        // transform texel → pixmap.
        let Some(rect) = Rect::from_xywh(x, y, w, h) else { return; };
        let path = PathBuilder::from_rect(rect);
        let pat_t = Transform::from_row(
            w / src.width() as f32, 0.0,
            0.0, h / src.height() as f32,
            x, y,
        );
        let mut paint = Paint::default();
        paint.shader = Pattern::new(
            src_ref, SpreadMode::Pad, FilterQuality::Bilinear, 1.0,
            pat_t,
        );
        dst.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Read positions out of a vertex blob. We only consume the first 2
/// floats of each vertex — additional channels (uv, color, etc.) are
/// ignored in v0.1 since textured materials aren't implemented.
fn read_vertex_positions(bytes: &[u8], stride: usize, count: usize) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let o = i * stride;
        if o + 8 > bytes.len() {
            break;
        }
        let x = f32::from_le_bytes([bytes[o], bytes[o+1], bytes[o+2], bytes[o+3]]);
        let y = f32::from_le_bytes([bytes[o+4], bytes[o+5], bytes[o+6], bytes[o+7]]);
        out.push((x, y));
    }
    out
}

/// Convert a triangle list into a tiny-skia Path. Each triangle becomes
/// a closed 3-line subpath. Slow for large meshes but correct, and
/// tiny-skia's rasterizer flattens this efficiently for fills.
fn build_triangle_path(positions: &[(f32, f32)], indices: &[u32]) -> Option<tiny_skia::Path> {
    if indices.len() < 3 {
        return None;
    }
    let mut pb = PathBuilder::new();
    for tri in indices.chunks_exact(3) {
        let (i0, i1, i2) = (tri[0] as usize, tri[1] as usize, tri[2] as usize);
        if i0 >= positions.len() || i1 >= positions.len() || i2 >= positions.len() {
            continue;
        }
        let p0 = positions[i0];
        let p1 = positions[i1];
        let p2 = positions[i2];
        pb.move_to(p0.0, p0.1);
        pb.line_to(p1.0, p1.1);
        pb.line_to(p2.0, p2.1);
        pb.close();
    }
    pb.finish()
}

fn build_paint_from_material(mat: Option<&Material>) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.anti_alias = true;

    let Some(mat) = mat else {
        paint.shader = Shader::SolidColor(Color::from_rgba8(0xcc, 0xcc, 0xcc, 0xff));
        return paint;
    };

    if mat.has_gradient() && mat.gradient_stop_count >= 2 {
        let mut stops: Vec<GradientStop> = Vec::with_capacity(mat.gradient_stop_count as usize);
        for i in 0..mat.gradient_stop_count as usize {
            let (off, rgba) = mat.gradient_stops[i];
            let (r, g, b, a) = unpack_rgba8(rgba);
            stops.push(GradientStop::new(off, Color::from_rgba8(r, g, b, a)));
        }
        if let Some(grad) = LinearGradient::new(
            Point::from_xy(mat.gradient_x0, mat.gradient_y0),
            Point::from_xy(mat.gradient_x1, mat.gradient_y1),
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            paint.shader = grad;
            return paint;
        }
    }

    let (r, g, b, a) = unpack_rgba8(mat.base_color);
    paint.shader = Shader::SolidColor(Color::from_rgba8(r, g, b, a));
    paint
}

/// Material colors are packed as `0xAABBGGRR` little-endian-encoded u32
/// to match the wire format. Unpack into channel bytes.
fn unpack_rgba8(rgba: u32) -> (u8, u8, u8, u8) {
    let r = (rgba & 0xff) as u8;
    let g = ((rgba >> 8) & 0xff) as u8;
    let b = ((rgba >> 16) & 0xff) as u8;
    let a = ((rgba >> 24) & 0xff) as u8;
    (r, g, b, a)
}

impl GpuBackend for TinySkiaBackend {
    fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        self.pixmap = Pixmap::new(width, height)
            .expect("tiny-skia Pixmap reallocation");
    }

    fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
    }

    fn render_frame(
        &mut self,
        scene: &SceneGraph,
        cas: &CasStore,
        _frame: u64,
        _cursor: Option<(f32, f32)>,
    ) {
        // Background — neutral dark, will be replaced once the screen
        // node has a wallpaper material. Always clear so stale pixels
        // from the previous frame don't bleed through transparent items.
        self.clear_to(Color::from_rgba8(0x14, 0x18, 0x22, 0xff));
        self.rasterize_scene(scene, cas);
    }

    fn render_frame_with_overlays(
        &mut self,
        scene: &SceneGraph,
        cas: &CasStore,
        frame: u64,
        cursor: Option<(f32, f32)>,
        _overlays: &[WindowOverlay],
    ) {
        // FBO compositing not implemented yet — fall through to the
        // screen-only render. Per-window FBOs are step 2(c.2)+.
        self.render_frame(scene, cas, frame, cursor);
    }

    fn tessellate_path(
        &mut self,
        _segments_data: &[u8],
        _tolerance: f32,
        _fill: bool,
    ) -> Option<(Vec<f32>, Vec<u16>)> {
        // tiny-skia renders paths analytically; we don't pre-tessellate.
        // The Metal backend uses lyon for this. fresco-server's
        // CommandFrontend tessellates on submit (via render::tessellate),
        // so by the time we see RenderItems they're already triangles.
        // Returning None here means: "this backend doesn't pre-tessellate".
        None
    }
}
