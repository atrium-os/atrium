use crate::render::backend::GpuBackend;
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
    stencil_fill_pipeline: RenderPipelineState,
    stencil_fill_ds: DepthStencilState,
    stencil_cover_ds: DepthStencilState,
    stencil_texture: Option<Texture>,
    depth_state: DepthStencilState,
    tess_pipeline: ComputePipelineState,
    tess_vertex_buf: Buffer,
    tess_index_buf: Buffer,
    tess_counter_buf: Buffer,
    tess_contour_buf: Buffer,
    width: u32,
    height: u32,
}

impl GpuBackend for MetalRenderer {
    fn new(window: Arc<Window>, width: u32, height: u32) -> Self {
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

        // stencil fill pipeline: no color write, stencil INVERT
        let stencil_fill_desc = RenderPipelineDescriptor::new();
        stencil_fill_desc.set_vertex_function(Some(&vert));
        stencil_fill_desc.set_fragment_function(Some(&frag));
        stencil_fill_desc.color_attachments().object_at(0).unwrap()
            .set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        stencil_fill_desc.color_attachments().object_at(0).unwrap()
            .set_write_mask(MTLColorWriteMask::empty());
        stencil_fill_desc.set_stencil_attachment_pixel_format(MTLPixelFormat::Stencil8);
        let stencil_fill_pipeline = device.new_render_pipeline_state(&stencil_fill_desc)
            .expect("failed to create stencil fill pipeline");

        // stencil fill depth-stencil state: invert stencil on every fragment
        let sf_ds_desc = DepthStencilDescriptor::new();
        let sf_stencil = StencilDescriptor::new();
        sf_stencil.set_stencil_compare_function(MTLCompareFunction::Always);
        sf_stencil.set_depth_stencil_pass_operation(MTLStencilOperation::Invert);
        sf_stencil.set_read_mask(0xFF);
        sf_stencil.set_write_mask(0xFF);
        sf_ds_desc.set_front_face_stencil(Some(&sf_stencil));
        sf_ds_desc.set_back_face_stencil(Some(&sf_stencil));
        let stencil_fill_ds = device.new_depth_stencil_state(&sf_ds_desc);

        // stencil cover depth-stencil state: pass where stencil != 0, then zero it
        let sc_ds_desc = DepthStencilDescriptor::new();
        let sc_stencil = StencilDescriptor::new();
        sc_stencil.set_stencil_compare_function(MTLCompareFunction::NotEqual);
        sc_stencil.set_depth_stencil_pass_operation(MTLStencilOperation::Zero);
        sc_stencil.set_read_mask(0xFF);
        sc_stencil.set_write_mask(0xFF);
        sc_ds_desc.set_front_face_stencil(Some(&sc_stencil));
        sc_ds_desc.set_back_face_stencil(Some(&sc_stencil));
        let stencil_cover_ds = device.new_depth_stencil_state(&sc_ds_desc);

        let tess_library = device.new_library_with_source(TESS_SHADER_SRC, &CompileOptions::new())
            .expect("failed to compile tessellation shaders");
        let tess_fn = tess_library.get_function("tessellate_fill", None).unwrap();
        let tess_pipeline = device.new_compute_pipeline_state_with_function(&tess_fn)
            .expect("failed to create tessellation pipeline");

        let tess_vertex_buf = device.new_buffer(
            4 * 1024 * 1024, // 4MB vertex output
            MTLResourceOptions::StorageModeShared,
        );
        let tess_index_buf = device.new_buffer(
            2 * 1024 * 1024, // 2MB index output
            MTLResourceOptions::StorageModeShared,
        );
        let tess_counter_buf = device.new_buffer(
            16, // 4 u32 counters
            MTLResourceOptions::StorageModeShared,
        );
        let tess_contour_buf = device.new_buffer(
            16384 * 8, // 16K float2 points = 128KB
            MTLResourceOptions::StorageModeShared,
        );

        Self {
            device,
            queue,
            layer,
            pipeline,
            stencil_fill_pipeline,
            stencil_fill_ds,
            stencil_cover_ds,
            stencil_texture: None,
            depth_state,
            tess_pipeline,
            tess_vertex_buf,
            tess_index_buf,
            tess_counter_buf,
            tess_contour_buf,
            width,
            height,
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.layer.set_drawable_size(CGSize::new(width as f64, height as f64));
    }

    fn tessellate_path(&mut self, segments_data: &[u8], tolerance: f32, _fill: bool) -> Option<(Vec<f32>, Vec<u16>)> {
        let seg_count = segments_data.len() / 28;
        if seg_count == 0 { return None; }

        // GPU fan triangulation only correct for convex/simple shapes
        // complex glyphs (many segments, multiple subpaths) need CPU ear-clipping
        if seg_count > 12 { return None; }

        let seg_buf = self.device.new_buffer_with_data(
            segments_data.as_ptr() as *const _,
            segments_data.len() as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let max_verts: u32 = 65536;
        let params: [u32; 4] = [
            seg_count as u32,
            tolerance.to_bits(),
            1, // fill
            max_verts,
        ];
        let params_buf = self.device.new_buffer_with_data(
            params.as_ptr() as *const _,
            16,
            MTLResourceOptions::StorageModeShared,
        );

        unsafe {
            let ptr = self.tess_counter_buf.contents() as *mut u32;
            *ptr = 0;
            *ptr.add(1) = 0;
        }

        let cmd_buf = self.queue.new_command_buffer();
        let encoder = cmd_buf.new_compute_command_encoder();
        encoder.set_compute_pipeline_state(&self.tess_pipeline);
        encoder.set_buffer(0, Some(&seg_buf), 0);
        encoder.set_buffer(1, Some(&params_buf), 0);
        encoder.set_buffer(2, Some(&self.tess_vertex_buf), 0);
        encoder.set_buffer(3, Some(&self.tess_index_buf), 0);
        encoder.set_buffer(4, Some(&self.tess_counter_buf), 0);
        encoder.set_buffer(5, Some(&self.tess_contour_buf), 0);

        encoder.dispatch_threads(MTLSize::new(1, 1, 1), MTLSize::new(1, 1, 1));
        encoder.end_encoding();

        cmd_buf.commit();
        cmd_buf.wait_until_completed();

        let (vert_count, idx_count) = unsafe {
            let ptr = self.tess_counter_buf.contents() as *const u32;
            (*ptr as usize, *ptr.add(1) as usize)
        };

        if vert_count == 0 || idx_count == 0 { return None; }

        log::trace!("GPU tessellated: {} verts, {} indices ({} segments)", vert_count, idx_count, seg_count);

        let vert_floats = vert_count * 3;
        let mut verts = vec![0.0f32; vert_floats];
        unsafe {
            let src = self.tess_vertex_buf.contents() as *const f32;
            std::ptr::copy_nonoverlapping(src, verts.as_mut_ptr(), vert_floats);
        }

        let mut indices = vec![0u16; idx_count];
        unsafe {
            let src = self.tess_index_buf.contents() as *const u16;
            std::ptr::copy_nonoverlapping(src, indices.as_mut_ptr(), idx_count);
        }

        Some((verts, indices))
    }

    fn render_frame(&mut self, scene: &SceneGraph, cas: &CasStore, _frame: u64) {
        // ensure stencil texture exists at correct size
        let need_stencil = self.stencil_texture.is_none()
            || self.stencil_texture.as_ref().map(|t| (t.width(), t.height())) != Some((self.width as u64, self.height as u64));
        if need_stencil {
            let st_desc = TextureDescriptor::new();
            st_desc.set_pixel_format(MTLPixelFormat::Stencil8);
            st_desc.set_width(self.width as u64);
            st_desc.set_height(self.height as u64);
            st_desc.set_storage_mode(MTLStorageMode::Private);
            st_desc.set_usage(MTLTextureUsage::RenderTarget);
            self.stencil_texture = Some(self.device.new_texture(&st_desc));
        }

        objc2_autorelease(|_| {
            let drawable = match self.layer.next_drawable() {
                Some(d) => d,
                None => return,
            };

            let desc = RenderPassDescriptor::new();
            let color = desc.color_attachments().object_at(0).unwrap();
            color.set_texture(Some(drawable.texture()));
            color.set_load_action(MTLLoadAction::Clear);
            color.set_clear_color(MTLClearColor::new(0.05, 0.05, 0.08, 1.0));
            color.set_store_action(MTLStoreAction::Store);

            // attach stencil
            let stencil_att = desc.stencil_attachment().unwrap();
            stencil_att.set_texture(self.stencil_texture.as_deref());
            stencil_att.set_load_action(MTLLoadAction::Clear);
            stencil_att.set_clear_stencil(0);
            stencil_att.set_store_action(MTLStoreAction::DontCare);

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

                                let has_indices = mesh.index_count > 0 && mesh.index_data != NULL_HASH;
                                let ib = if has_indices {
                                    cas.load(&mesh.index_data).map(|indices| {
                                        self.device.new_buffer_with_data(
                                            indices.as_ptr() as *const _,
                                            indices.len() as u64,
                                            MTLResourceOptions::CPUCacheModeDefaultCache,
                                        )
                                    })
                                } else { None };
                                let idx_type = if mesh.index_format == 1 {
                                    MTLIndexType::UInt32
                                } else {
                                    MTLIndexType::UInt16
                                };

                                if item.stencil_fill {
                                    // pass 1: fill stencil with even-odd winding
                                    encoder.set_render_pipeline_state(&self.stencil_fill_pipeline);
                                    encoder.set_depth_stencil_state(&self.stencil_fill_ds);
                                    encoder.set_stencil_reference_value(0);
                                    if let Some(ref ib) = ib {
                                        encoder.draw_indexed_primitives(
                                            MTLPrimitiveType::Triangle,
                                            mesh.index_count as u64, idx_type, ib, 0);
                                    } else {
                                        encoder.draw_primitives(
                                            MTLPrimitiveType::Triangle, 0, mesh.vertex_count as u64);
                                    }

                                    // pass 2: cover — draw same geometry, stencil test non-zero, output color
                                    encoder.set_render_pipeline_state(&self.pipeline);
                                    encoder.set_depth_stencil_state(&self.stencil_cover_ds);
                                    encoder.set_stencil_reference_value(0);
                                    encoder.set_fragment_bytes(
                                        0, std::mem::size_of::<[f32; 4]>() as u64,
                                        color.as_ptr() as *const _);
                                    if let Some(ref ib) = ib {
                                        encoder.draw_indexed_primitives(
                                            MTLPrimitiveType::Triangle,
                                            mesh.index_count as u64, idx_type, ib, 0);
                                    } else {
                                        encoder.draw_primitives(
                                            MTLPrimitiveType::Triangle, 0, mesh.vertex_count as u64);
                                    }

                                    // restore normal pipeline state
                                    encoder.set_depth_stencil_state(&self.depth_state);
                                } else {
                                    // normal draw — no stencil
                                    if let Some(ref ib) = ib {
                                        encoder.draw_indexed_primitives(
                                            MTLPrimitiveType::Triangle,
                                            mesh.index_count as u64, idx_type, ib, 0);
                                    } else {
                                        encoder.draw_primitives(
                                            MTLPrimitiveType::Triangle, 0, mesh.vertex_count as u64);
                                    }
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

const TESS_SHADER_SRC: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct PathSegment {
    uint type_and_pad;
    float x0, y0;
    float x1, y1;
    float x2, y2;
};

struct TessParams {
    uint segment_count;
    float tolerance;
    uint flags;
    uint max_vertices;
};

// iterative cubic bezier flattening with explicit stack (no recursion)
// stack entry: 4 control points (p0, p1, p2, p3)
struct CubicEntry {
    float2 p0, p1, p2, p3;
};

kernel void tessellate_fill(
    const device PathSegment* segments [[buffer(0)]],
    constant TessParams& params [[buffer(1)]],
    device packed_float3* out_vertices [[buffer(2)]],
    device ushort* out_indices [[buffer(3)]],
    device atomic_uint* counters [[buffer(4)]],
    device float2* contour [[buffer(5)]],
    uint tid [[thread_position_in_grid]]
) {
    if (tid != 0) return;

    uint seg_count = params.segment_count;
    float tol_sq = params.tolerance * params.tolerance;
    int max_pts = 16384;
    int contour_count = 0;
    float2 cursor = float2(0.0);

    // explicit stack for iterative subdivision (max depth ~16 = 64K segments)
    CubicEntry stack[16];

    for (uint i = 0; i < seg_count && contour_count < max_pts; i++) {
        uint seg_type = segments[i].type_and_pad & 0xFF;
        float2 sp0 = float2(segments[i].x0, segments[i].y0);
        float2 sp1 = float2(segments[i].x1, segments[i].y1);
        float2 sp2 = float2(segments[i].x2, segments[i].y2);

        switch (seg_type) {
            case 0: // MoveTo
                cursor = sp0;
                contour[contour_count++] = cursor;
                break;

            case 1: // LineTo
                cursor = sp0;
                contour[contour_count++] = cursor;
                break;

            case 2: { // QuadTo — convert to cubic, then flatten
                // quad (p0,p1,p2) → cubic (p0, p0+2/3*(p1-p0), p2+2/3*(p1-p2), p2)
                float2 cp0 = cursor;
                float2 cp1 = cursor + (sp0 - cursor) * (2.0f / 3.0f);
                float2 cp2 = sp1 + (sp0 - sp1) * (2.0f / 3.0f);
                float2 cp3 = sp1;

                int top = 0;
                stack[top++] = {cp0, cp1, cp2, cp3};

                while (top > 0 && contour_count < max_pts) {
                    CubicEntry e = stack[--top];

                    float2 d1 = e.p1 - (e.p0 * 2.0 + e.p3) / 3.0;
                    float2 d2 = e.p2 - (e.p0 + e.p3 * 2.0) / 3.0;
                    float d = max(dot(d1, d1), dot(d2, d2));

                    if (d <= tol_sq) {
                        contour[contour_count++] = e.p3;
                    } else if (top < 15) {
                        float2 m01 = (e.p0 + e.p1) * 0.5;
                        float2 m12 = (e.p1 + e.p2) * 0.5;
                        float2 m23 = (e.p2 + e.p3) * 0.5;
                        float2 m012 = (m01 + m12) * 0.5;
                        float2 m123 = (m12 + m23) * 0.5;
                        float2 mid = (m012 + m123) * 0.5;
                        // push right half first (processed second)
                        stack[top++] = {mid, m123, m23, e.p3};
                        // push left half (processed first)
                        stack[top++] = {e.p0, m01, m012, mid};
                    } else {
                        contour[contour_count++] = e.p3;
                    }
                }
                cursor = sp1;
                break;
            }

            case 3: { // CubicTo — iterative de Casteljau
                int top = 0;
                stack[top++] = {cursor, sp0, sp1, sp2};

                while (top > 0 && contour_count < max_pts) {
                    CubicEntry e = stack[--top];

                    float2 d1 = e.p1 - (e.p0 * 2.0 + e.p3) / 3.0;
                    float2 d2 = e.p2 - (e.p0 + e.p3 * 2.0) / 3.0;
                    float d = max(dot(d1, d1), dot(d2, d2));

                    if (d <= tol_sq) {
                        contour[contour_count++] = e.p3;
                    } else if (top < 15) {
                        float2 m01 = (e.p0 + e.p1) * 0.5;
                        float2 m12 = (e.p1 + e.p2) * 0.5;
                        float2 m23 = (e.p2 + e.p3) * 0.5;
                        float2 m012 = (m01 + m12) * 0.5;
                        float2 m123 = (m12 + m23) * 0.5;
                        float2 mid = (m012 + m123) * 0.5;
                        stack[top++] = {mid, m123, m23, e.p3};
                        stack[top++] = {e.p0, m01, m012, mid};
                    } else {
                        contour[contour_count++] = e.p3;
                    }
                }
                cursor = sp2;
                break;
            }

            case 5: // Close
                if (contour_count > 0) {
                    contour[contour_count++] = contour[0];
                }
                break;
        }
    }

    if (contour_count < 3) return;

    // compute centroid
    float2 centroid = float2(0.0);
    for (int i = 0; i < contour_count; i++) {
        centroid += contour[i];
    }
    centroid /= float(contour_count);

    // fan triangulation from centroid
    uint vc = 0;
    uint ic = 0;

    out_vertices[vc++] = packed_float3{centroid.x, centroid.y, 0.5};
    for (int i = 0; i < contour_count; i++) {
        out_vertices[vc++] = packed_float3{contour[i].x, contour[i].y, 0.5};
    }

    for (int i = 0; i < contour_count; i++) {
        int next = (i + 1 < contour_count) ? i + 2 : 1;
        out_indices[ic++] = 0;
        out_indices[ic++] = ushort(i + 1);
        out_indices[ic++] = ushort(next);
    }

    atomic_store_explicit(&counters[0], vc, memory_order_relaxed);
    atomic_store_explicit(&counters[1], ic, memory_order_relaxed);
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
