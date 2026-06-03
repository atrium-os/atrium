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

/// Where the display engine reads the scanout framebuffer from. On a
/// desktop discrete GPU the connectors hang off the dGPU, so the final
/// frame must reach VRAM (`Device`); on UMA / laptop-iGPU systems the panel
/// is driven from system memory (`Host`). Only meaningful under
/// `Topology::Discrete` — under unified memory both domains are the same
/// physical RAM, so [`DeviceProfile::present_cost`] is zero regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanoutDomain {
    /// Display reads from host/system memory (UMA, or laptop-iGPU muxless).
    Host,
    /// Display reads from device VRAM (desktop dGPU with connectors on it).
    Device,
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
    /// Where the display scans out from (drives the present/copy cost on
    /// discrete: CPU-rendered content must reach VRAM for display).
    pub scanout_domain: ScanoutDomain,
}

impl DeviceProfile {
    /// Resolve a profile by CLI name (the `--device-profile` flag), or
    /// `None` for an unknown name. `passthrough` = zero-cost identity (the
    /// model in the data path but charging nothing).
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "passthrough" => Self::passthrough(),
            "uma-apple-m4-max" => Self::uma_apple_m4_max(),
            "discrete-rdna3-pcie4x16" => Self::discrete_rdna3_pcie4x16(),
            _ => return None,
        })
    }

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
            scanout_domain: ScanoutDomain::Host,
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
            scanout_domain: ScanoutDomain::Host, // UMA: display from system memory
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
            scanout_domain: ScanoutDomain::Device, // desktop dGPU: connectors on the card
        }
    }

    /// The per-frame **present/copy** cost to get `damage_bytes` of
    /// rendered output from `source` into the scanout domain for display —
    /// distinct from a compute wake. Zero when the source already matches
    /// the scanout domain, and **always zero under unified memory** (device
    /// and host are the same RAM). On a desktop dGPU (`scanout_domain =
    /// Device`), CPU-rendered content (`source = Host`) charges a link copy
    /// here — but on the *copy* power path (DMA / display engine), not the
    /// expensive compute array (see [`GpuPowerModel`]). That is why
    /// CPU-rendering a sparse surface can still win on a discrete part: you
    /// pay a small damage DMA, not a shader-array wake.
    pub fn present_cost(&self, damage_bytes: u64, source: ScanoutDomain) -> (f64, f64) {
        if self.passthrough
            || self.topology == Topology::Unified
            || source == self.scanout_domain
        {
            return (0.0, 0.0);
        }
        let b = damage_bytes as f64;
        // A link DMA into (or out of) VRAM for scanout: cheap copy path.
        let time = self.host_link_lat + b / self.host_link_bw.max(1.0);
        let energy = b * self.pj_per_byte_link;
        (time, energy)
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

/// Classify an IR op into `(alu_weight, mem_weight)` — *cost-weighted*,
/// not a flat +1. This is the payoff of walking our own `atrium-spv-ir`
/// instead of the SPIR-V opcode histogram: a typed enum lets us price each
/// op by its real micro-architectural cost. A divide / sqrt is several
/// ALU-cycles; a matrix·vector is a fan of mul-adds; an image sample pays
/// a filtered fetch worth several memory ops. Anything cheap or
/// structural classifies as `(0, 0)` → `other_ops`. Catch-all keeps us
/// robust if `atrium-spv-ir` grows new ops (they fall to `other`, exactly
/// as the histogram's default did).
fn classify_ir_op(op: &Op) -> (u32, u32) {
    use Op::*;
    match op {
        // Cheap scalar/vector arithmetic, bitwise, shifts, compares,
        // conversions, shuffles, select, derivatives — ~1 ALU op.
        IAdd(..) | ISub(..) | IMul(..) | INeg(..) | FAdd(..) | FSub(..)
        | FMul(..) | FNeg(..) | BitAnd(..) | BitOr(..) | BitXor(..)
        | BitNot(..) | Shl(..) | LShr(..) | AShr(..) | Clz(..) | Rbit(..)
        | IEq(..) | INe(..) | ULt(..) | ULe(..) | UGt(..) | UGe(..)
        | SLt(..) | SLe(..) | SGt(..) | SGe(..) | FOrdEq(..) | FOrdNe(..)
        | FOrdLt(..) | FOrdLe(..) | FOrdGt(..) | FOrdGe(..) | FUnordEq(..)
        | FUnordNe(..) | FUnordLt(..) | FUnordLe(..) | FUnordGt(..)
        | FUnordGe(..) | FFloor(..) | FCeil(..) | FTrunc(..) | FAbs(..)
        | FMin(..) | FMax(..) | ConvertSToF(..) | ConvertFToS(..)
        | ConvertUToF(..) | ConvertFToU(..) | SConvert(..) | UConvert(..)
        | FConvert(..) | Bitcast(..) | VectorShuffle { .. }
        | VectorExtract { .. } | VectorInsert { .. } | Select { .. }
        | PackHalf2x16(..) | UnpackHalf2x16(..) => (1, 0),
        // Divide / modulo / sqrt — multi-cycle ALU (~4×).
        UDiv(..) | SDiv(..) | UMod(..) | SMod(..) | FDiv(..) | FRem(..)
        | FSqrt(..) => (4, 0),
        // Dot ≈ width mul-adds; matrix·vector ≈ a fan of them.
        Dot(..) => (3, 0),
        MatrixTimesVector { .. } => (12, 0),
        // Fragment derivatives are cross-lane quad ops (~2×).
        DPdx(..) | DPdy(..) | Fwidth(..) | Derivative { .. } => (2, 0),
        // Plain memory traffic — ~1 mem op.
        Load(..) | LoadBuiltin(..) | Store { .. } | AccessChain { .. }
        | PtrOffsetDynamic { .. } => (0, 1),
        // Atomics are read-modify-write — pricier (~2×).
        AtomicLoad(..) | AtomicStore { .. } | AtomicIAdd { .. }
        | AtomicAnd { .. } | AtomicOr { .. } | AtomicXor { .. }
        | AtomicCompareExchange { .. } | AtomicSMin { .. }
        | AtomicSMax { .. } | AtomicUMin { .. } | AtomicUMax { .. }
        | AtomicExchange { .. } => (0, 2),
        // Sampled / fetched / stored image access — a filtered texel
        // fetch is worth several memory ops (~4×).
        ImageSampleImplicitLod { .. } | ImageSampleExplicitLod { .. }
        | ImageSampleDref { .. } | ImageFetch { .. } | ImageRead { .. }
        | ImageWrite { .. } | ImageGather { .. } | ImageReadLod { .. }
        | ImageWriteLod { .. } => (0, 4),
        // Constants, control flow, phi, barriers, handle plumbing,
        // size queries — structural, no flops/bytes charged.
        _ => (0, 0),
    }
}

/// Extract a cost-weighted [`ShaderCost`] by walking our own recovered IR
/// (`atrium-spv-ir`). More accurate than [`spirv_instruction_mix`] — see
/// [`classify_ir_op`]. Sums static instruction cost across every block of
/// every function (loop trip counts aren't statically known, same as the
/// histogram).
pub fn ir_instruction_mix(module: &atrium_spv_ir::Module) -> ShaderCost {
    let mut sc = ShaderCost::default();
    for func in &module.functions {
        for block in func.blocks.values() {
            for inst in &block.insts {
                let (alu, mem) = classify_ir_op(&inst.op);
                if alu == 0 && mem == 0 {
                    sc.other_ops += 1;
                } else {
                    sc.alu_ops += alu;
                    sc.mem_ops += mem;
                }
            }
        }
    }
    sc
}

/// The shader cost the model uses: prefer the cost-weighted IR walk
/// (our `atrium-spv-frontend` → `atrium-spv-ir`), falling back to the
/// robust opcode histogram if the frontend can't yet lower this module
/// (it's phase-staged; the histogram never fails). Best-effort accuracy
/// with a guaranteed floor.
pub fn shader_cost(spirv: &[u8]) -> ShaderCost {
    match atrium_spv_frontend::translate(spirv) {
        Ok(module) => ir_instruction_mix(&module),
        Err(_) => spirv_instruction_mix(spirv),
    }
}

/// Bring `Op` into scope for [`classify_ir_op`].
use atrium_spv_ir::Op;

/// Layer-2 execution roofline (§4 of the spec): modeled `(time_s,
/// energy_j)` for running a shader of `mix` over `invocations` (pixels for
/// a fragment shader, vertices for a vertex shader). `t = max(compute,
/// memory)`: compute = flops / (alu_lanes · clock · 2 [FMA]); memory =
/// bytes_touched / mem_bw. A vec4 mem op ≈ 16 B. Passthrough → zero.
pub fn exec_cost(profile: &DeviceProfile, mix: &ShaderCost, invocations: u64) -> (f64, f64) {
    let (compute_t, mem_t, flops, bytes) = roofline_branches(profile, mix, invocations);
    // flops/bytes are zero for passthrough/empty → energy 0 there.
    let energy = bytes * profile.pj_per_byte_mem + flops * 0.5;
    (compute_t.max(mem_t), energy)
}

/// The two roofline branch times + the work totals, shared by the ideal
/// [`exec_cost`] and the calibrated [`CalibrationProfile::exec_time_s`].
/// `(compute_t, mem_t, flops, bytes)`; all zero for passthrough / empty.
fn roofline_branches(
    profile: &DeviceProfile, mix: &ShaderCost, invocations: u64,
) -> (f64, f64, f64, f64) {
    if profile.passthrough || invocations == 0 || profile.alu_lanes == 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let inv = invocations as f64;
    let flops = mix.alu_ops as f64 * inv; // ~1 flop per ALU op
    let bytes = mix.mem_ops as f64 * 16.0 * inv; // vec4-ish granularity
    let compute_t = flops / (profile.alu_lanes * profile.clock_hz * 2.0);
    let mem_t = bytes / profile.mem_bw;
    (compute_t, mem_t, flops, bytes)
}

/// Per-device-*family* efficiency correction — the only genuinely
/// measured-per-device part of the cost model (see the calibration design).
///
/// [`DeviceProfile`] holds datasheet specs (no hardware needed);
/// [`exec_cost`] is the *ideal* roofline. Real GPUs reach only a fraction
/// of peak (occupancy, scheduling) and pay a fixed per-submit launch cost.
/// This profile captures that gap in **three scalars**, fit from measured
/// GPU time (D-M6) when the hardware exists. It is *optional*: the
/// [`Self::prior`] default is usable uncalibrated — the router only needs
/// the Tier-2↔Tier-3 crossover roughly right, and the hysteresis band plus
/// the orders-of-magnitude CPU/GPU gap absorb the residual error.
///
/// Calibrate **once per microarchitecture family** (an M4 Max and M4 Pro
/// share one; all RDNA3 share one), not per chip — the spec constants carry
/// the per-chip differences. For devices you'll never physically have
/// (virtual Tier-3), transfer a same-class device's profile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibrationProfile {
    /// Fraction of peak FLOP/s actually reached on compute-bound work, (0,1].
    pub compute_efficiency: f64,
    /// Fraction of peak bandwidth reached on memory-bound work, (0,1].
    pub bandwidth_efficiency: f64,
    /// Fixed per-submit overhead (command-buffer build + launch), seconds.
    pub launch_overhead_s: f64,
}

impl CalibrationProfile {
    /// The uncalibrated prior — usable before any measurement. Mid-range
    /// efficiencies + a typical GPU submit/launch latency. Transferable as
    /// a starting point to any modern tiled/wavefront GPU.
    pub fn prior() -> Self {
        CalibrationProfile {
            compute_efficiency: 0.5,
            bandwidth_efficiency: 0.7,
            launch_overhead_s: 30e-6,
        }
    }

    /// Calibrated execution time: the ideal roofline branches stretched by
    /// their (sub-unity) efficiencies, plus the fixed launch overhead. With
    /// the [`Self::prior`]'s ≤1 efficiencies this is always ≥ the ideal
    /// [`exec_cost`] time.
    pub fn exec_time_s(
        &self, profile: &DeviceProfile, mix: &ShaderCost, invocations: u64,
    ) -> f64 {
        let (ct, mt, _, _) = roofline_branches(profile, mix, invocations);
        if ct == 0.0 && mt == 0.0 {
            return 0.0;
        }
        let real = (ct / self.compute_efficiency).max(mt / self.bandwidth_efficiency);
        real + self.launch_overhead_s
    }

    /// Fold one measured frame into the fit: classify the frame as compute-
    /// or memory-bound from its ideal branches, then EWMA-update that
    /// branch's efficiency toward `ideal_branch_time / measured_work_time`
    /// (measured minus the fixed launch overhead). `alpha` ∈ (0,1] is the
    /// update weight (smaller = smoother). No-op for empty/zero frames.
    pub fn observe(
        &mut self, profile: &DeviceProfile, mix: &ShaderCost, invocations: u64,
        measured_s: f64, alpha: f64,
    ) {
        let (ct, mt, _, _) = roofline_branches(profile, mix, invocations);
        self.observe_branches(ct, mt, measured_s, alpha);
    }

    /// As [`Self::observe`] but from pre-aggregated ideal branch times — for
    /// a multi-draw frame whose per-draw `compute_t`/`mem_t` were summed
    /// (summing the per-draw `max` would lose the compute-vs-memory split
    /// the fit needs).
    pub fn observe_branches(
        &mut self, compute_t: f64, mem_t: f64, measured_s: f64, alpha: f64,
    ) {
        let ideal = compute_t.max(mem_t);
        if ideal <= 0.0 || measured_s <= 0.0 {
            return;
        }
        // Time the GPU actually spent on *work* (strip the fixed overhead).
        let work = (measured_s - self.launch_overhead_s).max(ideal * 0.01);
        if compute_t >= mem_t {
            let eff = (compute_t / work).clamp(0.01, 1.0);
            self.compute_efficiency = ewma(self.compute_efficiency, eff, alpha);
        } else {
            let eff = (mem_t / work).clamp(0.01, 1.0);
            self.bandwidth_efficiency = ewma(self.bandwidth_efficiency, eff, alpha);
        }
    }
}

/// Exponentially-weighted moving average update.
fn ewma(prev: f64, sample: f64, alpha: f64) -> f64 {
    prev * (1.0 - alpha) + sample * alpha
}

/// A [`Backend`] decorator that charges modeled device cost per op while
/// forwarding every call verbatim to the inner backend.
pub struct CostModelBackend<B: Backend> {
    inner: B,
    profile: DeviceProfile,
    mode: CostMode,
    ledger: Mutex<FrameLedger>,
    /// image raw-id → (width, height), for FS-invocation estimates.
    images: Mutex<std::collections::HashMap<u32, (u32, u32)>>,
    /// pipeline raw-id → (vs_mix, fs_mix), for the Layer-2 exec roofline.
    pipelines: Mutex<std::collections::HashMap<u32, (ShaderCost, ShaderCost)>>,
    /// Frames submitted — drives the periodic ledger summary log + the
    /// router's per-frame pseudo-clock.
    frames: std::sync::atomic::AtomicU64,
    /// Optional live router: when set, each submitted frame is scored
    /// Tier-2 vs Tier-3 and the verdict tallied. Observational — the inner
    /// backend still runs the frame (accounting-safe); the tally validates
    /// the router against real traffic before dispatch is rewired.
    router: Option<Mutex<crate::router::FrameRouter>>,
    /// (tier2_count, tier3_count) routing verdicts so far.
    route_tally: Mutex<(u64, u64)>,
    /// Per-surface tier assignment (slow migration from the smoothed
    /// verdict). Fed only when routing is enabled; keyed by the frame's
    /// render-target image id (the surface proxy at this layer).
    surfaces: Mutex<crate::router::SurfaceRouter>,
    /// Optional online calibration: when set, each frame's modeled cost is
    /// compared against the inner backend's measured GPU time (D-M6) and
    /// the efficiency scalars are EWMA-fit. Closes the calibration loop.
    calibration: Option<Mutex<CalibrationProfile>>,
}

impl<B: Backend> CostModelBackend<B> {
    /// Wrap `inner` with `profile`, accounting mode.
    pub fn new(inner: B, profile: DeviceProfile) -> Self {
        CostModelBackend {
            inner,
            profile,
            mode: CostMode::Accounting,
            ledger: Mutex::new(FrameLedger::default()),
            images: Mutex::new(std::collections::HashMap::new()),
            pipelines: Mutex::new(std::collections::HashMap::new()),
            frames: std::sync::atomic::AtomicU64::new(0),
            router: None,
            route_tally: Mutex::new((0, 0)),
            surfaces: Mutex::new(crate::router::SurfaceRouter::new(0.3, 0.1)),
            calibration: None,
        }
    }

    /// Snapshot the per-surface tier assignment counts `(tier2, tier3)`.
    pub fn surface_assignment_counts(&self) -> (usize, usize) {
        self.surfaces.lock().unwrap().assignment_counts()
    }

    /// Enable online calibration starting from `start` (typically
    /// [`CalibrationProfile::prior`]). Each submitted frame whose inner
    /// backend reports a measured GPU time (D-M6) folds into the fit via
    /// [`CalibrationProfile::observe`].
    pub fn with_calibration(mut self, start: CalibrationProfile) -> Self {
        self.calibration = Some(Mutex::new(start));
        self
    }

    /// Snapshot the current calibration profile, if enabled.
    pub fn calibration(&self) -> Option<CalibrationProfile> {
        self.calibration.as_ref().map(|c| *c.lock().unwrap())
    }

    /// Enable live Tier-2↔Tier-3 routing observation: each submitted frame
    /// is scored against `cpu` (the Tier-2 CPU profile) using this
    /// decorator's device profile as the Tier-3 side, with `power` as the
    /// GPU residency model and `mode` the policy. The verdict is tallied
    /// (and logged in the periodic summary) but not acted on — the frame
    /// still runs on the wrapped backend.
    pub fn with_routing(
        mut self,
        cpu: crate::router::CpuProfile,
        power: crate::router::GpuPowerModel,
        mode: crate::router::RouteMode,
    ) -> Self {
        let fr = crate::router::FrameRouter::new(cpu, self.profile.clone(), power, mode);
        self.router = Some(Mutex::new(fr));
        self
    }

    /// Snapshot the routing tally so far: `(tier2_count, tier3_count)`.
    pub fn route_tally(&self) -> (u64, u64) {
        *self.route_tally.lock().unwrap()
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
        self.images.lock().unwrap().insert(id.raw(), (w, h));
        self.inner.image_created(id, w, h)
    }
    fn image_created_layered(&self, id: ResourceId, w: u32, h: u32, layers: u32) {
        self.images.lock().unwrap().insert(id.raw(), (w, h));
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
        self.pipelines.lock().unwrap().insert(
            id.raw(),
            (shader_cost(vs), shader_cost(fs)),
        );
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
        // Layer-2 exec roofline: walk the FrameOp stream, charging each
        // Draw for its bound pipeline's VS (over vertex·instance count) +
        // FS (over the render-target pixel count, a full-coverage proxy).
        let mut time = 0.0;
        let mut energy = 0.0;
        let mut bytes = 0u64;
        // Tier-2/Tier-3 routing costs (joules), accumulated when routing is
        // enabled, via the router's own helpers (consistent units): Tier-3
        // uses this decorator's device profile (== the router's GPU side),
        // Tier-2 uses the router's configured CPU profile.
        let routing_cpu: Option<crate::router::CpuProfile> =
            self.router.as_ref().map(|r| r.lock().unwrap().cpu());
        let mut t2 = crate::router::Cost::default();
        let mut t3 = crate::router::Cost::default();
        // Calibration: sum the ideal roofline branches across draws so the
        // whole-frame modeled cost can be classified compute- vs memory-
        // bound against the measured GPU time.
        let calibrating = self.calibration.is_some();
        let mut cal_ct = 0.0;
        let mut cal_mt = 0.0;
        let mut cur_dims: Option<(u32, u32)> = None;
        let mut cur_mix: Option<(ShaderCost, ShaderCost)> = None;
        // The frame's render target = surface proxy for per-surface routing.
        let mut frame_target: Option<u32> = None;
        let le4 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let mut dec = aqueduct_gpu::frame::FrameDecoder::new(frame_buf);
        while let Ok(Some((op, body))) = dec.next() {
            match op {
                aqueduct_gpu::opcodes::FrameOp::BeginRenderPass if body.len() >= 4 => {
                    let target = le4(body);
                    frame_target.get_or_insert(target);
                    cur_dims = self.images.lock().unwrap().get(&target).copied();
                }
                aqueduct_gpu::opcodes::FrameOp::BindPipeline if body.len() >= 4 => {
                    cur_mix = self.pipelines.lock().unwrap().get(&le4(body)).copied();
                }
                aqueduct_gpu::opcodes::FrameOp::Draw if body.len() >= 8 => {
                    if let Some((vs, fs)) = cur_mix {
                        let verts = le4(&body[0..4]) as u64;
                        let insts = (le4(&body[4..8]) as u64).max(1);
                        let vs_inv = verts * insts;
                        let (t, e) = exec_cost(&self.profile, &vs, vs_inv);
                        time += t;
                        energy += e;
                        bytes += vs.mem_ops as u64 * vs_inv * 16;
                        let pixels = cur_dims.map(|(w, h)| w as u64 * h as u64);
                        if let Some(px) = pixels {
                            let (t2e, e2) = exec_cost(&self.profile, &fs, px);
                            time += t2e;
                            energy += e2;
                            bytes += fs.mem_ops as u64 * px * 16;
                        }
                        // Calibration branch totals (ideal roofline).
                        if calibrating {
                            let (vct, vmt, _, _) =
                                roofline_branches(&self.profile, &vs, vs_inv);
                            cal_ct += vct;
                            cal_mt += vmt;
                            if let Some(px) = pixels {
                                let (fct, fmt, _, _) =
                                    roofline_branches(&self.profile, &fs, px);
                                cal_ct += fct;
                                cal_mt += fmt;
                            }
                        }
                        // Routing costs (joules): VS over vertices + FS over
                        // pixels, on each tier.
                        if let Some(cpu) = &routing_cpu {
                            t2 = t2
                                .plus(crate::router::tier2_exec_cost(cpu, &vs, vs_inv));
                            t3 = t3
                                .plus(crate::router::tier3_exec_cost(&self.profile, &vs, vs_inv));
                            if let Some(px) = pixels {
                                t2 = t2.plus(crate::router::tier2_exec_cost(cpu, &fs, px));
                                t3 = t3.plus(crate::router::tier3_exec_cost(&self.profile, &fs, px));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.ledger.lock().unwrap().ops.push(OpCost {
            kind: OpKind::Exec, bytes, time_s: time, energy_j: energy,
        });
        let n = self.frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        // Live routing observation: score this frame Tier-2 vs Tier-3 and
        // tally the verdict. Pseudo-clock = frame index at a nominal 60 fps
        // (deterministic; gives the GPU residency timer a real timeline).
        if let Some(r) = &self.router {
            let now_s = n as f64 / 60.0;
            let routed = r.lock().unwrap().route_costs(t2, t3, now_s);
            let mut tally = self.route_tally.lock().unwrap();
            match routed.tier {
                crate::router::Tier::Tier2 => tally.0 += 1,
                crate::router::Tier::Tier3 => tally.1 += 1,
            }
            // Feed the verdict into per-surface slow migration (stage 1 of
            // acting on the verdict — still observational: we track the
            // assignment, dispatch is unchanged).
            if let Some(surf) = frame_target {
                self.surfaces.lock().unwrap().record(surf, routed.tier);
            }
        }
        // Periodic ledger summary so the live model is observable under
        // RUST_LOG (every 60 frames ≈ once/second at 60 fps). Cumulative —
        // non-destructive, so other consumers (the router) still drain it.
        if n % 60 == 0 {
            let l = self.ledger.lock().unwrap();
            let (rt2, rt3) = *self.route_tally.lock().unwrap();
            let cal = self.calibration.as_ref().map(|c| *c.lock().unwrap());
            let cal_str = match cal {
                Some(c) => format!(
                    "; calibration comp_eff={:.2} bw_eff={:.2} launch={:.0}us",
                    c.compute_efficiency, c.bandwidth_efficiency,
                    c.launch_overhead_s * 1e6),
                None => String::new(),
            };
            let (st2, st3) = self.surfaces.lock().unwrap().assignment_counts();
            log::info!(
                "device-model[{}]: {} frames, {} ops, modeled {:.3} ms exec, \
                 {:.1} mJ total; routing tally tier2={} tier3={}; \
                 surfaces tier2={} tier3={}{}",
                self.profile.name, n, l.len(),
                l.time_for(OpKind::Exec) * 1e3, l.total_energy_j() * 1e3, rt2, rt3,
                st2, st3, cal_str,
            );
        }
        let ok = self.inner.submit_frame(fence, timeline, frame_buf);
        // Close the calibration loop: fold this frame's measured GPU time
        // (D-M6, from the inner backend) into the efficiency fit.
        if let Some(cal) = &self.calibration {
            if let Some(measured) = self.inner.measured_gpu_time_s() {
                cal.lock().unwrap().observe_branches(cal_ct, cal_mt, measured, 0.2);
            }
        }
        ok
    }
    fn measured_gpu_time_s(&self) -> Option<f64> { self.inner.measured_gpu_time_s() }
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

    /// A minimal valid-enough SPIR-V module with `n` OpFAdd (ALU) ops.
    fn spirv_with_alu(n: u32) -> Vec<u8> {
        let mut words = vec![0x0723_0203u32, 0x0001_0000, 0, 100, 0];
        for _ in 0..n {
            words.extend([(5u32 << 16) | 129, 1, 2, 3, 4]); // OpFAdd wc=5
        }
        spirv_from_words(&words)
    }

    #[test]
    fn submit_frame_charges_layer2_exec() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::opcodes::FrameOp;

        let img = ResourceId(1);
        let pipe = ResourceId(2);
        let vs = spirv_with_alu(5);
        let fs = spirv_with_alu(3);

        // Build a frame: BeginRenderPass(img) + BindPipeline(pipe) + Draw.
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
        let frame = fb.as_bytes().to_vec();

        // Real profile → non-zero modeled exec time for the draw.
        let cm = CostModelBackend::new(StubBackend::new(), DeviceProfile::uma_apple_m4_max());
        cm.image_created(img, 64, 64);
        cm.pipeline_created(pipe, &vs, &fs);
        cm.submit_frame(ResourceId(9), 1, &frame);
        assert!(cm.ledger_snapshot().time_for(OpKind::Exec) > 0.0,
            "a draw charges Layer-2 exec time");

        // Passthrough → zero even with the same frame.
        let cm0 = CostModelBackend::new(StubBackend::new(), DeviceProfile::passthrough());
        cm0.image_created(img, 64, 64);
        cm0.pipeline_created(pipe, &vs, &fs);
        cm0.submit_frame(ResourceId(9), 1, &frame);
        assert_eq!(cm0.ledger_snapshot().time_for(OpKind::Exec), 0.0);
    }

    #[test]
    fn live_routing_tally_reflects_frame_weight() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::opcodes::FrameOp;
        use crate::router::{CpuProfile, GpuPowerModel, RouteMode};

        let build = |img: ResourceId, pipe: ResourceId| {
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
            fb.as_bytes().to_vec()
        };

        // Perf mode: a light frame loses the GPU-wake-latency race (→ CPU);
        // a heavy frame's GPU throughput wins despite the wake (→ GPU).
        let cm = CostModelBackend::new(StubBackend::new(), DeviceProfile::uma_apple_m4_max())
            .with_routing(CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(),
                RouteMode::Perf);

        // Light frame: 32×32, cheap shader → Tier-2.
        let (img_l, pipe_l) = (ResourceId(1), ResourceId(2));
        cm.image_created(img_l, 32, 32);
        cm.pipeline_created(pipe_l, &spirv_with_alu(2), &spirv_with_alu(2));
        cm.submit_frame(ResourceId(9), 1, &build(img_l, pipe_l));
        assert_eq!(cm.route_tally(), (1, 0), "light frame stays on the CPU");

        // Heavy frame: 4K, heavy fragment shader → Tier-3.
        let (img_h, pipe_h) = (ResourceId(3), ResourceId(4));
        cm.image_created(img_h, 3840, 2160);
        cm.pipeline_created(pipe_h, &spirv_with_alu(40), &spirv_with_alu(200));
        cm.submit_frame(ResourceId(9), 2, &build(img_h, pipe_h));
        assert_eq!(cm.route_tally(), (1, 1), "heavy frame routes to the GPU");

        // No-routing decorator never tallies.
        let plain = CostModelBackend::new(StubBackend::new(), DeviceProfile::uma_apple_m4_max());
        plain.image_created(img_l, 32, 32);
        plain.pipeline_created(pipe_l, &spirv_with_alu(2), &spirv_with_alu(2));
        plain.submit_frame(ResourceId(9), 1, &build(img_l, pipe_l));
        assert_eq!(plain.route_tally(), (0, 0));
    }

    #[test]
    fn heavy_surface_migrates_to_tier3_over_many_frames() {
        use aqueduct_gpu::frame::FrameBuilder;
        use aqueduct_gpu::opcodes::FrameOp;
        use crate::router::{CpuProfile, GpuPowerModel, RouteMode};

        let build = |img: ResourceId, pipe: ResourceId| {
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
            fb.as_bytes().to_vec()
        };

        let cm = CostModelBackend::new(StubBackend::new(), DeviceProfile::uma_apple_m4_max())
            .with_routing(CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(),
                RouteMode::Perf);
        let (img, pipe) = (ResourceId(3), ResourceId(4));
        cm.image_created(img, 3840, 2160);
        cm.pipeline_created(pipe, &spirv_with_alu(40), &spirv_with_alu(200));
        let frame = build(img, pipe);

        // One heavy frame votes Tier-3, but the surface holds on Tier-2…
        cm.submit_frame(ResourceId(9), 1, &frame);
        assert_eq!(cm.surface_assignment_counts(), (1, 0), "not after one frame");
        // …and migrates to Tier-3 only after a sustained heavy run.
        for t in 2..=20 { cm.submit_frame(ResourceId(9), t, &frame); }
        assert_eq!(cm.surface_assignment_counts(), (0, 1),
            "the heavy surface migrated to the GPU");
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
    fn ir_classifier_weights_ops_by_real_cost() {
        use atrium_spv_ir::{Op, Type, Value, ValueId};
        let v = || Value { id: ValueId(0), ty: Type::F32 };
        // Cheap arithmetic = 1; divide/sqrt = 4; dot = 3; derivative = 2.
        assert_eq!(classify_ir_op(&Op::FAdd(v(), v())), (1, 0));
        assert_eq!(classify_ir_op(&Op::FDiv(v(), v())), (4, 0), "divide is multi-cycle");
        assert_eq!(classify_ir_op(&Op::FSqrt(v())), (4, 0), "sqrt is multi-cycle");
        assert_eq!(classify_ir_op(&Op::Dot(v(), v())), (3, 0), "dot ≈ a few mul-adds");
        assert_eq!(classify_ir_op(&Op::DPdx(v())), (2, 0), "cross-lane quad op");
        // Memory: plain load = 1 mem; structural = neither.
        assert_eq!(classify_ir_op(&Op::Load(v())), (0, 1));
        assert_eq!(classify_ir_op(&Op::Return), (0, 0), "control flow → other");
    }

    #[test]
    fn present_cost_is_zero_on_uma_and_a_copy_on_discrete() {
        let uma = DeviceProfile::uma_apple_m4_max();
        // UMA: display reads system memory — no present copy, either source.
        assert_eq!(uma.present_cost(1 << 20, ScanoutDomain::Host), (0.0, 0.0));
        assert_eq!(uma.present_cost(1 << 20, ScanoutDomain::Device), (0.0, 0.0));

        let disc = DeviceProfile::discrete_rdna3_pcie4x16(); // scanout = Device
        // GPU output already in VRAM → no copy.
        assert_eq!(disc.present_cost(1 << 20, ScanoutDomain::Device), (0.0, 0.0));
        // CPU output in host memory → a link DMA to VRAM for scanout.
        let (t, e) = disc.present_cost(1 << 20, ScanoutDomain::Host);
        assert!(t > 0.0 && e > 0.0, "desktop dGPU: CPU content copies to VRAM for display");
        // …and it scales with damage size (cheap copy path, link bandwidth).
        let (t2, _) = disc.present_cost(2 << 20, ScanoutDomain::Host);
        assert!(t2 > t);
    }

    #[test]
    fn calibration_prior_is_usable_and_pessimistic() {
        // Uncalibrated, the calibrated time is ≥ the ideal roofline (sub-
        // unity efficiencies) and includes the launch overhead.
        let prof = DeviceProfile::uma_apple_m4_max();
        let mix = ShaderCost { alu_ops: 50, mem_ops: 4, other_ops: 0 };
        let cal = CalibrationProfile::prior();
        let (ideal, _) = exec_cost(&prof, &mix, 1_000_000);
        let real = cal.exec_time_s(&prof, &mix, 1_000_000);
        assert!(real > ideal, "real ({real}) exceeds the ideal roofline ({ideal})");
        assert!(real - ideal >= cal.launch_overhead_s * 0.99, "launch overhead included");
    }

    #[test]
    fn calibration_fit_converges_toward_measured() {
        // A compute-bound shader; feed a measured time slower than ideal and
        // watch compute_efficiency fall toward the implied fraction.
        let prof = DeviceProfile::uma_apple_m4_max();
        let mix = ShaderCost { alu_ops: 100, mem_ops: 0, other_ops: 0 }; // pure compute
        let (ideal, _) = exec_cost(&prof, &mix, 2_000_000);
        let mut cal = CalibrationProfile::prior();
        // The real GPU took 2× the ideal *work* time plus the fixed launch
        // overhead → the implied compute efficiency is exactly 0.5.
        let measured = ideal * 2.0 + cal.launch_overhead_s;
        for _ in 0..50 {
            cal.observe(&prof, &mix, 2_000_000, measured, 0.3);
        }
        assert!((cal.compute_efficiency - 0.5).abs() < 0.05,
            "compute_efficiency converged to ~0.5, got {}", cal.compute_efficiency);
        // The memory branch was never exercised → untouched.
        assert_eq!(cal.bandwidth_efficiency, CalibrationProfile::prior().bandwidth_efficiency);
        // And the calibrated time now ≈ the measured time.
        let real = cal.exec_time_s(&prof, &mix, 2_000_000);
        assert!((real - measured).abs() / measured < 0.05,
            "calibrated time ({real}) tracks measured ({measured})");
    }

    #[test]
    fn calibration_observe_ignores_degenerate_frames() {
        let prof = DeviceProfile::uma_apple_m4_max();
        let mix = ShaderCost { alu_ops: 10, mem_ops: 2, other_ops: 0 };
        let mut cal = CalibrationProfile::prior();
        let before = cal;
        cal.observe(&prof, &mix, 0, 1e-3, 0.3);      // zero invocations
        cal.observe(&prof, &mix, 1000, 0.0, 0.3);    // zero measured
        cal.observe(&DeviceProfile::passthrough(), &mix, 1000, 1e-3, 0.3); // no model
        assert_eq!(cal, before, "degenerate observations are no-ops");
    }

    #[test]
    fn profile_from_name_resolves_the_cli_flag() {
        assert!(DeviceProfile::from_name("passthrough").unwrap().passthrough);
        assert_eq!(DeviceProfile::from_name("uma-apple-m4-max").unwrap().name,
            "uma-apple-m4-max");
        assert!(!DeviceProfile::from_name("discrete-rdna3-pcie4x16").unwrap().passthrough);
        assert!(DeviceProfile::from_name("nonsense").is_none());
    }

    #[test]
    fn shader_cost_handles_a_real_fixture() {
        // A real compiled fragment shader from the atrium-core bundle.
        // Whichever path runs (IR walk if the frontend lowers it, else the
        // histogram floor), a real shader yields a non-empty mix.
        const FRAG: &[u8] =
            include_bytes!("../../bundles/atrium-core/pipelines/pipe_rectangle.frag.spv");
        let mix = shader_cost(FRAG);
        assert!(mix.alu_ops + mix.mem_ops > 0, "a real shader does real work");
        // Non-SPIR-V input degrades to the empty mix, never panics.
        assert_eq!(shader_cost(&[0xAB; 64]), ShaderCost::default());
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
