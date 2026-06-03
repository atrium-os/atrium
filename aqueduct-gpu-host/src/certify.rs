//! Tier-equivalence certification — the per-pipeline gate for *acting* on
//! routing verdicts (`docs/spec/energy-policy.md` §"Acting on the verdict").
//!
//! Routing is only safe because a routed op is pixel-identical on Tier-2
//! and Tier-3. Before the `RoutedSurface` layer (stage 2) may migrate a
//! surface between tiers, every pipeline that surface uses must be
//! **certified**: rendering the same frame on both backends yields the same
//! pixels (within a small tolerance for sRGB rounding). An uncertified — or
//! failed — pipeline pins its surface to one tier; routing then degrades to
//! "stay put", never to a wrong pixel.
//!
//! This module is the *gate*: the pixel comparison + the per-pipeline status
//! registry + the surface-eligibility policy. The orchestration that
//! actually renders a probe frame on both backends and calls
//! [`compare_framebuffers`] belongs to the dual-backend routing layer (it
//! needs both live backends); this gate is the pure, testable core it drives.

use std::collections::HashMap;

/// A pipeline's tier-equivalence status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certification {
    /// Not yet checked — treat as not migratable (pin the surface).
    Uncertified,
    /// Pixel-identical across tiers within tolerance — safe to migrate.
    Certified,
    /// Differed across tiers; the worst per-channel delta is recorded.
    Failed {
        /// Largest absolute per-channel difference observed (0..=255).
        max_channel_diff: u8,
    },
}

/// Compare two RGBA8 framebuffers channel-wise. Certify iff every channel
/// is within `tolerance` (kept small — a couple of LSBs — to absorb sRGB
/// rounding differences between the CPU rasteriser and the GPU, which the
/// tier-equivalence convention work already minimises). Mismatched lengths
/// or empty input → `Failed { 255 }` (a structural mismatch, not a near
/// miss).
pub fn compare_framebuffers(reference: &[u8], candidate: &[u8], tolerance: u8) -> Certification {
    if reference.is_empty() || reference.len() != candidate.len() {
        return Certification::Failed { max_channel_diff: 255 };
    }
    let mut max = 0u8;
    for (a, b) in reference.iter().zip(candidate) {
        max = max.max(a.abs_diff(*b));
    }
    if max <= tolerance {
        Certification::Certified
    } else {
        Certification::Failed { max_channel_diff: max }
    }
}

/// Per-pipeline tier-equivalence registry + the surface-migration gate.
#[derive(Debug, Default, Clone)]
pub struct CertificationRegistry {
    by_pipeline: HashMap<u32, Certification>,
}

impl CertificationRegistry {
    /// Empty registry — every pipeline is [`Certification::Uncertified`]
    /// until proven otherwise.
    pub fn new() -> Self {
        CertificationRegistry { by_pipeline: HashMap::new() }
    }

    /// Record (or update) a pipeline's certification. The usual source is a
    /// probe-frame differential run feeding [`compare_framebuffers`].
    pub fn set(&mut self, pipeline: u32, c: Certification) {
        self.by_pipeline.insert(pipeline, c);
    }

    /// A pipeline's status — [`Certification::Uncertified`] if never set.
    pub fn status(&self, pipeline: u32) -> Certification {
        self.by_pipeline.get(&pipeline).copied().unwrap_or(Certification::Uncertified)
    }

    /// Whether a single pipeline is certified migratable.
    pub fn is_certified(&self, pipeline: u32) -> bool {
        self.status(pipeline) == Certification::Certified
    }

    /// The gate: a surface may migrate **only if every pipeline it uses is
    /// certified**. An empty pipeline set is *not* eligible — a surface that
    /// has drawn nothing certifiable has nothing proven, so it stays put.
    pub fn surface_eligible(&self, pipelines: impl IntoIterator<Item = u32>) -> bool {
        let mut any = false;
        for p in pipelines {
            any = true;
            if !self.is_certified(p) {
                return false;
            }
        }
        any
    }

    /// Number of pipelines with a recorded status.
    pub fn len(&self) -> usize {
        self.by_pipeline.len()
    }

    /// Whether nothing has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.by_pipeline.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_and_near_identical_buffers_certify() {
        let a = [10u8, 20, 30, 255, 40, 50, 60, 255];
        assert_eq!(compare_framebuffers(&a, &a, 0), Certification::Certified);
        // One channel off by 1 — within an sRGB-rounding tolerance.
        let mut b = a;
        b[2] = 31;
        assert_eq!(compare_framebuffers(&a, &b, 1), Certification::Certified);
        // …but not under zero tolerance.
        assert_eq!(compare_framebuffers(&a, &b, 0),
            Certification::Failed { max_channel_diff: 1 });
    }

    #[test]
    fn divergent_buffers_fail_with_the_worst_delta() {
        let a = [10u8, 20, 30, 255];
        let b = [10u8, 20, 90, 255]; // channel 2 off by 60
        assert_eq!(compare_framebuffers(&a, &b, 2),
            Certification::Failed { max_channel_diff: 60 });
    }

    #[test]
    fn structural_mismatches_fail_hard() {
        assert_eq!(compare_framebuffers(&[], &[], 255),
            Certification::Failed { max_channel_diff: 255 });
        assert_eq!(compare_framebuffers(&[1, 2, 3], &[1, 2], 255),
            Certification::Failed { max_channel_diff: 255 });
    }

    #[test]
    fn registry_gates_surface_migration_on_all_pipelines() {
        let mut reg = CertificationRegistry::new();
        // Unknown pipeline → uncertified, not eligible.
        assert_eq!(reg.status(7), Certification::Uncertified);
        assert!(!reg.surface_eligible([7]));
        // An empty pipeline set proves nothing → not eligible.
        assert!(!reg.surface_eligible(std::iter::empty()));

        reg.set(7, Certification::Certified);
        reg.set(8, Certification::Certified);
        assert!(reg.surface_eligible([7, 8]), "all certified → eligible");

        // One failed pipeline pins the whole surface.
        reg.set(9, Certification::Failed { max_channel_diff: 40 });
        assert!(!reg.surface_eligible([7, 8, 9]));
    }
}
