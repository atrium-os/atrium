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

use crate::cost_model::{exec_cost, DeviceProfile, ShaderCost};

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

/// Tracks the modeled GPU's power residency across a frame timeline so the
/// router can price the wake of an asleep GPU, and so energy-policy can
/// publish the state as a slow read-only signal.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_model::DeviceProfile;

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
