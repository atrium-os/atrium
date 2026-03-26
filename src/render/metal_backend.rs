use crate::scene::graph::SceneGraph;
use crate::scene::nodes::*;
use crate::cas::store::CasStore;
use crate::command::protocol::NULL_HASH;

use metal::*;
use core_graphics_types::geometry::CGSize;
use objc2::rc::autoreleasepool as objc2_autorelease;
use objc2::runtime::AnyObject;
use raw_window_handle::HasWindowHandle;
use std::sync::Arc;
use winit::window::Window;

pub struct MetalRenderer {
    device: Device,
    queue: CommandQueue,
    layer: MetalLayer,
    pipeline: RenderPipelineState,
    depth_state: DepthStencilState,
    width: u32,
    height: u32,
}

impl MetalRenderer {
    pub fn new(window: Arc<Window>, width: u32, height: u32) -> Self {
        let device = Device::system_default().expect("no Metal device");
        let queue = device.new_command_queue();

        let layer = MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_presents_with_transaction(false);
        layer.set_drawable_size(CGSize::new(width as f64, height as f64));

        unsafe {
            let raw_window = window.window_handle().unwrap().as_raw();
            match raw_window {
                raw_window_handle::RawWindowHandle::AppKit(handle) => {
                    let ns_view: *mut AnyObject = handle.ns_view.as_ptr().cast();
                    let layer_ptr: *mut AnyObject = std::mem::transmute::<&MetalLayerRef, *mut AnyObject>(layer.as_ref());
                    let _: () = objc2::msg_send![&*ns_view, setWantsLayer: true];
                    let _: () = objc2::msg_send![&*ns_view, setLayer: layer_ptr];
                }
                _ => panic!("unsupported window handle"),
            }
        }

        let library = device.new_library_with_source(SHADER_SRC, &CompileOptions::new())
            .expect("failed to compile shaders");

        let vert = library.get_function("vertex_main", None).unwrap();
        let frag = library.get_function("fragment_main", None).unwrap();

        let desc = RenderPipelineDescriptor::new();
        desc.set_vertex_function(Some(&vert));
        desc.set_fragment_function(Some(&frag));
        desc.color_attachments().object_at(0).unwrap()
            .set_pixel_format(MTLPixelFormat::BGRA8Unorm);

        let pipeline = device.new_render_pipeline_state(&desc)
            .expect("failed to create pipeline");

        let depth_desc = DepthStencilDescriptor::new();
        depth_desc.set_depth_compare_function(MTLCompareFunction::Less);
        depth_desc.set_depth_write_enabled(true);
        let depth_state = device.new_depth_stencil_state(&depth_desc);

        Self {
            device,
            queue,
            layer,
            pipeline,
            depth_state,
            width,
            height,
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.layer.set_drawable_size(CGSize::new(width as f64, height as f64));
    }

    pub fn render_frame(&mut self, scene: &SceneGraph, cas: &CasStore, _frame: u64) {
        objc2_autorelease(|_| {
            let drawable = match self.layer.next_drawable() {
                Some(d) => d,
                None => return,
            };

            let desc = RenderPassDescriptor::new();
            let color = desc.color_attachments().object_at(0).unwrap();
            color.set_texture(Some(drawable.texture()));
            color.set_load_action(MTLLoadAction::Clear);

            // dark background
            color.set_clear_color(MTLClearColor::new(0.05, 0.05, 0.08, 1.0));
            color.set_store_action(MTLStoreAction::Store);

            let cmd_buf = self.queue.new_command_buffer();
            let encoder = cmd_buf.new_render_command_encoder(desc);

            encoder.set_render_pipeline_state(&self.pipeline);

            // build view-projection matrix
            let aspect = self.width as f32 / self.height.max(1) as f32;
            let vp = if let Some((cam, xform)) = scene.camera(cas) {
                let view = invert_rigid(&xform.matrix);
                let proj = perspective(cam.fov_y, cam.aspect_ratio, cam.near_plane, cam.far_plane);
                mat4_mul(&view, &proj)
            } else {
                let view = look_at([0.0, 2.0, 5.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
                let proj = perspective(std::f32::consts::FRAC_PI_4, aspect, 0.1, 100.0);
                mat4_mul(&view, &proj)
            };

            // render each item in the render list
            for (idx, item) in scene.render_list().iter().enumerate() {
                let mvp = mat4_mul(&item.world_matrix, &vp);

                // extract color from material
                let color = if item.material != NULL_HASH {
                    if let Some(data) = cas.load(&item.material) {
                        if let Some(NodeData::Material(mat)) = NodeData::parse(data) {
                            rgba8_to_float(mat.base_color)
                        } else {
                            [0.8, 0.8, 0.8, 1.0]
                        }
                    } else {
                        log::warn!("render[{}]: material {:02x}{:02x}.. not in CAS", idx, item.material[0], item.material[1]);
                        [0.8, 0.8, 0.8, 1.0]
                    }
                } else {
                    [0.8, 0.8, 0.8, 1.0]
                };

                // load mesh vertex data from CAS
                if item.mesh != NULL_HASH {
                    if let Some(mesh_data) = cas.load(&item.mesh) {
                        if let Some(NodeData::Mesh(mesh)) = NodeData::parse(mesh_data) {
                            if let Some(verts) = cas.load(&mesh.vertex_data) {
                                let vb = self.device.new_buffer_with_data(
                                    verts.as_ptr() as *const _,
                                    verts.len() as u64,
                                    MTLResourceOptions::CPUCacheModeDefaultCache,
                                );

                                // transpose row-major → column-major for Metal's float4x4
                                let mvp_t = transpose(&mvp);
                                encoder.set_vertex_bytes(
                                    1,
                                    std::mem::size_of::<[f32; 16]>() as u64,
                                    mvp_t.as_ptr() as *const _,
                                );
                                encoder.set_fragment_bytes(
                                    0,
                                    std::mem::size_of::<[f32; 4]>() as u64,
                                    color.as_ptr() as *const _,
                                );

                                encoder.set_vertex_buffer(0, Some(&vb), 0);

                                if mesh.index_count > 0 && mesh.index_data != NULL_HASH {
                                    if let Some(indices) = cas.load(&mesh.index_data) {
                                        let ib = self.device.new_buffer_with_data(
                                            indices.as_ptr() as *const _,
                                            indices.len() as u64,
                                            MTLResourceOptions::CPUCacheModeDefaultCache,
                                        );
                                        let idx_type = if mesh.index_format == 1 {
                                            MTLIndexType::UInt32
                                        } else {
                                            MTLIndexType::UInt16
                                        };
                                        encoder.draw_indexed_primitives(
                                            MTLPrimitiveType::Triangle,
                                            mesh.index_count as u64,
                                            idx_type,
                                            &ib,
                                            0,
                                        );
                                    }
                                } else {
                                    encoder.draw_primitives(
                                        MTLPrimitiveType::Triangle,
                                        0,
                                        mesh.vertex_count as u64,
                                    );
                                }
                            } else {
                                log::warn!("render[{}]: vertex_data {:02x}{:02x}.. not in CAS", idx, mesh.vertex_data[0], mesh.vertex_data[1]);
                            }
                        } else {
                            log::warn!("render[{}]: mesh blob didn't parse as Mesh", idx);
                        }
                    } else {
                        log::warn!("render[{}]: mesh {:02x}{:02x}.. not in CAS", idx, item.mesh[0], item.mesh[1]);
                    }
                }
            }

            encoder.end_encoding();
            cmd_buf.present_drawable(drawable);
            cmd_buf.commit();
        });
    }
}

const SHADER_SRC: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexOut {
    float4 position [[position]];
};

vertex VertexOut vertex_main(
    const device packed_float3* positions [[buffer(0)]],
    constant float4x4& mvp [[buffer(1)]],
    uint vid [[vertex_id]]
) {
    VertexOut out;
    float3 pos = positions[vid];
    // mvp is row-major from Rust, so multiply pos * mvp
    out.position = float4(pos, 1.0) * mvp;
    return out;
}

fragment float4 fragment_main(
    VertexOut in [[stage_in]],
    constant float4& color [[buffer(0)]]
) {
    return color;
}
"#;

fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    // Metal clip space: z in [0,1], row-major, row-vector (pos * M)
    // View space: camera looks down -Z, objects in front have negative z
    // w_clip = -z_view, z_ndc = z_clip/w_clip maps [-near,-far] → [0,1]
    let f = 1.0 / (fov_y / 2.0).tan();
    let dz = far - near;
    [
        f / aspect, 0.0,  0.0,               0.0,
        0.0,        f,    0.0,               0.0,
        0.0,        0.0, -far / dz,         -1.0,
        0.0,        0.0, -near * far / dz,   0.0,
    ]
}

fn look_at(eye: [f32; 3], center: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    // row-major: row i = basis vector i, row 3 = translation
    let f = normalize(sub(center, eye));
    let s = normalize(cross(f, up));
    let u = cross(s, f);
    [
         s[0],          s[1],          s[2],         0.0,
         u[0],          u[1],          u[2],         0.0,
        -f[0],         -f[1],         -f[2],         0.0,
        -dot(s, eye),  -dot(u, eye),   dot(f, eye),  1.0,
    ]
}

fn invert_rigid(m: &[f32; 16]) -> [f32; 16] {
    // row-major: transpose 3x3 rotation, recompute translation
    // m is row-major: row0=[m[0..4]], row1=[m[4..8]], row2=[m[8..12]], row3=[m[12..16]]
    let tx = m[12]; let ty = m[13]; let tz = m[14];
    [
        m[0],  m[4],  m[8],  0.0,
        m[1],  m[5],  m[9],  0.0,
        m[2],  m[6],  m[10], 0.0,
        -(m[0]*tx + m[4]*ty + m[8]*tz),
        -(m[1]*tx + m[5]*ty + m[9]*tz),
        -(m[2]*tx + m[6]*ty + m[10]*tz),
        1.0,
    ]
}

fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut r = [0f32; 16];
    for row in 0..4 {
        for col in 0..4 {
            let mut sum = 0.0;
            for k in 0..4 {
                sum += a[row * 4 + k] * b[k * 4 + col];
            }
            r[row * 4 + col] = sum;
        }
    }
    r
}

fn transpose(m: &[f32; 16]) -> [f32; 16] {
    [
        m[0], m[4], m[8],  m[12],
        m[1], m[5], m[9],  m[13],
        m[2], m[6], m[10], m[14],
        m[3], m[7], m[11], m[15],
    ]
}

fn rgba8_to_float(rgba: u32) -> [f32; 4] {
    [
        ((rgba >> 0) & 0xFF) as f32 / 255.0,
        ((rgba >> 8) & 0xFF) as f32 / 255.0,
        ((rgba >> 16) & 0xFF) as f32 / 255.0,
        ((rgba >> 24) & 0xFF) as f32 / 255.0,
    ]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] { [a[0]-b[0], a[1]-b[1], a[2]-b[2]] }
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0]]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 { a[0]*b[0] + a[1]*b[1] + a[2]*b[2] }
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len == 0.0 { return v; }
    [v[0]/len, v[1]/len, v[2]/len]
}

unsafe impl Send for MetalRenderer {}
