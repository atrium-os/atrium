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
    /// Already drawn (by a window renderer in pixel mode): `size` pixels,
    /// composed as they are.
    pub pixels: Option<std::sync::Arc<Pixels>>,
    pub rects: Vec<SceneNode>,
    pub paths: Vec<PathNode>,
    pub batches: Vec<TextureBatch>,
    pub glyph_batches: Vec<GlyphRunBatch>,
    /// Filled paths, in node id order (drawn after `paths`).
    pub fills: Vec<fresco_compute::fill::Fill>,
    /// The node id of each of `fills`, for reporting drops.
    pub fill_ids: Vec<u32>,
    /// The node ids of `rects`, `paths`, each batch's nodes and each glyph
    /// batch's runs, for reporting drops (empty when there are none to
    /// report, as for the chrome).
    pub rect_ids: Vec<u32>,
    pub path_ids: Vec<u32>,
    pub quad_ids: Vec<Vec<u32>>,
    pub run_ids: Vec<Vec<u32>>,
    /// The window uses groups (fresco-scene-groups.md): draw these, in this
    /// order, instead of by kind. Indices are into the vectors above, and
    /// each texture or glyph node has a batch of its own.
    pub order: Option<Vec<OrderItem>>,
}

/// One element of a grouped window, in paint order.
#[derive(Clone, Copy, Debug)]
pub struct OrderItem {
    pub item: fresco_compute::Node,
    /// Window pixels `[x0, y0, x1, y1)`, all the groups' clips intersected.
    pub clip: Option<[f32; 4]>,
    /// The node it came from, for drop reports.
    pub node: u32,
}


/// A window's finished pixels, BGRA8, `width * height` words.
pub struct Pixels {
    /// Unique per drawing, so a consumer can tell a new one from one it has.
    pub gen: u64,
    pub width: u32,
    pub height: u32,
    pub px: Vec<u32>,
    /// The renderer's sequence number for this drawing.
    pub seq: u32,
    /// What changed since earlier drawings: `(gen, rect)` says this one
    /// differs from drawing `gen` only inside `rect` (window pixels,
    /// `[x0, y0, x1, y1)`). Newest first, a few deep; a consumer holding
    /// a drawing not listed takes the whole window.
    pub since: Vec<(u64, [u32; 4])>,
}

/// Make `latest` the next drawing (`gen`, renderer `seq`), which differs
/// from it only inside `damage`; `paint` draws that part. Patched in place
/// when nothing else holds `latest`; otherwise `spare` (an earlier drawing
/// of the window nothing holds any more) is brought up to date from
/// `latest` and patched, so a frame costs what changed rather than a copy
/// of the window. Returns the drawing to keep as the next spare.
pub fn advance(
    latest: &mut std::sync::Arc<Pixels>,
    spare: Option<std::sync::Arc<Pixels>>,
    gen: u64,
    seq: u32,
    damage: [u32; 4],
    paint: impl FnOnce(&mut [u32]) -> Result<(), String>,
) -> Result<Option<std::sync::Arc<Pixels>>, String> {
    use std::sync::Arc;
    let mut since = vec![(latest.gen, damage)];
    let u = |r: [u32; 4]| [r[0].min(damage[0]), r[1].min(damage[1]), r[2].max(damage[2]), r[3].max(damage[3])];
    since.extend(latest.since.iter().map(|&(g, r)| (g, u(r))));
    since.truncate(SINCE_DEPTH);
    if let Some(p) = Arc::get_mut(latest) {
        paint(&mut p.px)?;
        (p.since, p.gen, p.seq) = (since, gen, seq);
        return Ok(spare);
    }
    let (w, h) = (latest.width, latest.height);
    let mut next = match spare {
        Some(mut sp) if Arc::strong_count(&sp) == 1 && Arc::weak_count(&sp) == 0 && sp.width == w && sp.height == h => {
            let b = Arc::get_mut(&mut sp).expect("checked");
            match latest.changed_since(b.gen) {
                Some(None) => {}
                Some(Some(r)) => copy_rect(&latest.px, &mut b.px, w, r),
                None => b.px.copy_from_slice(&latest.px),
            }
            sp
        }
        _ => Arc::new(Pixels { gen: 0, width: w, height: h, px: latest.px.clone(), seq: 0, since: Vec::new() }),
    };
    let b = Arc::get_mut(&mut next).expect("unshared");
    paint(&mut b.px)?;
    (b.since, b.gen, b.seq) = (since, gen, seq);
    Ok(Some(std::mem::replace(latest, next)))
}

/// Copy rect `r` (`[x0, y0, x1, y1)`, clamped) of a `width`-wide image.
fn copy_rect(src: &[u32], dst: &mut [u32], width: u32, r: [u32; 4]) {
    let w = width as usize;
    let rows = src.len().min(dst.len()) / w.max(1);
    let (x0, x1) = ((r[0] as usize).min(w), (r[2] as usize).min(w));
    for y in (r[1] as usize).min(rows)..(r[3] as usize).min(rows) {
        dst[y * w + x0..y * w + x1].copy_from_slice(&src[y * w + x0..y * w + x1]);
    }
}

/// How many earlier drawings [`Pixels::since`] reaches back.
pub const SINCE_DEPTH: usize = 6;

impl Pixels {
    /// Where this drawing differs from drawing `gen`: `Some(None)` nowhere
    /// (it is that drawing), `Some(Some(rect))` inside `rect`, `None`
    /// unknown (anywhere).
    pub fn changed_since(&self, gen: u64) -> Option<Option<[u32; 4]>> {
        if gen == self.gen {
            return Some(None);
        }
        self.since.iter().find(|(g, _)| *g == gen).map(|&(_, r)| Some(r))
    }
    /// The alpha of pixel (x, y), 0 outside.
    pub fn alpha_at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        (self.px[(y * self.width + x) as usize] >> 24) as u8
    }
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

/// The kernels' scene input across frames: the texture slot table, and
/// the fills already flattened and tiled.
#[derive(Default)]
pub struct SceneState {
    slots: HashMap<u32, fresco_compute::Texture>,
    fills: std::sync::Mutex<fresco_compute::fill::FillCache>,
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
        self.pack_report(f, width, height).0
    }

    /// `pack`, also naming the nodes that were dropped (by node id, with
    /// a `fresco_protocol::fill_drop` reason).
    pub fn pack_report(&self, f: &WindowFrame, width: u32, height: u32) -> (fresco_compute::Frame, Vec<(u32, u8)>) {
        use fresco_compute as fc;
        let mut scene = fc::Scene {
            rects: f.rects.iter().map(|n| fc::Rect {
                position: n.position, size: n.size, color: n.color, radius: n.radius,
            }).collect(),
            paths: f.paths.iter().map(|n| fc::Path {
                model: n.model, angle: n.extra[0], color: n.color,
            }).collect(),
            fills: f.fills.clone(),
            ..Default::default()
        };
        if let Some(order) = &f.order {
            return self.pack_grouped(f, order, scene, width, height);
        }
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
        // The frame's batch of each of the scene's (a batch whose slot is
        // gone is left out).
        let (mut quad_of, mut run_of) = (Vec::new(), Vec::new());
        for (i, b) in f.batches.iter().enumerate() {
            let Some(texture) = tex_index(b.slot_id, &mut scene) else { continue };
            quad_of.push(i);
            scene.texture_batches.push(fc::TextureBatch {
                texture,
                quads: b.nodes.iter().map(|n| n.model).collect(),
            });
        }
        for (i, b) in f.glyph_batches.iter().enumerate() {
            let Some(atlas) = tex_index(b.atlas_slot_id, &mut scene) else { continue };
            run_of.push(i);
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
        let (mut frame, drops) = {
            let mut cache = self.fills.lock().unwrap_or_else(|e| e.into_inner());
            fc::Frame::pack_cached(&scene, width, height, width, Some(&mut cache))
        };
        // Window 0 is the screen's background; every other window's
        // surface is transparent where the window draws nothing.
        if f.id != 0 {
            frame.header.clear = fc::TRANSPARENT;
        }
        let id = |n: fc::Node| -> Option<u32> {
            match n {
                fc::Node::Rect(i) => f.rect_ids.get(i).copied(),
                fc::Node::Path(i) => f.path_ids.get(i).copied(),
                fc::Node::Fill(i) => f.fill_ids.get(i).copied(),
                fc::Node::Quad(b, q) => f.quad_ids.get(*quad_of.get(b)?)?.get(q).copied(),
                fc::Node::Run(b, r) => f.run_ids.get(*run_of.get(b)?)?.get(r).copied(),
            }
        };
        let drops = drops.into_iter().filter_map(|(n, why)| Some((id(n)?, why))).collect();
        (frame, drops)
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
    if state.has_groups() {
        grouped_frame(state, &mut w);
        return Some(w);
    }
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
        w.rect_ids.push(*id);
        w.rects.push(translate_rect(&state.rect_nodes[id], 0.0, 0.0));
    }
    let mut path_ids: Vec<&u32> = state.path_nodes.keys().collect();
    path_ids.sort_unstable();
    for id in path_ids {
        w.path_ids.push(*id);
        w.paths.push(translate_path(&state.path_nodes[id], 0.0, 0.0));
    }
    let mut fill_ids: Vec<&u32> = state.fill_nodes.keys().collect();
    fill_ids.sort_unstable();
    for id in fill_ids {
        w.fill_ids.push(*id);
        w.fills.push(translate_fill(&state.fill_nodes[id]));
    }
    let mut tex_by_slot: std::collections::BTreeMap<u32, (Vec<TextureNode>, Vec<u32>)> =
        std::collections::BTreeMap::new();
    let mut tex_ids: Vec<&u32> = state.texture_nodes.keys().collect();
    tex_ids.sort_unstable();
    for id in tex_ids {
        let p = &state.texture_nodes[id];
        let e = tex_by_slot.entry(p.slot_id).or_default();
        e.0.push(translate_texture(p, 0.0, 0.0));
        e.1.push(*id);
    }
    for (slot_id, (nodes, ids)) in tex_by_slot {
        w.batches.push(TextureBatch { slot_id, nodes });
        w.quad_ids.push(ids);
    }
    /* glyph_run nodes grouped by atlas slot. Each batch's `glyphs`
     * vector concatenates all runs in slot, with each node's
     * meta[1] (glyph_offset) pointing at this batch's slice. */
    let mut glyph_by_slot: std::collections::BTreeMap<u32, (GlyphRunBatch, Vec<u32>)> =
        std::collections::BTreeMap::new();
    let mut glyph_ids: Vec<&u32> = state.glyph_run_nodes.keys().collect();
    glyph_ids.sort_unstable();
    for id in glyph_ids {
        let p = &state.glyph_run_nodes[id];
        let (entry, ids) = glyph_by_slot
            .entry(p.atlas_slot_id)
            .or_insert_with(|| (GlyphRunBatch {
                atlas_slot_id: p.atlas_slot_id,
                nodes:  Vec::new(),
                glyphs: Vec::new(),
            }, Vec::new()));
        ids.push(*id);
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
    for (b, ids) in glyph_by_slot.into_values() {
        w.glyph_batches.push(b);
        w.run_ids.push(ids);
    }
    Some(w)
}

impl SceneState {
    /// `pack_report` for a window that uses groups: its elements in tree
    /// order, kinds interleaved, each under its clip.
    fn pack_grouped(
        &self,
        f: &WindowFrame,
        order: &[OrderItem],
        mut scene: fresco_compute::Scene,
        width: u32,
        height: u32,
    ) -> (fresco_compute::Frame, Vec<(u32, u8)>) {
        use fresco_compute as fc;
        // Batches whose slot is not uploaded are left out, as in the flat
        // path; `remap` says where each batch landed.
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
        let mut tex_remap = Vec::with_capacity(f.batches.len());
        for b in &f.batches {
            tex_remap.push(tex_index(b.slot_id, &mut scene).map(|texture| {
                scene.texture_batches.push(fc::TextureBatch { texture, quads: b.nodes.iter().map(|n| n.model).collect() });
                scene.texture_batches.len() - 1
            }));
        }
        let mut glyph_remap = Vec::with_capacity(f.glyph_batches.len());
        for b in &f.glyph_batches {
            glyph_remap.push(tex_index(b.atlas_slot_id, &mut scene).map(|atlas| {
                let runs = b.nodes.iter().map(|n| {
                    let (count, off) = (n.meta[0].max(0) as usize, n.meta[1].max(0) as usize);
                    let glyphs = b.glyphs.get(off..off + count).unwrap_or(&[]);
                    fc::GlyphRun {
                        origin: [n.origin[0], n.origin[1]],
                        color: n.color,
                        glyphs: glyphs.iter().map(|g| fc::Glyph { d_offset: g.d_offset, atlas_uv: g.atlas_uv, bearing: g.bearing }).collect(),
                    }
                }).collect();
                scene.glyph_batches.push(fc::GlyphBatch { atlas, runs });
                scene.glyph_batches.len() - 1
            }));
        }
        let mut placed = Vec::with_capacity(order.len());
        let mut node_of: HashMap<fc::Node, u32> = HashMap::with_capacity(order.len());
        for o in order {
            let item = match o.item {
                fc::Node::Quad(b, q) => match tex_remap.get(b).copied().flatten() {
                    Some(b) => fc::Node::Quad(b, q),
                    None => continue,
                },
                fc::Node::Run(b, r) => match glyph_remap.get(b).copied().flatten() {
                    Some(b) => fc::Node::Run(b, r),
                    None => continue,
                },
                other => other,
            };
            placed.push(fc::Placed { item, clip: o.clip });
            node_of.insert(item, o.node);
        }
        let (mut frame, drops) = {
            let mut cache = self.fills.lock().unwrap_or_else(|e| e.into_inner());
            fc::Frame::pack_ordered(&scene, &placed, width, height, width, Some(&mut cache))
        };
        if f.id != 0 {
            frame.header.clear = fc::TRANSPARENT;
        }
        let drops = drops.into_iter().filter_map(|(n, why)| Some((*node_of.get(&n)?, why))).collect();
        (frame, drops)
    }
}

/// A group's composed transform: uniform scale, then translation (window
/// pixels), in f64 (fresco-scene-groups.md §2.3).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Xf {
    s: f64,
    tx: f64,
    ty: f64,
}

impl Xf {
    const ID: Xf = Xf { s: 1.0, tx: 0.0, ty: 0.0 };
    fn then(self, t: &[f32; 6]) -> Xf {
        // self ∘ t, t = [s, 0, 0, s, e, f].
        Xf { s: self.s * t[0] as f64, tx: self.s * t[4] as f64 + self.tx, ty: self.s * t[5] as f64 + self.ty }
    }
    fn x(self, x: f32) -> f32 { (self.s * x as f64 + self.tx) as f32 }
    fn y(self, y: f32) -> f32 { (self.s * y as f64 + self.ty) as f32 }
    fn len(self, v: f32) -> f32 { (self.s * v as f64) as f32 }
    fn rect(self, r: [f32; 4]) -> [f32; 4] { [self.x(r[0]), self.y(r[1]), self.x(r[2]), self.y(r[3])] }
}

fn meet(a: Option<[f32; 4]>, b: Option<[f32; 4]>) -> Option<[f32; 4]> {
    match (a, b) {
        (Some(a), Some(b)) => Some([a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])]),
        (a, None) => a,
        (None, b) => b,
    }
}

fn empty(c: Option<[f32; 4]>) -> bool {
    c.is_some_and(|c| c[0] >= c[2] || c[1] >= c[3])
}

/// Fill `w` from a window that uses groups: unplaced leaves first, in the
/// flat path's kind-then-id order (so a client that never places a node
/// draws as before), then the tree, depth first, siblings by (order, id).
fn grouped_frame(state: &fresco_scene_server::command::envelope_frontend::WindowSceneState, w: &mut WindowFrame) {
    use fresco_compute::Node as Item;
    let mut order: Vec<OrderItem> = Vec::new();
    let sorted = |keys: Vec<&u32>| { let mut k: Vec<u32> = keys.into_iter().copied().collect(); k.sort_unstable(); k };
    let unplaced = |id: &u32| !state.placements.contains_key(id);

    // One element, under transform `x`, opacity `op` and clip `clip`.
    let emit = |w: &mut WindowFrame, order: &mut Vec<OrderItem>, id: u32, x: Xf, op: f32, clip: Option<[f32; 4]>| {
        let item = if let Some(p) = state.rect_nodes.get(&id) {
            w.rects.push(SceneNode {
                position: [x.x(p.x), x.y(p.y)],
                size: [x.len(p.w), x.len(p.h)],
                color: [p.r, p.g, p.b, p.a * op],
                radius: x.len(p.radius),
                _pad: [0.0; 3],
            });
            Item::Rect(w.rects.len() - 1)
        } else if let Some(p) = state.path_nodes.get(&id) {
            let mut n = translate_path(p, 0.0, 0.0);
            n.model = [x.x(p.cx), x.y(p.cy), x.len(p.length), x.len(p.width)];
            n.color[3] *= op;
            w.paths.push(n);
            Item::Path(w.paths.len() - 1)
        } else if let Some(p) = state.fill_nodes.get(&id) {
            let mut fl = translate_fill(p);
            let t = fl.transform;
            let s = x.s as f32;
            fl.transform = [s * t[0], s * t[1], s * t[2], s * t[3], x.x(t[4]), x.y(t[5])];
            fl.clip = meet(fl.clip.map(|c| x.rect(c)), clip);
            fl.color[3] *= op;
            w.fills.push(fl);
            w.fill_ids.push(id);
            Item::Fill(w.fills.len() - 1)
        } else if let Some(p) = state.texture_nodes.get(&id) {
            let mut n = translate_texture(p, 0.0, 0.0);
            let [qx, qy, qw, qh] = n.model;
            n.model = [x.x(qx), x.y(qy), x.len(qw), x.len(qh)];
            w.batches.push(TextureBatch { slot_id: p.slot_id, nodes: vec![n] });
            Item::Quad(w.batches.len() - 1, 0)
        } else if let Some(p) = state.glyph_run_nodes.get(&id) {
            // Glyphs are atlas bitmaps at the size they were shaped: a run
            // moves with its group but does not scale (§3 step 1).
            let glyphs = p.glyphs.iter().map(|g| GlyphInstance {
                d_offset: [g.dx, g.dy],
                atlas_uv: [g.atlas_u as f32, g.atlas_v as f32, g.atlas_w as f32, g.atlas_h as f32],
                bearing: [g.bearing_x, g.bearing_y],
                ..Default::default()
            }).collect::<Vec<_>>();
            w.glyph_batches.push(GlyphRunBatch {
                atlas_slot_id: p.atlas_slot_id,
                nodes: vec![GlyphRunNode {
                    origin: [x.x(p.x), x.y(p.y), 0.0, 0.0],
                    atlas_dim: [p.atlas_width as f32, p.atlas_height as f32, 0.0, 0.0],
                    color: [p.r, p.g, p.b, p.a * op],
                    meta: [glyphs.len() as i32, 0, 0, 0],
                }],
                glyphs,
            });
            Item::Run(w.glyph_batches.len() - 1, 0)
        } else {
            return;
        };
        order.push(OrderItem { item, clip, node: id });
    };

    // The flat layer: kind, then id, as `window_frame` draws a flat window.
    for id in sorted(state.rect_nodes.keys().filter(|i| unplaced(i)).collect()) { emit(w, &mut order, id, Xf::ID, 1.0, None); }
    for id in sorted(state.path_nodes.keys().filter(|i| unplaced(i)).collect()) { emit(w, &mut order, id, Xf::ID, 1.0, None); }
    for id in sorted(state.fill_nodes.keys().filter(|i| unplaced(i)).collect()) { emit(w, &mut order, id, Xf::ID, 1.0, None); }
    for id in sorted(state.texture_nodes.keys().filter(|i| unplaced(i)).collect()) { emit(w, &mut order, id, Xf::ID, 1.0, None); }
    for id in sorted(state.glyph_run_nodes.keys().filter(|i| unplaced(i)).collect()) { emit(w, &mut order, id, Xf::ID, 1.0, None); }

    // The tree. Children of each parent (None = root), by (order, id).
    let mut kids: HashMap<Option<u32>, Vec<(u32, u32)>> = HashMap::new();
    for (&id, g) in &state.group_nodes {
        kids.entry(g.parent).or_default().push((g.order, id));
    }
    for (&id, p) in &state.placements {
        kids.entry(p.parent).or_default().push((p.order, id));
    }
    for v in kids.values_mut() {
        v.sort_unstable();
    }
    // Iterative depth-first walk (§2.3): a stack of (node, transform,
    // opacity, clip), children pushed in reverse so they pop in order.
    let mut stack: Vec<(u32, Xf, f32, Option<[f32; 4]>)> = Vec::new();
    let push_kids = |stack: &mut Vec<_>, of: Option<u32>, x: Xf, op: f32, clip: Option<[f32; 4]>| {
        if let Some(v) = kids.get(&of) {
            for &(_, id) in v.iter().rev() {
                stack.push((id, x, op, clip));
            }
        }
    };
    push_kids(&mut stack, None, Xf::ID, 1.0, None);
    while let Some((id, x, op, clip)) = stack.pop() {
        if let Some(g) = state.group_nodes.get(&id) {
            // The group's clip is in its parent's space: map it with `x`.
            let c = meet(clip, g.clip.map(|c| x.rect(c)));
            if empty(c) {
                continue; // nothing of it can show: skip the subtree
            }
            push_kids(&mut stack, Some(id), x.then(&g.transform), op * g.opacity, c);
        } else {
            emit(w, &mut order, id, x, op, clip);
        }
    }
    w.order = Some(order);
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

fn translate_fill(p: &fresco_protocol::FillParams) -> fresco_compute::fill::Fill {
    use fresco_compute::fill::{Fill, FillRule, Gradient, GradientShape};
    use fresco_protocol::FillPaint;
    let stops = |s: &[fresco_protocol::GradientStop]| s.iter().map(|s| (s.offset, [s.r, s.g, s.b, s.a])).collect();
    let (color, gradient) = match &p.paint {
        FillPaint::Solid { r, g, b, a } => ([*r, *g, *b, *a * p.opacity], None),
        FillPaint::Linear { x0, y0, x1, y1, stops: s } => (
            [0.0, 0.0, 0.0, p.opacity],
            Some(Gradient { shape: GradientShape::Linear { p0: [*x0, *y0], p1: [*x1, *y1] }, stops: stops(s) }),
        ),
        FillPaint::Radial { cx, cy, radius, stops: s } => (
            [0.0, 0.0, 0.0, p.opacity],
            Some(Gradient { shape: GradientShape::Radial { center: [*cx, *cy], radius: *radius }, stops: stops(s) }),
        ),
    };
    Fill {
        verbs: p.verbs.clone(),
        points: p.points.clone(),
        transform: p.transform,
        rule: if p.rule == 1 { FillRule::EvenOdd } else { FillRule::NonZero },
        color,
        clip: p.clip,
        gradient,
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

#[cfg(test)]
mod advance_tests {
    use super::*;
    use std::sync::Arc;

    /// Drawings patched in place, through the spare, or by a copy, while
    /// readers hold them at random: each one is exactly what was drawn.
    #[test]
    fn advance_keeps_every_drawing_exact() {
        let (w, h) = (37u32, 23u32);
        let mut truth = vec![0u32; (w * h) as usize];
        let mut latest = Arc::new(Pixels { gen: 1, width: w, height: h, px: truth.clone(), seq: 1, since: Vec::new() });
        let mut spare = None;
        let mut held: Vec<Arc<Pixels>> = Vec::new();
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for step in 2..400u64 {
            let x0 = (next() % w as u64) as u32;
            let y0 = (next() % h as u64) as u32;
            let x1 = x0 + 1 + (next() % (w - x0) as u64) as u32;
            let y1 = y0 + 1 + (next() % (h - y0) as u64) as u32;
            let paint = |px: &mut [u32]| {
                for y in y0..y1 {
                    for x in x0..x1 {
                        px[(y * w + x) as usize] = (step as u32) << 8 | x ^ y;
                    }
                }
                Ok(())
            };
            paint(&mut truth).unwrap();
            // A reader holds the latest now and then, for a step or two.
            if next() % 3 == 0 {
                held.push(latest.clone());
            }
            if held.len() > 1 || next() % 2 == 0 {
                held.clear();
            }
            spare = advance(&mut latest, spare, step, step as u32, [x0, y0, x1, y1], paint).unwrap();
            assert_eq!(latest.px, truth, "step {step}");
            assert_eq!((latest.gen, latest.seq), (step, step as u32));
            assert_eq!(latest.since[0], (step - 1, [x0, y0, x1, y1]));
        }
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// Past the op budget, drops come back as the dropped nodes' ids,
    /// across a glyph batch whose atlas is gone (left out of the scene).
    #[test]
    fn budget_drops_are_reported_by_node_id() {
        let mut s = SceneState::default();
        s.slots.insert(9, fresco_compute::Texture {
            width: 8, height: 8, format: fresco_compute::TexFormat::R8, data: vec![255; 64],
        });
        let n = fresco_compute::MAX_OPS as usize - 2;
        let rect = |i: usize| SceneNode {
            position: [(i % 60) as f32, (i / 60 % 60) as f32], size: [2.0, 2.0],
            color: [1.0, 0.0, 0.0, 1.0], ..Default::default()
        };
        let g = GlyphInstance { atlas_uv: [0.0, 0.0, 4.0, 4.0], bearing: [0.0, 4.0], ..Default::default() };
        let run = |off: i32, count: i32| GlyphRunNode {
            origin: [5.0, 20.0, 0.0, 0.0], color: [0.0, 0.0, 0.0, 1.0], meta: [count, off, 0, 0], ..Default::default()
        };
        let f = WindowFrame {
            id: 1,
            size: (64, 64),
            rects: (0..n).map(rect).collect(),
            rect_ids: (0..n as u32).map(|i| 1000 + i).collect(),
            glyph_batches: vec![
                GlyphRunBatch { atlas_slot_id: 5, nodes: vec![run(0, 3)], glyphs: vec![g; 3] },
                GlyphRunBatch { atlas_slot_id: 9, nodes: vec![run(0, 5), run(5, 1)], glyphs: vec![g; 6] },
            ],
            run_ids: vec![vec![7], vec![20, 21]],
            ..Default::default()
        };
        let (frame, drops) = s.pack_report(&f, 64, 64);
        assert_eq!(frame.header.op_count as usize, n);
        let why = fresco_protocol::fill_drop::OP_BUDGET;
        assert_eq!(drops, vec![(20, why), (21, why)]);
    }
}

#[cfg(test)]
mod group_tests {
    //! Windows that use groups (fresco-scene-groups.md §2–3).
    use super::*;
    use fresco_compute::reference;
    use fresco_protocol::{GroupParams, PlaceParams};
    use fresco_scene_server::command::envelope_frontend::WindowSceneState;

    const W: u32 = 64;

    fn rect(s: &mut WindowSceneState, id: u32, x: f32, y: f32, w: f32, h: f32, c: [f32; 4]) {
        s.leaf_replaces(id);
        s.rect_nodes.insert(id, RectParams { x, y, w, h, r: c[0], g: c[1], b: c[2], a: c[3], radius: 0.0 });
        s.node_writers.insert(id, 1);
    }
    fn group(s: &mut WindowSceneState, id: u32, parent: Option<u32>, order: u32, t: [f32; 6], clip: Option<[f32; 4]>) {
        s.set_group(id, GroupParams { parent, order, transform: t, clip, opacity: 1.0, flags: 0 }, 1).unwrap();
    }
    fn place(s: &mut WindowSceneState, id: u32, parent: Option<u32>, order: u32) {
        s.place(id, PlaceParams { parent, order }, 1).unwrap();
    }
    fn flat(s: &WindowSceneState) -> WindowFrame {
        // What `window_frame` builds for a window without groups.
        let mut w = WindowFrame { id: 1, size: (W, W), ..Default::default() };
        let mut ids: Vec<&u32> = s.rect_nodes.keys().collect();
        ids.sort_unstable();
        for id in ids {
            w.rect_ids.push(*id);
            w.rects.push(translate_rect(&s.rect_nodes[id], 0.0, 0.0));
        }
        w
    }
    fn grouped(s: &WindowSceneState) -> WindowFrame {
        let mut w = WindowFrame { id: 1, size: (W, W), ..Default::default() };
        grouped_frame(s, &mut w);
        w
    }
    fn draw(w: &WindowFrame) -> Vec<u32> {
        reference::render(&SceneState::default().pack(w, W, W))
    }
    const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
    const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];
    const ID: [f32; 6] = GroupParams::IDENTITY;

    #[test]
    fn unplaced_leaves_draw_as_a_flat_window() {
        let mut s = WindowSceneState::new();
        rect(&mut s, 2, 0.0, 0.0, 40.0, 40.0, RED);
        rect(&mut s, 1, 20.0, 20.0, 40.0, 40.0, BLUE);
        let want = draw(&flat(&s));
        group(&mut s, 100, None, 0, ID, None); // an empty group: the window now uses groups
        assert!(s.has_groups());
        assert_eq!(draw(&grouped(&s)), want);
    }

    #[test]
    fn a_translated_group_equals_the_same_rects_moved_by_hand() {
        let mut s = WindowSceneState::new();
        group(&mut s, 100, None, 0, [1.0, 0.0, 0.0, 1.0, 10.0, 5.0], None);
        rect(&mut s, 1, 0.0, 0.0, 8.0, 8.0, RED);
        place(&mut s, 1, Some(100), 0);
        let mut by_hand = WindowSceneState::new();
        rect(&mut by_hand, 1, 10.0, 5.0, 8.0, 8.0, RED);
        assert_eq!(draw(&grouped(&s)), draw(&flat(&by_hand)));
        // Scale 2 about the group's origin, then the translation.
        group(&mut s, 100, None, 0, [2.0, 0.0, 0.0, 2.0, 10.0, 5.0], None);
        let mut by_hand = WindowSceneState::new();
        rect(&mut by_hand, 1, 10.0, 5.0, 16.0, 16.0, RED);
        assert_eq!(draw(&grouped(&s)), draw(&flat(&by_hand)));
    }

    #[test]
    fn siblings_draw_by_order_then_id_not_by_kind_or_id() {
        let mut s = WindowSceneState::new();
        group(&mut s, 100, None, 0, ID, None);
        rect(&mut s, 1, 0.0, 0.0, 64.0, 64.0, RED);
        rect(&mut s, 2, 0.0, 0.0, 64.0, 64.0, BLUE);
        // Higher id, lower order: drawn first, so red (id 1) ends on top.
        place(&mut s, 2, Some(100), 1);
        place(&mut s, 1, Some(100), 2);
        let px = draw(&grouped(&s));
        let mut red = WindowSceneState::new();
        rect(&mut red, 1, 0.0, 0.0, 64.0, 64.0, RED);
        assert_eq!(px, draw(&flat(&red)));
    }

    #[test]
    fn clips_nest_and_a_hidden_subtree_is_skipped() {
        let mut s = WindowSceneState::new();
        // Outer clip [0,0,32,64), inner group translated by 16 with its own
        // clip [0,0,40,64) in the outer's space: the meet is [0,0,32,64).
        group(&mut s, 100, None, 0, ID, Some([0.0, 0.0, 32.0, 64.0]));
        group(&mut s, 101, Some(100), 0, [1.0, 0.0, 0.0, 1.0, 16.0, 0.0], Some([0.0, 0.0, 40.0, 64.0]));
        rect(&mut s, 1, 0.0, 0.0, 64.0, 64.0, RED);
        place(&mut s, 1, Some(101), 0);
        let mut by_hand = WindowSceneState::new();
        rect(&mut by_hand, 1, 16.0, 0.0, 16.0, 64.0, RED);
        assert_eq!(draw(&grouped(&s)), draw(&flat(&by_hand)));
        // A group whose clip misses the window: nothing under it is packed.
        group(&mut s, 101, Some(100), 0, ID, Some([100.0, 0.0, 120.0, 64.0]));
        let w = grouped(&s);
        assert!(w.order.as_ref().unwrap().is_empty() && w.rects.is_empty());
    }

    #[test]
    fn a_scrolled_list_is_one_transform_change() {
        // A viewport at y 16..48 holding a 200 px tall column of 8 px rows.
        let mut s = WindowSceneState::new();
        group(&mut s, 100, None, 0, ID, Some([0.0, 16.0, 64.0, 48.0]));
        group(&mut s, 101, Some(100), 0, [1.0, 0.0, 0.0, 1.0, 0.0, 16.0], None);
        for i in 0..25u32 {
            rect(&mut s, i + 1, 0.0, i as f32 * 8.0, 64.0, 8.0, if i % 2 == 0 { RED } else { BLUE });
            place(&mut s, i + 1, Some(101), i);
        }
        let at_0 = draw(&grouped(&s));
        // Scroll by 8: the content group moves up; the viewport stays.
        group(&mut s, 101, Some(100), 0, [1.0, 0.0, 0.0, 1.0, 0.0, 8.0], None);
        let at_8 = draw(&grouped(&s));
        let row = |px: &[u32], y: usize| px[y * W as usize];
        assert_eq!(row(&at_0, 20), row(&at_8, 28)); // shifted by 8 inside the viewport
        assert_eq!(row(&at_0, 8), row(&at_8, 8)); // outside it, unchanged (clear)
        assert_eq!(row(&at_8, 50), row(&at_0, 50));
    }

    #[test]
    fn opacity_multiplies_down_the_tree() {
        let mut s = WindowSceneState::new();
        s.set_group(100, GroupParams { parent: None, order: 0, transform: ID, clip: None, opacity: 0.5, flags: 0 }, 1).unwrap();
        s.set_group(101, GroupParams { parent: Some(100), order: 0, transform: ID, clip: None, opacity: 0.5, flags: 0 }, 1).unwrap();
        rect(&mut s, 1, 0.0, 0.0, 8.0, 8.0, RED);
        place(&mut s, 1, Some(101), 0);
        let w = grouped(&s);
        assert_eq!(w.rects[0].color[3], 0.25);
    }
}
