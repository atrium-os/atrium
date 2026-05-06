//! Renderer — Vulkan instance, surface, device, swapchain, render pass.
//!
//! Step 2 scope: clear-color render pass per frame. No compute, no
//! bundles, no indirect-instanced draw. Those land in steps 7-9.
//!
//! Lifetime / ownership:
//!   - `Entry` owns the dlopen of libvulkan.
//!   - `Instance` is created from the entry; owns surface.
//!   - `Surface` is bound to the `Window` (via raw-window-handle).
//!   - `Device` owns swapchain, render pass, framebuffers, pool, sync.
//!   - `Renderer` owns all of the above. Drop reverses creation order
//!     after `vkDeviceWaitIdle`.
//!
//! Resize: when the swapchain reports out-of-date or the window is
//! resized via `resize()`, we tear down the swapchain-derived state
//! (swapchain, image views, framebuffers) and recreate it. The render
//! pass + sync primitives + command pool survive.

use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use ash::khr;
use ash::vk;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::frame::{OpFrameResources, SceneNode, TextureNode};
use crate::pipeline::{op_kind, OpPipelines};
use crate::resource::{self, Resource, UploadRequest};

/// Op-ids for atrium-core. Hardcoded for the POC; future versions look
/// up by name via the bundle manifest. Mirror the values in
/// `atrium-rpc-display::scene_ops`.
const OP_ID_RECT:    u32 = 0x1000;
const OP_ID_TEXTURE: u32 = 0x1001;

/// One batch of texture nodes that all reference the same slot. The
/// App builds these from `SceneState`; the renderer issues one
/// dispatch+draw per batch. Cheap because the renderer reuses the
/// texture op's buffers across batches (with barriers in between).
pub struct TextureBatch {
    pub slot_id: u32,
    pub nodes:   Vec<TextureNode>,
}

/// Pre-recorded handles + count for one op's contribution to a frame.
/// Built per-render() before the cb is recorded; lets the recording
/// loop work with raw vk handles instead of fighting the borrow
/// checker over self.op_pipelines / op_frames / resources.
struct DrawPlan {
    compute_pipeline: vk::Pipeline,
    compute_layout:   vk::PipelineLayout,
    compute_set:      vk::DescriptorSet,
    render_pipeline:  vk::Pipeline,
    render_layout:    vk::PipelineLayout,
    render_set:       vk::DescriptorSet,
    counter_buf:      vk::Buffer,
    instance_buf:     vk::Buffer,
    n:                u32,
}

/// Atrium teal — a recognisable color so the user can confirm this is
/// our render and not the OS's default fill. RGBA float.
const CLEAR_COLOR: [f32; 4] = [0.04, 0.50, 0.55, 1.0];

pub struct Renderer {
    /* Kept around for lifetime; suppresses unused-field warnings. */
    _entry:    ash::Entry,
    instance:  ash::Instance,

    surface_loader: khr::surface::Instance,
    surface:        vk::SurfaceKHR,

    physical_device: vk::PhysicalDevice,
    /* queue_family kept around so future swapchain recreation /
     * compute-pool wiring can read it without re-querying. Suppress
     * dead-code until step 7+. */
    #[allow(dead_code)]
    queue_family:    u32,
    device:          ash::Device,
    queue:           vk::Queue,

    /* Swapchain-dependent state. Recreated on resize. */
    swapchain_loader: khr::swapchain::Device,
    swapchain:        vk::SwapchainKHR,
    surface_format:   vk::SurfaceFormatKHR,
    extent:           vk::Extent2D,
    images:           Vec<vk::Image>,
    image_views:      Vec<vk::ImageView>,
    framebuffers:     Vec<vk::Framebuffer>,

    /* Stable across swapchain recreate. */
    render_pass:    vk::RenderPass,
    cmd_pool:       vk::CommandPool,
    cmd_buffer:     vk::CommandBuffer,

    /* Sync (one frame in flight). */
    image_available: vk::Semaphore,
    render_finished: vk::Semaphore,
    in_flight_fence: vk::Fence,

    /* Loaded bundles' pipelines, keyed by op-id. Empty until
     * load_bundle() is called. Step 7+ dispatches by op-id. */
    op_pipelines: HashMap<u32, OpPipelines>,

    /* Per-op compute-pass GPU buffers + descriptor set. Allocated in
     * load_bundle() at max_instances from the bundle manifest. */
    op_frames:    HashMap<u32, OpFrameResources>,

    /// Cached, decoded scene nodes for the rect op; refilled by the App
    /// from `SceneState` before each render(). Keeping a Vec on the
    /// renderer rather than passing in by ref lets us reuse the
    /// allocation across frames.
    rect_nodes:   Vec<SceneNode>,
    texture_batches: Vec<TextureBatch>,

    /// Last instance count we logged; suppresses spam since render()
    /// runs at vsync but the scene rarely changes.
    last_logged_count: u32,

    /* Slot ID → uploaded GPU resource. Populated by process_uploads(),
     * which the App calls before render() with whatever the dispatcher
     * thread queued onto SceneState since the last frame. Step 9+
     * binds these into the texture op's render pipeline. */
    resources:    HashMap<u32, Resource>,
}

impl Renderer {
    /// Create a renderer bound to `window`. The window must outlive the
    /// renderer (the surface holds a reference indirectly via the
    /// platform's window handle).
    pub fn new(window: &(impl HasWindowHandle + HasDisplayHandle))
        -> Result<Self>
    {
        let entry = unsafe { ash::Entry::load() }
            .context("ash::Entry::load — install vulkan-loader + MoltenVK")?;

        let instance = create_instance(&entry, window)?;

        let surface_loader = khr::surface::Instance::new(&entry, &instance);
        let surface = unsafe {
            ash_window::create_surface(
                &entry, &instance,
                window.display_handle()
                    .map_err(|e| anyhow!("display_handle: {e}"))?
                    .as_raw(),
                window.window_handle()
                    .map_err(|e| anyhow!("window_handle: {e}"))?
                    .as_raw(),
                None,
            )
        }.context("create_surface")?;

        let (physical_device, queue_family) =
            pick_physical_device(&instance, &surface_loader, surface)?;
        log_physical_device(&instance, physical_device);

        let device = create_device(&instance, physical_device, queue_family)?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        let swapchain_loader = khr::swapchain::Device::new(&instance, &device);

        let (swapchain, surface_format, extent, images) =
            create_swapchain(
                &instance, &device, physical_device,
                &surface_loader, surface, &swapchain_loader,
                None,   /* no old swapchain on first creation */
            )?;

        let render_pass = create_render_pass(&device, surface_format)?;

        let image_views = create_image_views(&device, &images, surface_format)?;
        let framebuffers = create_framebuffers(
            &device, render_pass, &image_views, extent)?;

        let (cmd_pool, cmd_buffer) = create_command_pool(&device, queue_family)?;

        let (image_available, render_finished, in_flight_fence) =
            create_sync(&device)?;

        Ok(Self {
            _entry: entry,
            instance,
            surface_loader, surface,
            physical_device, queue_family,
            device, queue,
            swapchain_loader, swapchain, surface_format, extent,
            images, image_views, framebuffers,
            render_pass, cmd_pool, cmd_buffer,
            image_available, render_finished, in_flight_fence,
            op_pipelines: HashMap::new(),
            op_frames:    HashMap::new(),
            rect_nodes:   Vec::new(),
            texture_batches: Vec::new(),
            last_logged_count: u32::MAX,
            resources:    HashMap::new(),
        })
    }

    /// AOT-compile every op in `bundle_path` and register by op-id.
    /// Per `docs/spec/fresco-rendering-stack.md` §3.1, bundle pipelines
    /// are created at startup, not per frame.
    ///
    /// Pipelines are stored in `self.op_pipelines`; steps 7-8 will
    /// look them up by op-id and bind them in the per-frame compute
    /// pass + render pass. For step 4, just creating them validates
    /// that the bundle's SPIR-V + the host's descriptor-set layouts
    /// agree (Vulkan would reject pipeline creation otherwise).
    pub fn load_bundle(&mut self, bundle_path: &Path) -> Result<()> {
        let bundle = fresco_bundle::Bundle::load(bundle_path)
            .with_context(|| format!("load bundle {}", bundle_path.display()))?;
        log::info!("bundle '{}' v{}: {} op(s)",
            bundle.manifest.name, bundle.manifest.version, bundle.manifest.ops.len());

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
            log::info!("  op {} '{}' → pipelines + buffers (cap {} instances)",
                op.id, op.name, max_instances);

            if self.op_pipelines.contains_key(&op.id) {
                /* Op-ID collisions across bundles get a warning per
                 * §3.4 of the spec ("last bundle wins"). For the POC
                 * we only load one bundle, so this fires only on a
                 * packaging mistake within the bundle. */
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

    /// Hand the renderer the rect-op nodes to render this frame. Called
    /// by the App after draining `SceneState::nodes`. Replaces any
    /// previously-staged nodes (rendering is stateless across frames).
    pub fn set_rect_nodes(&mut self, nodes: Vec<SceneNode>) {
        self.rect_nodes = nodes;
    }

    /// Hand the renderer the texture-op batches to render this frame.
    /// One batch per slot referenced by the current scene; the
    /// renderer issues one dispatch+draw cycle per batch since each
    /// uses a different bound texture.
    pub fn set_texture_batches(&mut self, batches: Vec<TextureBatch>) {
        self.texture_batches = batches;
    }

    /// SLOT_CLEAR / texture replacement notification. Drops any cached
    /// per-slot descriptor that referenced the old ImageView, so the
    /// next frame allocates a fresh one against whatever's bound.
    pub fn invalidate_slot(&mut self, slot_id: u32) {
        if let Some(f) = self.op_frames.get_mut(&OP_ID_TEXTURE) {
            f.drop_texture_slot(slot_id);
        }
    }

    /// Lookup compiled pipelines for an op. Used by steps 7-8.
    #[allow(dead_code)]
    pub fn op_pipelines(&self, op_id: u32) -> Option<&OpPipelines> {
        self.op_pipelines.get(&op_id)
    }

    /// Drain pending upload + clear requests from the dispatcher.
    /// Called by the App on the main thread before each `render()`,
    /// since Vulkan resource operations must run on the same thread
    /// that owns the device + queue.
    ///
    /// One-frame latency: a SLOT_SET sent by the client at frame N
    /// becomes a usable resource at frame N+1. Acceptable for the
    /// POC; a production server would use a dedicated transfer queue
    /// to avoid the gap.
    pub fn process_uploads(
        &mut self,
        uploads: Vec<UploadRequest>,
        clears:  Vec<u32>,
    ) -> Result<()> {
        /* Clears first: free the old image bound to a slot before any
         * upload could overwrite it (last-write-wins on slot reuse). */
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
                    /* Replace any existing resource on this slot. */
                    if let Some(prev) = self.resources.remove(&slot_id) {
                        unsafe {
                            self.device.device_wait_idle().ok();
                            match prev {
                                Resource::Texture(t) => t.destroy(&self.device),
                            }
                        }
                    }
                    /* New ImageView → invalidate any stale per-slot
                     * descriptor before next-frame ensure_texture_descriptor
                     * resolves it against the fresh view. */
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

    /// Borrow the resource bound to `slot_id`. Steps 9+ use this to
    /// look up textures when binding descriptor sets for the texture
    /// op.
    #[allow(dead_code)]
    pub fn resource(&self, slot_id: u32) -> Option<&Resource> {
        self.resources.get(&slot_id)
    }

    /// Render one frame: clear to `CLEAR_COLOR` and present.
    pub fn render(&mut self) -> Result<()> {
        unsafe {
            self.device.wait_for_fences(
                &[self.in_flight_fence], true, u64::MAX)?;

            /* Step 7 verification: prior frame's compute pass is now
             * retired (fence signalled), so the counter buffer is safe
             * to read from the host. Log only on change to avoid
             * vsync-rate spam. */
            if let Some(frame) = self.op_frames.get(&OP_ID_RECT) {
                let n = frame.read_counter();
                if n != self.last_logged_count {
                    log::info!("rect compute: {} instance(s) emitted", n);
                    self.last_logged_count = n;
                }
            }

            /* Acquire next swapchain image. OUT_OF_DATE → recreate. */
            let acquire = self.swapchain_loader.acquire_next_image(
                self.swapchain, u64::MAX,
                self.image_available, vk::Fence::null());
            let image_index = match acquire {
                Ok((idx, _suboptimal)) => idx,
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    self.recreate_swapchain()?;
                    return Ok(());
                }
                Err(e) => return Err(anyhow!("acquire_next_image: {e:?}")),
            };

            self.device.reset_fences(&[self.in_flight_fence])?;

            /* ── Pre-record: stage host data + resolve handles into
             *    a plan struct so cb recording doesn't fight the borrow
             *    checker over self.op_pipelines / op_frames / resources. */
            let mut plans: Vec<DrawPlan> = Vec::new();

            /* Rect plan. */
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
                });
            }

            /* Texture plans — one per batch. POC limitation: all
             * batches share the texture op's single instance/scene
             * buffer pair, so issuing N batches in one cb would clobber
             * scene data before earlier dispatches ran. Render only the
             * first batch and warn; future work: per-slot OpFrameResources
             * or a per-batch buffer ring. */
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
                    });
                } else if view.is_none() {
                    log::warn!("texture batch slot={} not bound; skipping",
                        batch.slot_id);
                }
            }

            /* Record. */
            self.device.reset_command_buffer(
                self.cmd_buffer, vk::CommandBufferResetFlags::empty())?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            self.device.begin_command_buffer(self.cmd_buffer, &begin)?;

            /* ── Compute passes: zero counter + dispatch traversal,
             *    one cycle per plan. ── */
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

            let clear = [vk::ClearValue {
                color: vk::ClearColorValue { float32: CLEAR_COLOR },
            }];
            let rp_begin = vk::RenderPassBeginInfo::default()
                .render_pass(self.render_pass)
                .framebuffer(self.framebuffers[image_index as usize])
                .render_area(vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent: self.extent,
                })
                .clear_values(&clear);
            self.device.cmd_begin_render_pass(
                self.cmd_buffer, &rp_begin, vk::SubpassContents::INLINE);

            /* Dynamic viewport + scissor are set once for the pass
             * regardless of how many ops draw. */
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
                if plan.n == 0 { continue; }
                self.device.cmd_bind_pipeline(
                    self.cmd_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    plan.render_pipeline);
                self.device.cmd_bind_descriptor_sets(
                    self.cmd_buffer,
                    vk::PipelineBindPoint::GRAPHICS,
                    plan.render_layout,
                    0, &[plan.render_set], &[]);
                self.device.cmd_draw(self.cmd_buffer, 4, plan.n, 0, 0);
            }

            self.device.cmd_end_render_pass(self.cmd_buffer);
            self.device.end_command_buffer(self.cmd_buffer)?;

            /* Submit. */
            let wait_sems   = [self.image_available];
            let signal_sems = [self.render_finished];
            let stages      = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let cmd_bufs    = [self.cmd_buffer];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cmd_bufs)
                .signal_semaphores(&signal_sems);
            self.device.queue_submit(
                self.queue, &[submit], self.in_flight_fence)?;

            /* Present. */
            let swapchains   = [self.swapchain];
            let image_indices = [image_index];
            let present = vk::PresentInfoKHR::default()
                .wait_semaphores(&signal_sems)
                .swapchains(&swapchains)
                .image_indices(&image_indices);
            let res = self.swapchain_loader.queue_present(self.queue, &present);
            match res {
                Ok(_) => {}
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR)
              | Err(vk::Result::SUBOPTIMAL_KHR) => self.recreate_swapchain()?,
                Err(e) => return Err(anyhow!("queue_present: {e:?}")),
            }
        }
        Ok(())
    }

    /// Mark the swapchain as needing recreation. Called from window
    /// resize handlers. Lazy: actual recreation happens at next render.
    pub fn resize(&mut self) -> Result<()> {
        self.recreate_swapchain()
    }

    fn recreate_swapchain(&mut self) -> Result<()> {
        unsafe {
            self.device.device_wait_idle()?;
            for fb in self.framebuffers.drain(..) {
                self.device.destroy_framebuffer(fb, None);
            }
            for v in self.image_views.drain(..) {
                self.device.destroy_image_view(v, None);
            }
            let old = self.swapchain;
            let (sc, format, extent, images) = create_swapchain(
                &self.instance, &self.device, self.physical_device,
                &self.surface_loader, self.surface,
                &self.swapchain_loader, Some(old))?;
            self.swapchain_loader.destroy_swapchain(old, None);
            self.swapchain       = sc;
            self.surface_format  = format;
            self.extent          = extent;
            self.images          = images;
            self.image_views = create_image_views(
                &self.device, &self.images, self.surface_format)?;
            self.framebuffers = create_framebuffers(
                &self.device, self.render_pass,
                &self.image_views, self.extent)?;
            for frame in self.op_frames.values() {
                frame.write_screen(self.extent.width, self.extent.height);
            }
        }
        Ok(())
    }
}

impl Drop for Renderer {
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
            self.device.destroy_fence(self.in_flight_fence, None);
            self.device.destroy_semaphore(self.render_finished, None);
            self.device.destroy_semaphore(self.image_available, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            for fb in &self.framebuffers {
                self.device.destroy_framebuffer(*fb, None);
            }
            for v in &self.image_views {
                self.device.destroy_image_view(*v, None);
            }
            self.swapchain_loader.destroy_swapchain(self.swapchain, None);
            self.device.destroy_render_pass(self.render_pass, None);
            self.device.destroy_device(None);
            self.surface_loader.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

// ── construction helpers ─────────────────────────────────────────────

fn create_instance(
    entry: &ash::Entry,
    window: &(impl HasWindowHandle + HasDisplayHandle),
) -> Result<ash::Instance> {
    let app_info = vk::ApplicationInfo::default()
        .application_name(c"fresco-server-poc")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"fresco")
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(vk::API_VERSION_1_3);

    /* Surface extensions required by the window system. */
    let mut exts: Vec<*const c_char> = ash_window::enumerate_required_extensions(
        window.display_handle()
            .map_err(|e| anyhow!("display_handle: {e}"))?
            .as_raw())?
        .to_vec();

    /* macOS / MoltenVK requires the portability-enumeration extension. */
    exts.push(khr::portability_enumeration::NAME.as_ptr());
    /* And on macOS we need this for vkGetPhysicalDeviceProperties2 etc. */
    exts.push(khr::get_physical_device_properties2::NAME.as_ptr());

    let create_flags = vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;

    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&exts)
        .flags(create_flags);

    let instance = unsafe { entry.create_instance(&create_info, None) }
        .context("create_instance")?;
    Ok(instance)
}

fn pick_physical_device(
    instance: &ash::Instance,
    surface_loader: &khr::surface::Instance,
    surface: vk::SurfaceKHR,
) -> Result<(vk::PhysicalDevice, u32)> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;
    for &pd in &devices {
        let props = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        for (i, p) in props.iter().enumerate() {
            if p.queue_flags.contains(vk::QueueFlags::GRAPHICS)
               && unsafe { surface_loader.get_physical_device_surface_support(
                       pd, i as u32, surface)? }
            {
                return Ok((pd, i as u32));
            }
        }
    }
    Err(anyhow!("no suitable physical device with graphics + present queue"))
}

fn log_physical_device(instance: &ash::Instance, pd: vk::PhysicalDevice) {
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy().into_owned();
    let api  = props.api_version;
    log::info!("vulkan device: {name} (api {}.{}.{})",
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

    /* Required extensions: swapchain (for present) + portability_subset
     * on MoltenVK. The latter is harmless to enable on platforms that
     * don't need it — drivers ignore unknown extensions only at the
     * INSTANCE layer; at the device layer we enable it conditionally. */
    let mut exts: Vec<*const c_char> = vec![
        khr::swapchain::NAME.as_ptr(),
    ];
    /* Probe whether portability_subset is available; if so, enable it. */
    let avail = unsafe { instance.enumerate_device_extension_properties(physical_device) }?;
    let has_portability = avail.iter().any(|e| {
        let cname = unsafe { CStr::from_ptr(e.extension_name.as_ptr()) };
        cname == khr::portability_subset::NAME
    });
    if has_portability {
        exts.push(khr::portability_subset::NAME.as_ptr());
    }

    let queues = [queue_info];
    let info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queues)
        .enabled_extension_names(&exts);

    let device = unsafe { instance.create_device(physical_device, &info, None) }
        .context("create_device")?;
    Ok(device)
}

fn create_swapchain(
    instance: &ash::Instance,
    device:   &ash::Device,
    physical_device: vk::PhysicalDevice,
    surface_loader:  &khr::surface::Instance,
    surface:  vk::SurfaceKHR,
    swapchain_loader: &khr::swapchain::Device,
    old_swapchain:    Option<vk::SwapchainKHR>,
) -> Result<(vk::SwapchainKHR, vk::SurfaceFormatKHR, vk::Extent2D, Vec<vk::Image>)>
{
    let _ = (instance, device);  /* unused outside of caller's lifetime; reserved */

    let caps = unsafe {
        surface_loader.get_physical_device_surface_capabilities(physical_device, surface)
    }?;
    let formats = unsafe {
        surface_loader.get_physical_device_surface_formats(physical_device, surface)
    }?;
    let modes = unsafe {
        surface_loader.get_physical_device_surface_present_modes(physical_device, surface)
    }?;

    let surface_format = formats.iter()
        .copied()
        .find(|f| f.format == vk::Format::B8G8R8A8_SRGB
                  && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR)
        .unwrap_or(formats[0]);

    /* Prefer FIFO (vsync; mandatory). Mailbox would be lower latency
     * but not all drivers support it on macOS. */
    let present_mode = if modes.contains(&vk::PresentModeKHR::FIFO) {
        vk::PresentModeKHR::FIFO
    } else {
        modes[0]
    };

    let extent = if caps.current_extent.width != u32::MAX {
        caps.current_extent
    } else {
        vk::Extent2D { width: 1920, height: 1080 }
    };

    let mut image_count = caps.min_image_count + 1;
    if caps.max_image_count > 0 && image_count > caps.max_image_count {
        image_count = caps.max_image_count;
    }

    let mut info = vk::SwapchainCreateInfoKHR::default()
        .surface(surface)
        .min_image_count(image_count)
        .image_format(surface_format.format)
        .image_color_space(surface_format.color_space)
        .image_extent(extent)
        .image_array_layers(1)
        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .pre_transform(caps.current_transform)
        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
        .present_mode(present_mode)
        .clipped(true);
    if let Some(old) = old_swapchain {
        info = info.old_swapchain(old);
    }

    let swapchain = unsafe { swapchain_loader.create_swapchain(&info, None) }?;
    let images = unsafe { swapchain_loader.get_swapchain_images(swapchain) }?;
    Ok((swapchain, surface_format, extent, images))
}

fn create_image_views(
    device: &ash::Device,
    images: &[vk::Image],
    fmt: vk::SurfaceFormatKHR,
) -> Result<Vec<vk::ImageView>> {
    images.iter().map(|&img| {
        let info = vk::ImageViewCreateInfo::default()
            .image(img)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt.format)
            .components(vk::ComponentMapping::default())
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        Ok(unsafe { device.create_image_view(&info, None) }?)
    }).collect()
}

fn create_render_pass(device: &ash::Device, fmt: vk::SurfaceFormatKHR)
    -> Result<vk::RenderPass>
{
    let attachment = vk::AttachmentDescription::default()
        .format(fmt.format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);

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

fn create_framebuffers(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    views: &[vk::ImageView],
    extent: vk::Extent2D,
) -> Result<Vec<vk::Framebuffer>> {
    views.iter().map(|&v| {
        let attachments = [v];
        let info = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(&attachments)
            .width(extent.width)
            .height(extent.height)
            .layers(1);
        Ok(unsafe { device.create_framebuffer(&info, None) }?)
    }).collect()
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

fn create_sync(device: &ash::Device)
    -> Result<(vk::Semaphore, vk::Semaphore, vk::Fence)>
{
    let sem = vk::SemaphoreCreateInfo::default();
    let fen = vk::FenceCreateInfo::default()
        .flags(vk::FenceCreateFlags::SIGNALED);
    Ok((
        unsafe { device.create_semaphore(&sem, None) }?,
        unsafe { device.create_semaphore(&sem, None) }?,
        unsafe { device.create_fence(&fen, None) }?,
    ))
}
