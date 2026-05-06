//! Headless Vulkan renderer — no swapchain, no surface, no window.
//!
//! Renders into an off-screen `vk::Image` at a configurable size,
//! then copies the rendered pixels into a host-visible buffer that
//! the consumer (frescod, conformance tests, lavapipe-in-QEMU CI)
//! can read back.
//!
//! This is the **production-target** Vulkan path for Atrium:
//! - In QEMU + lavapipe, there's no display surface; we render to a
//!   buffer and dump to PNG (CI lane) or hand to frescod's
//!   atrium-gpu-rs scanout BO (production path).
//! - On real FreeBSD HW (D5+), frescod owns the display via the
//!   Atrium GPU ABI; the rendered image gets memcpy'd into the
//!   scanout BO before page-flip.
//! - On macOS dev with venus passthrough, headless still works —
//!   the guest's lavapipe or venus-host drivers don't need a WSI
//!   surface to produce pixels.
//!
//! The windowed `Renderer` (in `renderer.rs`) is kept for the
//! archived POC reference and stays useful for the macOS-host
//! dev workflow when running a Vulkan demo directly. Production
//! frescod uses this headless path exclusively.

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use ash::vk;

use crate::frame::{GlyphInstance, GlyphRunNode, OpFrameResources, PathNode, SceneNode};
use crate::pipeline::{op_kind, OpPipelines};
use crate::renderer::TextureBatch;
use crate::resource::{self, Resource, UploadRequest};

/// One glyph_run dispatch batched by atlas slot. The renderer issues
/// one compute + one draw cycle per batch; nodes within the batch all
/// reference the same atlas. Mirrors `TextureBatch`'s shape but with
/// the per-glyph-instance data carried alongside the SceneNode list.
#[derive(Clone, Debug)]
pub struct GlyphRunBatch {
    /// Atlas slot id; the per-slot descriptor set is allocated lazily
    /// (same pattern as the texture op).
    pub atlas_slot_id: u32,
    /// Scene nodes for this batch. Each node's `meta[1]` (glyph_offset)
    /// is into THIS batch's `glyphs` buffer (the renderer fixes up
    /// offsets when assembling the per-frame storage).
    pub nodes:  Vec<GlyphRunNode>,
    /// Glyph instances referenced by the nodes' meta offsets.
    pub glyphs: Vec<GlyphInstance>,
}

/// Op-ids for atrium-core's bundled ops. Hardcoded for now; per the
/// spec §3.4 closed registry, these are pinned. Mirrors the constants
/// in `fresco-protocol::scene_ops::*`.
const OP_ID_RECT:            u32 = 0x1000;
const OP_ID_TEXTURE:         u32 = 0x1001;
const OP_ID_PATH:            u32 = 0x1002;
const OP_ID_TEXT_GLYPH_RUN:  u32 = 0x2000;

/// Atrium teal — the default clear color, same value as the windowed
/// `Renderer`. Recognisable so smoke-tests can confirm the render-pass
/// path is alive (vs an all-black image which could mean "render pass
/// didn't run").
const CLEAR_COLOR: [f32; 4] = [0.04, 0.50, 0.55, 1.0];

/// Pre-recorded handles + count for one op's contribution to a frame.
/// Same shape as the windowed Renderer's DrawPlan.
struct DrawPlan {
    compute_pipeline: vk::Pipeline,
    compute_layout:   vk::PipelineLayout,
    compute_set:      vk::DescriptorSet,
    render_pipeline:  vk::Pipeline,
    render_layout:    vk::PipelineLayout,
    render_set:       vk::DescriptorSet,
    counter_buf:      vk::Buffer,
    instance_buf:     vk::Buffer,
    /// Compute-dispatch input count (one work-item per scene node).
    n:                u32,
    /// Total instance count for the indirect draw. Equal to `n` for
    /// ops that expand 1 node → 1 instance (Rect, Texture, Path);
    /// for GlyphRun, equal to the total glyph count across all nodes
    /// in this batch (one node expands to N glyphs).
    draw_instances:   u32,
}

const COLOR_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// A headless Vulkan device that renders into a single off-screen
/// color image. The image is **device-local** (fast for GPU writes);
/// pixel readback goes through a host-visible staging buffer.
///
/// Single-frame-in-flight: each `clear_and_readback` does a full
/// submit + queue-wait + buffer copy. Production rendering will use
/// the same instance/device but call into the existing render-pass
/// machinery in `renderer.rs` (extracted to shared helpers in a
/// follow-up).
pub struct HeadlessRenderer {
    /* Lifetime: dropped LIFO. */
    _entry:    ash::Entry,
    instance:  ash::Instance,

    physical_device: vk::PhysicalDevice,
    #[allow(dead_code)]
    queue_family:    u32,
    device:          ash::Device,
    queue:           vk::Queue,

    extent:          vk::Extent2D,

    /* Off-screen render target: device-local image we render into. */
    color_image:     vk::Image,
    color_memory:    vk::DeviceMemory,
    color_view:      vk::ImageView,

    /* Render-pass scaffolding so the off-screen image can be a real
     * color attachment (begin/end render pass + cmd_draw inside).
     * Same shape as `renderer.rs::create_render_pass`, minus the
     * PRESENT_SRC final layout (we transition to TRANSFER_SRC manually
     * before the readback copy). */
    render_pass: vk::RenderPass,
    framebuffer: vk::Framebuffer,

    /* Host-visible staging buffer for readback. */
    readback_buffer: vk::Buffer,
    readback_memory: vk::DeviceMemory,
    readback_size:   vk::DeviceSize,

    cmd_pool:   vk::CommandPool,
    cmd_buffer: vk::CommandBuffer,
    fence:      vk::Fence,

    /* Bundle dispatch state. Populated by `load_bundle()`; consumed
     * per-frame by `render_to_buffer()`. */
    op_pipelines: HashMap<u32, OpPipelines>,
    op_frames:    HashMap<u32, OpFrameResources>,
    resources:    HashMap<u32, Resource>,

    /* Staging buffers for the next frame. Caller pushes via
     * set_rect_nodes / set_texture_batches; render_to_buffer drains. */
    rect_nodes:      Vec<SceneNode>,
    texture_batches: Vec<TextureBatch>,
    path_nodes:      Vec<PathNode>,
    glyph_run_batches: Vec<GlyphRunBatch>,

    /// Last instance count we logged; suppresses spam.
    last_logged_count: u32,
}

impl HeadlessRenderer {
    /// Create a headless renderer that targets an `extent.width ×
    /// extent.height` BGRA8 image. Picks the first physical device
    /// that exposes a graphics queue. No surface / present support.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }
            .context("ash::Entry::load — install vulkan-loader (lavapipe/venus/vendor)")?;

        let instance = create_instance(&entry)?;
        let (physical_device, queue_family) = pick_physical_device(&instance)?;
        log_physical_device(&instance, physical_device);
        let device = create_device(&instance, physical_device, queue_family)?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let extent = vk::Extent2D { width, height };

        /* Allocate render target image. */
        let mem_props = unsafe {
            instance.get_physical_device_memory_properties(physical_device)
        };
        let (color_image, color_memory) = create_color_image(
            &device, &mem_props, extent, COLOR_FORMAT)?;
        let color_view = create_image_view(&device, color_image, COLOR_FORMAT)?;

        let render_pass = create_render_pass(&device, COLOR_FORMAT)?;
        let framebuffer = create_framebuffer(
            &device, render_pass, color_view, extent)?;

        /* Allocate readback buffer. 4 bytes/pixel for BGRA8. */
        let readback_size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;
        let (readback_buffer, readback_memory) = create_buffer(
            &device, &mem_props, readback_size,
            vk::BufferUsageFlags::TRANSFER_DST,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;

        let (cmd_pool, cmd_buffer) = create_command_pool(&device, queue_family)?;

        let fence_info = vk::FenceCreateInfo::default();
        let fence = unsafe { device.create_fence(&fence_info, None) }
            .context("create_fence")?;

        Ok(Self {
            _entry: entry, instance,
            physical_device, queue_family,
            device, queue,
            extent,
            color_image, color_memory, color_view,
            render_pass, framebuffer,
            readback_buffer, readback_memory, readback_size,
            cmd_pool, cmd_buffer, fence,
            op_pipelines: HashMap::new(),
            op_frames:    HashMap::new(),
            resources:    HashMap::new(),
            rect_nodes:   Vec::new(),
            texture_batches: Vec::new(),
            path_nodes:   Vec::new(),
            glyph_run_batches: Vec::new(),
            last_logged_count: u32::MAX,
        })
    }

    /// AOT-compile every op in `bundle_path` and register by op-id.
    /// Same machinery as the windowed Renderer's load_bundle — pipelines
    /// + per-op frame resources land here so the per-frame recording
    /// (M2.4c) can dispatch by op-id.
    pub fn load_bundle(&mut self, bundle_path: &Path) -> Result<()> {
        let bundle = fresco_bundle::Bundle::load(bundle_path)
            .with_context(|| format!("load bundle {}", bundle_path.display()))?;
        log::info!("bundle '{}' v{}: {} op(s)",
            bundle.manifest.name,
            bundle.manifest.version,
            bundle.manifest.ops.len());

        let max_instances = bundle.manifest.gpu_resources
            .get("max_instances")
            .and_then(|v| v.as_u64())
            .unwrap_or(65_536) as u32;

        for op in bundle.ops() {
            let pipelines = OpPipelines::create(
                &self.device, op, self.render_pass, self.extent)?;
            let frame = OpFrameResources::create(
                &self.device, &self.instance, self.physical_device,
                op_kind(op.id),
                pipelines.compute_set_layout,
                pipelines.render_set_layout,
                max_instances)?;
            frame.write_screen(self.extent.width, self.extent.height);
            log::info!("  op {} '{}' → pipelines + buffers (cap {})",
                op.id, op.name, max_instances);

            if self.op_pipelines.contains_key(&op.id) {
                log::warn!("op-id {} collision: replacing existing entry", op.id);
                let prev = self.op_pipelines.remove(&op.id).unwrap();
                let prev_frame = self.op_frames.remove(&op.id);
                unsafe {
                    self.device.device_wait_idle().ok();
                    prev.destroy(&self.device);
                    if let Some(f) = prev_frame { f.destroy(&self.device); }
                }
            }
            self.op_pipelines.insert(op.id, pipelines);
            self.op_frames.insert(op.id, frame);
        }
        Ok(())
    }

    /// Number of ops with compiled pipelines (post-`load_bundle`).
    pub fn op_count(&self) -> usize { self.op_pipelines.len() }

    /// Stage rect-op nodes for the next `render_to_buffer` call.
    pub fn set_rect_nodes(&mut self, nodes: Vec<SceneNode>) {
        self.rect_nodes = nodes;
    }

    /// Stage texture-op batches (one per slot) for the next render.
    pub fn set_texture_batches(&mut self, batches: Vec<TextureBatch>) {
        self.texture_batches = batches;
    }

    /// Stage path-op (rotated quad) nodes for the next render.
    pub fn set_path_nodes(&mut self, nodes: Vec<PathNode>) {
        self.path_nodes = nodes;
    }

    /// Stage glyph_run batches for the next render. One batch per
    /// distinct atlas slot; the renderer dispatches one compute +
    /// one indirect draw cycle per batch.
    pub fn set_glyph_run_batches(&mut self, batches: Vec<GlyphRunBatch>) {
        self.glyph_run_batches = batches;
    }

    /// SLOT_CLEAR / texture replacement notification: drop any cached
    /// per-slot descriptor that referenced the old ImageView. Applies
    /// to both Texture and GlyphRun ops since they share the per-slot
    /// descriptor pattern.
    pub fn invalidate_slot(&mut self, slot_id: u32) {
        if let Some(f) = self.op_frames.get_mut(&OP_ID_TEXTURE) {
            f.drop_texture_slot(slot_id);
        }
        if let Some(f) = self.op_frames.get_mut(&OP_ID_TEXT_GLYPH_RUN) {
            f.drop_texture_slot(slot_id);
        }
    }

    /// Drain pending CAS upload + clear requests. Called before
    /// `render_to_buffer`.
    pub fn process_uploads(
        &mut self,
        uploads: Vec<UploadRequest>,
        clears:  Vec<u32>,
    ) -> Result<()> {
        for slot_id in clears {
            if let Some(r) = self.resources.remove(&slot_id) {
                unsafe {
                    self.device.device_wait_idle().ok();
                    match r {
                        Resource::Texture(t) => t.destroy(&self.device),
                    }
                }
                self.invalidate_slot(slot_id);
                log::info!("freed resource slot={}", slot_id);
            }
        }
        for req in uploads {
            match req {
                UploadRequest::Texture { slot_id, bytes, width, height, format } => {
                    let tex = resource::upload_texture(
                        &self.device, self.queue, self.cmd_pool,
                        self.physical_device, &self.instance,
                        &bytes, width, height, format,
                    )?;
                    if let Some(prev) = self.resources.remove(&slot_id) {
                        unsafe {
                            self.device.device_wait_idle().ok();
                            match prev {
                                Resource::Texture(t) => t.destroy(&self.device),
                            }
                        }
                    }
                    self.invalidate_slot(slot_id);
                    self.resources.insert(slot_id, Resource::Texture(tex));
                    log::info!("uploaded texture slot={} {}x{} {}B",
                        slot_id, width, height, bytes.len());
                }
                UploadRequest::TextureRegion {
                    slot_id, bytes, dst_x, dst_y, width, height,
                } => {
                    let Some(Resource::Texture(t)) = self.resources.get(&slot_id) else {
                        log::warn!("TextureRegion for slot={} but no image bound; \
                                    drop", slot_id);
                        continue;
                    };
                    let image = t.image;
                    resource::upload_texture_region(
                        &self.device, self.queue, self.cmd_pool,
                        self.physical_device, &self.instance,
                        image, &bytes, dst_x, dst_y, width, height,
                    )?;
                    log::info!("patched slot={} ({},{} {}x{}) {}B",
                        slot_id, dst_x, dst_y, width, height, bytes.len());
                }
            }
        }
        Ok(())
    }

    /// Render one frame into the off-screen image, copy pixels into
    /// the readback buffer, return. After this call, `read_pixels` /
    /// `read_pixels_vec` produce the rendered frame's pixels.
    ///
    /// Walks staged rect/texture data through the same compute +
    /// indirect-draw machinery the windowed Renderer uses; the only
    /// substantive difference is the post-draw image-layout transition
    /// + cmd_copy_image_to_buffer, replacing the windowed path's
    /// acquire/present.
    pub fn render_to_buffer(&mut self) -> Result<()> {
        unsafe {
            /* Pre-record: build DrawPlan list. Same logic as windowed
             * Renderer. */
            let mut plans: Vec<DrawPlan> = Vec::new();

            if let (Some(pipes), Some(frame)) =
                (self.op_pipelines.get(&OP_ID_RECT),
                 self.op_frames.get(&OP_ID_RECT))
            {
                let n = frame.write_scene(&self.rect_nodes);
                if n < self.rect_nodes.len() as u32 {
                    log::warn!("rect nodes {} > cap {}; dropping excess",
                        self.rect_nodes.len(), frame.max_instances);
                }
                plans.push(DrawPlan {
                    compute_pipeline:  pipes.compute_pipeline,
                    compute_layout:    pipes.compute_pipe_layout,
                    compute_set:       frame.descriptor_set,
                    render_pipeline:   pipes.render_pipeline,
                    render_layout:     pipes.render_pipe_layout,
                    render_set:        frame.render_set
                        .expect("rect frame missing single render_set"),
                    counter_buf:       frame.counter_buf,
                    instance_buf:      frame.instance_buf,
                    n,
                    draw_instances:    n,
                });
            }

            /* Path-op plan: same shape as rect, just a different
             * (compute_pipeline, render_pipeline, frame). */
            if let (Some(pipes), Some(frame)) =
                (self.op_pipelines.get(&OP_ID_PATH),
                 self.op_frames.get(&OP_ID_PATH))
            {
                let n = frame.write_path_scene(&self.path_nodes);
                if n < self.path_nodes.len() as u32 {
                    log::warn!("path nodes {} > cap {}; dropping excess",
                        self.path_nodes.len(), frame.max_instances);
                }
                plans.push(DrawPlan {
                    compute_pipeline:  pipes.compute_pipeline,
                    compute_layout:    pipes.compute_pipe_layout,
                    compute_set:       frame.descriptor_set,
                    render_pipeline:   pipes.render_pipeline,
                    render_layout:     pipes.render_pipe_layout,
                    render_set:        frame.render_set
                        .expect("path frame missing single render_set"),
                    counter_buf:       frame.counter_buf,
                    instance_buf:      frame.instance_buf,
                    n,
                    draw_instances:    n,
                });
            }

            if let Some(batch) = self.texture_batches.first() {
                if self.texture_batches.len() > 1 {
                    log::warn!("only first texture batch rendered (POC limit; \
                                {} batches received)", self.texture_batches.len());
                }
                let view = self.resources.get(&batch.slot_id)
                    .map(|r| match r { Resource::Texture(t) => t.view });
                let pipes = self.op_pipelines.get(&OP_ID_TEXTURE);
                if let (Some(view), Some(pipes)) = (view, pipes) {
                    let device_ref = &self.device;
                    let frame = self.op_frames.get_mut(&OP_ID_TEXTURE)
                        .expect("texture op frame missing");
                    let n = frame.write_texture_scene(&batch.nodes);
                    let render_set = frame.ensure_texture_descriptor(
                        device_ref, batch.slot_id, view)?;
                    plans.push(DrawPlan {
                        compute_pipeline:  pipes.compute_pipeline,
                        compute_layout:    pipes.compute_pipe_layout,
                        compute_set:       frame.descriptor_set,
                        render_pipeline:   pipes.render_pipeline,
                        render_layout:     pipes.render_pipe_layout,
                        render_set,
                        counter_buf:       frame.counter_buf,
                        instance_buf:      frame.instance_buf,
                        n,
                        draw_instances:    n,
                    });
                } else if view.is_none() {
                    log::warn!("texture batch slot={} not bound; skipping",
                        batch.slot_id);
                }
            }

            /* Glyph_run plan: one batch per atlas slot. Each batch
             * carries (a) the SceneNode list (one per text run) and
             * (b) the per-glyph storage data. The per-batch path
             * writes both into the shared scene + glyphs buffers and
             * dispatches compute + indirect draw against the per-slot
             * atlas binding (lazily allocated, same pattern as
             * Texture). For now we render the first batch only;
             * multi-atlas support comes when atrium-text grows
             * multiple fonts. */
            if let Some(batch) = self.glyph_run_batches.first() {
                if self.glyph_run_batches.len() > 1 {
                    log::warn!("only first glyph_run batch rendered (POC \
                                limit; {} batches received)",
                        self.glyph_run_batches.len());
                }
                let view = self.resources.get(&batch.atlas_slot_id)
                    .map(|r| match r { Resource::Texture(t) => t.view });
                let pipes = self.op_pipelines.get(&OP_ID_TEXT_GLYPH_RUN);
                if let (Some(view), Some(pipes)) = (view, pipes) {
                    let device_ref = &self.device;
                    let frame = self.op_frames.get_mut(&OP_ID_TEXT_GLYPH_RUN)
                        .expect("glyph_run op frame missing");
                    /* Write per-frame glyphs storage and scene nodes.
                     * Caller has already placed correct meta[1] offsets
                     * in each scene node referencing this batch's
                     * glyphs[] starting at index 0 (single-batch path). */
                    let _ng = frame.write_glyphs(&batch.glyphs);
                    let n = frame.write_glyph_run_scene(&batch.nodes);
                    if n < batch.nodes.len() as u32 {
                        log::warn!("glyph_run nodes {} > cap {}; \
                                    dropping excess",
                            batch.nodes.len(), frame.max_instances);
                    }
                    let render_set = frame.ensure_texture_descriptor(
                        device_ref, batch.atlas_slot_id, view)?;
                    plans.push(DrawPlan {
                        compute_pipeline:  pipes.compute_pipeline,
                        compute_layout:    pipes.compute_pipe_layout,
                        compute_set:       frame.descriptor_set,
                        render_pipeline:   pipes.render_pipeline,
                        render_layout:     pipes.render_pipe_layout,
                        render_set,
                        counter_buf:       frame.counter_buf,
                        instance_buf:      frame.instance_buf,
                        n,
                        draw_instances:    batch.glyphs.len() as u32,
                    });
                } else if view.is_none() {
                    log::warn!("glyph_run batch atlas_slot={} not bound; \
                                skipping", batch.atlas_slot_id);
                }
            }

            self.device.reset_command_buffer(
                self.cmd_buffer, vk::CommandBufferResetFlags::empty())?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device.begin_command_buffer(self.cmd_buffer, &begin)?;

            /* Compute passes: zero counter + dispatch traversal,
             * one cycle per plan. */
            for plan in &plans {
                self.device.cmd_fill_buffer(
                    self.cmd_buffer, plan.counter_buf, 0, vk::WHOLE_SIZE, 0);
                let counter_barrier = vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ
                                   | vk::AccessFlags::SHADER_WRITE)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .buffer(plan.counter_buf)
                    .offset(0).size(vk::WHOLE_SIZE);
                self.device.cmd_pipeline_barrier(
                    self.cmd_buffer,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[], &[counter_barrier], &[]);

                if plan.n > 0 {
                    self.device.cmd_bind_pipeline(
                        self.cmd_buffer,
                        vk::PipelineBindPoint::COMPUTE,
                        plan.compute_pipeline);
                    self.device.cmd_bind_descriptor_sets(
                        self.cmd_buffer,
                        vk::PipelineBindPoint::COMPUTE,
                        plan.compute_layout,
                        0, &[plan.compute_set], &[]);
                    let groups = (plan.n + 63) / 64;
                    self.device.cmd_dispatch(self.cmd_buffer, groups, 1, 1);
                }

                let post = [
                    vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                        .dst_access_mask(vk::AccessFlags::HOST_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .buffer(plan.counter_buf)
                        .offset(0).size(vk::WHOLE_SIZE),
                    vk::BufferMemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_READ)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .buffer(plan.instance_buf)
                        .offset(0).size(vk::WHOLE_SIZE),
                ];
                self.device.cmd_pipeline_barrier(
                    self.cmd_buffer,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::VERTEX_SHADER
                        | vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[], &post, &[]);
            }

            /* Render pass: clear + draws. */
            let clears = [vk::ClearValue {
                color: vk::ClearColorValue { float32: CLEAR_COLOR },
            }];
            let rp_begin = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: self.extent,
                })
                .clear_values(&clears);
            self.device.cmd_begin_render_pass(
                self.cmd_buffer, &rp_begin, vk::SubpassContents::INLINE);

            if !plans.is_empty() {
                let viewports = [vk::Viewport {
                    x: 0.0, y: 0.0,
                    width:  self.extent.width  as f32,
                    height: self.extent.height as f32,
                    min_depth: 0.0, max_depth: 1.0,
                }];
                let scissors = [vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: self.extent,
                }];
                self.device.cmd_set_viewport(self.cmd_buffer, 0, &viewports);
                self.device.cmd_set_scissor(self.cmd_buffer, 0, &scissors);
            }
            for plan in &plans {
                if plan.draw_instances == 0 { continue; }
                self.device.cmd_bind_pipeline(
                    self.cmd_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    plan.render_pipeline);
                self.device.cmd_bind_descriptor_sets(
                    self.cmd_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    plan.render_layout,
                    0, &[plan.render_set], &[]);
                self.device.cmd_draw(self.cmd_buffer, 4, plan.draw_instances, 0, 0);
            }
            self.device.cmd_end_render_pass(self.cmd_buffer);

            /* Post-render: image layout COLOR_ATTACHMENT_OPTIMAL →
             * TRANSFER_SRC_OPTIMAL, then copy to readback buffer.
             * Replaces the windowed path's acquire/present. */
            transition_image(
                &self.device, self.cmd_buffer, self.color_image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::AccessFlags::TRANSFER_READ,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER);

            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0).buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0, base_array_layer: 0, layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: self.extent.width, height: self.extent.height, depth: 1,
                });
            self.device.cmd_copy_image_to_buffer(
                self.cmd_buffer, self.color_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback_buffer, &[region]);

            self.device.end_command_buffer(self.cmd_buffer)?;

            self.device.reset_fences(&[self.fence])?;
            let submit = vk::SubmitInfo::default()
                .command_buffers(std::slice::from_ref(&self.cmd_buffer));
            self.device.queue_submit(self.queue, &[submit], self.fence)?;
            self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;

            /* Counter readback (verification logging — same as windowed). */
            if let Some(frame) = self.op_frames.get(&OP_ID_RECT) {
                let n = frame.read_counter();
                if n != self.last_logged_count {
                    log::info!("rect compute: {} instance(s) emitted", n);
                    self.last_logged_count = n;
                }
            }
        }
        Ok(())
    }

    /// Width × height of the off-screen render target.
    pub fn extent(&self) -> (u32, u32) { (self.extent.width, self.extent.height) }

    /// Clear the render target to `color` (via a real render pass)
    /// and copy the pixels back into the host staging buffer. Returns
    /// once the GPU is idle and `read_pixels` can be called.
    ///
    /// Uses `cmd_begin_render_pass` with `LOAD_OP_CLEAR` so the
    /// render-pass machinery is exercised — same code path the future
    /// `record_frame` will use, just without any draws inside.
    ///
    /// `color` is a BGRA8 quadruple (each component 0..=255).
    pub fn clear_and_readback(&mut self, color: [u8; 4]) -> Result<()> {
        unsafe {
            self.device.reset_command_buffer(
                self.cmd_buffer, vk::CommandBufferResetFlags::empty())?;

            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device.begin_command_buffer(self.cmd_buffer, &begin)?;

            /* Begin render pass — clears via LOAD_OP_CLEAR. The render
             * pass's initial_layout is UNDEFINED (we don't care about
             * the previous contents), and final_layout is
             * COLOR_ATTACHMENT_OPTIMAL (we transition to TRANSFER_SRC
             * ourselves below). */
            let clear_value = vk::ClearValue {
                color: vk::ClearColorValue { float32: [
                    color[2] as f32 / 255.0,
                    color[1] as f32 / 255.0,
                    color[0] as f32 / 255.0,
                    color[3] as f32 / 255.0,
                ]},
            };
            let clears = [clear_value];
            let rp_begin = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffer)
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: self.extent,
                })
                .clear_values(&clears);
            self.device.cmd_begin_render_pass(
                self.cmd_buffer, &rp_begin, vk::SubpassContents::INLINE);
            /* No draws yet — M2.4c lands compute + indirect draw here. */
            self.device.cmd_end_render_pass(self.cmd_buffer);

            /* COLOR_ATTACHMENT_OPTIMAL → TRANSFER_SRC_OPTIMAL  for the
             * upcoming copy-to-buffer. */
            transition_image(
                &self.device, self.cmd_buffer, self.color_image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                vk::AccessFlags::TRANSFER_READ,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER);

            /* Copy image → readback buffer. */
            let region = vk::BufferImageCopy::default()
                .buffer_offset(0)
                .buffer_row_length(0).buffer_image_height(0)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0, base_array_layer: 0, layer_count: 1,
                })
                .image_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
                .image_extent(vk::Extent3D {
                    width: self.extent.width, height: self.extent.height, depth: 1,
                });
            self.device.cmd_copy_image_to_buffer(
                self.cmd_buffer, self.color_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.readback_buffer, &[region]);

            self.device.end_command_buffer(self.cmd_buffer)?;

            /* Reset + submit + wait. Simple synchronous pattern; enough
             * for the smoke-test path. */
            self.device.reset_fences(&[self.fence])?;
            let submit = vk::SubmitInfo::default()
                .command_buffers(std::slice::from_ref(&self.cmd_buffer));
            self.device.queue_submit(self.queue, &[submit], self.fence)?;
            self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
        }
        Ok(())
    }

    /// Read the most-recently-written pixels into a borrowed slice.
    /// `dst.len()` must equal `width * height * 4` (BGRA8).
    pub fn read_pixels(&self, dst: &mut [u8]) -> Result<()> {
        if dst.len() as vk::DeviceSize != self.readback_size {
            return Err(anyhow!(
                "read_pixels: dst.len()={} but readback buffer is {} bytes",
                dst.len(), self.readback_size));
        }
        unsafe {
            let p = self.device.map_memory(
                self.readback_memory, 0, self.readback_size,
                vk::MemoryMapFlags::empty())?;
            std::ptr::copy_nonoverlapping(
                p as *const u8, dst.as_mut_ptr(), dst.len());
            self.device.unmap_memory(self.readback_memory);
        }
        Ok(())
    }

    /// Convenience: allocate a fresh `Vec<u8>` and read pixels into it.
    pub fn read_pixels_vec(&self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.readback_size as usize];
        self.read_pixels(&mut buf)?;
        Ok(buf)
    }
}

impl Drop for HeadlessRenderer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            for (_, r) in self.resources.drain() {
                match r {
                    Resource::Texture(t) => t.destroy(&self.device),
                }
            }
            for (_, f) in self.op_frames.drain() {
                f.destroy(&self.device);
            }
            for (_, p) in self.op_pipelines.drain() {
                p.destroy(&self.device);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_framebuffer(self.framebuffer, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_image_view(self.color_view, None);
            self.device.destroy_image(self.color_image, None);
            self.device.free_memory(self.color_memory, None);
            self.device.destroy_buffer(self.readback_buffer, None);
            self.device.free_memory(self.readback_memory, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
        let _ = self.physical_device;
    }
}

// ── helpers ──────────────────────────────────────────────────────────

fn create_instance(entry: &ash::Entry) -> Result<ash::Instance> {
    let app_info = vk::ApplicationInfo::default()
        .application_name(c"fresco-vulkan-headless")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"fresco")
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(vk::API_VERSION_1_3);

    /* Always-on extensions: portability_enumeration is needed on macOS
     * (MoltenVK reports as a portable Vulkan implementation). On real
     * Vulkan loaders it's a no-op. */
    let exts: Vec<*const c_char> = vec![
        ash::khr::portability_enumeration::NAME.as_ptr(),
        ash::khr::get_physical_device_properties2::NAME.as_ptr(),
    ];

    let create_flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;

    let info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&exts)
        .flags(create_flags);

    Ok(unsafe { entry.create_instance(&info, None) }
        .context("create_instance")?)
}

fn pick_physical_device(instance: &ash::Instance)
    -> Result<(vk::PhysicalDevice, u32)>
{
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    for &pd in &devices {
        let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        for (i, p) in props.iter().enumerate() {
            if p.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                return Ok((pd, i as u32));
            }
        }
    }
    Err(anyhow!("no Vulkan device with a graphics queue"))
}

fn log_physical_device(instance: &ash::Instance, pd: vk::PhysicalDevice) {
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy().into_owned();
    let api  = props.api_version;
    log::info!("vulkan headless device: {name} (api {}.{}.{})",
        vk::api_version_major(api),
        vk::api_version_minor(api),
        vk::api_version_patch(api));
}

fn create_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    queue_family: u32,
) -> Result<ash::Device> {
    let queue_priorities = [1.0f32];
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(&queue_priorities);

    /* portability_subset must be enabled when the implementation is
     * non-conformant (MoltenVK on macOS); harmless to probe and skip
     * if the device doesn't expose it (real vendor Vulkan, lavapipe). */
    let avail = unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
    let has_portability = avail.iter().any(|e| {
        let cname = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
        cname == ash::khr::portability_subset::NAME
    });
    let mut exts: Vec<*const c_char> = Vec::new();
    if has_portability {
        exts.push(ash::khr::portability_subset::NAME.as_ptr());
    }

    let queues = [queue_info];
    let info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&exts);

    Ok(unsafe { instance.create_device(physical_device, &info, None) }
        .context("create_device")?)
}

fn create_color_image(
    device:     &ash::Device,
    mem_props:  &vk::PhysicalDeviceMemoryProperties,
    extent:     vk::Extent2D,
    format:     vk::Format,
) -> Result<(vk::Image, vk::DeviceMemory)> {
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D { width: extent.width, height: extent.height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT
             | vk::ImageUsageFlags::TRANSFER_SRC
             | vk::ImageUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { device.create_image(&info, None) }?;

    let req = unsafe { device.get_image_memory_requirements(image) };
    let mem_type = find_memory_type(
        mem_props, req.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size).memory_type_index(mem_type);
    let memory = unsafe { device.allocate_memory(&alloc, None) }?;
    unsafe { device.bind_image_memory(image, memory, 0) }?;

    Ok((image, memory))
}

fn create_image_view(
    device: &ash::Device,
    image:  vk::Image,
    format: vk::Format,
) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(format)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0, level_count: 1,
            base_array_layer: 0, layer_count: 1,
        });
    Ok(unsafe { device.create_image_view(&info, None) }?)
}

fn create_buffer(
    device:    &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    size:      vk::DeviceSize,
    usage:     vk::BufferUsageFlags,
    props:     vk::MemoryPropertyFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let info = vk::BufferCreateInfo::default()
        .size(size).usage(usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buf = unsafe { device.create_buffer(&info, None) }?;
    let req = unsafe { device.get_buffer_memory_requirements(buf) };
    let mem_type = find_memory_type(mem_props, req.memory_type_bits, props)?;
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size).memory_type_index(mem_type);
    let memory = unsafe { device.allocate_memory(&alloc, None) }?;
    unsafe { device.bind_buffer_memory(buf, memory, 0) }?;
    Ok((buf, memory))
}

fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    needed:  vk::MemoryPropertyFlags,
) -> Result<u32> {
    for i in 0..props.memory_type_count {
        let bit = 1u32 << i;
        if (type_bits & bit) != 0
           && props.memory_types[i as usize].property_flags.contains(needed)
        {
            return Ok(i);
        }
    }
    Err(anyhow!("no memory type matches bits={type_bits:#x} props={needed:?}"))
}

fn create_command_pool(device: &ash::Device, queue_family: u32)
    -> Result<(vk::CommandPool, vk::CommandBuffer)>
{
    let info = vk::CommandPoolCreateInfo::default()
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
        .queue_family_index(queue_family);
    let pool = unsafe { device.create_command_pool(&info, None) }?;
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let bufs = unsafe { device.allocate_command_buffers(&alloc) }?;
    Ok((pool, bufs[0]))
}

fn create_render_pass(device: &ash::Device, format: vk::Format)
    -> Result<vk::RenderPass>
{
    let attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        /* Headless: end in COLOR_ATTACHMENT_OPTIMAL; the caller
         * transitions to TRANSFER_SRC_OPTIMAL before copy-to-buffer. */
        .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);

    let color_ref = [vk::AttachmentReference {
        attachment: 0,
        layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
    }];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref);

    let dependency = vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE);

    let attachments = [attachment];
    let subpasses   = [subpass];
    let deps        = [dependency];
    let info = vk::RenderPassCreateInfo::default()
        .attachments(&attachments)
        .subpasses(&subpasses)
        .dependencies(&deps);
    Ok(unsafe { device.create_render_pass(&info, None) }?)
}

fn create_framebuffer(
    device:      &ash::Device,
    render_pass: vk::RenderPass,
    view:        vk::ImageView,
    extent:      vk::Extent2D,
) -> Result<vk::Framebuffer> {
    let attachments = [view];
    let info = vk::FramebufferCreateInfo::default()
        .render_pass(render_pass)
        .attachments(&attachments)
        .width(extent.width)
        .height(extent.height)
        .layers(1);
    Ok(unsafe { device.create_framebuffer(&info, None) }?)
}

fn transition_image(
    device:        &ash::Device,
    cb:            vk::CommandBuffer,
    image:         vk::Image,
    old_layout:    vk::ImageLayout,
    new_layout:    vk::ImageLayout,
    src_access:    vk::AccessFlags,
    dst_access:    vk::AccessFlags,
    src_stage:     vk::PipelineStageFlags,
    dst_stage:     vk::PipelineStageFlags,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .old_layout(old_layout)
        .new_layout(new_layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0, level_count: 1,
            base_array_layer: 0, layer_count: 1,
        });
    unsafe {
        device.cmd_pipeline_barrier(
            cb, src_stage, dst_stage, vk::DependencyFlags::empty(),
            &[], &[], &[barrier]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-test: create a 64×64 headless renderer, clear to a known
    /// BGRA color via a real render pass, read back, verify every
    /// pixel matches.
    ///
    /// This test requires a Vulkan loader to be available
    /// (lavapipe / venus / vendor / MoltenVK on macOS). If none is
    /// available the test is skipped (init returns Err).
    #[test]
    fn clear_64x64_red_round_trip() {
        let mut r = match HeadlessRenderer::new(64, 64) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping: no Vulkan loader available ({e})");
                return;
            }
        };

        let color = [0x00, 0x00, 0xFF, 0xFF];  /* BGRA = pure red */
        r.clear_and_readback(color).expect("clear_and_readback");
        let pixels = r.read_pixels_vec().expect("read_pixels");

        assert_eq!(pixels.len(), 64 * 64 * 4);

        /* Every pixel must match the cleared color. Drivers may swizzle
         * BGRA8 differently — check just the RGB channels with a
         * tolerance for sRGB rounding. */
        let mut nonred = 0;
        for px in pixels.chunks_exact(4) {
            if px[0] < 0xF0 || px[1] > 0x10 || px[2] > 0x10 {
                nonred += 1;
            }
        }
        assert_eq!(nonred, 0, "expected all-red, found {nonred} divergent pixels");
    }

    /// Test bundle loading: AOT-compile the atrium-core bundle's
    /// pipelines + allocate per-op frame resources. Skipped if the
    /// bundle's SPIR-V hasn't been built yet (run
    /// `bundles/atrium-core/build.sh` to generate).
    #[test]
    fn load_atrium_core_bundle() {
        let bundle_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("bundles/atrium-core");

        /* Skip if SPIR-V hasn't been built. The CI lane (M3) will
         * have build.sh run as a pre-test step. */
        if !bundle_path.join("compute/op_rectangle.comp.spv").exists() {
            eprintln!("skipping: SPIR-V not built (run bundles/atrium-core/build.sh)");
            return;
        }

        let mut r = match HeadlessRenderer::new(256, 256) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping: no Vulkan loader available ({e})");
                return;
            }
        };

        r.load_bundle(&bundle_path).expect("load_bundle");
        assert!(r.op_count() >= 2, "atrium-core has rect + texture, got {}", r.op_count());
    }

    /// End-to-end render: 10 rect nodes through compute + draw.
    /// Validates: scene buffer write → compute dispatch → atomic counter
    /// → instance buffer → indirect draw → render pass → image-to-buffer
    /// copy → host readback.
    ///
    /// Skipped if SPIR-V or Vulkan loader missing.
    #[test]
    fn render_10_rects_end_to_end() {
        let bundle_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("bundles/atrium-core");
        if !bundle_path.join("compute/op_rectangle.comp.spv").exists() {
            eprintln!("skipping: SPIR-V not built");
            return;
        }
        let mut r = match HeadlessRenderer::new(512, 512) {
            Ok(r) => r,
            Err(e) => { eprintln!("skipping: {e}"); return; }
        };
        r.load_bundle(&bundle_path).expect("load_bundle");

        /* 10 rects across the screen — opaque white to be obvious. */
        let mut nodes = Vec::new();
        for i in 0..10 {
            nodes.push(SceneNode {
                position: [10.0 + i as f32 * 40.0, 100.0],
                size:     [30.0, 30.0],
                color:    [1.0, 1.0, 1.0, 1.0],
            });
        }
        r.set_rect_nodes(nodes);

        r.render_to_buffer().expect("render_to_buffer");

        let pixels = r.read_pixels_vec().expect("read_pixels");
        assert_eq!(pixels.len(), 512 * 512 * 4);

        /* The cleared (teal) background should not be the entire image —
         * count pixels that are clearly NOT teal (i.e. our white rects). */
        let mut white_pixels = 0;
        for px in pixels.chunks_exact(4) {
            let (b, g, r) = (px[0], px[1], px[2]);
            if b > 0xC0 && g > 0xC0 && r > 0xC0 {
                white_pixels += 1;
            }
        }
        /* 10 rects × 30×30 = 9000 white pixels expected (give or take
         * AA, sRGB rounding). Lower bound is the verification: did the
         * compute kernel + draw actually produce non-clear pixels? */
        assert!(white_pixels > 1000,
            "expected white rect pixels, found {white_pixels}");
    }
}
