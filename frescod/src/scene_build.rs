//! Programmatic SceneGraph construction for frescod.
//!
//! Builds CAS-stored mesh + material blobs in fresco-server's wire
//! format, then emits `RenderItem`s via `SceneGraph::compose_append`.
//! This is the bridge between frescod's Rust-native scene
//! authoring and fresco-server's hash-addressed render path — every
//! frame, the SceneGraph it produces could equivalently have come
//! from a network protocol client.

use fresco_scene_server::cas::store::CasStore;
use fresco_scene_server::command::protocol::{Hash256, NULL_HASH};
use fresco_scene_server::scene::graph::{RenderItem, SceneGraph};

/// 8-byte BlobHeader + body. fresco-server's `NodeData::parse` reads
/// the header to dispatch on type_id; the body layout is per-type.
fn encode_blob(type_id: u16, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&type_id.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // version
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// NODE_MATERIAL_SOLID (0x0200): 4-byte body holding RGBA8 packed as
/// `0xAABBGGRR` little-endian.
pub fn store_solid_material(cas: &mut CasStore, rgba: [u8; 4]) -> Hash256 {
    let packed = u32::from_le_bytes(rgba);
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&packed.to_le_bytes());
    body.extend_from_slice(&[0u8; 4]); // pad to 8 bytes (parser requires p.len() >= 8)
    cas.store(&encode_blob(0x0200, 0, &body))
}

/// NODE_MATERIAL_GRADIENT (0x0201). Body:
///   [0..4]    x0 f32
///   [4..8]    y0 f32
///   [8..12]   x1 f32
///   [12..16]  y1 f32
///   [16..20]  stop_count u32 (then 8-byte aligned)
///   per stop: offset f32 + rgba u32 (8 bytes)
///
/// `gradient_type` lives in the BlobHeader's flags (bits 2..3): 0 =
/// linear, 1 = radial, etc. We always emit linear (type=0) here.
pub fn store_linear_gradient(
    cas: &mut CasStore,
    p0: (f32, f32),
    p1: (f32, f32),
    stops: &[(f32, [u8; 4])],
) -> Hash256 {
    let n = stops.len().min(8);
    let mut body = Vec::with_capacity(20 + n * 8);
    body.extend_from_slice(&p0.0.to_le_bytes());
    body.extend_from_slice(&p0.1.to_le_bytes());
    body.extend_from_slice(&p1.0.to_le_bytes());
    body.extend_from_slice(&p1.1.to_le_bytes());
    body.extend_from_slice(&(n as u32).to_le_bytes());
    for (off, rgba) in stops.iter().take(n) {
        body.extend_from_slice(&off.to_le_bytes());
        body.extend_from_slice(&u32::from_le_bytes(*rgba).to_le_bytes());
    }
    // gradient_type = 0 (linear) — encoded in flags bits 2..3 as 0.
    cas.store(&encode_blob(0x0201, 0, &body))
}

/// Store a vertex blob (raw bytes, prefixed with 8-byte header that
/// the renderer skips). For 2D we use POSITION xyz layout (stride 12)
/// so fresco-server's stride table matches; we set z=0 throughout.
pub fn store_vertex_data_xy(cas: &mut CasStore, verts: &[(f32, f32)]) -> Hash256 {
    let mut body = Vec::with_capacity(verts.len() * 12);
    for (x, y) in verts {
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        body.extend_from_slice(&0f32.to_le_bytes()); // z
    }
    // Type 0 = bulk; renderer skips the 8-byte header.
    cas.store(&encode_blob(0x0000, 0, &body))
}

pub fn store_index_data_u16(cas: &mut CasStore, indices: &[u16]) -> Hash256 {
    let mut body = Vec::with_capacity(indices.len() * 2);
    for i in indices {
        body.extend_from_slice(&i.to_le_bytes());
    }
    cas.store(&encode_blob(0x0000, 0, &body))
}

/// NODE_MESH (0x0100). Body:
///   [0..4]    vertex_count u32
///   [4..8]    index_count u32
///   [8..40]   vertex_data hash
///   [40..72]  index_data hash
///
/// Header flags encode the vertex layout. We set bit 0x0100 (POSITION
/// f32x3) so `compute_vertex_stride` returns 12. Index format = u16
/// (flag bit 0x08 not set).
pub fn store_mesh(
    cas: &mut CasStore,
    vertex_count: u32,
    index_count: u32,
    vertex_data: Hash256,
    index_data: Hash256,
) -> Hash256 {
    let mut body = Vec::with_capacity(72);
    body.extend_from_slice(&vertex_count.to_le_bytes());
    body.extend_from_slice(&index_count.to_le_bytes());
    body.extend_from_slice(&vertex_data);
    body.extend_from_slice(&index_data);
    let flags = 0x0100u32; // POSITION f32x3
    cas.store(&encode_blob(0x0100, flags, &body))
}

/// Convenience: build a rectangle mesh in [0..1]² and return its hash.
/// World transform places it at the desired pixel rect.
pub fn store_unit_rect(cas: &mut CasStore) -> Hash256 {
    let verts = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
    let v_hash = store_vertex_data_xy(cas, &verts);
    let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
    let i_hash = store_index_data_u16(cas, &indices);
    store_mesh(cas, 4, 6, v_hash, i_hash)
}

/// 2D affine in column-major 4x4 form, with (sx, sy) scale and (tx, ty)
/// translation. Z column is identity. Used by RenderItem.world_matrix;
/// fresco-server's MVP composition expects column-major.
pub fn affine_2d(sx: f32, sy: f32, tx: f32, ty: f32) -> [f32; 16] {
    [
        sx,  0.0, 0.0, 0.0,    // col 0
        0.0, sy,  0.0, 0.0,    // col 1
        0.0, 0.0, 1.0, 0.0,    // col 2
        tx,  ty,  0.0, 1.0,    // col 3
    ]
}

/// Push one solid-filled rectangle into the scene at pixel (x, y, w, h).
pub fn push_rect(
    scene: &mut SceneGraph,
    cas: &mut CasStore,
    rect_mesh: Hash256,
    x: f32, y: f32, w: f32, h: f32,
    rgba: [u8; 4],
    render_order: u16,
) {
    let mat = store_solid_material(cas, rgba);
    scene.compose_append([RenderItem {
        world_matrix: affine_2d(w, h, x, y),
        mesh: rect_mesh,
        material: mat,
        render_order,
        flags: 0x01, // overlay / ortho space
        stencil_fill: false,
        clip_rect: None,
    }]);
}

/// Push one rectangle with a linear gradient fill.
pub fn push_gradient_rect(
    scene: &mut SceneGraph,
    cas: &mut CasStore,
    rect_mesh: Hash256,
    x: f32, y: f32, w: f32, h: f32,
    p0: (f32, f32),
    p1: (f32, f32),
    stops: &[(f32, [u8; 4])],
    render_order: u16,
) {
    let mat = store_linear_gradient(cas, p0, p1, stops);
    scene.compose_append([RenderItem {
        world_matrix: affine_2d(w, h, x, y),
        mesh: rect_mesh,
        material: mat,
        render_order,
        flags: 0x01,
        stencil_fill: false,
        clip_rect: None,
    }]);
}

#[allow(dead_code)] // for symmetry / future use
pub const NULL: Hash256 = NULL_HASH;

// ─────────────────────────────────────────────────────────────────────
// Scene-tree blobs — used by clients sending content over the wire so
// SceneGraph::traverse can walk a complete tree from a single root hash.
// ─────────────────────────────────────────────────────────────────────

/// NODE_TRANSFORM (0x0004): 64-byte body holding a 4x4 column-major
/// matrix as 16 little-endian f32 values.
pub fn store_transform_matrix(cas: &mut CasStore, m: &[f32; 16]) -> Hash256 {
    let mut body = Vec::with_capacity(64);
    for v in m {
        body.extend_from_slice(&v.to_le_bytes());
    }
    cas.store(&encode_blob(0x0004, 0, &body))
}

/// NODE_RENDERABLE (0x0005). Body:
///   [0..32]   mesh hash
///   [32..64]  material hash
pub fn store_renderable(cas: &mut CasStore, mesh: Hash256, material: Hash256) -> Hash256 {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&mesh);
    body.extend_from_slice(&material);
    cas.store(&encode_blob(0x0005, 0, &body))
}

/// NODE_SCENE_NODE (0x0002). Body:
///   [0..32]   transform hash
///   [32..64]  renderable hash
///   [64..96]  children (node_list) hash, or NULL_HASH for leaf
///
/// BlobHeader.flags bit 0x01 = VISIBLE — required, or `traverse_node`
/// silently skips the node.
pub fn store_scene_node(
    cas: &mut CasStore,
    transform: Hash256,
    renderable: Hash256,
    children: Hash256,
) -> Hash256 {
    let mut body = Vec::with_capacity(96);
    body.extend_from_slice(&transform);
    body.extend_from_slice(&renderable);
    body.extend_from_slice(&children);
    cas.store(&encode_blob(0x0002, 0x01, &body))
}

/// NODE_NODE_LIST (0x0009). Body:
///   [0..4]    count u32
///   [4..]     count × 32-byte hashes
pub fn store_node_list(cas: &mut CasStore, nodes: &[Hash256]) -> Hash256 {
    let mut body = Vec::with_capacity(4 + nodes.len() * 32);
    body.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
    for h in nodes {
        body.extend_from_slice(h);
    }
    cas.store(&encode_blob(0x0009, 0, &body))
}

/// NODE_SCENE_ROOT (0x0001). Body:
///   [0..32]   child_list hash (NODE_NODE_LIST)
///   [32..64]  camera hash (or NULL_HASH for default ortho)
pub fn store_scene_root(cas: &mut CasStore, child_list: Hash256, camera: Hash256) -> Hash256 {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&child_list);
    body.extend_from_slice(&camera);
    cas.store(&encode_blob(0x0001, 0, &body))
}

// ─────────────────────────────────────────────────────────────────────
// Higher-level primitives — circles, lines, rotated rects.
//
// All built as triangle-mesh blobs. tiny-skia could rasterize these
// analytically via Path, but going through meshes keeps the SceneGraph
// path uniform: every visible item is a CAS-stored triangle mesh that
// the backend draws the same way. Path-as-path support is a future
// optimization.
// ─────────────────────────────────────────────────────────────────────

/// Disk mesh: N-segment triangle fan with center at (0,0), radius 1.
/// Caller transforms via `world_matrix` to position + scale.
/// `n_segments` ≥ 8 looks reasonable; 64 is smooth at desktop sizes.
pub fn store_disk(cas: &mut CasStore, n_segments: u32) -> Hash256 {
    let n = n_segments.max(3) as usize;
    let mut verts: Vec<(f32, f32)> = Vec::with_capacity(n + 1);
    verts.push((0.0, 0.0)); // center
    for i in 0..n {
        let theta = (i as f32) * std::f32::consts::TAU / (n as f32);
        verts.push((theta.cos(), theta.sin()));
    }
    let mut indices: Vec<u16> = Vec::with_capacity(n * 3);
    for i in 0..n {
        let next = ((i + 1) % n) + 1;
        indices.extend_from_slice(&[0u16, (i + 1) as u16, next as u16]);
    }
    let v_hash = store_vertex_data_xy(cas, &verts);
    let i_hash = store_index_data_u16(cas, &indices);
    store_mesh(cas, verts.len() as u32, indices.len() as u32, v_hash, i_hash)
}

/// Annulus / ring mesh: outer + inner circles, radius 1 outer / `inner_r` inner.
/// Useful for stroked circles via a single fill (cheaper than stroking a path).
pub fn store_ring(cas: &mut CasStore, n_segments: u32, inner_r: f32) -> Hash256 {
    let n = n_segments.max(3) as usize;
    let mut verts: Vec<(f32, f32)> = Vec::with_capacity(n * 2);
    for i in 0..n {
        let theta = (i as f32) * std::f32::consts::TAU / (n as f32);
        let (cx, sy) = (theta.cos(), theta.sin());
        verts.push((cx, sy));               // outer
        verts.push((cx * inner_r, sy * inner_r)); // inner
    }
    let mut indices: Vec<u16> = Vec::with_capacity(n * 6);
    for i in 0..n {
        let i_o = (i * 2) as u16;
        let i_i = (i * 2 + 1) as u16;
        let n_o = (((i + 1) % n) * 2) as u16;
        let n_i = (((i + 1) % n) * 2 + 1) as u16;
        // Two triangles per segment: (i_o, i_i, n_o), (n_o, i_i, n_i)
        indices.extend_from_slice(&[i_o, i_i, n_o, n_o, i_i, n_i]);
    }
    let v_hash = store_vertex_data_xy(cas, &verts);
    let i_hash = store_index_data_u16(cas, &indices);
    store_mesh(cas, verts.len() as u32, indices.len() as u32, v_hash, i_hash)
}

/// Push an oriented "hand" — a thin rectangle of length L pointing along
/// `angle_rad` from (cx, cy), with `width` thickness. Implemented by
/// rotating + translating the unit-rect mesh.
pub fn push_oriented_rect(
    scene: &mut SceneGraph,
    cas: &mut CasStore,
    rect_mesh: Hash256,
    cx: f32, cy: f32,
    angle_rad: f32,
    length: f32, width: f32,
    rgba: [u8; 4],
    render_order: u16,
) {
    // Build a 2D rotation+translation into a 4x4 matrix.
    // The unit rect is in [0..1]² — we want it to span (-width/2 .. width/2)
    // along the perpendicular and (0 .. length) along the angle direction.
    //
    // Approach: place the rect in local space at (0, -width/2) with size
    // (length, width), then rotate by angle around origin (cx,cy)
    // local: x ∈ [0, length], y ∈ [-w/2, +w/2]
    //   -> world: (cx + length*cos(a) - y*sin(a), cy + length*sin(a) + y*cos(a))
    //
    // Apply via affine: world = Rot(a) * Scale(length, width) * Trans(0, -0.5) * unit_rect
    // unit_rect has corners (0,0), (1,0), (1,1), (0,1)
    let cos_a = angle_rad.cos();
    let sin_a = angle_rad.sin();

    // Combined matrix in column-major.
    // After ScaleTrans: (u, v) -> (u*length, (v-0.5)*width)
    // After Rot:        (x', y') -> (x'*cos_a - y'*sin_a, x'*sin_a + y'*cos_a)
    // After Trans(cx,cy): + (cx, cy)
    //
    // World column 0 (∂world/∂u): (length*cos_a, length*sin_a, 0)
    // World column 1 (∂world/∂v): (-width*sin_a, width*cos_a, 0)
    // World column 3 (constant):  ((-0.5*width)*-sin_a + cx, (-0.5*width)*cos_a + cy, 0, 1)
    //                         =   (cx + 0.5*width*sin_a, cy - 0.5*width*cos_a, ...)
    let world = [
        length * cos_a,                         length * sin_a,                    0.0, 0.0, // col 0
        -width  * sin_a,                        width  * cos_a,                    0.0, 0.0, // col 1
        0.0,                                    0.0,                               1.0, 0.0, // col 2
        cx + 0.5 * width * sin_a,               cy - 0.5 * width * cos_a,          0.0, 1.0, // col 3
    ];

    let mat = store_solid_material(cas, rgba);
    scene.compose_append([RenderItem {
        world_matrix: world,
        mesh: rect_mesh,
        material: mat,
        render_order,
        flags: 0x01,
        stencil_fill: false,
        clip_rect: None,
    }]);
}

/// Push a disk (filled circle) at (cx, cy) with radius r.
pub fn push_disk(
    scene: &mut SceneGraph,
    cas: &mut CasStore,
    disk_mesh: Hash256,
    cx: f32, cy: f32, r: f32,
    rgba: [u8; 4],
    render_order: u16,
) {
    let mat = store_solid_material(cas, rgba);
    scene.compose_append([RenderItem {
        world_matrix: affine_2d(r, r, cx, cy),
        mesh: disk_mesh,
        material: mat,
        render_order,
        flags: 0x01,
        stencil_fill: false,
        clip_rect: None,
    }]);
}

/// Push a ring (circle outline) at (cx, cy) outer radius `r`, inner
/// radius `r * inner_ratio`.
pub fn push_ring(
    scene: &mut SceneGraph,
    cas: &mut CasStore,
    ring_mesh: Hash256,
    cx: f32, cy: f32, r: f32,
    rgba: [u8; 4],
    render_order: u16,
) {
    let mat = store_solid_material(cas, rgba);
    scene.compose_append([RenderItem {
        world_matrix: affine_2d(r, r, cx, cy),
        mesh: ring_mesh,
        material: mat,
        render_order,
        flags: 0x01,
        stencil_fill: false,
        clip_rect: None,
    }]);
}
