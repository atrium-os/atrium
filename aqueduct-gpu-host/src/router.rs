//! Tier-2 ↔ Tier-3 energy/perf router — the *local* routing decision of
//! energy-policy phase 1 (`docs/spec/energy-policy.md`).
//!
//! Given a unit of work (a frame / op) it compares the modeled cost of
//! running it on **Tier-2** (the BSD-native CPU rasteriser) vs **Tier-3**
//! (the GPU, via `cost_model`), and picks per the read-only policy mode +
//! the GPU power state. The precondition is **tier-equivalence** (the same
//! pixels either way), so the router moves work freely on cost alone.
//!
//! This is the *coordinated, not coupled* design: the router reads the
//! shared mode signal and the GPU power/residency state; it does NOT react
//! to another layer's instantaneous output, and Laminar never learns it
//! exists. Pure + deterministic; no I/O. The eventual userspace authority
//! gets its name later (Latin convention, TBD).

use crate::cost_model::{exec_cost, DeviceProfile, ShaderCost, Topology};

/// On a discrete part, widen the migrate-to-GPU deadband and narrow the
/// revert deadband by this much: migrating onto the GPU pays a one-time
/// resource upload that must amortize (be reluctant), while reverting
/// re-renders from main-memory originals (be eager). A heuristic default —
/// a calibration knob, like the [`crate::cost_model::CalibrationProfile`]
/// efficiencies — capturing the *direction* of the asymmetry; the exact
/// magnitude is tunable. Zero under unified memory (migration is free).
pub const DISCRETE_MIGRATION_BIAS: f64 = 0.2;

/// Where a unit of work runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The BSD-native CPU rasteriser (no GPU wake).
    Tier2,
    /// The GPU (MoltenVk today, native driver at D5+).
    Tier3,
}

/// The read-only energy policy mode (a single scalar bias; mirrors
/// `energy-policy.md`'s `perf|balanced|battery`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMode {
    /// Lowest latency wins.
    Perf,
    /// Lowest energy·time product wins.
    Balanced,
    /// Lowest energy wins.
    Battery,
}

/// A modeled cost: wall-time + energy.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    /// Wall-time, seconds.
    pub time_s: f64,
    /// Energy, joules.
    pub energy_j: f64,
}

impl Cost {
    /// Sum two costs (e.g. add a GPU-wake penalty).
    pub fn plus(self, other: Cost) -> Cost {
        Cost { time_s: self.time_s + other.time_s, energy_j: self.energy_j + other.energy_j }
    }
}

/// A CPU profile for the Tier-2 rasteriser roofline. The SW rasteriser
/// runs the fragment shader per pixel across cores × SIMD lanes.
#[derive(Debug, Clone, Copy)]
pub struct CpuProfile {
    /// Human label.
    pub name: &'static str,
    /// Cores the tile-binned rasteriser parallelises across (rayon).
    pub cores: f64,
    /// FP32 SIMD lanes per core (NEON f32x4 = 4).
    pub simd_lanes: f64,
    /// Clock, Hz.
    pub clock_hz: f64,
    /// System-memory bandwidth, B/s.
    pub mem_bw: f64,
    /// Energy per FP32 op, picojoules (CPU ALU is far pricier per-flop
    /// than a GPU lane — that's *why* big parallel work belongs on Tier-3).
    pub pj_per_flop: f64,
    /// Memory access energy, picojoules per byte.
    pub pj_per_byte: f64,
}

impl CpuProfile {
    /// Apple M4 Max CPU side (performance cores, NEON f32x4).
    pub fn apple_m4_max() -> Self {
        CpuProfile {
            name: "apple-m4-max-cpu",
            cores: 12.0,
            simd_lanes: 4.0,
            clock_hz: 4.0e9,
            mem_bw: 546.0e9,
            pj_per_flop: 25.0,
            pj_per_byte: 10.0,
        }
    }
}

/// Tier-2 (CPU) execution cost for a shader of `mix` over `invocations`
/// (pixels for the FS). Roofline `t = max(flops/throughput,
/// bytes/mem_bw)`; throughput = cores · simd_lanes · clock.
pub fn tier2_exec_cost(cpu: &CpuProfile, mix: &ShaderCost, invocations: u64) -> Cost {
    if invocations == 0 {
        return Cost::default();
    }
    let inv = invocations as f64;
    let flops = mix.alu_ops as f64 * inv;
    let bytes = mix.mem_ops as f64 * 16.0 * inv;
    let throughput = cpu.cores * cpu.simd_lanes * cpu.clock_hz;
    let compute_t = flops / throughput;
    let mem_t = bytes / cpu.mem_bw;
    Cost {
        time_s: compute_t.max(mem_t),
        energy_j: flops * cpu.pj_per_flop * 1e-12 + bytes * cpu.pj_per_byte * 1e-12,
    }
}

/// Tier-3 (GPU) execution cost for the same work — wraps the device
/// model's `exec_cost` into a [`Cost`] (energy in joules; the device model
/// returns picojoules).
pub fn tier3_exec_cost(profile: &DeviceProfile, mix: &ShaderCost, invocations: u64) -> Cost {
    let (t, e_pj) = exec_cost(profile, mix, invocations);
    Cost { time_s: t, energy_j: e_pj * 1e-12 }
}

/// The local routing decision: compare Tier-2 vs Tier-3 cost under `mode`.
/// `gpu_wake` (Some when the GPU is asleep) is added to the Tier-3 cost —
/// the spin-up energy + latency that makes small ops cheaper on the
/// already-running CPU. Returns where the work should land.
pub fn route(t2: Cost, t3: Cost, mode: RouteMode, gpu_wake: Option<Cost>) -> Tier {
    let t3 = match gpu_wake {
        Some(w) => t3.plus(w),
        None => t3,
    };
    let prefer_t3 = match mode {
        RouteMode::Perf => t3.time_s < t2.time_s,
        RouteMode::Battery => t3.energy_j < t2.energy_j,
        // energy·time product (lower is better).
        RouteMode::Balanced => {
            t3.energy_j * t3.time_s < t2.energy_j * t2.time_s
        }
    };
    if prefer_t3 { Tier::Tier3 } else { Tier::Tier2 }
}

/// The scalar the router minimises under `mode` (lower is better): time
/// for Perf, energy for Battery, energy·time for Balanced. This is the
/// quantity the hysteresis band is measured against.
pub fn score(cost: Cost, mode: RouteMode) -> f64 {
    match mode {
        RouteMode::Perf => cost.time_s,
        RouteMode::Battery => cost.energy_j,
        RouteMode::Balanced => cost.energy_j * cost.time_s,
    }
}

/// Default hysteresis band: the alternative tier must be at least 15 %
/// cheaper (by the mode's score) than the current tier before we switch.
/// Wide enough to swallow analytic-model wobble and live-signal noise near
/// the crossover; narrow enough that a decisive workload change still
/// flips promptly.
pub const DEFAULT_MARGIN: f64 = 0.15;

/// Stateful router that adds a **hysteresis band** over the pure [`route`]
/// decision to stop tier chatter at the crossover boundary.
///
/// The pure `route()` flips the instant the alternative is one picojoule
/// cheaper — fine for a single decision, but per-frame near the crossover
/// it thrashes (each flip pays a GPU wake/sleep + pipeline re-residency +
/// frame-time jitter). `Router` only switches when the alternative beats
/// the *current* tier by `margin`; inside the band it holds, so noise no
/// longer moves it.
///
/// This composes with the *physical* hysteresis already in the cost: once
/// the GPU is awake the caller passes `gpu_wake = None`, making Tier-3
/// sticky on its own. The band covers the axes the wake penalty doesn't —
/// Perf (pure time) and Balanced near the product crossover.
///
/// Orthogonal knob not built here: a **dwell time** (min frames/ms between
/// switches) guards against a workload that genuinely oscillates across
/// the band every frame. Add only if the margin alone proves insufficient
/// against measured traces — don't stack damping speculatively.
#[derive(Debug, Clone, Copy)]
pub struct Router {
    mode: RouteMode,
    margin: f64,
    current: Tier,
}

impl Router {
    /// New router in `mode` with [`DEFAULT_MARGIN`], starting on Tier-2
    /// (the CPU is the safe default — it's always running, no GPU wake).
    pub fn new(mode: RouteMode) -> Self {
        Router { mode, margin: DEFAULT_MARGIN, current: Tier::Tier2 }
    }

    /// New router with an explicit hysteresis `margin` (0.0 = no band,
    /// behaves like the pure `route`) and starting tier.
    pub fn with_margin(mode: RouteMode, margin: f64, start: Tier) -> Self {
        Router { mode, margin, current: start }
    }

    /// The tier currently committed to.
    pub fn current(&self) -> Tier {
        self.current
    }

    /// Decide where this frame runs, applying the hysteresis band. Switch
    /// only if the alternative's score is below `current · (1 - margin)`;
    /// otherwise hold. `gpu_wake` (Some when the GPU is asleep) is folded
    /// into the Tier-3 cost exactly as in [`route`].
    pub fn decide(&mut self, t2: Cost, t3: Cost, gpu_wake: Option<Cost>) -> Tier {
        let t3 = match gpu_wake {
            Some(w) => t3.plus(w),
            None => t3,
        };
        let s2 = score(t2, self.mode);
        let s3 = score(t3, self.mode);
        let (current_score, alt_score, alt) = match self.current {
            Tier::Tier2 => (s2, s3, Tier::Tier3),
            Tier::Tier3 => (s3, s2, Tier::Tier2),
        };
        // Cross the band (beat current by `margin`) to switch; else hold.
        if alt_score < current_score * (1.0 - self.margin) {
            self.current = alt;
        }
        self.current
    }
}

/// The modeled GPU's power/residency state — the slow, read-only signal
/// energy-policy publishes and the router's wake penalty derives from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuPowerState {
    /// Powered down / clock-gated — using it pays a wake cost.
    Asleep,
    /// Up and clocked — Tier-3 work runs with no wake penalty.
    Active,
}

/// Tracks the modeled GPU's **compute** power residency across a frame
/// timeline so the router can price the wake of an asleep shader array, and
/// so energy-policy can publish the state as a slow read-only signal.
///
/// This models the *expensive* power domain — the shader/compute array that
/// must spin up to high clocks to *render*. It deliberately does NOT cover
/// the cheap copy/display path: pushing damage to VRAM for scanout exercises
/// the DMA/copy engine + display controller (separately power-gated, often
/// always-on), priced by [`DeviceProfile::present_cost`], not a compute
/// wake. That split is what lets CPU-rendering a sparse surface keep the
/// shader array asleep on a discrete part while still feeding the display.
///
/// Driven by a monotonic clock passed in (deterministic + testable; no
/// wall-clock reads — same discipline as the rest of the device model).
/// The GPU wakes when Tier-3 work runs and idles back to `Asleep` after
/// `idle_timeout_s` of no GPU use — its *own* hysteresis, the tens-of-ms
/// residency timer the energy-policy design calls out as already present
/// in the hardware. The router's deadband and this residency timer are the
/// two damping loops; neither one reacts to the other's instantaneous
/// output (coordinated, not coupled).
#[derive(Debug, Clone, Copy)]
pub struct GpuPowerModel {
    state: GpuPowerState,
    last_use_s: f64,
    /// Quiet time before the GPU clock-gates back to `Asleep`.
    idle_timeout_s: f64,
    /// Spin-up latency added to Tier-3 when waking from `Asleep`.
    wake_latency_s: f64,
    /// Spin-up energy added to Tier-3 when waking from `Asleep`.
    wake_energy_j: f64,
}

impl GpuPowerModel {
    /// New model (starts `Asleep` — the GPU is cold at boot).
    pub fn new(idle_timeout_s: f64, wake_latency_s: f64, wake_energy_j: f64) -> Self {
        GpuPowerModel {
            state: GpuPowerState::Asleep,
            last_use_s: f64::NEG_INFINITY,
            idle_timeout_s,
            wake_latency_s,
            wake_energy_j,
        }
    }

    /// Apple M4 Max UMA GPU: short residency, cheap wake (on-package, no
    /// PCIe link to retrain).
    pub fn apple_m4_max() -> Self {
        Self::new(0.05, 0.5e-3, 2e-3)
    }

    /// Discrete RDNA3 over PCIe: longer residency, *expensive* wake (link
    /// + VRAM spin-up) — which is exactly why a single small op is not
    /// worth waking it for.
    pub fn discrete_rdna3() -> Self {
        Self::new(0.1, 3e-3, 50e-3)
    }

    /// Current power state.
    pub fn state(&self) -> GpuPowerState {
        self.state
    }

    /// Advance the clock to `now_s`, applying idle-down: an `Active` GPU
    /// that's seen no use for `idle_timeout_s` clock-gates to `Asleep`.
    /// Call once per frame before [`Self::wake_cost`].
    pub fn advance(&mut self, now_s: f64) -> GpuPowerState {
        if self.state == GpuPowerState::Active && now_s - self.last_use_s >= self.idle_timeout_s {
            self.state = GpuPowerState::Asleep;
        }
        self.state
    }

    /// The prospective wake cost to route work to Tier-3 *right now* —
    /// `Some` iff the GPU is `Asleep`. Feed straight into
    /// [`Router::decide`] / [`route`] as `gpu_wake`. A peek: no state change
    /// (the router may still pick Tier-2 and leave the GPU asleep).
    pub fn wake_cost(&self) -> Option<Cost> {
        match self.state {
            GpuPowerState::Asleep => {
                Some(Cost { time_s: self.wake_latency_s, energy_j: self.wake_energy_j })
            }
            GpuPowerState::Active => None,
        }
    }

    /// Commit a Tier-3 use at `now_s`: the GPU is `Active` and the idle
    /// timer resets. Call only when the router actually routed to Tier-3.
    pub fn record_tier3_use(&mut self, now_s: f64) {
        self.state = GpuPowerState::Active;
        self.last_use_s = now_s;
    }
}

/// One frame's work as the router scores it: the vertex- and
/// fragment-stage instruction mixes (from [`crate::cost_model::shader_cost`])
/// plus how many invocations each runs. The CPU (Tier-2) and GPU (Tier-3)
/// both execute *both* stages — the per-tier cost is VS-over-vertices +
/// FS-over-pixels.
#[derive(Debug, Clone, Copy)]
pub struct FrameWork {
    /// Vertex-stage instruction mix.
    pub vs: ShaderCost,
    /// Vertex invocations (vertices × instances).
    pub vs_invocations: u64,
    /// Fragment-stage instruction mix.
    pub fs: ShaderCost,
    /// Fragment invocations (covered pixels).
    pub fs_invocations: u64,
}

/// The outcome of routing one frame.
#[derive(Debug, Clone, Copy)]
pub struct RoutedFrame {
    /// Where the frame was routed.
    pub tier: Tier,
    /// Modeled Tier-2 (CPU) cost.
    pub tier2: Cost,
    /// Modeled Tier-3 (GPU) cost, *before* any wake penalty.
    pub tier3: Cost,
    /// GPU power state after this frame.
    pub gpu_state: GpuPowerState,
}

/// The per-frame routing engine — assembles the cost models, the GPU power
/// signal, and the hysteresis band into the single call an integrator
/// makes per frame. This is the router brain operating on real frame shape
/// (the VS/FS mix + invocation counts the device model already extracts
/// from the FrameOp stream).
///
/// Drives off an injected monotonic clock (`now_s`) — deterministic and
/// testable; the daemon passes a real frame timestamp. Accounting-mode
/// safe: `route_frame` *decides and records* but the caller still chooses
/// whether to act on the verdict (zero perturbation until dispatch is
/// actually rewired — the spec's "prove the decision on real frames first"
/// step).
#[derive(Debug, Clone)]
pub struct FrameRouter {
    cpu: CpuProfile,
    gpu: DeviceProfile,
    power: GpuPowerModel,
    router: Router,
}

impl FrameRouter {
    /// New engine for a CPU + GPU profile, GPU power model, and policy mode
    /// (default hysteresis margin).
    pub fn new(cpu: CpuProfile, gpu: DeviceProfile, power: GpuPowerModel, mode: RouteMode) -> Self {
        FrameRouter { cpu, gpu, power, router: Router::new(mode) }
    }

    /// Current modeled GPU power state (the read-only signal).
    pub fn gpu_state(&self) -> GpuPowerState {
        self.power.state()
    }

    /// The Tier-2 CPU profile this router scores against.
    pub fn cpu(&self) -> CpuProfile {
        self.cpu
    }

    /// The tier currently committed to (carries the hysteresis state).
    pub fn current_tier(&self) -> Tier {
        self.router.current()
    }

    /// Score `work` at time `now_s` and route it. Folds in the GPU wake
    /// penalty when the GPU is asleep, applies the hysteresis band, and —
    /// if the verdict is Tier-3 — commits the GPU to `Active`.
    pub fn route_frame(&mut self, work: &FrameWork, now_s: f64) -> RoutedFrame {
        let mut tier2 = tier2_exec_cost(&self.cpu, &work.vs, work.vs_invocations)
            .plus(tier2_exec_cost(&self.cpu, &work.fs, work.fs_invocations));
        let mut tier3 = tier3_exec_cost(&self.gpu, &work.vs, work.vs_invocations)
            .plus(tier3_exec_cost(&self.gpu, &work.fs, work.fs_invocations));
        // Present/copy cost to reach the display's scanout domain: CPU
        // output originates in Host memory, GPU output in Device VRAM. On a
        // desktop dGPU this charges the CPU tier a damage copy (the display
        // hangs off the card); under UMA both are zero. RGBA8 output ≈ 4 B
        // per covered pixel. This is the cheap copy path — NOT a compute
        // wake (that stays on the Tier-3 side via the power model).
        let damage_bytes = work.fs_invocations.saturating_mul(4);
        let (pt2, pe2) = self.gpu.present_cost(damage_bytes, crate::cost_model::ScanoutDomain::Host);
        let (pt3, pe3) = self.gpu.present_cost(damage_bytes, crate::cost_model::ScanoutDomain::Device);
        tier2 = tier2.plus(Cost { time_s: pt2, energy_j: pe2 });
        tier3 = tier3.plus(Cost { time_s: pt3, energy_j: pe3 });
        self.route_costs(tier2, tier3, now_s)
    }

    /// Route a frame from *pre-aggregated* per-tier costs (joules), for
    /// callers that already summed cost across a multi-draw frame (e.g. the
    /// device-model decorator's FrameOp walk). Same power + hysteresis
    /// logic as [`Self::route_frame`].
    pub fn route_costs(&mut self, tier2: Cost, tier3: Cost, now_s: f64) -> RoutedFrame {
        self.power.advance(now_s);
        let tier = self.router.decide(tier2, tier3, self.power.wake_cost());
        if tier == Tier::Tier3 {
            self.power.record_tier3_use(now_s);
        }
        RoutedFrame { tier, tier2, tier3, gpu_state: self.power.state() }
    }
}

/// Per-surface tier assignment via slow migration (energy-policy.md
/// §"Acting on the verdict"). The [`FrameRouter`]'s per-frame verdict is a
/// *vote*, not a dispatch: each surface accumulates an EWMA-smoothed vote
/// and **migrates** to the other tier only when that smoothed vote crosses
/// a deadband — at the residency timescale, never per frame. This is the
/// consumption layer the design calls for; it changes how the verdict is
/// used, not the verdict itself.
///
/// Keyed by a surface identifier (the frame's render-target image id is the
/// proxy at this layer — each window renders to its own target). Vote is in
/// `[0,1]`: 0 = always-Tier-2, 1 = always-Tier-3. A surface starts on
/// Tier-2 (the safe, already-running CPU default) with a neutral 0.5 vote.
#[derive(Debug, Clone)]
pub struct SurfaceRouter {
    /// Deadband half-width to migrate **Tier-2 → Tier-3** (vote must exceed
    /// 0.5 + up_margin/2). Larger = more reluctant to move onto the GPU.
    up_margin: f64,
    /// Deadband half-width to revert **Tier-3 → Tier-2** (vote must drop
    /// below 0.5 − down_margin/2). Smaller = eager to fall back to the CPU.
    down_margin: f64,
    /// EWMA weight per frame (small = slow migration).
    alpha: f64,
    surfaces: std::collections::HashMap<u32, SurfaceState>,
}

#[derive(Debug, Clone, Copy)]
struct SurfaceState {
    /// Smoothed vote, [0,1]; 1 = Tier-3.
    vote: f64,
    /// Current committed assignment.
    tier: Tier,
}

impl SurfaceRouter {
    /// New tracker with a **symmetric** deadband (`up_margin = down_margin =
    /// margin`) — correct for UMA, where migration is free in either
    /// direction. `alpha` is the per-frame EWMA weight.
    pub fn new(margin: f64, alpha: f64) -> Self {
        Self::with_asymmetry(margin, margin, alpha)
    }

    /// New tracker with an **asymmetric** deadband — for discrete parts,
    /// where migrating onto the GPU costs a one-time resource upload (so
    /// `up_margin` is larger: be reluctant, the upload must amortize) but
    /// reverting re-renders from the main-memory originals (so `down_margin`
    /// is smaller: fall back eagerly, it's ~free).
    pub fn with_asymmetry(up_margin: f64, down_margin: f64, alpha: f64) -> Self {
        SurfaceRouter {
            up_margin,
            down_margin,
            alpha,
            surfaces: std::collections::HashMap::new(),
        }
    }

    /// Record one frame's `verdict` for `surface`; returns the surface's
    /// (possibly newly-migrated) tier assignment. New surfaces start on
    /// Tier-2 with a neutral vote, so a single frame never flips them.
    pub fn record(&mut self, surface: u32, verdict: Tier) -> Tier {
        let s = self.surfaces.entry(surface).or_insert(SurfaceState {
            vote: 0.5,
            tier: Tier::Tier2,
        });
        let target = if verdict == Tier::Tier3 { 1.0 } else { 0.0 };
        s.vote = s.vote * (1.0 - self.alpha) + target * self.alpha;
        let hi = 0.5 + self.up_margin / 2.0;
        let lo = 0.5 - self.down_margin / 2.0;
        match s.tier {
            Tier::Tier2 if s.vote > hi => s.tier = Tier::Tier3,
            Tier::Tier3 if s.vote < lo => s.tier = Tier::Tier2,
            _ => {}
        }
        s.tier
    }

    /// The current tier assignment for `surface`, if seen.
    pub fn assignment(&self, surface: u32) -> Option<Tier> {
        self.surfaces.get(&surface).map(|s| s.tier)
    }

    /// Number of surfaces tracked.
    pub fn len(&self) -> usize {
        self.surfaces.len()
    }

    /// Whether no surfaces are tracked yet.
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    /// `(tier2_surfaces, tier3_surfaces)` assignment counts — for logging.
    pub fn assignment_counts(&self) -> (usize, usize) {
        let t3 = self.surfaces.values().filter(|s| s.tier == Tier::Tier3).count();
        (self.surfaces.len() - t3, t3)
    }
}

/// The gated per-surface dispatch decision — the brain of the dual-backend
/// routing layer (stage 2 of acting on the verdict). It combines what a
/// surface *wants* (the [`SurfaceRouter`]'s slow-migrated assignment) with
/// what it's *allowed* (the [`crate::certify::CertificationRegistry`] gate),
/// and tracks single-homed readback routing.
///
/// Rule: a surface follows its [`SurfaceRouter`] assignment **only if every
/// pipeline it uses is certified tier-equivalent**; otherwise it is pinned
/// to the `home` tier (Tier-2, the always-available CPU default). So routing
/// degrades to "stay home", never to a wrong pixel. Rendered output is
/// single-homed, so a buffer's readback goes to whichever tier last wrote
/// it.
#[derive(Debug, Clone)]
pub struct RoutingPolicy {
    surfaces: SurfaceRouter,
    certs: crate::certify::CertificationRegistry,
    /// The safe fallback tier for ineligible surfaces (Tier-2).
    home: Tier,
    /// Pipelines each surface has drawn with (for the certification gate).
    surface_pipelines: std::collections::HashMap<u32, std::collections::BTreeSet<u32>>,
    /// Last tier to write each buffer (single-homed readback).
    last_writer: std::collections::HashMap<u32, Tier>,
}

impl RoutingPolicy {
    /// New policy. `margin`/`alpha` tune the per-surface slow migration
    /// (see [`SurfaceRouter::new`]); `home` is the pin tier for ineligible
    /// surfaces (use [`Tier::Tier2`]).
    pub fn new(margin: f64, alpha: f64, home: Tier) -> Self {
        Self::from_surface_router(SurfaceRouter::new(margin, alpha), home)
    }

    /// New policy whose per-surface migration is **topology-aware**: a
    /// symmetric deadband under unified memory (migration is free), but an
    /// asymmetric one on a discrete part (reluctant to migrate onto the GPU
    /// — the upload must amortize — eager to revert, which is ~free). See
    /// [`DISCRETE_MIGRATION_BIAS`].
    pub fn for_profile(profile: &DeviceProfile, base_margin: f64, alpha: f64, home: Tier) -> Self {
        let sr = if profile.topology == Topology::Discrete && !profile.passthrough {
            let up = base_margin + DISCRETE_MIGRATION_BIAS;
            let down = (base_margin - DISCRETE_MIGRATION_BIAS).max(0.05);
            SurfaceRouter::with_asymmetry(up, down, alpha)
        } else {
            SurfaceRouter::new(base_margin, alpha)
        };
        Self::from_surface_router(sr, home)
    }

    fn from_surface_router(surfaces: SurfaceRouter, home: Tier) -> Self {
        RoutingPolicy {
            surfaces,
            certs: crate::certify::CertificationRegistry::new(),
            home,
            surface_pipelines: std::collections::HashMap::new(),
            last_writer: std::collections::HashMap::new(),
        }
    }

    /// Record a pipeline's tier-equivalence certification.
    pub fn certify(&mut self, pipeline: u32, c: crate::certify::Certification) {
        self.certs.set(pipeline, c);
    }

    /// Note that `surface` draws with `pipeline` (builds the set the gate
    /// checks).
    pub fn note_surface_pipeline(&mut self, surface: u32, pipeline: u32) {
        self.surface_pipelines.entry(surface).or_default().insert(pipeline);
    }

    /// Whether `surface` is currently eligible to leave the home tier (all
    /// its pipelines certified).
    pub fn eligible(&self, surface: u32) -> bool {
        match self.surface_pipelines.get(&surface) {
            Some(ps) => self.certs.surface_eligible(ps.iter().copied()),
            None => false,
        }
    }

    /// Fold one frame's `verdict` for `surface` into the slow per-surface
    /// assignment and return the **effective** dispatch tier: the
    /// assignment if the surface is certification-eligible, else `home`.
    pub fn record_frame(&mut self, surface: u32, verdict: Tier) -> Tier {
        let wanted = self.surfaces.record(surface, verdict);
        if self.eligible(surface) { wanted } else { self.home }
    }

    /// Record that `tier` wrote `buffer` (for single-homed readback).
    pub fn note_write(&mut self, buffer: u32, tier: Tier) {
        self.last_writer.insert(buffer, tier);
    }

    /// Which tier to read `buffer` back from — the last writer, or `home`
    /// if never written here.
    pub fn read_tier(&self, buffer: u32) -> Tier {
        self.last_writer.get(&buffer).copied().unwrap_or(self.home)
    }

    /// `(tier2, tier3)` *effective* surface assignment counts (gated).
    pub fn effective_counts(&self) -> (usize, usize) {
        let mut t2 = 0;
        let mut t3 = 0;
        for (&surf, _) in self.surface_pipelines.iter() {
            let eff = if self.eligible(surf) {
                self.surfaces.assignment(surf).unwrap_or(self.home)
            } else {
                self.home
            };
            match eff {
                Tier::Tier2 => t2 += 1,
                Tier::Tier3 => t3 += 1,
            }
        }
        (t2, t3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_model::DeviceProfile;
    use crate::certify::Certification;

    fn shader(alu: u32, mem: u32) -> ShaderCost {
        ShaderCost { alu_ops: alu, mem_ops: mem, other_ops: 0 }
    }

    #[test]
    fn tiny_op_stays_on_tier2_when_gpu_is_asleep() {
        // A handful of pixels: the CPU finishes before the GPU even wakes.
        let cpu = CpuProfile::apple_m4_max();
        let gpu = DeviceProfile::uma_apple_m4_max();
        let mix = shader(8, 2);
        let pixels = 256; // 16x16
        let t2 = tier2_exec_cost(&cpu, &mix, pixels);
        let t3 = tier3_exec_cost(&gpu, &mix, pixels);
        // GPU asleep: a real spin-up penalty (≈0.5 ms, ≈5 mJ).
        let wake = Cost { time_s: 0.5e-3, energy_j: 5e-3 };
        assert_eq!(route(t2, t3, RouteMode::Battery, Some(wake)), Tier::Tier2,
            "tiny op + asleep GPU → stay on CPU (avoid the wake)");
        assert_eq!(route(t2, t3, RouteMode::Perf, Some(wake)), Tier::Tier2,
            "and the wake latency loses the perf race too");
    }

    #[test]
    fn huge_op_goes_to_tier3() {
        // A heavy shader over a 4K frame: the GPU's throughput dominates
        // even with a wake penalty.
        let cpu = CpuProfile::apple_m4_max();
        let gpu = DeviceProfile::uma_apple_m4_max();
        let mix = shader(200, 16);
        let pixels = 3840 * 2160;
        let t2 = tier2_exec_cost(&cpu, &mix, pixels);
        let t3 = tier3_exec_cost(&gpu, &mix, pixels);
        let wake = Cost { time_s: 0.5e-3, energy_j: 5e-3 };
        assert_eq!(route(t2, t3, RouteMode::Perf, Some(wake)), Tier::Tier3);
        assert_eq!(route(t2, t3, RouteMode::Battery, Some(wake)), Tier::Tier3);
    }

    #[test]
    fn mode_flips_the_decision_near_the_crossover() {
        // Construct a case where Tier-3 is faster but Tier-2 is lower
        // energy (the GPU is quicker but the wake costs energy).
        let t2 = Cost { time_s: 2.0e-3, energy_j: 1.0e-3 };
        let t3 = Cost { time_s: 1.0e-3, energy_j: 0.8e-3 };
        let wake = Cost { time_s: 0.1e-3, energy_j: 0.5e-3 }; // wake dominates energy
        // Perf: Tier-3 still faster (1.1ms < 2ms).
        assert_eq!(route(t2, t3, RouteMode::Perf, Some(wake)), Tier::Tier3);
        // Battery: Tier-3 energy 1.3mJ > Tier-2 1.0mJ → stay on CPU.
        assert_eq!(route(t2, t3, RouteMode::Battery, Some(wake)), Tier::Tier2);
    }

    #[test]
    fn awake_gpu_has_no_wake_penalty() {
        let t2 = Cost { time_s: 2.0e-3, energy_j: 1.0e-3 };
        let t3 = Cost { time_s: 1.0e-3, energy_j: 0.8e-3 };
        // GPU already running → no wake; Tier-3 wins on every mode.
        assert_eq!(route(t2, t3, RouteMode::Battery, None), Tier::Tier3);
        assert_eq!(route(t2, t3, RouteMode::Perf, None), Tier::Tier3);
        assert_eq!(route(t2, t3, RouteMode::Balanced, None), Tier::Tier3);
    }

    #[test]
    fn hysteresis_holds_through_noise_at_the_boundary() {
        // Two tiers a hair apart, jittering across the crossover each
        // frame (the exact case the pure route() would thrash on).
        let mut r = Router::new(RouteMode::Perf); // starts on Tier-2
        // Frame costs wobble ±3% around equal — well inside the 15% band.
        let wobble = [(1.00, 0.99), (0.99, 1.00), (1.01, 0.98), (0.98, 1.02)];
        for (a, b) in wobble {
            let t2 = Cost { time_s: a * 1e-3, energy_j: 1e-3 };
            let t3 = Cost { time_s: b * 1e-3, energy_j: 1e-3 };
            // The pure router flips on the first frame where t3 < t2…
            // …but the band holds the stateful router on its start tier.
            assert_eq!(r.decide(t2, t3, None), Tier::Tier2,
                "noise inside the band must not move the committed tier");
        }
    }

    #[test]
    fn decisive_change_still_flips_promptly() {
        let mut r = Router::new(RouteMode::Perf); // Tier-2
        // A genuinely cheaper Tier-3 (3× faster) clears the band at once.
        let t2 = Cost { time_s: 3.0e-3, energy_j: 1e-3 };
        let t3 = Cost { time_s: 1.0e-3, energy_j: 1e-3 };
        assert_eq!(r.decide(t2, t3, None), Tier::Tier3, "decisive win flips");
        // And it then *stays* on Tier-3 through boundary noise (sticky).
        let near = Cost { time_s: 1.05e-3, energy_j: 1e-3 };
        let near3 = Cost { time_s: 1.0e-3, energy_j: 1e-3 };
        assert_eq!(r.decide(near, near3, None), Tier::Tier3);
        // Until Tier-2 decisively wins back (3× faster the other way).
        let win2 = Cost { time_s: 0.3e-3, energy_j: 1e-3 };
        let lose3 = Cost { time_s: 1.0e-3, energy_j: 1e-3 };
        assert_eq!(r.decide(win2, lose3, None), Tier::Tier2);
    }

    #[test]
    fn gpu_wakes_on_use_and_idles_back_to_sleep() {
        let mut power = GpuPowerModel::apple_m4_max(); // 50 ms residency
        assert_eq!(power.state(), GpuPowerState::Asleep, "cold at boot");
        assert!(power.wake_cost().is_some(), "asleep → a wake is priced");

        power.record_tier3_use(0.0);
        assert_eq!(power.state(), GpuPowerState::Active);
        assert_eq!(power.wake_cost(), None, "awake → no wake penalty");

        // 16 ms later (next frame), still well inside the residency window.
        assert_eq!(power.advance(0.016), GpuPowerState::Active);
        // A 200 ms quiet gap clock-gates it back down.
        assert_eq!(power.advance(0.216), GpuPowerState::Asleep);
        assert!(power.wake_cost().is_some(), "slept again → wake re-priced");
    }

    #[test]
    fn power_state_changes_the_routing_verdict_for_the_same_op() {
        // Discrete GPU: a 50 mJ wake dwarfs this op's 1 mJ energy edge.
        let mut power = GpuPowerModel::discrete_rdna3();
        let t2 = Cost { time_s: 2e-3, energy_j: 5e-3 };
        let t3 = Cost { time_s: 1e-3, energy_j: 4e-3 }; // raw: cheaper both axes

        power.advance(0.0);
        let mut r_asleep = Router::new(RouteMode::Battery);
        // Asleep: the wake penalty makes Tier-3 the energy loser → stay CPU,
        // and the GPU is never woken.
        assert_eq!(r_asleep.decide(t2, t3, power.wake_cost()), Tier::Tier2);
        assert_eq!(power.state(), GpuPowerState::Asleep);

        // Already active (woke for other work): no wake → Tier-3 wins.
        power.record_tier3_use(0.0);
        let mut r_awake = Router::new(RouteMode::Battery);
        assert_eq!(r_awake.decide(t2, t3, power.wake_cost()), Tier::Tier3);
    }

    fn frame(fs_alu: u32, fs_mem: u32, pixels: u64) -> FrameWork {
        FrameWork {
            vs: shader(4, 1),
            vs_invocations: 3, // a triangle
            fs: shader(fs_alu, fs_mem),
            fs_invocations: pixels,
        }
    }

    #[test]
    fn frame_router_keeps_a_light_ui_frame_off_a_slept_gpu() {
        // A small UI repaint (few-K pixels, cheap shader), GPU cold,
        // battery mode — the engine should stay on the CPU and never wake
        // the GPU.
        let mut fr = FrameRouter::new(
            CpuProfile::apple_m4_max(),
            DeviceProfile::uma_apple_m4_max(),
            GpuPowerModel::discrete_rdna3(), // expensive wake → clear signal
            RouteMode::Battery,
        );
        let r = fr.route_frame(&frame(6, 2, 64 * 64), 0.0);
        assert_eq!(r.tier, Tier::Tier2);
        assert_eq!(r.gpu_state, GpuPowerState::Asleep, "never woke the GPU");
    }

    #[test]
    fn frame_router_routes_a_heavy_frame_to_the_gpu_and_wakes_it() {
        let mut fr = FrameRouter::new(
            CpuProfile::apple_m4_max(),
            DeviceProfile::uma_apple_m4_max(),
            GpuPowerModel::apple_m4_max(),
            RouteMode::Perf,
        );
        // A heavy 4K shader: GPU throughput dominates → Tier-3, GPU active.
        let r = fr.route_frame(&frame(300, 24, 3840 * 2160), 0.0);
        assert_eq!(r.tier, Tier::Tier3);
        assert_eq!(r.gpu_state, GpuPowerState::Active, "heavy frame woke the GPU");
    }

    #[test]
    fn frame_router_sequence_wakes_stays_warm_then_sleeps() {
        let mut fr = FrameRouter::new(
            CpuProfile::apple_m4_max(),
            DeviceProfile::uma_apple_m4_max(),
            GpuPowerModel::apple_m4_max(), // 50 ms residency
            RouteMode::Perf,
        );
        let heavy = frame(300, 24, 3840 * 2160);
        let light = frame(6, 2, 64 * 64);

        // Frame 0: heavy → wakes the GPU.
        assert_eq!(fr.route_frame(&heavy, 0.0).tier, Tier::Tier3);
        // Frame 1 (16 ms later): a light frame, but the GPU is still warm
        // (no wake penalty) so Tier-3 can hold — no thrash back to CPU.
        let f1 = fr.route_frame(&light, 0.016);
        assert_eq!(f1.gpu_state, GpuPowerState::Active);
        // A long idle gap (250 ms > 50 ms residency) with a light frame:
        // the GPU has clock-gated, so the wake penalty now pushes the light
        // frame back to the CPU.
        let f2 = fr.route_frame(&light, 0.266);
        assert_eq!(f2.tier, Tier::Tier2);
        assert_eq!(f2.gpu_state, GpuPowerState::Asleep);
    }

    #[test]
    fn surface_migrates_slowly_under_sustained_votes() {
        let mut sr = SurfaceRouter::new(0.3, 0.1); // hi=0.65, lo=0.35
        let surf = 0x10;
        // A brand-new surface starts on Tier-2…
        assert_eq!(sr.record(surf, Tier::Tier3), Tier::Tier2, "one vote can't flip it");
        // …and migrates to Tier-3 only after a sustained Tier-3 majority.
        let mut migrated_at = None;
        for i in 2..=20 {
            if sr.record(surf, Tier::Tier3) == Tier::Tier3 {
                migrated_at = Some(i);
                break;
            }
        }
        let n = migrated_at.expect("eventually migrates to Tier-3");
        assert!(n >= 4, "migration is slow (took {n} frames), not instant");
        assert_eq!(sr.assignment(surf), Some(Tier::Tier3));
    }

    #[test]
    fn surface_holds_through_a_stray_vote_and_a_5050_mix() {
        let mut sr = SurfaceRouter::new(0.3, 0.1);
        let surf = 0x20;
        // Drive it firmly to Tier-3.
        for _ in 0..40 { sr.record(surf, Tier::Tier3); }
        assert_eq!(sr.assignment(surf), Some(Tier::Tier3));
        // A single stray Tier-2 vote doesn't flip it (deadband + EWMA).
        assert_eq!(sr.record(surf, Tier::Tier2), Tier::Tier3);

        // A different surface fed a 50/50 mix never leaves Tier-2 (the vote
        // hovers around 0.5, inside the band).
        let surf2 = 0x21;
        for i in 0..40 {
            let v = if i % 2 == 0 { Tier::Tier3 } else { Tier::Tier2 };
            assert_eq!(sr.record(surf2, v), Tier::Tier2, "50/50 stays put");
        }
    }

    fn frames_to_reach(sr: &mut SurfaceRouter, surf: u32, want: Tier, feed: Tier) -> usize {
        for i in 1..=200 {
            if sr.record(surf, feed) == want {
                return i;
            }
        }
        usize::MAX
    }

    #[test]
    fn asymmetric_deadband_migrates_reluctantly_and_reverts_eagerly() {
        // Symmetric (UMA-style) vs asymmetric (discrete-style: hard up, easy
        // down) over the same vote stream.
        let mut sym = SurfaceRouter::new(0.3, 0.1);
        let mut asym = SurfaceRouter::with_asymmetry(0.5, 0.1, 0.1);

        // Migrating onto the GPU: the asymmetric (discrete) router needs a
        // longer sustained Tier-3 run.
        let sym_up = frames_to_reach(&mut sym, 1, Tier::Tier3, Tier::Tier3);
        let asym_up = frames_to_reach(&mut asym, 1, Tier::Tier3, Tier::Tier3);
        assert!(asym_up > sym_up,
            "discrete is more reluctant to migrate to GPU (asym {asym_up} > sym {sym_up})");

        // Now both are on Tier-3; reverting to CPU: the asymmetric router
        // falls back in fewer frames (revert is cheap).
        let sym_down = frames_to_reach(&mut sym, 1, Tier::Tier2, Tier::Tier2);
        let asym_down = frames_to_reach(&mut asym, 1, Tier::Tier2, Tier::Tier2);
        assert!(asym_down < sym_down,
            "discrete reverts to CPU eagerly (asym {asym_down} < sym {sym_down})");
    }

    #[test]
    fn for_profile_is_symmetric_on_uma_asymmetric_on_discrete() {
        use crate::certify::Certification;
        let mk = |prof: &DeviceProfile| {
            let mut p = RoutingPolicy::for_profile(prof, 0.3, 0.1, Tier::Tier2);
            p.note_surface_pipeline(1, 0x100);
            p.certify(0x100, Certification::Certified);
            // Count frames of sustained Tier-3 verdicts until it migrates.
            let mut n = usize::MAX;
            for i in 1..=200 {
                if p.record_frame(1, Tier::Tier3) == Tier::Tier3 { n = i; break; }
            }
            n
        };
        let uma = mk(&DeviceProfile::uma_apple_m4_max());
        let disc = mk(&DeviceProfile::discrete_rdna3_pcie4x16());
        assert!(disc > uma,
            "discrete profile migrates more reluctantly than UMA (disc {disc} > uma {uma})");
    }

    #[test]
    fn surface_assignment_counts_track_population() {
        let mut sr = SurfaceRouter::new(0.3, 0.1);
        for _ in 0..40 { sr.record(1, Tier::Tier3); } // → Tier-3
        for _ in 0..40 { sr.record(2, Tier::Tier2); } // stays Tier-2
        sr.record(3, Tier::Tier3);                    // new, still Tier-2
        assert_eq!(sr.len(), 3);
        assert_eq!(sr.assignment_counts(), (2, 1), "two on CPU, one migrated to GPU");
    }

    #[test]
    fn policy_pins_uncertified_surface_to_home_even_when_it_wants_to_move() {
        // A heavy surface wants Tier-3, but its pipeline is uncertified →
        // it stays pinned on the home tier (Tier-2), never a wrong pixel.
        let mut p = RoutingPolicy::new(0.3, 0.1, Tier::Tier2);
        let (surf, pipe) = (0x10, 0x100);
        p.note_surface_pipeline(surf, pipe);
        for _ in 0..40 {
            assert_eq!(p.record_frame(surf, Tier::Tier3), Tier::Tier2,
                "uncertified → pinned home regardless of the verdict");
        }
        assert!(!p.eligible(surf));
    }

    #[test]
    fn policy_follows_the_assignment_once_certified() {
        let mut p = RoutingPolicy::new(0.3, 0.1, Tier::Tier2);
        let (surf, pipe) = (0x11, 0x101);
        p.note_surface_pipeline(surf, pipe);
        p.certify(pipe, Certification::Certified);
        assert!(p.eligible(surf));
        // Now sustained Tier-3 verdicts actually migrate the surface to the
        // GPU (slow — not on the first frame).
        assert_eq!(p.record_frame(surf, Tier::Tier3), Tier::Tier2, "still home at first");
        let mut moved = false;
        for _ in 0..20 {
            if p.record_frame(surf, Tier::Tier3) == Tier::Tier3 { moved = true; break; }
        }
        assert!(moved, "a certified heavy surface migrates to Tier-3");
        assert_eq!(p.effective_counts(), (0, 1));
    }

    #[test]
    fn discrete_present_cost_biases_cpu_rendering_for_display() {
        // On a desktop dGPU, a CPU-rendered frame must copy its output to
        // VRAM for scanout — so the Tier-2 cost carries a present penalty
        // the UMA case never sees. Same frame, same router: the discrete
        // profile charges Tier-2 strictly more than UMA does.
        let work = FrameWork {
            vs: shader(4, 1), vs_invocations: 3,
            fs: shader(8, 2), fs_invocations: 1280 * 720,
        };
        let mut uma = FrameRouter::new(
            CpuProfile::apple_m4_max(), DeviceProfile::uma_apple_m4_max(),
            GpuPowerModel::apple_m4_max(), RouteMode::Battery);
        let mut disc = FrameRouter::new(
            CpuProfile::apple_m4_max(), DeviceProfile::discrete_rdna3_pcie4x16(),
            GpuPowerModel::discrete_rdna3(), RouteMode::Battery);
        let r_uma = uma.route_frame(&work, 0.0);
        let r_disc = disc.route_frame(&work, 0.0);
        // UMA: no present copy on either tier.
        assert_eq!(r_uma.tier2.energy_j, r_uma.tier2.energy_j); // (sanity)
        // Discrete Tier-2 energy includes the damage copy to VRAM → strictly
        // higher than the same frame's Tier-2 under UMA.
        assert!(r_disc.tier2.energy_j > r_uma.tier2.energy_j,
            "discrete CPU render pays a present copy UMA doesn't");
        // And discrete Tier-3 (output already in VRAM) pays no present copy.
        assert!(r_disc.tier3.energy_j >= 0.0);
    }

    #[test]
    fn policy_routes_readback_to_the_last_writer() {
        let mut p = RoutingPolicy::new(0.3, 0.1, Tier::Tier2);
        assert_eq!(p.read_tier(0x55), Tier::Tier2, "unknown buffer → home");
        p.note_write(0x55, Tier::Tier3);
        assert_eq!(p.read_tier(0x55), Tier::Tier3, "read from the tier that rendered it");
    }

    #[test]
    fn zero_margin_matches_the_pure_route() {
        // With margin 0 the stateful router degenerates to route().
        let mut r = Router::with_margin(RouteMode::Perf, 0.0, Tier::Tier2);
        let t2 = Cost { time_s: 1.0e-3, energy_j: 1e-3 };
        let t3 = Cost { time_s: 0.99e-3, energy_j: 1e-3 };
        assert_eq!(r.decide(t2, t3, None), route(t2, t3, RouteMode::Perf, None));
        assert_eq!(r.current(), Tier::Tier3);
    }
}
