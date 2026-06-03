//! Per-resource residency tracker for single-homed routing
//! (`docs/spec/energy-policy.md` §"Single-homed resource residency").
//!
//! Instead of mirroring every resource op to both backends, the router
//! *records* each op against its resource and tracks which tiers the
//! resource is **resident** on. A tier is made resident for a resource only
//! when a frame dispatched to that tier needs it (lazy materialisation) —
//! so a CPU-routed surface never uploads its textures to VRAM, the discrete
//! win.
//!
//! The replay is a retain-log: per resource, the ordered ops needed to
//! recreate it on a fresh tier. (Collapsing superseded writes to bound the
//! log to *current* state — per the design — is a later refinement; today
//! the full op history is retained.)

use std::collections::{HashMap, HashSet};

use crate::backend::Backend;
use crate::router::Tier;

/// One recorded resource op, replayable against any backend.
pub type ReplayOp = Box<dyn Fn(&dyn Backend) + Send + Sync>;

/// Tracks recorded resource ops + per-tier residency.
#[derive(Default)]
pub struct ResidencyTracker {
    /// resource id → ordered ops to recreate it on a tier.
    ops: HashMap<u32, Vec<ReplayOp>>,
    t2_resident: HashSet<u32>,
    t3_resident: HashSet<u32>,
}

impl ResidencyTracker {
    /// Empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `resource` is materialised on `tier`.
    pub fn is_resident(&self, resource: u32, tier: Tier) -> bool {
        match tier {
            Tier::Tier2 => self.t2_resident.contains(&resource),
            Tier::Tier3 => self.t3_resident.contains(&resource),
        }
    }

    /// Record a resource op, and apply it immediately to any tier the
    /// resource is *already* resident on (a live tier must stay current).
    pub fn record(&mut self, resource: u32, op: ReplayOp, t2: &dyn Backend, t3: &dyn Backend) {
        if self.t2_resident.contains(&resource) {
            op(t2);
        }
        if self.t3_resident.contains(&resource) {
            op(t3);
        }
        self.ops.entry(resource).or_default().push(op);
    }

    /// Materialise each of `resources` on `tier`: replay its recorded ops to
    /// `backend` (in order) if not already resident, then mark it resident.
    pub fn materialize(
        &mut self,
        resources: impl IntoIterator<Item = u32>,
        tier: Tier,
        backend: &dyn Backend,
    ) {
        for r in resources {
            let already = match tier {
                Tier::Tier2 => self.t2_resident.contains(&r),
                Tier::Tier3 => self.t3_resident.contains(&r),
            };
            if already {
                continue;
            }
            if let Some(ops) = self.ops.get(&r) {
                for op in ops {
                    op(backend);
                }
            }
            match tier {
                Tier::Tier2 => self.t2_resident.insert(r),
                Tier::Tier3 => self.t3_resident.insert(r),
            };
        }
    }

    /// Materialise the **whole** recorded world on `tier` — the conservative
    /// fallback when a frame's resource set can't be fully introspected (an
    /// undecoded op might reference a resource we'd otherwise miss).
    pub fn materialize_all(&mut self, tier: Tier, backend: &dyn Backend) {
        let all: Vec<u32> = self.ops.keys().copied().collect();
        self.materialize(all, tier, backend);
    }

    /// Forget a resource (on destroy): drop its ops + residency.
    pub fn forget(&mut self, resource: u32) {
        self.ops.remove(&resource);
        self.t2_resident.remove(&resource);
        self.t3_resident.remove(&resource);
    }

    /// `(t2_resident, t3_resident)` resource counts — observability.
    pub fn resident_counts(&self) -> (usize, usize) {
        (self.t2_resident.len(), self.t3_resident.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqueduct_gpu::backends::{BackendId, GpuVendor};
    use aqueduct_gpu::ids::ResourceId;
    use std::sync::Mutex;

    /// Records which image ids were created on it.
    struct Rec {
        created: Mutex<Vec<u32>>,
    }
    impl Rec {
        fn new() -> Self {
            Rec { created: Mutex::new(Vec::new()) }
        }
    }
    impl Backend for Rec {
        fn identity(&self) -> BackendId {
            BackendId::new(GpuVendor::Software, 0)
        }
        fn caps(&self) -> u64 {
            0
        }
        fn max_frame_bytes(&self) -> u32 {
            0
        }
        fn max_fences_inflight(&self) -> u32 {
            0
        }
        fn allocate_memory(&self, _s: u64, _u: u8) -> [u8; 32] {
            [0; 32]
        }
        fn submit_frame(&self, _f: ResourceId, _t: u64, _b: &[u8]) -> bool {
            true
        }
        fn image_created(&self, id: ResourceId, _w: u32, _h: u32) {
            self.created.lock().unwrap().push(id.raw());
        }
    }

    fn op(id: u32) -> ReplayOp {
        Box::new(move |b: &dyn Backend| b.image_created(ResourceId(id), 8, 8))
    }

    #[test]
    fn lazy_materialization_keeps_an_unused_tier_empty() {
        let (t2, t3) = (Rec::new(), Rec::new());
        let mut r = ResidencyTracker::new();
        // Record two resources — neither tier resident yet, so nothing
        // applied: a recorded op is not an upload.
        r.record(0x10, op(0x10), &t2, &t3);
        r.record(0x20, op(0x20), &t2, &t3);
        assert!(t2.created.lock().unwrap().is_empty());
        assert!(t3.created.lock().unwrap().is_empty());

        // Materialise both on Tier-2 only.
        r.materialize([0x10, 0x20], Tier::Tier2, &t2);
        assert_eq!(*t2.created.lock().unwrap(), vec![0x10, 0x20]);
        assert!(t3.created.lock().unwrap().is_empty(), "the GPU stays empty");
        assert_eq!(r.resident_counts(), (2, 0));
    }

    #[test]
    fn materialize_is_idempotent_and_per_resource() {
        let (t2, t3) = (Rec::new(), Rec::new());
        let mut r = ResidencyTracker::new();
        r.record(0x10, op(0x10), &t2, &t3);
        r.record(0x20, op(0x20), &t2, &t3);
        r.materialize([0x10], Tier::Tier2, &t2);
        r.materialize([0x10], Tier::Tier2, &t2); // again → no-op
        assert_eq!(*t2.created.lock().unwrap(), vec![0x10], "no double-replay");
        // 0x20 only lands when asked for.
        r.materialize([0x20], Tier::Tier2, &t2);
        assert_eq!(*t2.created.lock().unwrap(), vec![0x10, 0x20]);
    }

    #[test]
    fn recording_on_a_live_tier_applies_immediately() {
        let (t2, t3) = (Rec::new(), Rec::new());
        let mut r = ResidencyTracker::new();
        r.record(0x10, op(0x10), &t2, &t3);
        r.materialize([0x10], Tier::Tier2, &t2); // 0x10 now live on t2
        // A *new* op on the live resource must reach the live tier at once.
        r.record(0x10, op(0x11), &t2, &t3);
        assert_eq!(*t2.created.lock().unwrap(), vec![0x10, 0x11]);
        assert!(t3.created.lock().unwrap().is_empty());
    }

    #[test]
    fn materialize_all_is_the_whole_world_fallback() {
        let (t2, t3) = (Rec::new(), Rec::new());
        let mut r = ResidencyTracker::new();
        r.record(0x10, op(0x10), &t2, &t3);
        r.record(0x20, op(0x20), &t2, &t3);
        r.materialize_all(Tier::Tier3, &t3);
        assert_eq!(*t3.created.lock().unwrap(), vec![0x10, 0x20]);
        assert_eq!(r.resident_counts(), (0, 2));
    }

    #[test]
    fn forget_drops_a_resource() {
        let (t2, t3) = (Rec::new(), Rec::new());
        let mut r = ResidencyTracker::new();
        r.record(0x10, op(0x10), &t2, &t3);
        r.materialize([0x10], Tier::Tier2, &t2);
        r.forget(0x10);
        assert_eq!(r.resident_counts(), (0, 0));
        assert!(!r.is_resident(0x10, Tier::Tier2));
    }
}
