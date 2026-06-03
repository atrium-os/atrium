//! `RoutingBackend` — the dual-backend dispatch layer (stage 2 of acting on
//! the verdict, `docs/spec/energy-policy.md`). It owns a Tier-2 (CPU) and a
//! Tier-3 (GPU) backend, mirrors resource creation to both so either can
//! render any frame, and dispatches each frame's `submit_frame` to the tier
//! the [`RoutingPolicy`] assigns — gated on per-pipeline tier-equivalence
//! certification, with single-homed readback.
//!
//! This is the first *behaviour-changing* layer (everything below it only
//! observed). It is meant to sit behind a flag and a certification step:
//! until a surface's pipelines are certified, `RoutingPolicy` pins it to the
//! home tier, so dispatch degrades to "render where you always did".
//!
//! Resource handling here mirrors creation to both backends (the simplest
//! correct mechanism). The spec's single-homed-with-replay is a later
//! memory/bandwidth optimisation; correctness (no chatter) comes from the
//! *slow per-surface migration*, not from how resources are homed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aqueduct_gpu::ids::ResourceId;
use aqueduct_gpu::backends::{BackendId, GpuVendor};

use crate::backend::Backend;
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
            frames: AtomicU64::new(0),
            scored: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        }
    }

    /// `(scored, skipped)` frame counts — the router's own decision
    /// overhead, made observable. `skipped` frames paid no verdict cost
    /// (pinned surfaces dispatch home unconditionally). A high skipped:scored
    /// ratio means the router is spending almost nothing to decide.
    pub fn decision_stats(&self) -> (u64, u64) {
        (self.scored.load(Ordering::Relaxed), self.skipped.load(Ordering::Relaxed))
    }

    /// Certify a pipeline as tier-equivalent (gates surface migration).
    pub fn certify(&self, pipeline: ResourceId, c: crate::certify::Certification) {
        self.policy.lock().unwrap().certify(pipeline.raw(), c);
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

    // ── Resource lifecycle: mirror to both backends. ──────────────────
    fn image_created(&self, id: ResourceId, w: u32, h: u32) {
        self.images.lock().unwrap().insert(id.raw(), (w, h));
        self.t2.image_created(id, w, h);
        self.t3.image_created(id, w, h);
    }
    fn image_created_layered(&self, id: ResourceId, w: u32, h: u32, layers: u32) {
        self.images.lock().unwrap().insert(id.raw(), (w, h));
        self.t2.image_created_layered(id, w, h, layers);
        self.t3.image_created_layered(id, w, h, layers);
    }
    fn set_image_format(&self, id: ResourceId, fmt: u32) {
        self.t2.set_image_format(id, fmt);
        self.t3.set_image_format(id, fmt);
    }
    fn image_destroyed(&self, id: ResourceId) {
        self.t2.image_destroyed(id);
        self.t3.image_destroyed(id);
    }
    fn depth_image_created(&self, id: ResourceId, w: u32, h: u32) {
        self.t2.depth_image_created(id, w, h);
        self.t3.depth_image_created(id, w, h);
    }
    fn depth_image_destroyed(&self, id: ResourceId) {
        self.t2.depth_image_destroyed(id);
        self.t3.depth_image_destroyed(id);
    }
    fn buffer_created(&self, id: ResourceId, size: u64) {
        self.t2.buffer_created(id, size);
        self.t3.buffer_created(id, size);
    }
    fn buffer_destroyed(&self, id: ResourceId) {
        self.t2.buffer_destroyed(id);
        self.t3.buffer_destroyed(id);
    }
    #[allow(clippy::too_many_arguments)]
    fn sampler_created(
        &self, id: ResourceId, min_f: u8, mag_f: u8, mip_f: u8,
        addr: [u8; 3], aniso: f32, min_lod: f32, max_lod: f32,
        cmp_en: u8, cmp_op: u32,
    ) {
        self.t2.sampler_created(id, min_f, mag_f, mip_f, addr, aniso, min_lod, max_lod, cmp_en, cmp_op);
        self.t3.sampler_created(id, min_f, mag_f, mip_f, addr, aniso, min_lod, max_lod, cmp_en, cmp_op);
    }
    fn sampler_destroyed(&self, id: ResourceId) {
        self.t2.sampler_destroyed(id);
        self.t3.sampler_destroyed(id);
    }
    fn pipeline_created(&self, id: ResourceId, vs: &[u8], fs: &[u8]) {
        self.pipelines.lock().unwrap().insert(id.raw(), (shader_cost(vs), shader_cost(fs)));
        self.t2.pipeline_created(id, vs, fs);
        self.t3.pipeline_created(id, vs, fs);
    }
    fn present(&self, id: ResourceId, surface: u64, frame: u64) {
        self.t2.present(id, surface, frame);
        self.t3.present(id, surface, frame);
    }
    fn bind_pipeline_tier2(&self, p: ResourceId, s: crate::Tier2ShaderId) {
        self.t2.bind_pipeline_tier2(p, s);
        self.t3.bind_pipeline_tier2(p, s);
    }
    fn bind_pipeline_tier2_vs(&self, p: ResourceId, s: crate::Tier2ShaderId) {
        self.t2.bind_pipeline_tier2_vs(p, s);
        self.t3.bind_pipeline_tier2_vs(p, s);
    }
    fn bind_pipeline_layout(&self, p: ResourceId, l: aqueduct_gpu::VertexInputState) {
        self.t2.bind_pipeline_layout(p, l.clone());
        self.t3.bind_pipeline_layout(p, l);
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
        self.t2.bind_pipeline_raster_state(p, depth, blend, blend_extra, raster, topo, stencil, prim_restart);
        self.t3.bind_pipeline_raster_state(p, depth, blend, blend_extra, raster, topo, stencil, prim_restart);
    }
    fn bind_pipeline_vs_varying_bytes(&self, p: ResourceId, b: u32) {
        self.t2.bind_pipeline_vs_varying_bytes(p, b);
        self.t3.bind_pipeline_vs_varying_bytes(p, b);
    }
    fn bind_pipeline_fs_implicit_lod(&self, p: ResourceId, v: bool) {
        self.t2.bind_pipeline_fs_implicit_lod(p, v);
        self.t3.bind_pipeline_fs_implicit_lod(p, v);
    }
    fn bind_pipeline_sample_count(&self, p: ResourceId, n: u32) {
        self.t2.bind_pipeline_sample_count(p, n);
        self.t3.bind_pipeline_sample_count(p, n);
    }
    fn bind_pipeline_fs_derivatives(&self, p: ResourceId, v: bool) {
        self.t2.bind_pipeline_fs_derivatives(p, v);
        self.t3.bind_pipeline_fs_derivatives(p, v);
    }
    fn bind_pipeline_fs_writes_depth(&self, p: ResourceId, v: bool) {
        self.t2.bind_pipeline_fs_writes_depth(p, v);
        self.t3.bind_pipeline_fs_writes_depth(p, v);
    }

    // ── Data movement: writes mirror to both; reads follow the writer. ─
    fn image_write_pixels(&self, id: ResourceId, row_pitch: u32, pixels: &[u8]) -> Result<(), String> {
        let a = self.t2.image_write_pixels(id, row_pitch, pixels);
        let b = self.t3.image_write_pixels(id, row_pitch, pixels);
        a.and(b)
    }
    #[allow(clippy::too_many_arguments)]
    fn image_write_region_pixels(
        &self, id: ResourceId, dx: u32, dy: u32, w: u32, h: u32, row_pitch: u32, pixels: &[u8],
    ) -> Result<(), String> {
        let a = self.t2.image_write_region_pixels(id, dx, dy, w, h, row_pitch, pixels);
        let b = self.t3.image_write_region_pixels(id, dx, dy, w, h, row_pitch, pixels);
        a.and(b)
    }
    fn buffer_write_bytes(&self, id: ResourceId, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let a = self.t2.buffer_write_bytes(id, offset, bytes);
        let b = self.t3.buffer_write_bytes(id, offset, bytes);
        a.and(b)
    }
    fn buffer_read_bytes(&self, id: ResourceId, offset: u64, size: u64) -> Result<Vec<u8>, String> {
        // Single-homed readback: read from whichever tier last rendered it.
        match self.policy.lock().unwrap().read_tier(id.raw()) {
            Tier::Tier2 => self.t2.buffer_read_bytes(id, offset, size),
            Tier::Tier3 => self.t3.buffer_read_bytes(id, offset, size),
        }
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

        let mut policy = self.policy.lock().unwrap();
        for p in &pipes_used {
            policy.note_surface_pipeline(surf, *p);
        }
        // Only pay the verdict cost when the surface can act on it. A pinned
        // (uncertified) surface dispatches home unconditionally — scoring it
        // would burn CPU (and power) deciding something it cannot change.
        let effective = if policy.eligible(surf) {
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
        } else {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            policy.home()
        };
        // Single-homed readback bookkeeping: this tier wrote the render
        // target + any copy-destination buffers.
        policy.note_write(surf, effective);
        for b in &dst_buffers {
            policy.note_write(*b, effective);
        }
        drop(policy);

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
        // Readback follows the writer (Tier-2): marker == 2.
        let px = rb.buffer_read_bytes(dst, 0, 4).unwrap();
        assert_eq!(px, vec![2, 2, 2, 2], "readback from the tier that rendered");
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
