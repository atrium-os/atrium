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

impl NodeData {
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.is_empty() { return None; }
        let node_type = data[0];
        match node_type {
            0x01 if data.len() >= 128 => Some(Self::Root(parse_root(data))),
            0x02 if data.len() >= 128 => Some(Self::Node(parse_scene_node(data))),
            0x03 if data.len() >= 128 => Some(Self::Transform(parse_transform(data))),
            0x04 if data.len() >= 128 => Some(Self::Renderable(parse_renderable(data))),
            0x05 if data.len() >= 128 => Some(Self::Material(parse_material(data))),
            0x06 if data.len() >= 128 => Some(Self::Camera(parse_camera(data))),
            0x07 if data.len() >= 128 => Some(Self::Light(parse_light(data))),
            0x08 if data.len() >= 128 => Some(Self::Mesh(parse_mesh_header(data))),
            0x09 if data.len() >= 128 => Some(Self::Texture(parse_texture_header(data))),
            0x0D if data.len() >= 128 => Some(Self::Path(parse_path_header(data))),
            0x11 if data.len() >= 128 => Some(Self::Text(parse_text_node(data))),
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

fn parse_root(d: &[u8]) -> SceneRoot {
    SceneRoot {
        flags: d[1],
        frame_number: read_u32(d, 2),
        node_count: read_u32(d, 6),
        ambient_rgba: read_u32(d, 10),
        child_list: read_hash(d, 32),
        camera: read_hash(d, 64),
        environment: read_hash(d, 96),
    }
}

fn parse_scene_node(d: &[u8]) -> SceneNode {
    SceneNode {
        flags: d[1],
        layer_mask: read_u16(d, 2),
        bound_radius: read_f32(d, 8),
        transform: read_hash(d, 32),
        renderable: read_hash(d, 64),
        children: read_hash(d, 96),
    }
}

fn parse_transform(d: &[u8]) -> Transform {
    let mut matrix = [0f32; 16];
    for i in 0..16 {
        matrix[i] = read_f32(d, 64 + i * 4);
    }
    Transform {
        flags: d[1],
        scale_hint: read_f32(d, 4),
        matrix,
    }
}

fn parse_renderable(d: &[u8]) -> Renderable {
    Renderable {
        flags: d[1],
        render_order: read_u16(d, 2),
        lod_bias: read_f32(d, 8),
        mesh: read_hash(d, 32),
        material: read_hash(d, 64),
    }
}

fn parse_material(d: &[u8]) -> Material {
    let flags = d[1];
    let mut gradient_type = 0u8;
    let mut gradient_stop_count = 0u8;
    let mut gradient_x0 = 0.0f32;
    let mut gradient_y0 = 0.0f32;
    let mut gradient_x1 = 0.0f32;
    let mut gradient_y1 = 0.0f32;
    let mut gradient_stops = [(0.0f32, 0u32); 8];

    if flags & 0x02 != 0 {
        gradient_type = d[14];
        gradient_stop_count = d[15].min(8);
        gradient_x0 = read_f32(d, 16);
        gradient_y0 = read_f32(d, 20);
        gradient_x1 = read_f32(d, 24);
        gradient_y1 = read_f32(d, 28);
        for i in 0..gradient_stop_count as usize {
            let off = 32 + i * 8;
            gradient_stops[i] = (read_f32(d, off), read_u32(d, off + 4));
        }
    }

    Material {
        flags,
        base_color: read_u32(d, 2),
        metallic_roughness: read_u32(d, 6),
        emissive: read_u32(d, 10),
        gradient_type,
        gradient_stop_count,
        gradient_x0, gradient_y0, gradient_x1, gradient_y1,
        gradient_stops,
        shader: if flags & 0x02 != 0 { NULL_HASH } else { read_hash(d, 32) },
        albedo_tex: read_hash(d, 64),
        normal_tex: read_hash(d, 96),
    }
}

fn parse_camera(d: &[u8]) -> Camera {
    Camera {
        flags: d[1],
        fov_y: read_f32(d, 4),
        aspect_ratio: read_f32(d, 8),
        near_plane: read_f32(d, 12),
        far_plane: read_f32(d, 16),
        view_transform: read_hash(d, 32),
    }
}

fn parse_light(d: &[u8]) -> Light {
    let lt = match d[1] {
        1 => LightType::Directional,
        2 => LightType::Spot,
        _ => LightType::Point,
    };
    Light {
        light_type: lt,
        intensity: read_f32(d, 4),
        color: [read_f32(d, 8), read_f32(d, 12), read_f32(d, 16)],
        range: read_f32(d, 20),
        spot_angle: read_f32(d, 24),
        spot_outer: read_f32(d, 28),
        transform: read_hash(d, 32),
        shadow_config: read_hash(d, 64),
    }
}

fn parse_mesh_header(d: &[u8]) -> MeshHeader {
    let mut aabb = [0f32; 6];
    for i in 0..6 {
        aabb[i] = read_f32(d, 14 + i * 4);
    }
    MeshHeader {
        flags: d[1],
        vertex_count: read_u32(d, 2),
        index_count: read_u32(d, 6),
        vertex_stride: read_u16(d, 10),
        index_format: read_u16(d, 12),
        aabb,
        vertex_data: read_hash(d, 32),
        index_data: read_hash(d, 64),
    }
}

fn parse_path_header(d: &[u8]) -> PathHeader {
    PathHeader {
        flags: d[1],
        fill_rule: d[2],
        stroke_join: d[3],
        stroke_width: read_f32(d, 4),
        stroke_miter: read_f32(d, 8),
        tolerance: read_f32(d, 12),
        segment_count: read_u32(d, 16),
        subpath_count: read_u16(d, 20),
        path_data: read_hash(d, 32),
        cached_mesh: read_hash(d, 64),
    }
}

fn parse_texture_header(d: &[u8]) -> TextureHeader {
    TextureHeader {
        format: d[1],
        filter_mode: read_u16(d, 2),
        width: read_u32(d, 4),
        height: read_u32(d, 8),
        mip_levels: read_u16(d, 12),
        wrap_mode: read_u16(d, 14),
        pixel_data: read_hash(d, 32),
        mipchain: read_hash(d, 64),
    }
}

fn parse_text_node(d: &[u8]) -> TextNode {
    let text_len = d[1] as usize;
    let n = text_len.min(84);
    let text = String::from_utf8_lossy(&d[44..44 + n]).into_owned();
    TextNode {
        size: read_f32(d, 2),
        color: read_u32(d, 6),
        font_hash: read_hash(d, 12),
        text,
    }
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
