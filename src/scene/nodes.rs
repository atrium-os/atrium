use crate::command::protocol::Hash256;

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
    pub shader: Hash256,
    pub albedo_tex: Hash256,
    pub normal_tex: Hash256,
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
    Material {
        flags: d[1],
        base_color: read_u32(d, 2),
        metallic_roughness: read_u32(d, 6),
        emissive: read_u32(d, 10),
        shader: read_hash(d, 32),
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
