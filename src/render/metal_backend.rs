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
use std::collections::HashMap;
use std::sync::Arc;
use winit::window::Window;

pub struct MetalRenderer {
    device: Device,
    queue: CommandQueue,
    layer: MetalLayer,
    pipeline: RenderPipelineState,
    gradient_pipeline: RenderPipelineState,
    textured_pipeline: RenderPipelineState,
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
    /// CAS hash of NODE_TEXTURE blob → MTLTexture. Lazily populated
    /// on first sight of a texture; never evicted (GC story TBD).
    texture_cache: HashMap<[u8; 32], Texture>,
    /// Per-window framebuffers (B2). Each non-screen window renders
    /// its scene into its own offscreen color+stencil texture; the
    /// screen pass then composites these as textured quads.
    window_fbos: HashMap<u16, WindowFbo>,
    width: u32,
    height: u32,
    scale_factor: f64,
}

/// Offscreen framebuffer for one window. Lives until the window is
/// destroyed or resized (resize re-allocates). Holds a `last_resized`
/// timestamp so a cursor-driven drag-resize can be throttled to a
/// sane reallocation rate instead of churning Metal textures on
/// every move event.
pub struct WindowFbo {
    pub color:    Texture,
    pub stencil:  Texture,
    pub width:    u32,    // physical pixels
    pub height:   u32,
    pub last_resized: std::time::Instant,
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
        layer.set_maximum_drawable_count(3);
        let scale_factor = window.scale_factor();
        unsafe {
            let layer_ptr: *mut AnyObject = std::mem::transmute::<&MetalLayerRef, *mut AnyObject>(layer.as_ref());
            let _: () = objc2::msg_send![&*layer_ptr, setDisplaySyncEnabled: true];
            let _: () = objc2::msg_send![&*layer_ptr, setContentsScale: scale_factor];
            let enabled: bool = objc2::msg_send![&*layer_ptr, displaySyncEnabled];
            log::info!("CAMetalLayer: displaySyncEnabled={} maxDrawables=3 scale={}", enabled, scale_factor);
        }
        layer.set_framebuffer_only(true);

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
        // Stencil attachment is present in the screen render pass
        // (used by text-glyph fill+cover). Pipelines that draw into
        // that pass MUST declare the stencil format or Metal's
        // behavior becomes undefined once the stencil has been
        // touched — that's what was making decoration draws after
        // the first window's text glyphs silently no-op.
        desc.set_stencil_attachment_pixel_format(MTLPixelFormat::Stencil8);

        let pipeline = device.new_render_pipeline_state(&desc)
            .expect("failed to create pipeline");

        let frag_grad = library.get_function("fragment_gradient", None).unwrap();
        let grad_desc = RenderPipelineDescriptor::new();
        grad_desc.set_vertex_function(Some(&vert));
        grad_desc.set_fragment_function(Some(&frag_grad));
        grad_desc.color_attachments().object_at(0).unwrap()
            .set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        grad_desc.set_stencil_attachment_pixel_format(MTLPixelFormat::Stencil8);
        let gradient_pipeline = device.new_render_pipeline_state(&grad_desc)
            .expect("failed to create gradient pipeline");

        // Textured material pipeline: separate vertex shader because
        // textured meshes have stride 20 (POSITION+UV) vs. solid 12.
        // Alpha blending enabled — glyph atlases (text) carry coverage
        // in A; sampled images use A=255 so blending is a no-op for
        // them. Standard "premultiplied source over" form, but we
        // multiply src.rgb by src.a in the blend equation since our
        // glyph atlas isn't premultiplied (R=G=B=255, A=coverage).
        let vert_tex = library.get_function("vertex_textured", None).unwrap();
        let frag_tex = library.get_function("fragment_textured", None).unwrap();
        let tex_desc = RenderPipelineDescriptor::new();
        tex_desc.set_vertex_function(Some(&vert_tex));
        tex_desc.set_fragment_function(Some(&frag_tex));
        let tex_color = tex_desc.color_attachments().object_at(0).unwrap();
        tex_color.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        tex_color.set_blending_enabled(true);
        tex_color.set_rgb_blend_operation(MTLBlendOperation::Add);
        tex_color.set_alpha_blend_operation(MTLBlendOperation::Add);
        tex_color.set_source_rgb_blend_factor(MTLBlendFactor::SourceAlpha);
        tex_color.set_destination_rgb_blend_factor(MTLBlendFactor::OneMinusSourceAlpha);
        tex_color.set_source_alpha_blend_factor(MTLBlendFactor::One);
        tex_color.set_destination_alpha_blend_factor(MTLBlendFactor::OneMinusSourceAlpha);
        tex_desc.set_stencil_attachment_pixel_format(MTLPixelFormat::Stencil8);
        let textured_pipeline = device.new_render_pipeline_state(&tex_desc)
            .expect("failed to create textured pipeline");

        let depth_desc = DepthStencilDescriptor::new();
        // No depth attachment exists in the screen pass (color + stencil
        // only). With compare=Less and no depth attachment, behavior is
        // driver-dependent; the macOS Metal driver fails the test for
        // some draws after a stencil_fill cover pass has set this state,
        // making decorations of windows past the first invisible. Use
        // Always — depth ordering is enforced by render-list order.
        depth_desc.set_depth_compare_function(MTLCompareFunction::Always);
        depth_desc.set_depth_write_enabled(false);
        // Explicit no-op stencil descriptor: always-pass, never write.
        // Required because the regular/textured/gradient pipelines
        // declare a stencil_attachment_pixel_format (so they can
        // coexist in the screen pass with text-glyph stencil ops),
        // and Metal's behavior with a stencil-aware pipeline + a
        // depth-stencil state lacking a stencil descriptor is
        // implementation-defined — on this driver it ends up failing
        // the stencil test where text was previously drawn, masking
        // out the second window's decorations.
        let noop_stencil = StencilDescriptor::new();
        noop_stencil.set_stencil_compare_function(MTLCompareFunction::Always);
        noop_stencil.set_stencil_failure_operation(MTLStencilOperation::Keep);
        noop_stencil.set_depth_failure_operation(MTLStencilOperation::Keep);
        noop_stencil.set_depth_stencil_pass_operation(MTLStencilOperation::Keep);
        noop_stencil.set_read_mask(0xFF);
        noop_stencil.set_write_mask(0x00);
        depth_desc.set_front_face_stencil(Some(&noop_stencil));
        depth_desc.set_back_face_stencil(Some(&noop_stencil));
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
            gradient_pipeline,
            textured_pipeline,
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
            texture_cache: HashMap::new(),
            window_fbos: HashMap::new(),
            width,
            height,
            scale_factor,
        }
    }
}

impl GpuBackend for MetalRenderer {
    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.layer.set_drawable_size(CGSize::new(width as f64, height as f64));
    }

    fn set_scale(&mut self, scale: f64) {
        unsafe {
            let layer_ptr: *mut AnyObject = std::mem::transmute::<&MetalLayerRef, *mut AnyObject>(self.layer.as_ref());
            let _: () = objc2::msg_send![&*layer_ptr, setContentsScale: scale];
        }
    }

    fn tessellate_path(&mut self, segments_data: &[u8], tolerance: f32, _fill: bool) -> Option<(Vec<f32>, Vec<u16>)> {
        let seg_count = segments_data.len() / 28;
        if seg_count == 0 { return None; }

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

    fn render_frame(&mut self, scene: &SceneGraph, cas: &CasStore, _frame: u64, cursor: Option<(f32, f32)>) {
        self._render_frame_impl(scene, cas, _frame, cursor, &[]);
    }

    fn render_frame_with_overlays(&mut self,
                                  scene: &SceneGraph, cas: &CasStore,
                                  frame: u64, cursor: Option<(f32, f32)>,
                                  overlays: &[crate::render::backend::WindowOverlay]) {
        self._render_frame_impl(scene, cas, frame, cursor, overlays);
    }

    fn sync_fbos(&mut self, live: &HashMap<u16, (u32, u32)>) {
        for (&id, &(w, h)) in live.iter() {
            self.ensure_window_fbo(id, w, h);
        }
        let stale: Vec<u16> = self.window_fbos.keys().copied()
            .filter(|id| !live.contains_key(id)).collect();
        for id in stale {
            log::info!("FBO: window {} freed", id);
            self.window_fbos.remove(&id);
        }
    }

    fn render_window_to_fbo(&mut self, id: u16, scene: &SceneGraph, cas: &CasStore) {
        // Forward to the inherent method (defined in the FBO impl block).
        MetalRenderer::render_window_to_fbo(self, id, scene, cas);
    }
}

impl MetalRenderer {
    /// Ensure an MTLTexture exists for the given NODE_TEXTURE blob hash.
    /// Returns None if the blob is missing, malformed, or its pixel
    /// data isn't in CAS yet.
    fn ensure_texture(&mut self, cas: &CasStore, hash: &[u8; 32]) -> Option<&Texture> {
        if !self.texture_cache.contains_key(hash) {
            let blob = cas.load(hash)?;
            let parsed = NodeData::parse(blob)?;
            let hdr = match parsed { NodeData::Texture(h) => h, _ => return None };
            if hdr.format != 0 || hdr.width == 0 || hdr.height == 0 {
                log::warn!("texture {:02x}{:02x}: unsupported format/size", hash[0], hash[1]);
                return None;
            }
            let pixel_blob = cas.load(&hdr.pixel_data)?;
            // strip the 8-byte blob header from pixel data
            let pixels = if pixel_blob.len() > 8 { &pixel_blob[8..] } else { pixel_blob };
            let need = (hdr.width as usize) * (hdr.height as usize) * 4;
            if pixels.len() < need {
                log::warn!("texture {:02x}{:02x}: pixel blob too small ({} < {})",
                    hash[0], hash[1], pixels.len(), need);
                return None;
            }
            let desc = TextureDescriptor::new();
            desc.set_pixel_format(MTLPixelFormat::RGBA8Unorm);
            desc.set_width(hdr.width as u64);
            desc.set_height(hdr.height as u64);
            desc.set_storage_mode(MTLStorageMode::Managed);
            desc.set_usage(MTLTextureUsage::ShaderRead);
            let tex = self.device.new_texture(&desc);
            let region = MTLRegion {
                origin: MTLOrigin { x: 0, y: 0, z: 0 },
                size: MTLSize { width: hdr.width as u64, height: hdr.height as u64, depth: 1 },
            };
            tex.replace_region(region, 0, pixels.as_ptr() as *const _, (hdr.width * 4) as u64);
            log::info!("texture {:02x}{:02x}.. created: {}x{} ({} bytes)",
                hash[0], hash[1], hdr.width, hdr.height, need);
            self.texture_cache.insert(*hash, tex);
        }
        self.texture_cache.get(hash)
    }

    fn _render_frame_impl(&mut self, scene: &SceneGraph, cas: &CasStore, _frame: u64, cursor: Option<(f32, f32)>, overlays: &[crate::render::backend::WindowOverlay]) {
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

        // Pre-pass: ensure every textured material's MTLTexture exists.
        // We can't do this inside the render pass because next_drawable()
        // holds &self.layer and ensure_texture wants &mut self.
        for item in scene.render_list().iter() {
            if item.material == NULL_HASH { continue; }
            if let Some(data) = cas.load(&item.material) {
                if let Some(NodeData::Material(m)) = NodeData::parse(data) {
                    if m.albedo_tex != NULL_HASH {
                        let _ = self.ensure_texture(cas, &m.albedo_tex);
                    }
                }
            }
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

            // Ortho-pixel VP for overlay items (decorations, WM chrome).
            // Width/height in logical pixels — scale from physical.
            let logical_w = self.width as f32 / self.scale_factor as f32;
            let logical_h = self.height as f32 / self.scale_factor as f32;
            let ortho_vp = ortho_pixel_to_clip(logical_w, logical_h);

            self.draw_items(encoder, scene, cas, &vp, &ortho_vp,
                self.width, self.height, self.scale_factor as f32);

            // ── B2: composite per-window FBO textures ──────────────
            // Each overlay is a logical-pixel rect identifying which
            // window FBO to sample. Drawn as a textured quad through
            // ortho_pixel_to_clip, mapping (ov.x..ov.x+ov.w) →
            // (ov.y..ov.y+ov.h) in screen pixels.
            for ov in overlays {
                let Some(fbo) = self.window_fbos.get(&ov.id) else { continue; };
                let fbo_color = fbo.color.clone();
                // pos f32x3 + uv f32x2; 6 vertices for 2 triangles.
                let x0 = ov.x;
                let y0 = ov.y;
                let x1 = ov.x + ov.w;
                let y1 = ov.y + ov.h;
                #[rustfmt::skip]
                let verts: [f32; 30] = [
                    x0, y0, 0.0,  0.0, 0.0,
                    x1, y0, 0.0,  1.0, 0.0,
                    x1, y1, 0.0,  1.0, 1.0,
                    x0, y0, 0.0,  0.0, 0.0,
                    x1, y1, 0.0,  1.0, 1.0,
                    x0, y1, 0.0,  0.0, 1.0,
                ];
                let vb = self.device.new_buffer_with_data(
                    verts.as_ptr() as *const _,
                    (verts.len() * 4) as u64,
                    MTLResourceOptions::CPUCacheModeDefaultCache,
                );
                let mvp_t = transpose(&ortho_vp);
                let tint: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
                encoder.set_render_pipeline_state(&self.textured_pipeline);
                encoder.set_depth_stencil_state(&self.depth_state);
                encoder.set_scissor_rect(MTLScissorRect {
                    x: 0, y: 0, width: self.width as u64, height: self.height as u64,
                });
                encoder.set_vertex_buffer(0, Some(&vb), 0);
                encoder.set_vertex_bytes(1, std::mem::size_of::<[f32; 16]>() as u64,
                    mvp_t.as_ptr() as *const _);
                encoder.set_fragment_texture(0, Some(&fbo_color));
                encoder.set_fragment_bytes(0, std::mem::size_of::<[f32; 4]>() as u64,
                    tint.as_ptr() as *const _);
                encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, 6);
            }


            // cursor overlay (screen-space, no scene graph involvement)
            if let Some((cx, cy)) = cursor {
                let w = self.width as f32;
                let h = self.height as f32;
                let s = h * 0.03; // 3% of screen height
                let k = s / 23.0;
                // NDC: (0,0)=center, (-1,-1)=bottom-left, (1,1)=top-right
                let to_ndc_x = |px: f32| px / w * 2.0 - 1.0;
                let to_ndc_y = |py: f32| 1.0 - py / h * 2.0;
                // classic arrow cursor shape in screen pixels, then to NDC
                let pts: [(f32, f32); 7] = [
                    (cx, cy),
                    (cx, cy + 20.0 * k),
                    (cx + 5.0 * k, cy + 16.0 * k),
                    (cx + 8.5 * k, cy + 23.0 * k),
                    (cx + 11.5 * k, cy + 21.0 * k),
                    (cx + 8.0 * k, cy + 14.0 * k),
                    (cx + 14.0 * k, cy + 14.0 * k),
                ];
                // fan triangulation: 5 triangles from 7 vertices
                let mut cursor_verts = [0.0f32; 5 * 3 * 3]; // 5 tris * 3 verts * 3 floats
                for i in 0..5 {
                    let (ax, ay) = pts[0];
                    let (bx, by) = pts[i + 1];
                    let (ccx, ccy) = pts[i + 2];
                    let base = i * 9;
                    cursor_verts[base]     = to_ndc_x(ax);
                    cursor_verts[base + 1] = to_ndc_y(ay);
                    cursor_verts[base + 2] = 0.0;
                    cursor_verts[base + 3] = to_ndc_x(bx);
                    cursor_verts[base + 4] = to_ndc_y(by);
                    cursor_verts[base + 5] = 0.0;
                    cursor_verts[base + 6] = to_ndc_x(ccx);
                    cursor_verts[base + 7] = to_ndc_y(ccy);
                    cursor_verts[base + 8] = 0.0;
                }
                let cursor_vb = self.device.new_buffer_with_data(
                    cursor_verts.as_ptr() as *const _,
                    (cursor_verts.len() * 4) as u64,
                    MTLResourceOptions::CPUCacheModeDefaultCache,
                );
                let identity: [f32; 16] = [
                    1.0, 0.0, 0.0, 0.0,
                    0.0, 1.0, 0.0, 0.0,
                    0.0, 0.0, 1.0, 0.0,
                    0.0, 0.0, 0.0, 1.0,
                ];
                // dark outline (offset by 1px)
                let outline_color: [f32; 4] = [0.12, 0.12, 0.12, 1.0];
                let mut outline_verts = cursor_verts;
                let ox = 1.0 / w * 2.0;
                let oy = 1.0 / h * 2.0;
                for i in 0..15 {
                    outline_verts[i * 3] += ox;
                    outline_verts[i * 3 + 1] -= oy;
                }
                let outline_vb = self.device.new_buffer_with_data(
                    outline_verts.as_ptr() as *const _,
                    (outline_verts.len() * 4) as u64,
                    MTLResourceOptions::CPUCacheModeDefaultCache,
                );
                encoder.set_render_pipeline_state(&self.pipeline);
                encoder.set_depth_stencil_state(&self.depth_state);
                encoder.set_vertex_bytes(1, 64, identity.as_ptr() as *const _);
                encoder.set_fragment_bytes(0, 16, outline_color.as_ptr() as *const _);
                encoder.set_vertex_buffer(0, Some(&outline_vb), 0);
                encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, 15);
                // white body
                let body_color: [f32; 4] = [0.94, 0.94, 0.96, 1.0];
                encoder.set_fragment_bytes(0, 16, body_color.as_ptr() as *const _);
                encoder.set_vertex_buffer(0, Some(&cursor_vb), 0);
                encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, 15);
            }

            encoder.end_encoding();
            cmd_buf.present_drawable(drawable);
            cmd_buf.commit();
            cmd_buf.wait_until_completed();
        });
    }
}

const SHADER_SRC: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexOut {
    float4 position [[position]];
    float3 obj_pos;
};

// Gradient params: [type, stop_count, x0, y0, x1, y1, pad, pad,
//                   stop0_offset, stop0_r, stop0_g, stop0_b, stop0_a, ...]
// Total: 8 + 5*8 = 48 floats max
struct GradientParams {
    float type_and_count[2]; // [0]=type, [1]=stop_count
    float line[4];           // x0, y0, x1, y1
    float pad[2];
    float stops[40];         // 8 stops * 5 floats (offset, r, g, b, a)
};

vertex VertexOut vertex_main(
    const device packed_float3* positions [[buffer(0)]],
    constant float4x4& mvp [[buffer(1)]],
    uint vid [[vertex_id]]
) {
    VertexOut out;
    float3 pos = positions[vid];
    out.position = float4(pos, 1.0) * mvp;
    out.obj_pos = pos;
    return out;
}

fragment float4 fragment_main(
    VertexOut in [[stage_in]],
    constant float4& color [[buffer(0)]]
) {
    return color;
}

// ─── Textured material path ─────────────────────────────────────
// Vertex layout: POSITION f32x3 + UV f32x2 (stride 20).
struct TexturedVertexOut {
    float4 position [[position]];
    float2 uv;
};

vertex TexturedVertexOut vertex_textured(
    const device float* verts [[buffer(0)]],
    constant float4x4& mvp [[buffer(1)]],
    uint vid [[vertex_id]]
) {
    TexturedVertexOut out;
    uint base = vid * 5;
    float3 pos = float3(verts[base+0], verts[base+1], verts[base+2]);
    out.position = float4(pos, 1.0) * mvp;
    out.uv = float2(verts[base+3], verts[base+4]);
    return out;
}

fragment float4 fragment_textured(
    TexturedVertexOut in [[stage_in]],
    texture2d<float> albedo [[texture(0)]],
    constant float4& tint [[buffer(0)]]
) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    return albedo.sample(s, in.uv) * tint;
}

fragment float4 fragment_gradient(
    VertexOut in [[stage_in]],
    constant GradientParams& grad [[buffer(0)]]
) {
    int stop_count = int(grad.type_and_count[1]);
    if (stop_count < 2) return float4(1.0);

    float2 p0 = float2(grad.line[0], grad.line[1]);
    float2 p1 = float2(grad.line[2], grad.line[3]);
    float2 d = p1 - p0;
    float len2 = dot(d, d);
    float t = 0.0;
    if (len2 > 0.0001) {
        t = dot(float2(in.obj_pos.x, in.obj_pos.y) - p0, d) / len2;
    }
    t = clamp(t, 0.0, 1.0);

    // find surrounding stops and interpolate
    float4 result = float4(1.0);
    for (int i = 0; i < stop_count - 1; i++) {
        float off0 = grad.stops[i * 5];
        float off1 = grad.stops[(i + 1) * 5];
        if (t >= off0 && t <= off1) {
            float f = (off1 > off0) ? (t - off0) / (off1 - off0) : 0.0;
            float4 c0 = float4(grad.stops[i*5+1], grad.stops[i*5+2], grad.stops[i*5+3], grad.stops[i*5+4]);
            float4 c1 = float4(grad.stops[(i+1)*5+1], grad.stops[(i+1)*5+2], grad.stops[(i+1)*5+3], grad.stops[(i+1)*5+4]);
            result = mix(c0, c1, f);
            break;
        }
    }
    // before first stop
    if (t < grad.stops[0]) {
        result = float4(grad.stops[1], grad.stops[2], grad.stops[3], grad.stops[4]);
    }
    // after last stop
    if (t > grad.stops[(stop_count-1)*5]) {
        int last = (stop_count-1)*5;
        result = float4(grad.stops[last+1], grad.stops[last+2], grad.stops[last+3], grad.stops[last+4]);
    }
    return result;
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
    CubicEntry stack[16];

    // phase 1: flatten all curves into contour buffer, record subpath starts
    int subpath_starts[64];  // max 64 subpaths per path
    int num_subpaths = 0;

    for (uint i = 0; i < seg_count && contour_count < max_pts; i++) {
        uint seg_type = segments[i].type_and_pad & 0xFF;
        float2 sp0 = float2(segments[i].x0, segments[i].y0);
        float2 sp1 = float2(segments[i].x1, segments[i].y1);
        float2 sp2 = float2(segments[i].x2, segments[i].y2);

        switch (seg_type) {
            case 0: // MoveTo — start new subpath
                if (num_subpaths < 64) {
                    subpath_starts[num_subpaths++] = contour_count;
                }
                cursor = sp0;
                contour[contour_count++] = cursor;
                break;
            case 1:
                cursor = sp0;
                contour[contour_count++] = cursor;
                break;
            case 2: {
                float2 cp1 = cursor + (sp0 - cursor) * (2.0f / 3.0f);
                float2 cp2 = sp1 + (sp0 - sp1) * (2.0f / 3.0f);
                int top = 0;
                stack[top++] = {cursor, cp1, cp2, sp1};
                while (top > 0 && contour_count < max_pts) {
                    CubicEntry e = stack[--top];
                    float2 d1 = e.p1 - (e.p0 * 2.0 + e.p3) / 3.0;
                    float2 d2 = e.p2 - (e.p0 + e.p3 * 2.0) / 3.0;
                    if (max(dot(d1,d1), dot(d2,d2)) <= tol_sq) {
                        contour[contour_count++] = e.p3;
                    } else if (top < 15) {
                        float2 m01=(e.p0+e.p1)*0.5, m12=(e.p1+e.p2)*0.5, m23=(e.p2+e.p3)*0.5;
                        float2 m012=(m01+m12)*0.5, m123=(m12+m23)*0.5, mid=(m012+m123)*0.5;
                        stack[top++] = {mid, m123, m23, e.p3};
                        stack[top++] = {e.p0, m01, m012, mid};
                    } else { contour[contour_count++] = e.p3; }
                }
                cursor = sp1; break;
            }
            case 3: {
                int top = 0;
                stack[top++] = {cursor, sp0, sp1, sp2};
                while (top > 0 && contour_count < max_pts) {
                    CubicEntry e = stack[--top];
                    float2 d1 = e.p1 - (e.p0 * 2.0 + e.p3) / 3.0;
                    float2 d2 = e.p2 - (e.p0 + e.p3 * 2.0) / 3.0;
                    if (max(dot(d1,d1), dot(d2,d2)) <= tol_sq) {
                        contour[contour_count++] = e.p3;
                    } else if (top < 15) {
                        float2 m01=(e.p0+e.p1)*0.5, m12=(e.p1+e.p2)*0.5, m23=(e.p2+e.p3)*0.5;
                        float2 m012=(m01+m12)*0.5, m123=(m12+m23)*0.5, mid=(m012+m123)*0.5;
                        stack[top++] = {mid, m123, m23, e.p3};
                        stack[top++] = {e.p0, m01, m012, mid};
                    } else { contour[contour_count++] = e.p3; }
                }
                cursor = sp2; break;
            }
            case 5: // Close
                if (num_subpaths > 0 && contour_count > subpath_starts[num_subpaths-1]) {
                    contour[contour_count++] = contour[subpath_starts[num_subpaths-1]];
                }
                break;
        }
    }

    if (contour_count < 3 || num_subpaths == 0) return;

    // phase 2: fan triangulate each subpath independently
    uint vc = 0;
    uint ic = 0;

    for (int s = 0; s < num_subpaths; s++) {
        int start = subpath_starts[s];
        int end = (s + 1 < num_subpaths) ? subpath_starts[s + 1] : contour_count;
        int n = end - start;
        if (n < 3) continue;

        ushort base = ushort(vc);
        for (int j = start; j < end; j++) {
            out_vertices[vc++] = packed_float3{contour[j].x, contour[j].y, 0.5};
        }
        for (int j = 1; j < n - 1; j++) {
            out_indices[ic++] = base;
            out_indices[ic++] = ushort(base + j);
            out_indices[ic++] = ushort(base + j + 1);
        }
    }

    atomic_store_explicit(&counters[0], vc, memory_order_relaxed);
    atomic_store_explicit(&counters[1], ic, memory_order_relaxed);
}
"#;

/// Ortho projection: top-left origin in logical pixels, Y down,
/// maps (0..w, 0..h, *) into Metal clip space with z = 0.5 (overlay
/// always passes depth). Used for FLAG_OVERLAY render items.
fn ortho_pixel_to_clip(w: f32, h: f32) -> [f32; 16] {
    [
         2.0 / w,  0.0,      0.0, 0.0,
         0.0,     -2.0 / h,  0.0, 0.0,
         0.0,      0.0,      1.0, 0.0,
        -1.0,      1.0,      0.5, 1.0,
    ]
}

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

// ── B2: per-window framebuffer management ───────────────────────────
impl MetalRenderer {
    /// Draw each render item in the scene onto `encoder`. Used by both
    /// the screen pass and (later) per-window FBO passes; the encoder
    /// must already be configured with color + stencil attachments.
    /// `target_w/h` are physical pixels; `scale` is the device-pixel
    /// ratio used to convert clip rects from logical to physical.
    fn draw_items(&self,
                  encoder: &RenderCommandEncoderRef,
                  scene: &SceneGraph,
                  cas: &CasStore,
                  vp: &[f32; 16],
                  ortho_vp: &[f32; 16],
                  target_w: u32,
                  target_h: u32,
                  scale: f32) {
        encoder.set_render_pipeline_state(&self.pipeline);
        let full_scissor = MTLScissorRect {
            x: 0, y: 0, width: target_w as u64, height: target_h as u64,
        };
        for (idx, item) in scene.render_list().iter().enumerate() {
            let item_vp = if item.flags & 0x01 != 0 { ortho_vp } else { vp };
            let mvp = mat4_mul(&item.world_matrix, item_vp);

            if let Some(clip) = &item.clip_rect {
                let sx = ((clip[0] * scale).max(0.0) as u64).min(target_w as u64);
                let sy = ((clip[1] * scale).max(0.0) as u64).min(target_h as u64);
                let sw = ((clip[2] * scale).max(1.0) as u64).min(target_w as u64 - sx);
                let sh = ((clip[3] * scale).max(1.0) as u64).min(target_h as u64 - sy);
                encoder.set_scissor_rect(MTLScissorRect { x: sx, y: sy, width: sw, height: sh });
            } else {
                encoder.set_scissor_rect(full_scissor);
            }

            let parsed_mat = if item.material != NULL_HASH {
                if let Some(data) = cas.load(&item.material) {
                    if let Some(NodeData::Material(mat)) = NodeData::parse(data) {
                        Some(mat)
                    } else { None }
                } else {
                    log::warn!("render[{}]: material {:02x}{:02x}.. not in CAS", idx, item.material[0], item.material[1]);
                    None
                }
            } else { None };

            let use_gradient = parsed_mat.as_ref().map_or(false, |m| m.has_gradient() && m.gradient_stop_count >= 2);
            let color = parsed_mat.as_ref().map_or([0.8f32, 0.8, 0.8, 1.0], |m| rgba8_to_float(m.base_color));

            let textured_tex: Option<Texture> = parsed_mat.as_ref()
                .filter(|m| m.albedo_tex != NULL_HASH)
                .and_then(|m| self.texture_cache.get(&m.albedo_tex).cloned());
            let use_textured = textured_tex.is_some();

            if item.mesh == NULL_HASH { continue; }
            let mesh_data = match cas.load(&item.mesh) {
                Some(d) => d,
                None => { log::warn!("render[{}]: mesh {:02x}{:02x}.. not in CAS", idx, item.mesh[0], item.mesh[1]); continue; }
            };
            let mesh = match NodeData::parse(mesh_data) {
                Some(NodeData::Mesh(m)) => m,
                _ => { log::warn!("render[{}]: mesh blob didn't parse as Mesh", idx); continue; }
            };
            let verts_raw = match cas.load(&mesh.vertex_data) {
                Some(d) => d,
                None => { log::warn!("render[{}]: vertex_data {:02x}{:02x}.. not in CAS", idx, mesh.vertex_data[0], mesh.vertex_data[1]); continue; }
            };
            let verts = if verts_raw.len() > 8 { &verts_raw[8..] } else { verts_raw };
            let vb = self.device.new_buffer_with_data(
                verts.as_ptr() as *const _,
                verts.len() as u64,
                MTLResourceOptions::CPUCacheModeDefaultCache,
            );

            let mvp_t = transpose(&mvp);
            encoder.set_vertex_bytes(1, std::mem::size_of::<[f32; 16]>() as u64, mvp_t.as_ptr() as *const _);
            if use_textured {
                encoder.set_render_pipeline_state(&self.textured_pipeline);
                encoder.set_fragment_texture(0, Some(textured_tex.as_ref().unwrap()));
                encoder.set_fragment_bytes(0, std::mem::size_of::<[f32; 4]>() as u64, color.as_ptr() as *const _);
            } else if use_gradient {
                let mat = parsed_mat.as_ref().unwrap();
                let mut grad_buf = [0.0f32; 48];
                grad_buf[0] = mat.gradient_type as f32;
                grad_buf[1] = mat.gradient_stop_count as f32;
                grad_buf[2] = mat.gradient_x0;
                grad_buf[3] = mat.gradient_y0;
                grad_buf[4] = mat.gradient_x1;
                grad_buf[5] = mat.gradient_y1;
                for i in 0..mat.gradient_stop_count as usize {
                    let (off, rgba) = mat.gradient_stops[i];
                    let c = rgba8_to_float(rgba);
                    grad_buf[8 + i * 5]     = off;
                    grad_buf[8 + i * 5 + 1] = c[0];
                    grad_buf[8 + i * 5 + 2] = c[1];
                    grad_buf[8 + i * 5 + 3] = c[2];
                    grad_buf[8 + i * 5 + 4] = c[3];
                }
                encoder.set_render_pipeline_state(&self.gradient_pipeline);
                encoder.set_fragment_bytes(0, std::mem::size_of::<[f32; 48]>() as u64, grad_buf.as_ptr() as *const _);
            } else {
                encoder.set_render_pipeline_state(&self.pipeline);
                encoder.set_fragment_bytes(0, std::mem::size_of::<[f32; 4]>() as u64, color.as_ptr() as *const _);
            }

            encoder.set_vertex_buffer(0, Some(&vb), 0);

            let has_indices = mesh.index_count > 0 && mesh.index_data != NULL_HASH;
            let ib = if has_indices {
                cas.load(&mesh.index_data).map(|idx_raw| {
                    let indices = if idx_raw.len() > 8 { &idx_raw[8..] } else { idx_raw };
                    self.device.new_buffer_with_data(
                        indices.as_ptr() as *const _,
                        indices.len() as u64,
                        MTLResourceOptions::CPUCacheModeDefaultCache,
                    )
                })
            } else { None };
            let idx_type = if mesh.index_format == 1 { MTLIndexType::UInt32 } else { MTLIndexType::UInt16 };

            if item.stencil_fill {
                encoder.set_render_pipeline_state(&self.stencil_fill_pipeline);
                encoder.set_depth_stencil_state(&self.stencil_fill_ds);
                encoder.set_stencil_reference_value(0);
                if let Some(ref ib) = ib {
                    encoder.draw_indexed_primitives(MTLPrimitiveType::Triangle, mesh.index_count as u64, idx_type, ib, 0);
                } else {
                    encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, mesh.vertex_count as u64);
                }
                if use_gradient {
                    encoder.set_render_pipeline_state(&self.gradient_pipeline);
                } else {
                    encoder.set_render_pipeline_state(&self.pipeline);
                }
                encoder.set_depth_stencil_state(&self.stencil_cover_ds);
                encoder.set_stencil_reference_value(0);
                if let Some(ref ib) = ib {
                    encoder.draw_indexed_primitives(MTLPrimitiveType::Triangle, mesh.index_count as u64, idx_type, ib, 0);
                } else {
                    encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, mesh.vertex_count as u64);
                }
                encoder.set_depth_stencil_state(&self.depth_state);
            } else {
                if let Some(ref ib) = ib {
                    encoder.draw_indexed_primitives(MTLPrimitiveType::Triangle, mesh.index_count as u64, idx_type, ib, 0);
                } else {
                    encoder.draw_primitives(MTLPrimitiveType::Triangle, 0, mesh.vertex_count as u64);
                }
            }
        }
    }

    /// Ensure the FBO for this window exists at the given physical
    /// pixel size. Re-allocates color+stencil textures if the size
    /// changed and the previous reallocation was at least
    /// `RESIZE_DEBOUNCE_MS` ago — otherwise leaves the FBO at its
    /// last size (the next sync pass will pick up the now-stable
    /// final size). This keeps a cursor-driven drag from burning
    /// Metal textures at the input event rate.
    pub fn ensure_window_fbo(&mut self, id: u16, width: u32, height: u32) {
        const RESIZE_DEBOUNCE_MS: u128 = 33; // ~30 Hz
        if let Some(fbo) = self.window_fbos.get(&id) {
            if fbo.width == width && fbo.height == height { return; }
            if fbo.last_resized.elapsed().as_millis() < RESIZE_DEBOUNCE_MS {
                return;
            }
        }
        let color_desc = TextureDescriptor::new();
        color_desc.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        color_desc.set_width(width as u64);
        color_desc.set_height(height as u64);
        color_desc.set_storage_mode(MTLStorageMode::Private);
        color_desc.set_usage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        let color = self.device.new_texture(&color_desc);

        let stencil_desc = TextureDescriptor::new();
        stencil_desc.set_pixel_format(MTLPixelFormat::Stencil8);
        stencil_desc.set_width(width as u64);
        stencil_desc.set_height(height as u64);
        stencil_desc.set_storage_mode(MTLStorageMode::Private);
        stencil_desc.set_usage(MTLTextureUsage::RenderTarget);
        let stencil = self.device.new_texture(&stencil_desc);

        self.window_fbos.insert(id, WindowFbo {
            color, stencil, width, height,
            last_resized: std::time::Instant::now(),
        });
        log::info!("FBO: window {} allocated {}x{}", id, width, height);
    }

    /// Drop a window's FBO (called when the window is destroyed).
    pub fn drop_window_fbo(&mut self, id: u16) {
        self.window_fbos.remove(&id);
    }

    /// Render one window's scene into its offscreen FBO. Uses the
    /// window's own camera (perspective if scene.camera() is set,
    /// otherwise a sensible default). Runs in its own command buffer
    /// so the call is self-contained.
    pub fn render_window_to_fbo(&mut self, id: u16, scene: &SceneGraph, cas: &CasStore) {
        // Texture-cache pre-pass for this window's materials.
        for item in scene.render_list().iter() {
            if item.material == NULL_HASH { continue; }
            if let Some(data) = cas.load(&item.material) {
                if let Some(NodeData::Material(m)) = NodeData::parse(data) {
                    if m.albedo_tex != NULL_HASH {
                        let _ = self.ensure_texture(cas, &m.albedo_tex);
                    }
                }
            }
        }

        let Some(fbo) = self.window_fbos.get(&id) else { return; };
        let color_tex = fbo.color.clone();
        let stencil_tex = fbo.stencil.clone();
        let fbo_w = fbo.width;
        let fbo_h = fbo.height;

        objc2_autorelease(|_| {
            let desc = RenderPassDescriptor::new();
            let color = desc.color_attachments().object_at(0).unwrap();
            color.set_texture(Some(&color_tex));
            color.set_load_action(MTLLoadAction::Clear);
            color.set_clear_color(MTLClearColor::new(0.0, 0.0, 0.0, 0.0));
            color.set_store_action(MTLStoreAction::Store);

            let stencil_att = desc.stencil_attachment().unwrap();
            stencil_att.set_texture(Some(&stencil_tex));
            stencil_att.set_load_action(MTLLoadAction::Clear);
            stencil_att.set_clear_stencil(0);
            stencil_att.set_store_action(MTLStoreAction::DontCare);

            let cmd_buf = self.queue.new_command_buffer();
            let encoder = cmd_buf.new_render_command_encoder(desc);

            let aspect = fbo_w as f32 / fbo_h.max(1) as f32;
            let vp = if let Some((cam, xform)) = scene.camera(cas) {
                let view = invert_rigid(&xform.matrix);
                let proj = perspective(cam.fov_y, cam.aspect_ratio, cam.near_plane, cam.far_plane);
                mat4_mul(&view, &proj)
            } else {
                let view = look_at([0.0, 2.0, 5.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0]);
                let proj = perspective(std::f32::consts::FRAC_PI_4, aspect, 0.1, 100.0);
                mat4_mul(&view, &proj)
            };
            let ortho_vp = ortho_pixel_to_clip(fbo_w as f32, fbo_h as f32);

            self.draw_items(encoder, scene, cas, &vp, &ortho_vp,
                fbo_w, fbo_h, self.scale_factor as f32);

            encoder.end_encoding();
            cmd_buf.commit();
            cmd_buf.wait_until_completed();
        });
    }
}
