//! GPU device cost model — `CostModelBackend<B>` decorator + device
//! profiles. See `docs/spec/gpu-device-model.md`.
//!
//! Wraps any [`Backend`] and charges each data-movement / execution op
//! what it *would* cost on a chosen device topology (UMA APU vs discrete
//! PCIe dGPU), so the OS can do throughput / latency / energy analysis
//! and the energy router can be built + validated without the hardware.
//! Data still moves through the real backend unchanged; only the modeled
//! cost is recorded.
//!
//! **Phase D-M0 + D-M1 (this file).** The decorator forwards every
//! `Backend` call verbatim to the inner backend (functional behaviour is
//! identical with the decorator present or absent), and in **accounting
//! mode** (default) records a per-op `(time, energy)` to a [`FrameLedger`]
//! — zero timing perturbation. Layer 1 transfer model: LogGP/roofline
//! (`t = latency + bytes/bandwidth`), with the *topology* deciding whether
//! a host↔device copy happens at all (unified = coherent zero-copy;
//! discrete = a link DMA with the asymmetric readback penalty).
//!
//! **Deferred:** shaping mode (inject the modeled latency — D-M3), the
//! Layer-2 execution roofline (D-M4), and TOML profile loading. The
//! `passthrough` profile (default) models zero cost = today's behaviour.

use std::sync::Mutex;

use aqueduct_gpu::backends::BackendId;
use aqueduct_gpu::ids::ResourceId;

use crate::backend::Backend;

/// Memory topology — the single biggest determinant of transfer cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// One coherent pool shared by CPU + GPU (Apple silicon, APUs).
    /// Host↔device "copies" are coherent reads/writes — no link hop.
    Unified,
    /// Separate VRAM reached over a link (PCIe). Uploads + readbacks are
    /// DMAs across the link; readback is the asymmetric killer.
    Discrete,
}

/// How much VRAM a discrete device exposes host-visible (affects whether
/// a readback is a mapped VRAM read or a link round-trip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostVisible {
    /// No host-visible VRAM aperture — readback always crosses the link.
    None,
    /// Resizable BAR / full VRAM host-visible — readback is a mapped read.
    Full,
}

/// A device cost profile. Seeded from public specs; the analytic model
/// (§3.2 of the spec) reads these. `passthrough` = the zero-cost default.
#[derive(Debug, Clone)]
pub struct DeviceProfile {
    /// Human label (e.g. `"discrete-rdna3-pcie4x16"`).
    pub name: &'static str,
    /// `true` for the zero-cost identity profile (today's behaviour).
    pub passthrough: bool,
    /// Memory topology.
    pub topology: Topology,
    /// System/unified memory bandwidth, B/s (UMA), or VRAM bandwidth for
    /// discrete mapped reads.
    pub mem_bw: f64,
    /// Effective memory access latency, s.
    pub mem_latency: f64,
    /// Host↔device link bandwidth, B/s (0 for unified — no link).
    pub host_link_bw: f64,
    /// Host↔device link fixed latency (DMA setup + ring submit), s.
    pub host_link_lat: f64,
    /// Discrete: how much VRAM is host-visible (readback path).
    pub host_visible: HostVisible,
    /// Link transfer energy, picojoules per byte.
    pub pj_per_byte_link: f64,
    /// Memory access energy, picojoules per byte.
    pub pj_per_byte_mem: f64,
}

impl DeviceProfile {
    /// The zero-cost identity profile — the daemon's default; every op
    /// costs `(0, 0)` so the ledger is empty and nothing is perturbed.
    pub fn passthrough() -> Self {
        DeviceProfile {
            name: "passthrough",
            passthrough: true,
            topology: Topology::Unified,
            mem_bw: 0.0,
            mem_latency: 0.0,
            host_link_bw: 0.0,
            host_link_lat: 0.0,
            host_visible: HostVisible::Full,
            pj_per_byte_link: 0.0,
            pj_per_byte_mem: 0.0,
        }
    }

    /// Apple M4 Max — unified memory, ~546 GB/s, zero-copy host↔device.
    pub fn uma_apple_m4_max() -> Self {
        DeviceProfile {
            name: "uma-apple-m4-max",
            passthrough: false,
            topology: Topology::Unified,
            mem_bw: 546.0e9,
            mem_latency: 100.0e-9,
            host_link_bw: 0.0,
            host_link_lat: 0.0,
            host_visible: HostVisible::Full,
            pj_per_byte_link: 0.0,
            pj_per_byte_mem: 8.0,
        }
    }

    /// RDNA3 discrete over PCIe 4.0 x16 — ~960 GB/s VRAM, ~28 GB/s link,
    /// ReBAR host-visible VRAM.
    pub fn discrete_rdna3_pcie4x16() -> Self {
        DeviceProfile {
            name: "discrete-rdna3-pcie4x16",
            passthrough: false,
            topology: Topology::Discrete,
            mem_bw: 960.0e9,
            mem_latency: 250.0e-9,
            host_link_bw: 28.0e9,
            host_link_lat: 1.5e-6,
            host_visible: HostVisible::Full,
            pj_per_byte_link: 60.0,
            pj_per_byte_mem: 5.0,
        }
    }

    /// Look up a built-in profile by name; `None` if unknown.
    pub fn by_name(name: &str) -> Option<Self> {
        Some(match name {
            "passthrough" => Self::passthrough(),
            "uma-apple-m4-max" => Self::uma_apple_m4_max(),
            "discrete-rdna3-pcie4x16" => Self::discrete_rdna3_pcie4x16(),
            _ => return None,
        })
    }
}

/// Which side of the boundary a costed op moves data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Host → device (texture/buffer upload).
    Upload,
    /// Device → host (readback).
    Readback,
    /// GPU execution (the frame's own work + framebuffer traffic).
    Exec,
}

/// One modeled cost record.
#[derive(Debug, Clone, Copy)]
pub struct OpCost {
    /// What moved.
    pub kind: OpKind,
    /// Bytes involved.
    pub bytes: u64,
    /// Modeled wall-time, seconds.
    pub time_s: f64,
    /// Modeled energy, joules.
    pub energy_j: f64,
}

/// Per-frame (well, per-lifetime until drained) ledger of modeled costs.
/// The router queries it; offline analysis consumes it.
#[derive(Debug, Default, Clone)]
pub struct FrameLedger {
    /// All costed ops, in order.
    pub ops: Vec<OpCost>,
}

impl FrameLedger {
    /// Total modeled wall-time across all recorded ops, seconds.
    pub fn total_time_s(&self) -> f64 {
        self.ops.iter().map(|o| o.time_s).sum()
    }
    /// Total modeled energy across all recorded ops, joules.
    pub fn total_energy_j(&self) -> f64 {
        self.ops.iter().map(|o| o.energy_j).sum()
    }
    /// Modeled time for one op kind, seconds.
    pub fn time_for(&self, kind: OpKind) -> f64 {
        self.ops.iter().filter(|o| o.kind == kind).map(|o| o.time_s).sum()
    }
    /// Number of recorded ops.
    pub fn len(&self) -> usize { self.ops.len() }
    /// Whether the ledger is empty.
    pub fn is_empty(&self) -> bool { self.ops.is_empty() }
}

/// Cost-model enforcement mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostMode {
    /// Record modeled cost; do not perturb timing. The default.
    Accounting,
    /// Inject the modeled latency so the OS feels the device. **Deferred
    /// to D-M3** — currently behaves as `Accounting` (records only).
    Shaping,
}

/// Analytic transfer cost (LogGP/roofline). Returns `(time_s, energy_j)`
/// for moving `bytes` in `kind` direction on `profile`. §3.2 of the spec.
pub fn transfer_cost(profile: &DeviceProfile, kind: OpKind, bytes: u64) -> (f64, f64) {
    if profile.passthrough || bytes == 0 {
        return (0.0, 0.0);
    }
    let b = bytes as f64;
    let (time, energy) = match kind {
        OpKind::Upload => match profile.topology {
            // Unified: a coherent write into the shared pool — no link.
            Topology::Unified => (
                profile.mem_latency + b / profile.mem_bw,
                b * profile.pj_per_byte_mem,
            ),
            // Discrete: host → VRAM DMA over the link.
            Topology::Discrete => (
                profile.host_link_lat + b / profile.host_link_bw,
                b * (profile.pj_per_byte_link + profile.pj_per_byte_mem),
            ),
        },
        OpKind::Readback => match profile.topology {
            // Unified: a coherent read — symmetric with upload, no link.
            Topology::Unified => (
                profile.mem_latency + b / profile.mem_bw,
                b * profile.pj_per_byte_mem,
            ),
            // Discrete: the asymmetric killer. A mapped read at VRAM
            // bandwidth if the target is host-visible (ReBAR/full),
            // else a VRAM → host DMA back over the link.
            Topology::Discrete => match profile.host_visible {
                HostVisible::Full => (
                    profile.mem_latency + b / profile.mem_bw,
                    b * profile.pj_per_byte_mem,
                ),
                HostVisible::None => (
                    profile.host_link_lat + b / profile.host_link_bw,
                    b * (profile.pj_per_byte_link + profile.pj_per_byte_mem),
                ),
            },
        },
        // Exec: framebuffer/command memory traffic. The real execution
        // roofline is Layer 2 (D-M4); here we charge the touched bytes at
        // device memory bandwidth as a placeholder.
        OpKind::Exec => {
            let bw = if profile.topology == Topology::Discrete {
                profile.mem_bw
            } else {
                profile.mem_bw
            };
            (b / bw, b * profile.pj_per_byte_mem)
        }
    };
    (time, energy)
}

/// A [`Backend`] decorator that charges modeled device cost per op while
/// forwarding every call verbatim to the inner backend.
pub struct CostModelBackend<B: Backend> {
    inner: B,
    profile: DeviceProfile,
    mode: CostMode,
    ledger: Mutex<FrameLedger>,
}

impl<B: Backend> CostModelBackend<B> {
    /// Wrap `inner` with `profile`, accounting mode.
    pub fn new(inner: B, profile: DeviceProfile) -> Self {
        CostModelBackend {
            inner,
            profile,
            mode: CostMode::Accounting,
            ledger: Mutex::new(FrameLedger::default()),
        }
    }

    /// Set the enforcement mode (accounting vs shaping).
    pub fn with_mode(mut self, mode: CostMode) -> Self {
        self.mode = mode;
        self
    }

    /// The active device profile.
    pub fn profile(&self) -> &DeviceProfile { &self.profile }

    /// Snapshot the ledger so far (clones the records).
    pub fn ledger_snapshot(&self) -> FrameLedger {
        self.ledger.lock().unwrap().clone()
    }

    /// Take + clear the ledger (e.g. per-frame drain for the router).
    pub fn drain_ledger(&self) -> FrameLedger {
        std::mem::take(&mut *self.ledger.lock().unwrap())
    }

    /// Record one op's modeled cost (accounting). Shaping (deferred) would
    /// also sleep `time_s` here.
    fn charge(&self, kind: OpKind, bytes: u64) {
        let (time_s, energy_j) = transfer_cost(&self.profile, kind, bytes);
        // (Shaping mode would inject `time_s` here — D-M3.)
        let _ = self.mode;
        self.ledger
            .lock()
            .unwrap()
            .ops
            .push(OpCost { kind, bytes, time_s, energy_j });
    }
}

impl<B: Backend> Backend for CostModelBackend<B> {
    // ── Pure forwards (no data movement) ──────────────────────────────
    fn identity(&self) -> BackendId { self.inner.identity() }
    fn caps(&self) -> u64 { self.inner.caps() }
    fn max_frame_bytes(&self) -> u32 { self.inner.max_frame_bytes() }
    fn max_fences_inflight(&self) -> u32 { self.inner.max_fences_inflight() }
    fn allocate_memory(&self, size: u64, usage: u8) -> [u8; 32] {
        self.inner.allocate_memory(size, usage)
    }
    fn image_created(&self, id: ResourceId, w: u32, h: u32) {
        self.inner.image_created(id, w, h)
    }
    fn image_created_layered(&self, id: ResourceId, w: u32, h: u32, layers: u32) {
        self.inner.image_created_layered(id, w, h, layers)
    }
    fn set_image_format(&self, id: ResourceId, fmt: u32) {
        self.inner.set_image_format(id, fmt)
    }
    fn image_destroyed(&self, id: ResourceId) { self.inner.image_destroyed(id) }
    fn depth_image_created(&self, id: ResourceId, w: u32, h: u32) {
        self.inner.depth_image_created(id, w, h)
    }
    fn depth_image_destroyed(&self, id: ResourceId) {
        self.inner.depth_image_destroyed(id)
    }
    fn buffer_created(&self, id: ResourceId, size: u64) {
        self.inner.buffer_created(id, size)
    }
    fn buffer_destroyed(&self, id: ResourceId) { self.inner.buffer_destroyed(id) }
    #[allow(clippy::too_many_arguments)]
    fn sampler_created(
        &self, id: ResourceId, min_f: u8, mag_f: u8, mip_f: u8,
        addr: [u8; 3], aniso: f32, min_lod: f32, max_lod: f32,
        cmp_en: u8, cmp_op: u32,
    ) {
        self.inner.sampler_created(
            id, min_f, mag_f, mip_f, addr, aniso, min_lod, max_lod, cmp_en, cmp_op,
        )
    }
    fn sampler_destroyed(&self, id: ResourceId) { self.inner.sampler_destroyed(id) }
    fn pipeline_created(&self, id: ResourceId, vs: &[u8], fs: &[u8]) {
        self.inner.pipeline_created(id, vs, fs)
    }
    fn present(&self, id: ResourceId, surface: u64, frame: u64) {
        self.inner.present(id, surface, frame)
    }
    fn bind_pipeline_tier2(&self, p: ResourceId, s: crate::Tier2ShaderId) {
        self.inner.bind_pipeline_tier2(p, s)
    }
    fn bind_pipeline_tier2_vs(&self, p: ResourceId, s: crate::Tier2ShaderId) {
        self.inner.bind_pipeline_tier2_vs(p, s)
    }
    fn bind_pipeline_layout(&self, p: ResourceId, l: aqueduct_gpu::VertexInputState) {
        self.inner.bind_pipeline_layout(p, l)
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
        self.inner.bind_pipeline_raster_state(
            p, depth, blend, blend_extra, raster, topo, stencil, prim_restart,
        )
    }
    fn bind_pipeline_vs_varying_bytes(&self, p: ResourceId, b: u32) {
        self.inner.bind_pipeline_vs_varying_bytes(p, b)
    }
    fn bind_pipeline_fs_implicit_lod(&self, p: ResourceId, v: bool) {
        self.inner.bind_pipeline_fs_implicit_lod(p, v)
    }
    fn bind_pipeline_sample_count(&self, p: ResourceId, n: u32) {
        self.inner.bind_pipeline_sample_count(p, n)
    }
    fn bind_pipeline_fs_derivatives(&self, p: ResourceId, v: bool) {
        self.inner.bind_pipeline_fs_derivatives(p, v)
    }
    fn bind_pipeline_fs_writes_depth(&self, p: ResourceId, v: bool) {
        self.inner.bind_pipeline_fs_writes_depth(p, v)
    }

    // ── Costed data-movement / execution ops ──────────────────────────
    fn image_write_pixels(
        &self, id: ResourceId, row_pitch: u32, pixels: &[u8],
    ) -> Result<(), String> {
        self.charge(OpKind::Upload, pixels.len() as u64);
        self.inner.image_write_pixels(id, row_pitch, pixels)
    }
    #[allow(clippy::too_many_arguments)]
    fn image_write_region_pixels(
        &self, id: ResourceId, dx: u32, dy: u32, w: u32, h: u32,
        row_pitch: u32, pixels: &[u8],
    ) -> Result<(), String> {
        self.charge(OpKind::Upload, pixels.len() as u64);
        self.inner.image_write_region_pixels(id, dx, dy, w, h, row_pitch, pixels)
    }
    fn buffer_write_bytes(
        &self, id: ResourceId, offset: u64, bytes: &[u8],
    ) -> Result<(), String> {
        self.charge(OpKind::Upload, bytes.len() as u64);
        self.inner.buffer_write_bytes(id, offset, bytes)
    }
    fn buffer_read_bytes(
        &self, id: ResourceId, offset: u64, size: u64,
    ) -> Result<Vec<u8>, String> {
        self.charge(OpKind::Readback, size);
        self.inner.buffer_read_bytes(id, offset, size)
    }
    fn submit_frame(&self, fence: ResourceId, timeline: u64, frame_buf: &[u8]) -> bool {
        self.charge(OpKind::Exec, frame_buf.len() as u64);
        self.inner.submit_frame(fence, timeline, frame_buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::StubBackend;

    #[test]
    fn passthrough_is_zero_cost_and_forwards() {
        let cm = CostModelBackend::new(StubBackend::new(), DeviceProfile::passthrough());
        let img = ResourceId(1);
        cm.image_write_pixels(img, 64, &vec![0u8; 64 * 64 * 4]).ok();
        let _ = cm.buffer_read_bytes(ResourceId(2), 0, 4096);
        assert!(cm.submit_frame(ResourceId(3), 1, &[0u8; 32]));
        let ledger = cm.ledger_snapshot();
        // Ops recorded, but passthrough → zero modeled cost.
        assert_eq!(ledger.len(), 3);
        assert_eq!(ledger.total_time_s(), 0.0);
        assert_eq!(ledger.total_energy_j(), 0.0);
    }

    #[test]
    fn discrete_readback_is_slower_than_uma_without_rebar() {
        let bytes = 64 * 1024 * 1024u64; // 64 MiB readback
        // Discrete without host-visible VRAM → link round-trip.
        let mut disc = DeviceProfile::discrete_rdna3_pcie4x16();
        disc.host_visible = HostVisible::None;
        let (t_disc, _) = transfer_cost(&disc, OpKind::Readback, bytes);
        let (t_uma, _) = transfer_cost(&DeviceProfile::uma_apple_m4_max(), OpKind::Readback, bytes);
        assert!(t_disc > t_uma,
            "discrete link readback ({t_disc}s) must exceed UMA coherent read ({t_uma}s)");
        // ReBAR turns the discrete readback into a mapped VRAM read.
        let (t_rebar, _) = transfer_cost(
            &DeviceProfile::discrete_rdna3_pcie4x16(), OpKind::Readback, bytes);
        assert!(t_rebar < t_disc, "ReBAR readback must beat the link round-trip");
    }

    #[test]
    fn discrete_upload_charges_link_energy() {
        let bytes = 1024 * 1024u64;
        let (_, e_disc) = transfer_cost(
            &DeviceProfile::discrete_rdna3_pcie4x16(), OpKind::Upload, bytes);
        let (_, e_uma) = transfer_cost(
            &DeviceProfile::uma_apple_m4_max(), OpKind::Upload, bytes);
        // Discrete pays PCIe transfer energy on top of memory energy.
        assert!(e_disc > e_uma);
    }

    #[test]
    fn ledger_drains() {
        let cm = CostModelBackend::new(StubBackend::new(),
            DeviceProfile::discrete_rdna3_pcie4x16());
        let _ = cm.buffer_read_bytes(ResourceId(1), 0, 1 << 20);
        assert_eq!(cm.ledger_snapshot().len(), 1);
        let drained = cm.drain_ledger();
        assert_eq!(drained.len(), 1);
        assert!(drained.time_for(OpKind::Readback) > 0.0);
        assert!(cm.ledger_snapshot().is_empty(), "drain clears the ledger");
    }
}
