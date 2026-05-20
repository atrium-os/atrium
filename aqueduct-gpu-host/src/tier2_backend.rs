//! Tier-2 software-execution Backend.
//!
//! Implements the [`Backend`] trait by composition: an
//! internal Tier-2 shader registry plus a per-image
//! framebuffer map keyed by `ResourceId`. The interesting
//! method is [`Tier2Backend::run_fragment_shader_into`],
//! which routes a registered Tier-2 fragment shader's
//! output into a previously-created image.
//!
//! # Phase status
//!
//! **Phase 2 v5d step 2.** This is the scaffolding tier:
//! the Backend trait surface is implemented but
//! `submit_frame` is a stub. Real wire-protocol routing —
//! where a guest's draw call against a Tier-2-bound
//! pipeline kicks off `run_fragment_shader_into`
//! automatically — lands in v5e once the wire ops for
//! "bind a Tier-2 pipeline" are finalised.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use aqueduct_gpu::ids::ResourceId;
use aqueduct_gpu::backends::{BackendId, GpuVendor};

use crate::backend::Backend;
use crate::tier2_registry::{Tier2ExecError, Tier2Registry, Tier2ShaderId};

use aqueduct_gpu::frame::{
    BindIndexBufCmd, BindVertexBufCmd, DrawCmd, DrawIndexedCmd,
    FrameDecoder, IndexType, SetViewportCmd,
};
use aqueduct_gpu::opcodes::FrameOp;
use aqueduct_gpu::VertexInputState;

/// Backend that routes draws through Tier-2 compiled
/// fragment shaders. Image storage lives in this backend
/// (one RGBA8 buffer per registered image) so calls to
/// [`Tier2Backend::run_fragment_shader_into`] can write
/// pixels without going through `image_write_pixels`.
pub struct Tier2Backend {
    registry: Arc<Tier2Registry>,
    images:   Mutex<HashMap<u64, ImageStorage>>,
    /// Per-buffer byte storage keyed by `ResourceId.raw()`.
    /// Populated on `buffer_created`; filled by
    /// `buffer_write_bytes`. The draw-walker (D.3+) reads
    /// vertex / index data out of here.
    buffers:  Mutex<HashMap<u32, BufferStorage>>,
    /// Pipeline ResourceId.raw() → bound Tier-2 fragment
    /// shader. Populated by [`Tier2Backend::bind_pipeline`]
    /// (called by the Session when it processes
    /// `OP_GPU_PIPELINE_CREATE` for a Tier-2 pipeline).
    /// `submit_frame` consults this map to know which
    /// shader to fire for each `FrameOp::BindPipeline`
    /// record it walks.
    pipeline_shaders: Mutex<HashMap<u32, Tier2ShaderId>>,
    /// Vertex-input layout keyed by pipeline ResourceId.raw().
    /// Populated by [`Tier2Backend::bind_pipeline_layout`] when
    /// the session decodes a `Tier2PipelineStateBlob`. The
    /// frame-walker consults this map at Draw time to slice
    /// bound vertex buffers into per-vertex attribute bytes.
    pipeline_layouts: Mutex<HashMap<u32, VertexInputState>>,
    submissions: AtomicU64,
    presents:    AtomicU64,
    /// Total Draw / DrawIndexed records the frame-walker has
    /// dispatched against a Tier-2-bound pipeline. The D.3
    /// dispatch path is a stub — D.4 turns each dispatch into
    /// per-primitive `fill_image_triangle` calls — but the
    /// counter is observable now so the wire walker has unit
    /// tests of its own.
    draws_executed: AtomicU64,
    /// Draws / DrawIndexeds the walker decoded but skipped
    /// because no Tier-2 pipeline was bound at that point
    /// (e.g. a guest that submits geometry against a non-Tier-2
    /// pipeline). Observable for diagnostics.
    draws_skipped: AtomicU64,
    /// Per-vertex packed attribute bytes assembled by the most
    /// recent `dispatch_draw`. Exposed for D.4 testing; D.5
    /// turns this into the input of `fill_image_triangle`.
    last_assembled_vertices: Mutex<Option<AssembledVertices>>,
}

/// Output of [`Tier2Backend::assemble_vertices`]: per-vertex
/// attribute bytes laid out by (location, format). Lengths:
/// `attribute_offsets.len() == layout.attributes.len() + 1`,
/// `bytes.len() == vertex_count * stride_per_vertex` where the
/// total stride is `attribute_offsets.last()`.
#[derive(Debug, Clone)]
pub struct AssembledVertices {
    /// Number of vertices packed.
    pub vertex_count: u32,
    /// Per-vertex stride in bytes (sum of all attribute sizes,
    /// densely packed in shader-`location` order).
    pub stride: u32,
    /// Byte offset of each attribute within one vertex; final
    /// entry equals `stride` (so `[i..i+1]` gives the i-th
    /// attribute's range).
    pub attribute_offsets: Vec<u32>,
    /// Packed bytes: `vertex_count` records of `stride` bytes.
    pub bytes: Vec<u8>,
}

/// Per-render-pass bound state assembled by the frame walker.
///
/// One instance per `BeginRenderPass ... EndRenderPass` span.
/// Mutated as the walker visits each frame op; consumed by
/// `Draw` / `DrawIndexed` dispatch.
#[derive(Debug, Default)]
struct PassState {
    /// Currently-bound pipeline ResourceId.raw(), if any.
    pipeline_raw: Option<u32>,
    /// Tier-2 fragment shader bound to the current pipeline,
    /// looked up via `pipeline_shaders` at BindPipeline time.
    tier2_shader: Option<Tier2ShaderId>,
    /// Vertex-buffer bindings keyed by slot number.
    vertex_buffers: HashMap<u32, BoundVertexBuffer>,
    /// Currently-bound index buffer.
    index_buffer: Option<BoundIndexBuffer>,
    /// Current viewport (may be unset for a malformed frame; the
    /// walker tolerates it but a real draw would error in D.4+).
    viewport: Option<SetViewportCmd>,
    /// Latest push-constants block; tier-2 shaders consume it
    /// as their uniform area.
    push_constants: Vec<u8>,
    /// Number of Draw / DrawIndexed records issued in this pass.
    /// Used to gate the legacy "BindPipeline alone implies a
    /// fullscreen FS fill" fallback at EndRenderPass.
    draws_in_pass: u32,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // fields consumed by D.4+ vertex layout path
struct BoundVertexBuffer {
    buffer_raw: u32,
    offset: u64,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // fields consumed by D.8 indexed-draw path
struct BoundIndexBuffer {
    buffer_raw: u32,
    offset: u64,
    index_type: IndexType,
}

/// Per-image RGBA8 storage owned by the backend.
struct ImageStorage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// Per-buffer byte storage owned by the backend. `size` is the
/// declared capacity from `OP_GPU_BUFFER_CREATE`; `bytes` is
/// pre-zeroed to that size so partial writes via
/// `OP_GPU_BUFFER_WRITE` land at the right offsets without
/// needing growth.
struct BufferStorage {
    size: u64,
    bytes: Vec<u8>,
}

impl Tier2Backend {
    /// Construct a fresh Tier2Backend backed by the given
    /// registry. The registry can be shared across
    /// backends; image storage is per-backend.
    pub fn new(registry: Arc<Tier2Registry>) -> Self {
        Self {
            registry,
            images: Mutex::new(HashMap::new()),
            buffers: Mutex::new(HashMap::new()),
            pipeline_shaders: Mutex::new(HashMap::new()),
            pipeline_layouts: Mutex::new(HashMap::new()),
            last_assembled_vertices: Mutex::new(None),
            submissions: AtomicU64::new(0),
            presents:    AtomicU64::new(0),
            draws_executed: AtomicU64::new(0),
            draws_skipped:  AtomicU64::new(0),
        }
    }

    /// Cumulative count of Draw / DrawIndexed records executed
    /// against a Tier-2-bound pipeline by the frame walker.
    pub fn draw_count(&self) -> u64 {
        self.draws_executed.load(Ordering::Relaxed)
    }

    /// Cumulative count of Draw / DrawIndexed records the
    /// walker decoded but skipped because no Tier-2 pipeline
    /// was bound.
    pub fn draws_skipped(&self) -> u64 {
        self.draws_skipped.load(Ordering::Relaxed)
    }

    /// Read back a buffer's bytes (clone). Returns `None` if
    /// the buffer isn't registered. Used by tests + by the
    /// D.3 draw-walker to source vertex / index data.
    pub fn read_buffer_bytes(&self, buffer_id: ResourceId) -> Option<Vec<u8>> {
        let buffers = self.buffers.lock().unwrap();
        buffers.get(&buffer_id.raw()).map(|b| b.bytes.clone())
    }

    /// Associate a pipeline ResourceId with a Tier-2
    /// fragment shader. When `submit_frame` later sees a
    /// `BindPipeline` of this id followed by a draw, it
    /// fires `run_fragment_shader_into` for the active
    /// renderpass's target image.
    pub fn bind_pipeline(
        &self,
        pipeline_id: ResourceId,
        shader_id: Tier2ShaderId,
    ) {
        self.pipeline_shaders.lock().unwrap()
            .insert(pipeline_id.raw(), shader_id);
    }

    /// How many `submit_frame` calls have arrived. Useful
    /// for tests + diagnostics.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }

    /// How many `present` calls have arrived.
    pub fn present_count(&self) -> u64 {
        self.presents.load(Ordering::Relaxed)
    }

    /// Run a compiled fragment shader once per pixel of
    /// `image_id`, writing the result into the backend's
    /// image storage. The shader id and image must both
    /// have been registered (via `Tier2Registry::register`
    /// and `Backend::image_created` respectively).
    pub fn run_fragment_shader_into(
        &self,
        image_id: ResourceId,
        shader_id: Tier2ShaderId,
        push_constants: &[u8],
        uniforms: &[u8],
    ) -> Result<(), Tier2ExecError> {
        let mut images = self.images.lock().unwrap();
        let img = images.get_mut(&(image_id.raw() as u64))
            .ok_or(Tier2ExecError::UnknownShader(shader_id))?;
        self.registry.fill_image_fragment(
            shader_id,
            push_constants, uniforms,
            img.width, img.height,
            &mut img.pixels,
        )
    }

    /// Associate a pipeline ResourceId with a vertex-input
    /// layout (mirrors `bind_pipeline` but for geometry state).
    pub fn bind_layout(&self, pipeline_id: ResourceId, layout: VertexInputState) {
        self.pipeline_layouts.lock().unwrap()
            .insert(pipeline_id.raw(), layout);
    }

    /// Snapshot of the per-vertex bytes the most recent Draw
    /// assembled. `None` if no Draw has dispatched yet (or the
    /// last Draw was rejected before assembly).
    pub fn last_assembled_vertices(&self) -> Option<AssembledVertices> {
        self.last_assembled_vertices.lock().unwrap().clone()
    }

    /// Read back a registered image's RGBA8 pixels.
    /// `None` if the image isn't registered.
    pub fn read_image_pixels(&self, image_id: ResourceId) -> Option<Vec<u8>> {
        let images = self.images.lock().unwrap();
        images.get(&(image_id.raw() as u64)).map(|img| img.pixels.clone())
    }

    /// Run the per-pass state machine over `pass_bytes`. Walks
    /// every FrameOp record between BeginRenderPass and
    /// EndRenderPass, maintains bound state, and dispatches
    /// Draw / DrawIndexed against the bound Tier-2 shader.
    ///
    /// Legacy compat: if a Tier-2 pipeline is bound but no
    /// Draw is issued in this pass, fall back to a fullscreen
    /// FS fill at EndRenderPass (preserves the pre-D.3
    /// "BindPipeline implies fullscreen" shape used by
    /// existing integration tests; D.5+ migrates them to real
    /// Draw records).
    fn execute_pass(&self, target_id: ResourceId, pass_bytes: &[u8]) {
        let pipeline_shaders = self.pipeline_shaders.lock().unwrap().clone();
        let mut state = PassState::default();
        let mut decoder = FrameDecoder::new(pass_bytes);

        loop {
            let rec = match decoder.next() {
                Ok(Some(r)) => r,
                Ok(None) => break,
                Err(e) => {
                    log::warn!("Tier2Backend::execute_pass: \
                                decoder error on target {target_id}: {e}");
                    break;
                }
            };
            let (op, body) = rec;
            match op {
                FrameOp::BindPipeline => {
                    if body.len() >= 4 {
                        let raw = u32::from_le_bytes([
                            body[0], body[1], body[2], body[3]
                        ]);
                        state.pipeline_raw  = Some(raw);
                        state.tier2_shader  = pipeline_shaders.get(&raw).copied();
                    } else {
                        log::warn!("BindPipeline body too short ({} bytes)", body.len());
                    }
                }
                FrameOp::BindVertexBuf => match BindVertexBufCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.vertex_buffers.insert(cmd.binding, BoundVertexBuffer {
                            buffer_raw: cmd.buffer_id,
                            offset: cmd.offset,
                        });
                    }
                    Err(e) => log::warn!("malformed BindVertexBuf: {e}"),
                },
                FrameOp::BindIndexBuf => match BindIndexBufCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.index_buffer = Some(BoundIndexBuffer {
                            buffer_raw: cmd.buffer_id,
                            offset: cmd.offset,
                            index_type: cmd.index_type,
                        });
                    }
                    Err(e) => log::warn!("malformed BindIndexBuf: {e}"),
                },
                FrameOp::SetViewport => match SetViewportCmd::from_bytes(body) {
                    Ok(cmd) => state.viewport = Some(cmd),
                    Err(e) => log::warn!("malformed SetViewport: {e}"),
                },
                FrameOp::PushConstants => {
                    state.push_constants.clear();
                    state.push_constants.extend_from_slice(body);
                }
                FrameOp::Draw => match DrawCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.draws_in_pass = state.draws_in_pass.saturating_add(1);
                        self.dispatch_draw(target_id, &state, cmd);
                    }
                    Err(e) => log::warn!("malformed Draw: {e}"),
                },
                FrameOp::DrawIndexed => match DrawIndexedCmd::from_bytes(body) {
                    Ok(cmd) => {
                        state.draws_in_pass = state.draws_in_pass.saturating_add(1);
                        self.dispatch_draw_indexed(target_id, &state, cmd);
                    }
                    Err(e) => log::warn!("malformed DrawIndexed: {e}"),
                },
                FrameOp::EndRenderPass => break,
                // Ops we don't yet act on: SetScissor, BindDescriptors,
                // CopyBufToImg, CopyImgToBuf, Blit, PipelineBarrier,
                // Dispatch{,Indirect}, DrawIndirect, BeginRenderPass
                // (handled by the outer partition step). These will
                // grow handlers in later phases as needed.
                _ => {}
            }
        }

        // Legacy fallback: pre-D.3 tests bind a Tier-2 pipeline
        // and expect a fullscreen FS fill with no Draw. Preserve
        // that until D.5+ migrates them to real Draw records.
        if state.draws_in_pass == 0 {
            if let Some(shader_id) = state.tier2_shader {
                if let Err(e) = self.run_fragment_shader_into(
                    target_id, shader_id, &state.push_constants, &[])
                {
                    log::warn!("Tier2Backend: legacy fullscreen FS fill \
                                into {target_id} failed: {e}");
                }
            }
        }
    }

    /// Dispatch a `Draw` against the current pass state. D.4:
    /// looks up the bound pipeline's vertex-input layout, gathers
    /// per-vertex attribute bytes from each bound vertex buffer
    /// per the layout, packs them densely (location-order) into
    /// `last_assembled_vertices`. D.5 turns the packed bytes into
    /// `fill_image_triangle` calls.
    fn dispatch_draw(
        &self,
        target_id: ResourceId,
        state: &PassState,
        cmd: DrawCmd,
    ) {
        if state.tier2_shader.is_none() {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
            log::debug!("Draw on target {target_id} skipped: no Tier-2 pipeline bound");
            return;
        }
        if cmd.vertex_count == 0 {
            log::debug!("Draw on target {target_id} skipped: vertex_count=0");
            return;
        }

        let pipeline_raw = match state.pipeline_raw {
            Some(p) => p,
            None => {
                log::warn!("Draw on target {target_id}: tier2 shader bound but \
                            no pipeline id recorded (impossible state)");
                return;
            }
        };
        let layout = match self.pipeline_layouts.lock().unwrap().get(&pipeline_raw).cloned() {
            Some(l) => l,
            None => {
                log::warn!("Draw on target {target_id}: pipeline {pipeline_raw:#x} \
                            has no vertex-input layout; skipping");
                return;
            }
        };

        let assembled = match self.assemble_vertices(
            &layout, &state.vertex_buffers, cmd.first_vertex, cmd.vertex_count,
        ) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("Draw on target {target_id}: vertex assembly failed: {e}");
                return;
            }
        };

        *self.last_assembled_vertices.lock().unwrap() = Some(assembled);
        self.draws_executed.fetch_add(1, Ordering::Relaxed);
        log::debug!("Draw target={target_id} verts={} inst={} first_vert={} \
                     attrs={} stride={}",
                    cmd.vertex_count, cmd.instance_count, cmd.first_vertex,
                    layout.attributes.len(),
                    layout.bindings.iter().map(|b| b.stride).sum::<u32>());
    }

    /// Gather per-vertex attribute bytes for `vertex_count`
    /// vertices starting at `first_vertex`, sourcing each
    /// attribute from the bound buffer at `attr.binding`,
    /// offset `vertex_buffers[binding].offset + first_vertex *
    /// binding_stride + attr.offset`. The output packs attributes
    /// in shader-location order with no padding between them.
    fn assemble_vertices(
        &self,
        layout: &VertexInputState,
        vertex_buffers: &HashMap<u32, BoundVertexBuffer>,
        first_vertex: u32,
        vertex_count: u32,
    ) -> Result<AssembledVertices, String> {
        // Order attributes by shader location so the packed
        // record is shader-input-order (varying-friendly).
        let mut attrs: Vec<_> = layout.attributes.iter().collect();
        attrs.sort_by_key(|a| a.location);

        let mut out_offsets = Vec::with_capacity(attrs.len() + 1);
        let mut out_stride: u32 = 0;
        for a in &attrs {
            out_offsets.push(out_stride);
            out_stride += a.format.byte_size() as u32;
        }
        out_offsets.push(out_stride);

        // Per-binding source data.
        let buffers = self.buffers.lock().unwrap();
        let mut bytes = vec![0u8; (vertex_count as usize) * (out_stride as usize)];

        for v in 0..vertex_count {
            let global_v = first_vertex + v;
            let out_base = (v as usize) * (out_stride as usize);
            for (ai, a) in attrs.iter().enumerate() {
                let bind = layout.bindings.iter().find(|b| b.binding == a.binding)
                    .ok_or_else(|| format!(
                        "no binding desc for slot {}", a.binding))?;
                let slot = vertex_buffers.get(&a.binding)
                    .ok_or_else(|| format!(
                        "vertex buffer not bound at slot {}", a.binding))?;
                let src = buffers.get(&slot.buffer_raw).ok_or_else(|| format!(
                    "vertex buffer {:#x} not in backend storage", slot.buffer_raw))?;
                let src_off = (slot.offset as usize)
                    + (global_v as usize) * (bind.stride as usize)
                    + (a.offset as usize);
                let size = a.format.byte_size();
                let src_end = src_off + size;
                if src_end > src.bytes.len() {
                    return Err(format!(
                        "attribute @location {} (vertex {}) reads bytes \
                         {}..{} past buffer end {}",
                        a.location, global_v, src_off, src_end,
                        src.bytes.len()));
                }
                let out_off = out_base + (out_offsets[ai] as usize);
                bytes[out_off..out_off + size]
                    .copy_from_slice(&src.bytes[src_off..src_end]);
            }
        }

        Ok(AssembledVertices {
            vertex_count, stride: out_stride,
            attribute_offsets: out_offsets, bytes,
        })
    }

    /// Dispatch a `DrawIndexed` against the current pass state.
    /// D.3 stub; same shape as `dispatch_draw`. D.8 wires the
    /// index buffer slice.
    fn dispatch_draw_indexed(
        &self,
        target_id: ResourceId,
        state: &PassState,
        cmd: DrawIndexedCmd,
    ) {
        if state.tier2_shader.is_none() {
            self.draws_skipped.fetch_add(1, Ordering::Relaxed);
            log::debug!("DrawIndexed on target {target_id} skipped: no Tier-2 pipeline bound");
            return;
        }
        if cmd.index_count == 0 {
            return;
        }
        if state.index_buffer.is_none() {
            log::warn!("DrawIndexed on target {target_id}: no index buffer bound");
            return;
        }
        self.draws_executed.fetch_add(1, Ordering::Relaxed);
        log::debug!("DrawIndexed target={target_id} idx={} inst={} first_idx={} \
                     vertex_offset={} bindings={}",
                    cmd.index_count, cmd.instance_count, cmd.first_index,
                    cmd.vertex_offset, state.vertex_buffers.len());
    }
}

impl Backend for Tier2Backend {
    fn identity(&self) -> BackendId {
        BackendId::new(GpuVendor::Software, 2)
    }
    fn caps(&self) -> u64 { 0 }
    fn max_frame_bytes(&self) -> u32 { 1 << 20 }
    fn max_fences_inflight(&self) -> u32 { 16 }

    fn allocate_memory(&self, _size: u64, _usage: u8) -> [u8; 32] {
        let n = self.submissions.load(Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xC2;
        tok
    }

    fn image_created(&self, image_id: ResourceId, width: u32, height: u32) {
        if width == 0 || height == 0 { return; }
        const MAX_DIM: u32 = 16 * 1024;
        if width > MAX_DIM || height > MAX_DIM { return; }
        let pixels = vec![0u8; (width as usize) * (height as usize) * 4];
        self.images.lock().unwrap().insert(image_id.raw() as u64, ImageStorage {
            width, height, pixels,
        });
    }

    fn image_destroyed(&self, image_id: ResourceId) {
        self.images.lock().unwrap().remove(&(image_id.raw() as u64));
    }

    fn buffer_created(&self, buffer_id: ResourceId, size: u64) {
        // Cap at 256 MiB to bound a misbehaving guest. Real
        // limits are negotiated via OP_GPU_HANDSHAKE in a
        // later phase; this is a safety floor for bring-up.
        const MAX_BYTES: u64 = 256 * 1024 * 1024;
        if size == 0 || size > MAX_BYTES {
            log::warn!("Tier2Backend::buffer_created: buffer {buffer_id} \
                        size {size} out of range (max {MAX_BYTES})");
            return;
        }
        self.buffers.lock().unwrap().insert(buffer_id.raw(), BufferStorage {
            size, bytes: vec![0u8; size as usize],
        });
    }

    fn buffer_destroyed(&self, buffer_id: ResourceId) {
        self.buffers.lock().unwrap().remove(&buffer_id.raw());
    }

    fn buffer_write_bytes(
        &self,
        buffer_id: ResourceId,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), String> {
        let mut buffers = self.buffers.lock().unwrap();
        let buf = buffers.get_mut(&buffer_id.raw())
            .ok_or_else(|| format!("buffer {buffer_id} not registered"))?;
        let end = offset.checked_add(bytes.len() as u64)
            .ok_or_else(|| "buffer write offset+len overflows u64".to_string())?;
        if end > buf.size {
            return Err(format!(
                "buffer write end {end} exceeds size {}", buf.size,
            ));
        }
        let off = offset as usize;
        buf.bytes[off..off + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    fn submit_frame(
        &self,
        _fence_id: ResourceId,
        _timeline: u64,
        frame_buf: &[u8],
    ) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);

        // Partition into renderpasses (reuses the shared
        // helper used by SoftwareBackend).
        let passes = match crate::backend::partition_renderpasses(frame_buf) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("Tier2Backend::submit_frame: partition error: {e}");
                return true;
            }
        };

        for pass in &passes {
            let pass_bytes = &frame_buf[pass.byte_range.clone()];
            self.execute_pass(pass.target_id, pass_bytes);
        }
        true
    }

    fn present(
        &self,
        _image_id: ResourceId,
        _surface_id: u64,
        _frame_id: u64,
    ) {
        self.presents.fetch_add(1, Ordering::Relaxed);
    }

    fn bind_pipeline_tier2(
        &self,
        pipeline_id: ResourceId,
        tier2_shader_id: Tier2ShaderId,
    ) {
        self.bind_pipeline(pipeline_id, tier2_shader_id);
    }

    fn bind_pipeline_layout(
        &self,
        pipeline_id: ResourceId,
        layout: VertexInputState,
    ) {
        self.bind_layout(pipeline_id, layout);
    }
}
