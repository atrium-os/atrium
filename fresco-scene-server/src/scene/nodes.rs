use crate::command::protocol::{Hash256, NULL_HASH};

#[derive(Clone, Debug)]
pub struct SceneRoot {
    pub flags: u8,
    pub frame_number: u32,
    pub node_count: u32,
    pub ambient_rgba: u32,
    pub child_list: Hash256,
    pub camera: Hash256,
    pub environment: Hash256,
}

#[derive(Clone, Debug)]
pub struct SceneNode {
    pub flags: u8,
    pub layer_mask: u16,
    pub bound_radius: f32,
    pub transform: Hash256,
    pub renderable: Hash256,
    pub children: Hash256,
}

#[derive(Clone, Debug)]
pub struct Transform {
    pub flags: u8,
    pub scale_hint: f32,
    pub matrix: [f32; 16],
}

#[derive(Clone, Debug)]
pub struct Renderable {
    pub flags: u8,
    pub render_order: u16,
    pub lod_bias: f32,
    pub mesh: Hash256,
    pub material: Hash256,
}

#[derive(Clone, Debug)]
pub struct Material {
    pub flags: u8,
    pub base_color: u32,
    pub metallic_roughness: u32,
    pub emissive: u32,
    pub gradient_type: u8,
    pub gradient_stop_count: u8,
    pub gradient_x0: f32,
    pub gradient_y0: f32,
    pub gradient_x1: f32,
    pub gradient_y1: f32,
    pub gradient_stops: [(f32, u32); 8],
    pub shader: Hash256,
    pub albedo_tex: Hash256,
    pub normal_tex: Hash256,
    /// UV sub-rectangle of `albedo_tex` to sample, in normalized
    /// texture coords. Default is the full texture `[0.0, 0.0, 1.0,
    /// 1.0]`. Used to slice a shared atlas — one texture, many
    /// materials each pointing at a different glyph cell.
    pub uv_region: [f32; 4],
}

impl Material {
    pub fn has_gradient(&self) -> bool { self.flags & 0x02 != 0 }
}

#[derive(Clone, Debug)]
pub struct Camera {
    pub flags: u8,
    pub fov_y: f32,
    pub aspect_ratio: f32,
    pub near_plane: f32,
    pub far_plane: f32,
    pub view_transform: Hash256,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LightType {
    Point,
    Directional,
    Spot,
}

#[derive(Clone, Debug)]
pub struct Light {
    pub light_type: LightType,
    pub intensity: f32,
    pub color: [f32; 3],
    pub range: f32,
    pub spot_angle: f32,
    pub spot_outer: f32,
    pub transform: Hash256,
    pub shadow_config: Hash256,
}

#[derive(Clone, Debug)]
pub struct MeshHeader {
    pub flags: u8,
    pub vertex_count: u32,
    pub index_count: u32,
    pub vertex_stride: u16,
    pub index_format: u16,
    pub aabb: [f32; 6],
    pub vertex_data: Hash256,
    pub index_data: Hash256,
}

#[derive(Clone, Debug)]
pub struct TextureHeader {
    pub format: u8,
    pub filter_mode: u16,
    pub width: u32,
    pub height: u32,
    pub mip_levels: u16,
    pub wrap_mode: u16,
    pub pixel_data: Hash256,
    pub mipchain: Hash256,
}

#[derive(Clone, Debug)]
pub struct PathHeader {
    pub flags: u8,
    pub fill_rule: u8,
    pub stroke_join: u8,
    pub stroke_width: f32,
    pub stroke_miter: f32,
    pub tolerance: f32,
    pub segment_count: u32,
    pub subpath_count: u16,
    pub path_data: Hash256,
    pub cached_mesh: Hash256,
}

#[derive(Clone, Copy, Debug)]
pub enum PathSegment {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    QuadTo(f32, f32, f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    Close,
}

impl PathSegment {
    pub fn parse_segments(data: &[u8]) -> Vec<Self> {
        let mut segs = Vec::new();
        let mut i = 0;
        while i + 28 <= data.len() {
            let seg_type = data[i];
            let f = |off: usize| read_f32(data, i + 4 + off * 4);
            let seg = match seg_type {
                0 => PathSegment::MoveTo(f(0), f(1)),
                1 => PathSegment::LineTo(f(0), f(1)),
                2 => PathSegment::QuadTo(f(0), f(1), f(2), f(3)),
                3 => PathSegment::CubicTo(f(0), f(1), f(2), f(3), f(4), f(5)),
                5 => PathSegment::Close,
                _ => { i += 28; continue; }
            };
            segs.push(seg);
            i += 28;
        }
        segs
    }
}

#[derive(Clone, Debug)]
pub struct TextNode {
    pub size: f32,
    pub color: u32,
    pub font_hash: Hash256,
    pub text: String,
}

#[derive(Clone, Debug)]
pub enum NodeData {
    Root(SceneRoot),
    Node(SceneNode),
    Transform(Transform),
    Renderable(Renderable),
    Material(Material),
    Camera(Camera),
    Light(Light),
    Mesh(MeshHeader),
    Texture(TextureHeader),
    Path(PathHeader),
    Text(TextNode),
    Bulk(Vec<u8>),
}

pub struct BlobHeader {
    pub type_id: u16,
    pub version: u16,
    pub flags: u32,
}

pub fn parse_header(data: &[u8]) -> Option<(BlobHeader, &[u8])> {
    if data.len() < 8 { return None; }
    let type_id = read_u16(data, 0);
    let version = read_u16(data, 2);
    let flags = read_u32(data, 4);
    Some((BlobHeader { type_id, version, flags }, &data[8..]))
}

impl NodeData {
    pub fn parse(data: &[u8]) -> Option<Self> {
        let (hdr, p) = parse_header(data)?;
        match hdr.type_id {
            0x0001 if p.len() >= 64 => Some(Self::Root(parse_root(&hdr, p))),
            0x0002 if p.len() >= 96 => Some(Self::Node(parse_scene_node(&hdr, p))),
            0x0003 if p.len() >= 48 => Some(Self::Camera(parse_camera(&hdr, p))),
            0x0004 if p.len() >= 64 => Some(Self::Transform(parse_transform(p))),
            0x0005 if p.len() >= 64 => Some(Self::Renderable(parse_renderable(p))),
            0x0009 if p.len() >= 4 => Some(Self::Bulk(data.to_vec())), // NodeList handled separately
            0x0100 if p.len() >= 72 => Some(Self::Mesh(parse_mesh_header(&hdr, p))),
            0x0101 if p.len() >= 40 => Some(Self::Path(parse_path_header(&hdr, p))),
            0x0200 if p.len() >= 8 => Some(Self::Material(parse_material_solid(&hdr, p))),
            0x0201 if p.len() >= 20 => Some(Self::Material(parse_material_gradient(&hdr, p))),
            0x0203 if p.len() >= 36 => Some(Self::Material(parse_material_textured(p))),
            0x0300 if p.len() >= 40 => Some(Self::Text(parse_text_node(p))),
            0x0400 if p.len() >= 48 => Some(Self::Texture(parse_texture_header(p))),
            _ => Some(Self::Bulk(data.to_vec())),
        }
    }
}

fn read_hash(data: &[u8], offset: usize) -> Hash256 {
    let mut h = [0u8; 32];
    h.copy_from_slice(&data[offset..offset + 32]);
    h
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]])
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset+1]])
}

fn read_f32(data: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([data[offset], data[offset+1], data[offset+2], data[offset+3]])
}

// v1 parsers — all take payload slice (after 8-byte header)

fn parse_root(hdr: &BlobHeader, p: &[u8]) -> SceneRoot {
    SceneRoot {
        flags: hdr.flags as u8,
        frame_number: 0,
        node_count: 0,
        ambient_rgba: 0,
        child_list: read_hash(p, 0),
        camera: read_hash(p, 32),
        environment: NULL_HASH,
    }
}

fn parse_scene_node(hdr: &BlobHeader, p: &[u8]) -> SceneNode {
    SceneNode {
        flags: hdr.flags as u8,
        layer_mask: 0xFFFF,
        bound_radius: 0.0,
        transform: read_hash(p, 0),
        renderable: read_hash(p, 32),
        children: read_hash(p, 64),
    }
}

fn parse_transform(p: &[u8]) -> Transform {
    let mut matrix = [0f32; 16];
    for i in 0..16 {
        matrix[i] = read_f32(p, i * 4);
    }
    Transform {
        flags: 0,
        scale_hint: 1.0,
        matrix,
    }
}

fn parse_renderable(p: &[u8]) -> Renderable {
    Renderable {
        flags: 0,
        render_order: 0,
        lod_bias: 0.0,
        mesh: read_hash(p, 0),
        material: read_hash(p, 32),
    }
}

fn parse_material_solid(hdr: &BlobHeader, p: &[u8]) -> Material {
    Material {
        flags: 0,
        base_color: read_u32(p, 0),
        metallic_roughness: 0,
        emissive: 0,
        gradient_type: 0,
        gradient_stop_count: 0,
        gradient_x0: 0.0, gradient_y0: 0.0, gradient_x1: 0.0, gradient_y1: 0.0,
        gradient_stops: [(0.0, 0); 8],
        shader: NULL_HASH,
        albedo_tex: NULL_HASH,
        normal_tex: NULL_HASH,
        uv_region: [0.0, 0.0, 1.0, 1.0],
    }
}

/// NODE_MATERIAL_TEXTURED (0x0203) — payload layout:
///   [0..32]   albedo texture hash (NODE_TEXTURE blob)
///   [32..36]  tint color (RGBA8, 0xFFFFFFFF = no tint)
///   [36..52]  optional UV region: u0, v0, u1, v1 (4 × f32 LE)
///             present when payload length ≥ 52; otherwise defaults
///             to the full texture (0, 0, 1, 1).
fn parse_material_textured(p: &[u8]) -> Material {
    let uv_region = if p.len() >= 52 {
        [
            read_f32(p, 36), read_f32(p, 40),
            read_f32(p, 44), read_f32(p, 48),
        ]
    } else {
        [0.0, 0.0, 1.0, 1.0]
    };
    Material {
        flags: 0,
        base_color: read_u32(p, 32),       // tint
        metallic_roughness: 0,
        emissive: 0,
        gradient_type: 0,
        gradient_stop_count: 0,
        gradient_x0: 0.0, gradient_y0: 0.0, gradient_x1: 0.0, gradient_y1: 0.0,
        gradient_stops: [(0.0, 0); 8],
        shader: NULL_HASH,
        albedo_tex: read_hash(p, 0),
        normal_tex: NULL_HASH,
        uv_region,
    }
}

/// NODE_TEXTURE (0x0400) — payload layout (48 bytes minimum):
///   [0..4]    format         (u32, 0 = RGBA8)
///   [4..8]    width          (u32)
///   [8..12]   height         (u32)
///   [12..16]  filter+wrap    (low byte filter, next byte wrap, rest reserved)
///   [16..48]  pixel_data hash (NODE_PIXEL_DATA blob with raw bytes)
fn parse_texture_header(p: &[u8]) -> TextureHeader {
    let filter_wrap = read_u32(p, 12);
    TextureHeader {
        format: (read_u32(p, 0) & 0xff) as u8,
        filter_mode: (filter_wrap & 0xff) as u16,
        width:  read_u32(p, 4),
        height: read_u32(p, 8),
        mip_levels: 1,
        wrap_mode:  ((filter_wrap >> 8) & 0xff) as u16,
        pixel_data: read_hash(p, 16),
        mipchain:   NULL_HASH,
    }
}

fn parse_material_gradient(hdr: &BlobHeader, p: &[u8]) -> Material {
    let gradient_type = ((hdr.flags >> 2) & 0x03) as u8;
    let x0 = read_f32(p, 0);
    let y0 = read_f32(p, 4);
    let x1 = read_f32(p, 8);
    let y1 = read_f32(p, 12);
    let stop_count = read_u32(p, 16).min(8) as u8;
    let mut stops = [(0.0f32, 0u32); 8];
    for i in 0..stop_count as usize {
        let off = 20 + i * 8;
        if off + 8 <= p.len() {
            stops[i] = (read_f32(p, off), read_u32(p, off + 4));
        }
    }
    Material {
        flags: 0x02, // gradient flag for renderer
        base_color: if stop_count > 0 { stops[0].1 } else { 0 },
        metallic_roughness: 0,
        emissive: 0,
        gradient_type,
        gradient_stop_count: stop_count,
        gradient_x0: x0, gradient_y0: y0, gradient_x1: x1, gradient_y1: y1,
        gradient_stops: stops,
        shader: NULL_HASH,
        albedo_tex: NULL_HASH,
        normal_tex: NULL_HASH,
        uv_region: [0.0, 0.0, 1.0, 1.0],
    }
}

fn parse_camera(hdr: &BlobHeader, p: &[u8]) -> Camera {
    Camera {
        flags: hdr.flags as u8,
        fov_y: read_f32(p, 0),
        aspect_ratio: read_f32(p, 4),
        near_plane: read_f32(p, 8),
        far_plane: read_f32(p, 12),
        view_transform: read_hash(p, 16),
    }
}

fn parse_mesh_header(hdr: &BlobHeader, p: &[u8]) -> MeshHeader {
    let flags = hdr.flags;
    let stride = compute_vertex_stride(flags);
    MeshHeader {
        flags: (flags & 0xFF) as u8,
        vertex_count: read_u32(p, 0),
        index_count: read_u32(p, 4),
        vertex_stride: stride,
        index_format: if flags & 0x08 != 0 { 4 } else { 2 },
        aabb: [0.0; 6],
        vertex_data: read_hash(p, 8),
        index_data: read_hash(p, 40),
    }
}

fn compute_vertex_stride(flags: u32) -> u16 {
    let mut stride = 0u16;
    if flags & 0x0100 != 0 { stride += 12; } // POSITION f32x3
    if flags & 0x0200 != 0 { stride += 12; } // NORMAL f32x3
    if flags & 0x0400 != 0 { stride += 8; }  // UV0 f32x2
    if flags & 0x0800 != 0 { stride += 8; }  // UV1 f32x2
    if flags & 0x1000 != 0 { stride += 4; }  // COLOR u8x4
    if flags & 0x2000 != 0 { stride += 16; } // TANGENT f32x4
    if flags & 0x4000 != 0 { stride += 8; }  // JOINTS u16x4
    if flags & 0x8000 != 0 { stride += 16; } // WEIGHTS f32x4
    stride
}

fn parse_path_header(hdr: &BlobHeader, p: &[u8]) -> PathHeader {
    let flags = hdr.flags;
    let draw_mode = (flags & 0x03) as u8;
    let fill_rule = ((flags >> 2) & 0x03) as u8;
    PathHeader {
        flags: draw_mode,
        fill_rule,
        stroke_join: 0,
        stroke_width: read_f32(p, 4),
        stroke_miter: 0.0,
        tolerance: 0.0005,
        segment_count: read_u32(p, 0),
        subpath_count: 0,
        path_data: read_hash(p, 8),
        cached_mesh: NULL_HASH,
    }
}

fn parse_text_node(p: &[u8]) -> TextNode {
    let size = read_f32(p, 0);
    let color = read_u32(p, 4);
    let font_hash = read_hash(p, 8);
    let text_bytes = &p[40..];
    let text_len = text_bytes.iter().position(|&b| b == 0).unwrap_or(text_bytes.len());
    let text = String::from_utf8_lossy(&text_bytes[..text_len]).into_owned();
    TextNode { size, color, font_hash, text }
}

impl Transform {
    pub fn identity() -> Self {
        Self {
            flags: 0x01,
            scale_hint: 1.0,
            matrix: [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, 0.0, 1.0,
            ],
        }
    }

    pub fn from_trs(pos: [f32; 3], rot: [f32; 4], scale: f32) -> Self {
        let [qx, qy, qz, qw] = rot;
        let s = scale;

        let xx = qx * qx; let yy = qy * qy; let zz = qz * qz;
        let xy = qx * qy; let xz = qx * qz; let yz = qy * qz;
        let wx = qw * qx; let wy = qw * qy; let wz = qw * qz;

        Self {
            flags: if scale == 1.0 { 0x02 } else { 0x04 },
            scale_hint: scale,
            matrix: [
                s * (1.0 - 2.0 * (yy + zz)), s * 2.0 * (xy - wz),         s * 2.0 * (xz + wy),         0.0,
                s * 2.0 * (xy + wz),         s * (1.0 - 2.0 * (xx + zz)), s * 2.0 * (yz - wx),         0.0,
                s * 2.0 * (xz - wy),         s * 2.0 * (yz + wx),         s * (1.0 - 2.0 * (xx + yy)), 0.0,
                pos[0],                       pos[1],                       pos[2],                       1.0,
            ],
        }
    }
}
