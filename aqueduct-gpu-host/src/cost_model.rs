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
    /// FP32 ALU lanes (Layer 2 execution roofline).
    pub alu_lanes: f64,
    /// Modeled GPU clock, Hz (Layer 2 execution roofline).
    pub clock_hz: f64,
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
            alu_lanes: 0.0,
            clock_hz: 0.0,
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
            alu_lanes: 5120.0,
            clock_hz: 1.4e9,
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
            alu_lanes: 12288.0,
            clock_hz: 2.5e9,
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

/// A shader's instruction mix — a cheap roofline proxy extracted from the
/// SPIR-V opcode histogram (no full IR build). `alu_ops` drives the
/// compute roofline, `mem_ops` the memory roofline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShaderCost {
    /// Arithmetic / transcendental ops (FMA, dot, convert, GLSL ext, …).
    pub alu_ops: u32,
    /// Memory ops (Load/Store/AccessChain/Image read-write-sample).
    pub mem_ops: u32,
    /// Everything else (types, decorations, control flow, …).
    pub other_ops: u32,
}

/// Classify a SPIR-V opcode into the roofline buckets. Heuristic — a
/// representative subset of the hot arithmetic + memory opcodes; anything
/// unclassified is `other` (types/constants/decorations/control flow,
/// which don't feed the flops/bytes estimate).
fn classify_spirv_op(op: u16) -> u8 {
    // 0 = other, 1 = alu, 2 = mem.
    match op {
        // Arithmetic (int + float), conversions, bitwise, GLSL ext insts.
        12 // OpExtInst (GLSL450: sqrt/sin/pow/… — count as ALU)
        | 127..=142 // FNegate..VectorTimesScalar (FP/int add/sub/mul/div/mod)
        | 144..=148 // VectorTimesMatrix..Dot (matrix mul + dot)
        | 110..=114 // Convert{S,U}ToF / ConvertFTo{S,U} / FConvert
        | 194..=205 // shifts + bitwise + Not
        => 1,
        // Memory.
        61 | 62 // OpLoad / OpStore
        | 63 // OpCopyMemory
        | 65 | 66 // OpAccessChain / OpInBoundsAccessChain
        | 87..=99 // OpImageSample* / OpImageFetch / OpImageRead / OpImageWrite
        => 2,
        _ => 0,
    }
}

/// Extract the [`ShaderCost`] instruction mix from a SPIR-V module
/// (raw little-endian bytes). Returns the empty mix for non-SPIR-V input.
pub fn spirv_instruction_mix(spirv: &[u8]) -> ShaderCost {
    let mut sc = ShaderCost::default();
    if spirv.len() < 20 || spirv.len() % 4 != 0 {
        return sc;
    }
    let word = |i: usize| {
        u32::from_le_bytes([spirv[i * 4], spirv[i * 4 + 1], spirv[i * 4 + 2], spirv[i * 4 + 3]])
    };
    if word(0) != 0x0723_0203 {
        return sc; // not a SPIR-V magic header
    }
    let n_words = spirv.len() / 4;
    let mut i = 5; // skip the 5-word header
    while i < n_words {
        let w = word(i);
        let wc = (w >> 16) as usize;
        if wc == 0 {
            break; // malformed
        }
        match classify_spirv_op((w & 0xffff) as u16) {
            1 => sc.alu_ops += 1,
            2 => sc.mem_ops += 1,
            _ => sc.other_ops += 1,
        }
        i += wc;
    }
    sc
}

/// Layer-2 execution roofline (§4 of the spec): modeled `(time_s,
/// energy_j)` for running a shader of `mix` over `invocations` (pixels for
/// a fragment shader, vertices for a vertex shader). `t = max(compute,
/// memory)`: compute = flops / (alu_lanes · clock · 2 [FMA]); memory =
/// bytes_touched / mem_bw. A vec4 mem op ≈ 16 B. Passthrough → zero.
pub fn exec_cost(profile: &DeviceProfile, mix: &ShaderCost, invocations: u64) -> (f64, f64) {
    if profile.passthrough || invocations == 0 || profile.alu_lanes == 0.0 {
        return (0.0, 0.0);
    }
    let inv = invocations as f64;
    let flops = mix.alu_ops as f64 * inv; // ~1 flop per ALU op
    let bytes = mix.mem_ops as f64 * 16.0 * inv; // vec4-ish granularity
    let compute_t = flops / (profile.alu_lanes * profile.clock_hz * 2.0);
    let mem_t = bytes / profile.mem_bw;
    let time = compute_t.max(mem_t);
    // Energy: memory traffic + a small per-flop ALU term (~0.5 pJ/flop).
    let energy = bytes * profile.pj_per_byte_mem + flops * 0.5;
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

    fn spirv_from_words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn spirv_mix_classifies_opcodes() {
        // Header (5 words) + OpFAdd(129,wc5) + OpLoad(61,wc4) +
        // OpStore(62,wc3) + OpReturn(253,wc1).
        let words = [
            0x0723_0203, 0x0001_0000, 0, 100, 0, // header
            (5 << 16) | 129, 1, 2, 3, 4,          // OpFAdd → alu
            (4 << 16) | 61, 5, 6, 7,              // OpLoad → mem
            (3 << 16) | 62, 8, 9,                 // OpStore → mem
            (1 << 16) | 253,                      // OpReturn → other
        ];
        let mix = spirv_instruction_mix(&spirv_from_words(&words));
        assert_eq!(mix.alu_ops, 1, "one FAdd");
        assert_eq!(mix.mem_ops, 2, "Load + Store");
        assert_eq!(mix.other_ops, 1, "Return");
        // Non-SPIR-V input → empty mix.
        assert_eq!(spirv_instruction_mix(&[0xAB; 64]), ShaderCost::default());
    }

    #[test]
    fn exec_cost_scales_and_differs_by_device() {
        let mix = ShaderCost { alu_ops: 20, mem_ops: 4, other_ops: 0 };
        let uma = DeviceProfile::uma_apple_m4_max();
        let (t1, _) = exec_cost(&uma, &mix, 1_000_000);
        let (t2, _) = exec_cost(&uma, &mix, 2_000_000);
        assert!((t2 / t1 - 2.0).abs() < 1e-9, "exec time ∝ invocations");
        // Passthrough is free; a real profile is not.
        assert_eq!(exec_cost(&DeviceProfile::passthrough(), &mix, 1_000_000), (0.0, 0.0));
        assert!(t1 > 0.0);
        // The faster-clock / wider discrete part computes the same work
        // quicker (compute-bound here).
        let (t_disc, _) = exec_cost(&DeviceProfile::discrete_rdna3_pcie4x16(), &mix, 1_000_000);
        assert!(t_disc < t1, "wider+faster discrete beats UMA on a compute-bound shader");
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
