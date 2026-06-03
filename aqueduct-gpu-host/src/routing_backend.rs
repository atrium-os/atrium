//! `RoutingBackend` — the dual-backend dispatch layer (stage 2 of acting on
//! the verdict, `docs/spec/energy-policy.md`). It owns a Tier-2 (CPU) and a
//! Tier-3 (GPU) backend and dispatches each frame's `submit_frame` to the
//! tier the [`RoutingPolicy`] assigns — gated on per-pipeline tier-
//! equivalence certification, with single-homed readback.
//!
//! This is the first *behaviour-changing* layer (everything below it only
//! observed). It is meant to sit behind a flag and a certification step:
//! until a surface's pipelines are certified, `RoutingPolicy` pins it to the
//! home tier, so dispatch degrades to "render where you always did".
//!
//! Resources are **single-homed**: ops are recorded against their resource
//! (via [`crate::residency::ResidencyTracker`]) and materialised onto a tier
//! only when a frame dispatched there needs them — so a CPU-routed surface
//! never uploads to VRAM. The frame's resource set ([`crate::frame_resources`])
//! is materialised before dispatch; a frame that can't be fully introspected
//! falls back to whole-world materialisation (a missed texture is a wrong
//! pixel).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aqueduct_gpu::ids::ResourceId;
use aqueduct_gpu::backends::BackendId;

use crate::backend::Backend;

/// State-slot ids for latest-wins pipeline/image properties in the residency
/// log (so re-emitted state collapses instead of growing the log).
mod slot {
    pub const FORMAT: u8 = 0;
    pub const RASTER: u8 = 1;
    pub const LAYOUT: u8 = 2;
    pub const TIER2: u8 = 3;
    pub const TIER2_VS: u8 = 4;
    pub const VS_VARYING: u8 = 5;
    pub const FS_LOD: u8 = 6;
    pub const SAMPLE_COUNT: u8 = 7;
    pub const FS_DERIV: u8 = 8;
    pub const FS_DEPTH: u8 = 9;
}

/// Consecutive stable records before a Tier-2 surface counts as "settled".
const SETTLE_THRESHOLD: u32 = 16;
/// Once settled on Tier-2, re-score only every Nth frame (between ticks,
/// dispatch to the held assignment without paying the verdict).
const DECIMATE_EVERY: u64 = 8;
/// Per-channel tolerance (0..=255) for auto-certification's runtime
/// differential — a couple of LSBs to absorb sRGB / float→unorm rounding
/// between the CPU rasteriser and the GPU. Gross divergence still fails.
const AUTO_CERT_TOLERANCE: u8 = 4;
use crate::cost_model::{shader_cost, DeviceProfile, ShaderCost};
use crate::router::{
    tier2_exec_cost, tier3_exec_cost, Cost, CpuProfile, FrameRouter, GpuPowerModel, RouteMode,
    RoutingPolicy, Tier,
};

/// Owns two backends and routes per surface.
pub struct RoutingBackend {
    t2: Arc<dyn Backend>,
    t3: Arc<dyn Backend>,
    profile: DeviceProfile,
    cpu: CpuProfile,
    router: Mutex<FrameRouter>,
    policy: Mutex<RoutingPolicy>,
    pipelines: Mutex<HashMap<u32, (ShaderCost, ShaderCost)>>,
    images: Mutex<HashMap<u32, (u32, u32)>>,
    /// buffer id → size, to classify a full-coverage write as FullWrite
    /// (so a fully-rewritten dynamic buffer collapses in the retain-log).
    buffer_sizes: Mutex<HashMap<u32, u64>>,
    frames: AtomicU64,
    /// Frames for which a full routing verdict was computed (the surface
    /// was certification-eligible, so the decision could actually change
    /// where it ran).
    scored: AtomicU64,
    /// Frames whose verdict was *skipped* because the surface is pinned
    /// (uncertified) — it dispatches home regardless, so scoring would burn
    /// cycles for nothing. The router doesn't pay to decide what it can't
    /// act on.
    skipped: AtomicU64,
    /// Bring-up shortcut: when set, a created pipeline is certified
    /// tier-equivalent on sight (no probe). Lets switching be exercised
    /// end-to-end before real per-pipeline certification (probe / offline
    /// differential) is wired into dispatch. NOT the production safety gate.
    trust_all: bool,
    /// Production gate: when set, the *first* frame that uses an uncertified
    /// pipeline AND copies its render target to a readback buffer triggers a
    /// real differential certification (render on both tiers, compare the
    /// readback). The pipeline is then `Certified` or `Failed` for real — no
    /// trust shortcut. A `Failed` pipeline is never re-attempted; its surface
    /// stays pinned to home. This is the dispatch-side re-certification
    /// trigger of `docs/spec/gpu-driver-hotswap.md` (precondition #2).
    auto_certify: bool,
    /// Per-resource residency: resource ops are *recorded* here rather than
    /// mirrored to both backends, and materialised onto a tier only when a
    /// frame dispatched there needs them. So a CPU-routed surface never
    /// uploads its resources to the GPU.
    residency: Mutex<crate::residency::ResidencyTracker>,
}

impl RoutingBackend {
    /// Build a router over `t2` (CPU) and `t3` (GPU). `profile` is the
    /// Tier-3 device (drives exec cost + topology-aware migration); `cpu`
    /// the Tier-2 cost side; `power`/`mode` the GPU residency model + policy.
    pub fn new(
        t2: Arc<dyn Backend>,
        t3: Arc<dyn Backend>,
        profile: DeviceProfile,
        cpu: CpuProfile,
        power: GpuPowerModel,
        mode: RouteMode,
    ) -> Self {
        let router = FrameRouter::new(cpu, profile.clone(), power, mode);
        // Home = Tier-2 (the always-available CPU); topology-aware migration.
        let policy = RoutingPolicy::for_profile(&profile, 0.3, 0.1, Tier::Tier2);
        RoutingBackend {
            t2,
            t3,
            profile,
            cpu,
            router: Mutex::new(router),
            policy: Mutex::new(policy),
            pipelines: Mutex::new(HashMap::new()),
            images: Mutex::new(HashMap::new()),
            buffer_sizes: Mutex::new(HashMap::new()),
            frames: AtomicU64::new(0),
            scored: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            trust_all: false,
            auto_certify: false,
            residency: Mutex::new(crate::residency::ResidencyTracker::new()),
        }
    }

    /// Bring-up: certify every pipeline on creation (no probe) so surfaces
    /// become migration-eligible immediately. A shortcut to exercise
    /// switching end-to-end; the real gate is per-pipeline certification.
    pub fn with_trusted_tiers(mut self) -> Self {
        self.trust_all = true;
        self
    }

    /// Production gate: auto-certify each pipeline the first time a frame uses
    /// it and copies its target to a readback buffer (render on both tiers,
    /// compare). Replaces [`Self::with_trusted_tiers`] with a real
    /// pixel-equivalence check. Mutually sensible only without `trust_all`.
    pub fn with_auto_certify(mut self) -> Self {
        self.auto_certify = true;
        self
    }

    /// Record a `kind`-tagged resource op against `resource` (applied now to
    /// any tier it's already resident on; replayed on materialise). The kind
    /// drives retain-log bounding (see [`crate::residency::OpKind`]).
    fn record(
        &self,
        resource: u32,
        kind: crate::residency::OpKind,
        op: impl Fn(&dyn Backend) + Send + Sync + 'static,
    ) {
        self.residency.lock().unwrap().record(resource, kind, Box::new(op), &*self.t2, &*self.t3);
    }

    /// Destroy `resource` on whichever tiers hold it, then forget it.
    fn destroy(&self, resource: u32, on_tier: impl Fn(&dyn Backend)) {
        let mut res = self.residency.lock().unwrap();
        if res.is_resident(resource, Tier::Tier2) { on_tier(&*self.t2); }
        if res.is_resident(resource, Tier::Tier3) { on_tier(&*self.t3); }
        res.forget(resource);
    }

    /// `(t2, t3)` resident-resource counts — observability for the single-
    /// homing win (an unused tier should stay near zero).
    pub fn residency_counts(&self) -> (usize, usize) {
        self.residency.lock().unwrap().resident_counts()
    }

    /// Retained-op count for `resource` — observability for retain-log
    /// bounding (stays small under repeated full re-uploads).
    pub fn resource_op_count(&self, resource: ResourceId) -> usize {
        self.residency.lock().unwrap().op_count(resource.raw())
    }

    /// `(scored, skipped)` frame counts — the router's own decision
    /// overhead, made observable. `skipped` frames paid no verdict cost
    /// (pinned surfaces dispatch home unconditionally). A high skipped:scored
    /// ratio means the router is spending almost nothing to decide.
    pub fn decision_stats(&self) -> (u64, u64) {
        (self.scored.load(Ordering::Relaxed), self.skipped.load(Ordering::Relaxed))
    }

    /// Record a pipeline's tier-equivalence certification directly (e.g.
    /// from the offline differential oracle).
    pub fn certify(&self, pipeline: ResourceId, c: crate::certify::Certification) {
        self.policy.lock().unwrap().certify(pipeline.raw(), c);
    }

    /// A pipeline's current tier-equivalence status — observability for the
    /// certification gate (e.g. the daemon logging which pipelines are proven
    /// migratable vs. pinned).
    pub fn pipeline_certification(&self, pipeline: ResourceId) -> crate::certify::Certification {
        self.policy.lock().unwrap().pipeline_status(pipeline.raw())
    }

    /// Certify `pipeline` by rendering `probe_frame` on *both* backends and
    /// comparing the readback from `readback_buf` (a smoke differential).
    /// Records + returns the result; on success the pipeline's surfaces
    /// become migration-eligible. The probe's resources must already be
    /// created (the router mirrors them to both backends).
    pub fn certify_pipeline(
        &self,
        pipeline: ResourceId,
        probe_frame: &[u8],
        readback_buf: ResourceId,
        readback_size: u64,
        tolerance: u8,
    ) -> crate::certify::Certification {
        let c = self.differential_probe(probe_frame, readback_buf, readback_size, tolerance);
        self.policy.lock().unwrap().certify(pipeline.raw(), c);
        c
    }

    /// Render `probe_frame` on *both* tiers and compare the `readback_buf`
    /// readback — the shared core of explicit and automatic certification.
    /// Materialises only the probe's own resources on both tiers (proving a
    /// pipeline equivalent must not drag a surface's textures onto the GPU);
    /// falls back to the whole world when the resource set is incomplete.
    fn differential_probe(
        &self,
        probe_frame: &[u8],
        readback_buf: ResourceId,
        readback_size: u64,
        tolerance: u8,
    ) -> crate::certify::Certification {
        {
            let fr = crate::frame_resources::frame_resources(probe_frame);
            let mut res = self.residency.lock().unwrap();
            if fr.complete {
                let ids: Vec<u32> =
                    fr.all_ids().chain(std::iter::once(readback_buf.raw())).collect();
                res.materialize(ids.iter().copied(), Tier::Tier2, &*self.t2);
                res.materialize(ids, Tier::Tier3, &*self.t3);
            } else {
                res.materialize_all(Tier::Tier2, &*self.t2);
                res.materialize_all(Tier::Tier3, &*self.t3);
            }
        }
        crate::certify::differential_certify(
            &*self.t2, &*self.t3, probe_frame, readback_buf, readback_size, tolerance,
        )
    }

    /// The auto-certification trigger: certify every still-`Uncertified`
    /// pipeline in `pipes` by treating the client's own `frame_buf` (which
    /// renders them and copies the target to `readback_buf`) as the probe.
    /// One differential render serves the whole frame; its verdict is recorded
    /// for each uncertified pipeline. `Failed` / already-`Certified` pipelines
    /// are not passed here, so a failure is never re-run.
    fn auto_certify_frame(
        &self,
        frame_buf: &[u8],
        pipes: &[u32],
        readback_buf: ResourceId,
        readback_size: u64,
        tolerance: u8,
    ) -> crate::certify::Certification {
        let c = self.differential_probe(frame_buf, readback_buf, readback_size, tolerance);
        let mut policy = self.policy.lock().unwrap();
        for &p in pipes {
            policy.certify(p, c);
        }
        log::info!(
            "routing: auto-certified {} pipeline(s) via frame probe → {:?}",
            pipes.len(), c);
        c
    }

    /// `(tier2, tier3)` effective surface-assignment counts.
    pub fn assignment_counts(&self) -> (usize, usize) {
        self.policy.lock().unwrap().effective_counts()
    }
}

impl Backend for RoutingBackend {
    // ── Identity: report the GPU (Tier-3) backend; caps = the intersection
    //    so a client never asks for something one tier lacks. ────────────
    fn identity(&self) -> BackendId { self.t3.identity() }
    fn caps(&self) -> u64 { self.t2.caps() & self.t3.caps() }
    fn max_frame_bytes(&self) -> u32 { self.t2.max_frame_bytes().min(self.t3.max_frame_bytes()) }
    fn max_fences_inflight(&self) -> u32 {
        self.t2.max_fences_inflight().min(self.t3.max_fences_inflight())
    }
    fn allocate_memory(&self, size: u64, usage: u8) -> [u8; 32] {
        let _ = self.t2.allocate_memory(size, usage);
        self.t3.allocate_memory(size, usage)
    }

    // ── Resource lifecycle: RECORD (materialised lazily per tier). ────
    fn image_created(&self, id: ResourceId, w: u32, h: u32) {
        self.images.lock().unwrap().insert(id.raw(), (w, h));
        self.record(id.raw(), crate::residency::OpKind::Create, move |b| b.image_created(id, w, h));
    }
    fn image_created_layered(&self, id: ResourceId, w: u32, h: u32, layers: u32) {
        self.images.lock().unwrap().insert(id.raw(), (w, h));
        self.record(id.raw(), crate::residency::OpKind::Create, move |b| b.image_created_layered(id, w, h, layers));
    }
    fn set_image_format(&self, id: ResourceId, fmt: u32) {
        self.record(id.raw(), crate::residency::OpKind::State(slot::FORMAT), move |b| b.set_image_format(id, fmt));
    }
    fn image_destroyed(&self, id: ResourceId) {
        self.images.lock().unwrap().remove(&id.raw());
        self.destroy(id.raw(), move |b| b.image_destroyed(id));
    }
    fn depth_image_created(&self, id: ResourceId, w: u32, h: u32) {
        self.record(id.raw(), crate::residency::OpKind::Create, move |b| b.depth_image_created(id, w, h));
    }
    fn depth_image_destroyed(&self, id: ResourceId) {
        self.destroy(id.raw(), move |b| b.depth_image_destroyed(id));
    }
    fn buffer_created(&self, id: ResourceId, size: u64) {
        self.buffer_sizes.lock().unwrap().insert(id.raw(), size);
        self.record(id.raw(), crate::residency::OpKind::Create, move |b| b.buffer_created(id, size));
    }
    fn buffer_destroyed(&self, id: ResourceId) {
        self.buffer_sizes.lock().unwrap().remove(&id.raw());
        self.destroy(id.raw(), move |b| b.buffer_destroyed(id));
    }
    #[allow(clippy::too_many_arguments)]
    fn sampler_created(
        &self, id: ResourceId, min_f: u8, mag_f: u8, mip_f: u8,
        addr: [u8; 3], aniso: f32, min_lod: f32, max_lod: f32,
        cmp_en: u8, cmp_op: u32,
    ) {
        self.record(id.raw(), crate::residency::OpKind::Create, move |b| b.sampler_created(
            id, min_f, mag_f, mip_f, addr, aniso, min_lod, max_lod, cmp_en, cmp_op));
    }
    fn sampler_destroyed(&self, id: ResourceId) {
        self.destroy(id.raw(), move |b| b.sampler_destroyed(id));
    }
    fn pipeline_created(&self, id: ResourceId, vs: &[u8], fs: &[u8]) {
        self.pipelines.lock().unwrap().insert(id.raw(), (shader_cost(vs), shader_cost(fs)));
        if self.trust_all {
            self.policy.lock().unwrap().certify(id.raw(), crate::certify::Certification::Certified);
        }
        let (vs, fs) = (vs.to_vec(), fs.to_vec()); // own for deferred replay
        self.record(id.raw(), crate::residency::OpKind::Create, move |b| b.pipeline_created(id, &vs, &fs));
    }
    fn present(&self, id: ResourceId, surface: u64, frame: u64) {
        // Presentation isn't a resource upload — forward to both (the tier
        // without the image no-ops).
        self.t2.present(id, surface, frame);
        self.t3.present(id, surface, frame);
    }
    fn bind_pipeline_tier2(&self, p: ResourceId, s: crate::Tier2ShaderId) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::TIER2), move |b| b.bind_pipeline_tier2(p, s));
    }
    fn bind_pipeline_tier2_vs(&self, p: ResourceId, s: crate::Tier2ShaderId) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::TIER2_VS), move |b| b.bind_pipeline_tier2_vs(p, s));
    }
    fn bind_pipeline_layout(&self, p: ResourceId, l: aqueduct_gpu::VertexInputState) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::LAYOUT), move |b| b.bind_pipeline_layout(p, l.clone()));
    }
    #[allow(clippy::too_many_arguments)]
    fn bind_pipeline_raster_state(
        &self, p: ResourceId,
        depth: Option<aqueduct_gpu::Tier2DepthState>,
        blend: Option<aqueduct_gpu::Tier2BlendState>,
        blend_extra: &[aqueduct_gpu::Tier2BlendState],
        raster: Option<aqueduct_gpu::Tier2RasterState>,
        topo: aqueduct_gpu::Tier2PrimitiveTopology,
        stencil: Option<aqueduct_gpu::Tier2StencilState>,
        prim_restart: bool,
    ) {
        let blend_extra = blend_extra.to_vec();
        self.record(p.raw(), crate::residency::OpKind::State(slot::RASTER), move |b| b.bind_pipeline_raster_state(
            p, depth, blend, &blend_extra, raster, topo, stencil, prim_restart));
    }
    fn bind_pipeline_vs_varying_bytes(&self, p: ResourceId, b: u32) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::VS_VARYING), move |be| be.bind_pipeline_vs_varying_bytes(p, b));
    }
    fn bind_pipeline_fs_implicit_lod(&self, p: ResourceId, v: bool) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::FS_LOD), move |b| b.bind_pipeline_fs_implicit_lod(p, v));
    }
    fn bind_pipeline_sample_count(&self, p: ResourceId, n: u32) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::SAMPLE_COUNT), move |b| b.bind_pipeline_sample_count(p, n));
    }
    fn bind_pipeline_fs_derivatives(&self, p: ResourceId, v: bool) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::FS_DERIV), move |b| b.bind_pipeline_fs_derivatives(p, v));
    }
    fn bind_pipeline_fs_writes_depth(&self, p: ResourceId, v: bool) {
        self.record(p.raw(), crate::residency::OpKind::State(slot::FS_DEPTH), move |b| b.bind_pipeline_fs_writes_depth(p, v));
    }

    // ── Data movement: writes are RECORDED (replayed on materialise; and
    //    applied now to any live tier). Reads follow the writer. ─────────
    fn image_write_pixels(&self, id: ResourceId, row_pitch: u32, pixels: &[u8]) -> Result<(), String> {
        let pixels = pixels.to_vec();
        self.record(id.raw(), crate::residency::OpKind::FullWrite, move |b| {
            if let Err(e) = b.image_write_pixels(id, row_pitch, &pixels) {
                log::warn!("routing: deferred image_write_pixels: {e}");
            }
        });
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn image_write_region_pixels(
        &self, id: ResourceId, dx: u32, dy: u32, w: u32, h: u32, row_pitch: u32, pixels: &[u8],
    ) -> Result<(), String> {
        // A region covering the whole image is a full overwrite → collapse.
        let covers_all = dx == 0 && dy == 0
            && self.images.lock().unwrap().get(&id.raw())
                .map(|&(iw, ih)| w >= iw && h >= ih).unwrap_or(false);
        let kind = if covers_all {
            crate::residency::OpKind::FullWrite
        } else {
            crate::residency::OpKind::Write
        };
        let pixels = pixels.to_vec();
        self.record(id.raw(), kind, move |b| {
            if let Err(e) = b.image_write_region_pixels(id, dx, dy, w, h, row_pitch, &pixels) {
                log::warn!("routing: deferred image_write_region_pixels: {e}");
            }
        });
        Ok(())
    }
    fn buffer_write_bytes(&self, id: ResourceId, offset: u64, bytes: &[u8]) -> Result<(), String> {
        // A write from offset 0 spanning the whole buffer is a full overwrite
        // → collapse (the common dynamic-uniform-buffer rewrite).
        let covers_all = offset == 0
            && self.buffer_sizes.lock().unwrap().get(&id.raw())
                .map(|&size| bytes.len() as u64 >= size).unwrap_or(false);
        let kind = if covers_all {
            crate::residency::OpKind::FullWrite
        } else {
            crate::residency::OpKind::Write
        };
        let bytes = bytes.to_vec();
        self.record(id.raw(), kind, move |b| {
            if let Err(e) = b.buffer_write_bytes(id, offset, &bytes) {
                log::warn!("routing: deferred buffer_write_bytes: {e}");
            }
        });
        Ok(())
    }
    fn buffer_read_bytes(&self, id: ResourceId, offset: u64, size: u64) -> Result<Vec<u8>, String> {
        // Single-homed readback: read from whichever tier last rendered it,
        // materialising the buffer there first if needed.
        let tier = self.policy.lock().unwrap().read_tier(id.raw());
        let be: &dyn Backend = match tier { Tier::Tier2 => &*self.t2, Tier::Tier3 => &*self.t3 };
        self.residency.lock().unwrap().materialize([id.raw()], tier, be);
        be.buffer_read_bytes(id, offset, size)
    }

    fn measured_gpu_time_s(&self) -> Option<f64> { self.t3.measured_gpu_time_s() }

    fn submit_frame(&self, fence: ResourceId, timeline: u64, frame_buf: &[u8]) -> bool {
        // Cheap structural pass: surface (render target), pipelines used,
        // copy-dst buffers, and the per-draw (mix, invocation) tuples. The
        // *expensive* part (per-tier cost float math + the verdict) is
        // deferred until we know it can change the outcome.
        let n = self.frames.fetch_add(1, Ordering::Relaxed) + 1;
        let le4 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let mut surface: Option<u32> = None;
        let mut cur_dims: Option<(u32, u32)> = None;
        let mut cur_mix: Option<(ShaderCost, ShaderCost)> = None;
        let mut pipes_used: Vec<u32> = Vec::new();
        let mut dst_buffers: Vec<u32> = Vec::new();
        // (vs, fs, vs_invocations, fs_pixels).
        let mut draws: Vec<(ShaderCost, ShaderCost, u64, Option<u64>)> = Vec::new();
        {
            let pipelines = self.pipelines.lock().unwrap();
            let images = self.images.lock().unwrap();
            let mut dec = aqueduct_gpu::frame::FrameDecoder::new(frame_buf);
            while let Ok(Some((op, body))) = dec.next() {
                match op {
                    aqueduct_gpu::opcodes::FrameOp::BeginRenderPass if body.len() >= 4 => {
                        let target = le4(body);
                        surface.get_or_insert(target);
                        cur_dims = images.get(&target).copied();
                    }
                    aqueduct_gpu::opcodes::FrameOp::BindPipeline if body.len() >= 4 => {
                        let pid = le4(body);
                        pipes_used.push(pid);
                        cur_mix = pipelines.get(&pid).copied();
                    }
                    aqueduct_gpu::opcodes::FrameOp::Draw if body.len() >= 8 => {
                        if let Some((vs, fs)) = cur_mix {
                            let vs_inv = le4(&body[0..4]) as u64 * (le4(&body[4..8]) as u64).max(1);
                            let px = cur_dims.map(|(w, h)| w as u64 * h as u64);
                            draws.push((vs, fs, vs_inv, px));
                        }
                    }
                    aqueduct_gpu::opcodes::FrameOp::CopyImgToBuf if body.len() >= 8 => {
                        dst_buffers.push(le4(&body[4..8]));
                    }
                    _ => {}
                }
            }
        }
        let surf = surface.unwrap_or(0);

        // Auto-certification trigger: the first frame that uses an uncertified
        // pipeline *and* copies its target to a readback buffer is its own
        // certification probe — render on both tiers, compare. Done before the
        // eligibility check below so a freshly-proven pipeline is eligible this
        // very frame. Only `Uncertified` pipelines are attempted (a `Failed`
        // one is never re-run); needs a readback buffer + known target dims.
        if self.auto_certify && !dst_buffers.is_empty() {
            let uncertified: Vec<u32> = {
                let policy = self.policy.lock().unwrap();
                pipes_used.iter().copied()
                    .filter(|p| policy.pipeline_status(*p)
                        == crate::certify::Certification::Uncertified)
                    .collect()
            };
            if !uncertified.is_empty() {
                let dst = dst_buffers[0];
                let size = self.buffer_sizes.lock().unwrap().get(&dst).copied()
                    .or_else(|| cur_dims.map(|(w, h)| w as u64 * h as u64 * 4))
                    .unwrap_or(0);
                if size > 0 {
                    self.auto_certify_frame(
                        frame_buf, &uncertified, ResourceId(dst), size, AUTO_CERT_TOLERANCE);
                }
            }
        }

        let mut policy = self.policy.lock().unwrap();
        for p in &pipes_used {
            policy.note_surface_pipeline(surf, *p);
        }
        // Pay the verdict cost only when it can change something:
        // - pinned (uncertified) surface → dispatches home unconditionally;
        // - a surface long-settled on Tier-2 (CPU) → re-scoring just confirms
        //   "still home"; decimate it (the GPU is idle, so no power-model
        //   bookkeeping is lost by skipping). Tier-3-settled surfaces are
        //   always scored: the verdict is negligible beside the GPU frame,
        //   and it keeps the residency model fed.
        let eligible = policy.eligible(surf);
        let settled_on_cpu = policy.surface_assignment(surf) == Some(Tier::Tier2)
            && policy.stable_streak(surf) >= SETTLE_THRESHOLD;
        let effective = if !eligible {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            policy.home()
        } else if settled_on_cpu && n % DECIMATE_EVERY != 0 {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            policy.surface_assignment(surf).unwrap_or_else(|| policy.home())
        } else {
            let mut t2 = Cost::default();
            let mut t3 = Cost::default();
            for (vs, fs, vs_inv, px) in &draws {
                t2 = t2.plus(tier2_exec_cost(&self.cpu, vs, *vs_inv));
                t3 = t3.plus(tier3_exec_cost(&self.profile, vs, *vs_inv));
                if let Some(px) = px {
                    t2 = t2.plus(tier2_exec_cost(&self.cpu, fs, *px));
                    t3 = t3.plus(tier3_exec_cost(&self.profile, fs, *px));
                }
            }
            let verdict = self.router.lock().unwrap().route_costs(t2, t3, n as f64 / 60.0).tier;
            self.scored.fetch_add(1, Ordering::Relaxed);
            policy.record_frame(surf, verdict)
        };
        // Single-homed readback bookkeeping: this tier wrote the render
        // target + any copy-destination buffers.
        policy.note_write(surf, effective);
        for b in &dst_buffers {
            policy.note_write(*b, effective);
        }
        drop(policy);

        // Observability: report the per-surface assignment periodically so a
        // live migration is visible in the daemon log (e.g. over Carillon).
        if n % 4 == 0 {
            let (a2, a3) = self.policy.lock().unwrap().effective_counts();
            log::info!(
                "routing: frame {n} → {effective:?}; surfaces tier2={a2} tier3={a3} \
                 (scored={} skipped={})",
                self.scored.load(Ordering::Relaxed), self.skipped.load(Ordering::Relaxed));
        }

        // Materialise the frame's resources on the dispatch tier *before*
        // dispatch — a draw against an unmaterialised resource is a wrong
        // pixel. Use the introspected set when complete; otherwise fall back
        // to the whole world (an undecoded op might reference a texture).
        let be: &dyn Backend = match effective {
            Tier::Tier2 => &*self.t2,
            Tier::Tier3 => &*self.t3,
        };
        {
            let fr = crate::frame_resources::frame_resources(frame_buf);
            let mut res = self.residency.lock().unwrap();
            if fr.complete {
                res.materialize(fr.all_ids(), effective, be);
            } else {
                res.materialize_all(effective, be);
            }
        }

        match effective {
            Tier::Tier2 => self.t2.submit_frame(fence, timeline, frame_buf),
            Tier::Tier3 => self.t3.submit_frame(fence, timeline, frame_buf),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certify::Certification;
    use aqueduct_gpu::backends::GpuVendor;
    use aqueduct_gpu::frame::FrameBuilder;
    use aqueduct_gpu::opcodes::FrameOp;

    /// A backend that records how many frames + reads it received, and
    /// returns a one-byte marker on readback so the test can tell which
    /// tier served it.
    struct Counting {
        marker: u8,
        submits: AtomicU64,
        reads: AtomicU64,
    }
    impl Counting {
        fn new(marker: u8) -> Arc<Self> {
            Arc::new(Counting { marker, submits: AtomicU64::new(0), reads: AtomicU64::new(0) })
        }
    }
    impl Backend for Counting {
        fn identity(&self) -> BackendId { BackendId::new(GpuVendor::Software, 0) }
        fn caps(&self) -> u64 { u64::MAX }
        fn max_frame_bytes(&self) -> u32 { 1 << 20 }
        fn max_fences_inflight(&self) -> u32 { 8 }
        fn allocate_memory(&self, _s: u64, _u: u8) -> [u8; 32] { [0; 32] }
        fn submit_frame(&self, _f: ResourceId, _t: u64, _b: &[u8]) -> bool {
            self.submits.fetch_add(1, Ordering::Relaxed); true
        }
        fn buffer_read_bytes(&self, _id: ResourceId, _o: u64, size: u64) -> Result<Vec<u8>, String> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(vec![self.marker; size as usize])
        }
    }

    fn frame(img: ResourceId, pipe: ResourceId, dst: ResourceId) -> Vec<u8> {
        let mut fb = FrameBuilder::new(4096);
        let mut brp = img.raw().to_le_bytes().to_vec();
        brp.extend_from_slice(&[0, 0, 0, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
        let mut draw = 3u32.to_le_bytes().to_vec();
        draw.extend_from_slice(&1u32.to_le_bytes());
        draw.extend_from_slice(&[0u8; 8]);
        fb.push(FrameOp::Draw, &draw).unwrap();
        let mut cib = img.raw().to_le_bytes().to_vec();
        cib.extend_from_slice(&dst.raw().to_le_bytes());
        cib.extend_from_slice(&[0u8; 8]);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();
        fb.as_bytes().to_vec()
    }

    fn spirv_alu(n: u32) -> Vec<u8> {
        let mut w = vec![0x0723_0203u32, 0x0001_0000, 0, 100, 0];
        for _ in 0..n { w.extend([(5u32 << 16) | 129, 1, 2, 3, 4]); }
        w.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    #[test]
    fn uncertified_surface_dispatches_to_home_tier2_and_mirrors_resources() {
        let t2 = Counting::new(2);
        let t3 = Counting::new(3);
        let rb = RoutingBackend::new(
            t2.clone(), t3.clone(), DeviceProfile::uma_apple_m4_max(),
            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), RouteMode::Perf);
        let (img, pipe, dst) = (ResourceId(1), ResourceId(2), ResourceId(3));
        rb.image_created(img, 64, 64);
        rb.pipeline_created(pipe, &spirv_alu(4), &spirv_alu(4));
        // Heavy verdict notwithstanding, the surface is UNcertified → home.
        for t in 1..=30 { rb.submit_frame(ResourceId(9), t, &frame(img, pipe, dst)); }
        assert_eq!(t2.submits.load(Ordering::Relaxed), 30, "all frames ran on Tier-2 (home)");
        assert_eq!(t3.submits.load(Ordering::Relaxed), 0, "GPU never used while uncertified");
        // And the router spent NO verdict cycles — every frame was skipped
        // because a pinned surface can't act on a verdict anyway.
        assert_eq!(rb.decision_stats(), (0, 30), "no scoring for a pinned surface");
        // THE SINGLE-HOMING WIN: the GPU received zero resource uploads —
        // a CPU-routed surface never materialised on Tier-3.
        let (t2_res, t3_res) = rb.residency_counts();
        assert!(t2_res >= 3, "img + pipeline + buffer resident on the CPU");
        assert_eq!(t3_res, 0, "nothing uploaded to the GPU");
        // Readback follows the writer (Tier-2): marker == 2.
        let px = rb.buffer_read_bytes(dst, 0, 4).unwrap();
        assert_eq!(px, vec![2, 2, 2, 2], "readback from the tier that rendered");
    }

    #[test]
    fn differential_certify_passes_on_match_fails_on_divergence() {
        let probe = frame(ResourceId(1), ResourceId(2), ResourceId(3));
        // Both backends render the same pixels → certified.
        let a = Counting::new(7);
        let b = Counting::new(7);
        assert_eq!(
            crate::certify::differential_certify(&*a, &*b, &probe, ResourceId(3), 16, 0),
            Certification::Certified);
        // Divergent pixels → failed, with the per-channel delta.
        let c = Counting::new(2);
        let d = Counting::new(3);
        assert_eq!(
            crate::certify::differential_certify(&*c, &*d, &probe, ResourceId(3), 16, 0),
            Certification::Failed { max_channel_diff: 1 });
    }

    #[test]
    fn certify_pipeline_unlocks_scoring() {
        // Two backends that render identically → the pipeline certifies, and
        // its surface flips from pinned (skipped) to eligible (scored).
        let t2 = Counting::new(7);
        let t3 = Counting::new(7);
        let rb = RoutingBackend::new(
            t2.clone(), t3.clone(), DeviceProfile::uma_apple_m4_max(),
            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), RouteMode::Perf);
        let (img, pipe, dst) = (ResourceId(1), ResourceId(2), ResourceId(3));
        rb.image_created(img, 64, 64);
        rb.pipeline_created(pipe, &spirv_alu(4), &spirv_alu(4));
        rb.buffer_created(dst, 64);
        let probe = frame(img, pipe, dst);

        // Uncertified → the frame is dispatched home without scoring.
        rb.submit_frame(ResourceId(9), 1, &probe);
        assert_eq!(rb.decision_stats(), (0, 1));

        // Certify via differential probe.
        assert_eq!(rb.certify_pipeline(pipe, &probe, dst, 16, 0), Certification::Certified);

        // Now the surface is eligible → subsequent frames are scored.
        rb.submit_frame(ResourceId(9), 2, &probe);
        assert_eq!(rb.decision_stats(), (1, 1), "certified surface is now scored");
    }

    #[test]
    fn auto_certify_proves_a_pipeline_on_its_first_frame_then_scores() {
        // With `.with_auto_certify()`, the first frame that uses an uncertified
        // pipeline AND copies its target to a buffer certifies it for real
        // (both tiers render identically → match) — no explicit certify call,
        // no trust shortcut. The surface is then eligible *that same frame*.
        let t2 = Counting::new(7);
        let t3 = Counting::new(7); // identical readback → Certified
        let rb = RoutingBackend::new(
            t2.clone(), t3.clone(), DeviceProfile::uma_apple_m4_max(),
            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), RouteMode::Perf)
            .with_auto_certify();
        let (img, pipe, dst) = (ResourceId(1), ResourceId(2), ResourceId(3));
        rb.image_created(img, 64, 64);
        rb.pipeline_created(pipe, &spirv_alu(4), &spirv_alu(4));
        rb.buffer_created(dst, 64);
        let f = frame(img, pipe, dst);

        rb.submit_frame(ResourceId(9), 1, &f);
        assert_eq!(rb.policy.lock().unwrap().pipeline_status(pipe.raw()),
            Certification::Certified, "auto-certified on first copy-bearing frame");
        assert_eq!(rb.decision_stats(), (1, 0),
            "the auto-certified surface was scored on the very first frame");
    }

    #[test]
    fn auto_certify_failure_pins_the_surface_and_never_retries() {
        // Divergent tiers → the differential FAILS. The surface stays pinned
        // to home, and — crucially — a `Failed` pipeline is never re-probed,
        // so later frames pay no differential cost.
        let t2 = Counting::new(2);
        let t3 = Counting::new(40); // |2-40| = 38 ≫ tolerance → Failed
        let rb = RoutingBackend::new(
            t2.clone(), t3.clone(), DeviceProfile::uma_apple_m4_max(),
            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), RouteMode::Perf)
            .with_auto_certify();
        let (img, pipe, dst) = (ResourceId(1), ResourceId(2), ResourceId(3));
        rb.image_created(img, 64, 64);
        rb.pipeline_created(pipe, &spirv_alu(4), &spirv_alu(4));
        rb.buffer_created(dst, 64);
        let f = frame(img, pipe, dst);

        rb.submit_frame(ResourceId(9), 1, &f);
        assert!(matches!(rb.policy.lock().unwrap().pipeline_status(pipe.raw()),
            Certification::Failed { .. }), "divergent tiers fail certification");
        // The differential read each tier exactly once (frame 1's probe).
        let reads_t2 = t2.reads.load(Ordering::Relaxed);
        assert_eq!(reads_t2, 1, "one differential probe ran");

        rb.submit_frame(ResourceId(9), 2, &f);
        assert_eq!(t2.reads.load(Ordering::Relaxed), reads_t2,
            "a Failed pipeline is not re-certified on subsequent frames");
        assert_eq!(rb.decision_stats(), (0, 2),
            "the pinned surface dispatched home both frames, never scored");
    }

    #[test]
    fn settled_cpu_surface_is_decimated_not_rescored_every_frame() {
        // A certified LIGHT surface settles on Tier-2 (home). Once settled,
        // the router stops re-scoring it every frame — it dispatches to the
        // held assignment, paying near-nothing to confirm "still home".
        let t2 = Counting::new(2);
        let t3 = Counting::new(3);
        let rb = RoutingBackend::new(
            t2.clone(), t3.clone(), DeviceProfile::uma_apple_m4_max(),
            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), RouteMode::Perf);
        let (img, pipe, dst) = (ResourceId(1), ResourceId(2), ResourceId(3));
        rb.image_created(img, 32, 32); // light → stays on Tier-2
        rb.pipeline_created(pipe, &spirv_alu(2), &spirv_alu(2));
        rb.buffer_created(dst, 64);
        rb.certify(pipe, Certification::Certified);
        let f = frame(img, pipe, dst);

        for t in 1..=40 { rb.submit_frame(ResourceId(9), t, &f); }
        let (scored, skipped) = rb.decision_stats();
        assert_eq!(scored + skipped, 40);
        // Early frames scored until settled; later frames mostly decimated.
        assert!(scored > 0, "scored while settling");
        assert!(skipped > 0, "decimated once settled — not re-scored every frame");
        // It never left the CPU (light frame), and everything ran on Tier-2.
        assert_eq!(t3.submits.load(Ordering::Relaxed), 0);
        assert_eq!(t2.submits.load(Ordering::Relaxed), 40);
    }

    #[test]
    fn full_buffer_rewrites_stay_bounded_partials_do_not() {
        let t2 = Counting::new(2);
        let t3 = Counting::new(3);
        let rb = RoutingBackend::new(
            t2.clone(), t3.clone(), DeviceProfile::uma_apple_m4_max(),
            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), RouteMode::Perf);
        // A 64-byte dynamic uniform buffer, fully rewritten 10× (offset 0,
        // full span) → classified FullWrite → collapses to create + latest.
        let dyn_buf = ResourceId(0x100);
        rb.buffer_created(dyn_buf, 64);
        for _ in 0..10 { rb.buffer_write_bytes(dyn_buf, 0, &[7u8; 64]).unwrap(); }
        assert_eq!(rb.resource_op_count(dyn_buf), 2, "create + one full write (9 collapsed)");

        // Partial writes (offset != 0) accumulate — can't be collapsed safely.
        let part_buf = ResourceId(0x101);
        rb.buffer_created(part_buf, 64);
        for i in 0..5 { rb.buffer_write_bytes(part_buf, 8 * i + 8, &[1u8; 4]).unwrap(); }
        assert_eq!(rb.resource_op_count(part_buf), 6, "create + five partial writes");
    }

    #[test]
    fn certified_heavy_surface_migrates_to_tier3() {
        let t2 = Counting::new(2);
        let t3 = Counting::new(3);
        let rb = RoutingBackend::new(
            t2.clone(), t3.clone(), DeviceProfile::uma_apple_m4_max(),
            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), RouteMode::Perf);
        let (img, pipe, dst) = (ResourceId(1), ResourceId(2), ResourceId(3));
        rb.image_created(img, 3840, 2160); // heavy → Tier-3 verdict
        rb.pipeline_created(pipe, &spirv_alu(40), &spirv_alu(200));
        rb.certify(pipe, Certification::Certified); // now allowed to migrate
        for t in 1..=40 { rb.submit_frame(ResourceId(9), t, &frame(img, pipe, dst)); }
        assert!(t3.submits.load(Ordering::Relaxed) > 0,
            "a certified heavy surface eventually ran on Tier-3");
        // A certified surface IS scored every frame (it can act on the
        // verdict) — the overhead is paid only where it can pay off.
        assert_eq!(rb.decision_stats(), (40, 0), "certified surface is scored, not skipped");
        // After migration, readback comes from Tier-3 (marker 3).
        assert_eq!(rb.buffer_read_bytes(dst, 0, 2).unwrap(), vec![3, 3]);
        assert_eq!(rb.assignment_counts(), (0, 1));
    }
}
