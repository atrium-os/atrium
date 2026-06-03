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
}
