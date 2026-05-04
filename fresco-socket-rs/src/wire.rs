//! Wire-format encoders for fresco-server's CAS-stored scene blobs.
//!
//! Mirrors the parsers in `fresco_scene_server::scene::nodes::*`. Every
//! function here produces the bytes that `NodeData::parse` would
//! consume — feed the result into [`crate::Connection::upload_blob`]
//! to land it in the server's CAS and get back the SHA-256 hash.
//!
//! Field layouts cross-referenced from `scene/nodes.rs` parser
//! functions so this stays the inverse. The 8-byte BlobHeader has
//! `(type_id u16 le, version u16 le, flags u32 le)` followed by the
//! type-specific body.

use fresco_scene_server::command::protocol::Hash256;

/// 8-byte BlobHeader + body. Caller passes the type_id and any flags
/// bits the parser cares about (e.g. SceneNode VISIBLE = 0x01).
pub fn blob(type_id: u16, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&type_id.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // version
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// NODE_MATERIAL_SOLID (0x0200). 8-byte body: RGBA u32 (`0xAABBGGRR` LE) + 4 bytes pad.
pub fn solid_material(rgba: [u8; 4]) -> Vec<u8> {
    let packed = u32::from_le_bytes(rgba);
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&packed.to_le_bytes());
    body.extend_from_slice(&[0u8; 4]);
    blob(0x0200, 0, &body)
}

/// NODE_MATERIAL_GRADIENT (0x0201). Body: x0 y0 x1 y1 (f32×4) + count u32 + count×(off f32, rgba u32).
/// `gradient_type` is encoded in BlobHeader.flags bits 2..3; we always emit linear (=0).
pub fn linear_gradient(p0: (f32, f32), p1: (f32, f32), stops: &[(f32, [u8; 4])]) -> Vec<u8> {
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
    blob(0x0201, 0, &body)
}

/// Vertex blob — bulk bytes (type_id 0x0000); the renderer skips the
/// 8-byte BlobHeader. Use POSITION xyz layout (stride 12; z=0).
pub fn vertex_data_xy(verts: &[(f32, f32)]) -> Vec<u8> {
    let mut body = Vec::with_capacity(verts.len() * 12);
    for (x, y) in verts {
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        body.extend_from_slice(&0f32.to_le_bytes());
    }
    blob(0x0000, 0, &body)
}

/// u16 indices (default index format). Set Mesh's flags bit 0x08 for u32.
pub fn index_data_u16(indices: &[u16]) -> Vec<u8> {
    let mut body = Vec::with_capacity(indices.len() * 2);
    for i in indices {
        body.extend_from_slice(&i.to_le_bytes());
    }
    blob(0x0000, 0, &body)
}

/// NODE_MESH (0x0100). Body: vertex_count u32, index_count u32, vertex_data hash, index_data hash.
/// BlobHeader.flags bit 0x0100 = POSITION f32x3 (stride 12).
pub fn mesh(vc: u32, ic: u32, vhash: Hash256, ihash: Hash256) -> Vec<u8> {
    let mut body = Vec::with_capacity(72);
    body.extend_from_slice(&vc.to_le_bytes());
    body.extend_from_slice(&ic.to_le_bytes());
    body.extend_from_slice(&vhash);
    body.extend_from_slice(&ihash);
    blob(0x0100, 0x0100, &body)
}

/// NODE_TRANSFORM (0x0004). 64-byte body: 4x4 column-major f32 matrix.
pub fn transform_matrix(m: &[f32; 16]) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    for v in m {
        body.extend_from_slice(&v.to_le_bytes());
    }
    blob(0x0004, 0, &body)
}

/// NODE_RENDERABLE (0x0005). Body: mesh hash + material hash.
pub fn renderable(mesh_h: Hash256, material_h: Hash256) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&mesh_h);
    body.extend_from_slice(&material_h);
    blob(0x0005, 0, &body)
}

/// NODE_SCENE_NODE (0x0002). Body: transform hash, renderable hash, children hash.
/// **VISIBLE bit (0x01 in flags) is required** — without it `traverse_node`
/// silently early-returns.
pub fn scene_node(transform: Hash256, renderable: Hash256, children: Hash256) -> Vec<u8> {
    let mut body = Vec::with_capacity(96);
    body.extend_from_slice(&transform);
    body.extend_from_slice(&renderable);
    body.extend_from_slice(&children);
    blob(0x0002, 0x01, &body)
}

/// NODE_NODE_LIST (0x0009). Body: count u32 + count × 32-byte hashes.
pub fn node_list(nodes: &[Hash256]) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + nodes.len() * 32);
    body.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
    for h in nodes {
        body.extend_from_slice(h);
    }
    blob(0x0009, 0, &body)
}

/// NODE_SCENE_ROOT (0x0001). Body: child_list hash + camera hash.
pub fn scene_root(child_list: Hash256, camera: Hash256) -> Vec<u8> {
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(&child_list);
    body.extend_from_slice(&camera);
    blob(0x0001, 0, &body)
}

/// Raw pixel-data blob (NODE_PIXEL_DATA-style, type_id 0x0000 bulk).
/// Pixel layout = RGBA8, premultiplied (matches tiny-skia's Pixmap),
/// w*h*4 bytes.
pub fn pixel_data(rgba: &[u8]) -> Vec<u8> {
    blob(0x0000, 0, rgba)
}

/// NODE_TEXTURE (0x0400). Body (48 bytes):
///   [0..4]    format u32 (0 = RGBA8)
///   [4..8]    width u32
///   [8..12]   height u32
///   [12..16]  filter+wrap (low byte filter, next byte wrap)
///   [16..48]  pixel_data hash
pub fn texture(width: u32, height: u32, pixel_data_hash: Hash256) -> Vec<u8> {
    let mut body = Vec::with_capacity(48);
    body.extend_from_slice(&0u32.to_le_bytes());      // format = RGBA8
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());      // filter+wrap defaults
    body.extend_from_slice(&pixel_data_hash);
    blob(0x0400, 0, &body)
}

/// NODE_MATERIAL_TEXTURED (0x0203). Body (≥36 bytes):
///   [0..32]   albedo texture hash
///   [32..36]  tint color (RGBA8; 0xFFFFFFFF = no tint)
pub fn material_textured(texture_hash: Hash256, tint: [u8; 4]) -> Vec<u8> {
    let packed = u32::from_le_bytes(tint);
    let mut body = Vec::with_capacity(36);
    body.extend_from_slice(&texture_hash);
    body.extend_from_slice(&packed.to_le_bytes());
    blob(0x0203, 0, &body)
}

/// NODE_MATERIAL_TEXTURED with explicit UV sub-region. Body (52 bytes):
///   [0..32]   albedo texture hash
///   [32..36]  tint color
///   [36..52]  uv region: u0, v0, u1, v1 (4 × f32 LE)
///
/// Use this to slice a shared atlas: one CAS-stored texture, many
/// cheap materials each pointing at a different cell. Glyph atlases
/// are the motivating case (94 ASCII materials, one atlas).
pub fn material_textured_uv(
    texture_hash: Hash256,
    tint: [u8; 4],
    uv_region: [f32; 4],
) -> Vec<u8> {
    let packed = u32::from_le_bytes(tint);
    let mut body = Vec::with_capacity(52);
    body.extend_from_slice(&texture_hash);
    body.extend_from_slice(&packed.to_le_bytes());
    for v in uv_region {
        body.extend_from_slice(&v.to_le_bytes());
    }
    blob(0x0203, 0, &body)
}

/// 2D affine in 4x4 column-major form (z = identity). Used as
/// `RenderItem.world_matrix` / `Transform.matrix` — fresco-server's
/// MVP composition expects column-major.
pub fn affine_2d(sx: f32, sy: f32, tx: f32, ty: f32) -> [f32; 16] {
    [
        sx,  0.0, 0.0, 0.0,
        0.0, sy,  0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        tx,  ty,  0.0, 1.0,
    ]
}
