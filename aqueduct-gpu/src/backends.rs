//! Backend identification — what code the AOT compiler emits.
//!
//! Every shader binary in Tessera is keyed by
//! `(spirv_hash, backend_id, compiler_version)`. The `backend_id`
//! identifies *which native ISA / compiler target* a particular
//! compiled artifact corresponds to. atrium-pkg's install-time
//! shader-precompile hook detects the system's installed GPU
//! backends via `IOC_GPU_LIST_BACKENDS` (in the atrium-gpu kmod)
//! and compiles each app's SPIR-V once per detected backend.
//!
//! At runtime the ICD asks the host endpoint (or kmod) "what
//! backend are you executing on?" via `OP_GPU_HANDSHAKE` and uses
//! the answer when constructing `OP_GPU_SHADER_RESOLVE` requests.
//! See `aqueduct-gpu.md` §4.1 and §4.2.

use serde::{Deserialize, Serialize};

/// GPU vendor lineage — the family the backend targets.
///
/// Not the same as "GPU model": one vendor may have multiple
/// distinct backends (e.g., AMD GCN vs RDNA generations). The pair
/// `(vendor, generation)` resolves to a single backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum GpuVendor {
    /// AMD GPUs (radv / amdgpu-llvm backend).
    Amd      = 1,
    /// Intel GPUs (anv backend).
    Intel    = 2,
    /// NVIDIA GPUs via the open NVK driver — proprietary NVIDIA
    /// drivers are excluded by Atrium licensing policy.
    Nvidia   = 3,
    /// Apple GPUs via MoltenVK during bring-up. Goes away in D5+
    /// when the host endpoint disappears.
    Apple    = 4,
    /// Atrium-gpu — our own native ISA. Used on D5+ HW; the kmod
    /// is the only backend on that path.
    AtriumGpu = 5,
    /// Software rasteriser (llvmpipe / lavapipe / vulkan-sw).
    /// Kept as a CI / fallback target; usable but slow.
    Software = 6,
}

impl GpuVendor {
    /// Render to wire u8.
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Wire u8 → typed vendor; `None` for unrecognised values.
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => GpuVendor::Amd,
            2 => GpuVendor::Intel,
            3 => GpuVendor::Nvidia,
            4 => GpuVendor::Apple,
            5 => GpuVendor::AtriumGpu,
            6 => GpuVendor::Software,
            _ => return None,
        })
    }

    /// Human-readable name for logs / error messages.
    pub const fn name(self) -> &'static str {
        match self {
            GpuVendor::Amd       => "amd",
            GpuVendor::Intel     => "intel",
            GpuVendor::Nvidia    => "nvidia-nvk",
            GpuVendor::Apple     => "apple-moltenvk",
            GpuVendor::AtriumGpu => "atrium-gpu",
            GpuVendor::Software  => "software",
        }
    }
}

/// Backend identification — vendor × generation. The host endpoint
/// reports its current `BackendId` in `OP_GPU_HANDSHAKE`; clients
/// use it to construct `OP_GPU_SHADER_RESOLVE` lookups.
///
/// `generation` is vendor-specific and opaque to the wire — it's
/// the discriminant within the vendor's family of ISAs. Conventions:
///
/// - AMD: GCN1=1, GCN2=2, ..., RDNA1=10, RDNA2=11, RDNA3=12, RDNA4=13
/// - Intel: Gen8=8, Gen9=9, Gen11=11, Gen12=12 (Tigerlake/Alderlake),
///   Gen12HP=120 (Xe-HPC ARC)
/// - NVIDIA: Pascal=60, Turing=75, Ampere=86, Lovelace=89, Blackwell=120
/// - Apple: M1=1, M2=2, M3=3, M4=4
/// - atrium-gpu: 1 for v1 ISA; bump on each ISA revision
/// - Software: 0 (no generation distinction)
///
/// These conventions match the values atrium-pkg's compile worker
/// passes to the corresponding Mesa backend toolchains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BackendId {
    /// GPU vendor lineage.
    pub vendor:     GpuVendor,
    /// Vendor-specific generation identifier.
    pub generation: u16,
}

impl BackendId {
    /// Construct a backend identifier.
    pub const fn new(vendor: GpuVendor, generation: u16) -> Self {
        Self { vendor, generation }
    }

    /// Compact-string form for use in Tessera cache keys, e.g.
    /// `"amd:12"` for RDNA3 or `"apple:4"` for M4. Used by the
    /// shader-precompile hook to construct the
    /// `(spirv_hash, backend_id, compiler_version)` Tessera key.
    pub fn cache_key(self) -> String {
        format!("{}:{}", self.vendor.name(), self.generation)
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.cache_key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_wire_roundtrip() {
        for v in [GpuVendor::Amd, GpuVendor::Intel, GpuVendor::Nvidia,
                  GpuVendor::Apple, GpuVendor::AtriumGpu, GpuVendor::Software] {
            assert_eq!(GpuVendor::from_u8(v.as_u8()), Some(v));
        }
        assert_eq!(GpuVendor::from_u8(0), None);
        assert_eq!(GpuVendor::from_u8(99), None);
    }

    #[test]
    fn cache_key_shape() {
        assert_eq!(BackendId::new(GpuVendor::Amd, 12).cache_key(), "amd:12");
        assert_eq!(BackendId::new(GpuVendor::Apple, 4).cache_key(),
                   "apple-moltenvk:4");
        assert_eq!(BackendId::new(GpuVendor::AtriumGpu, 1).cache_key(),
                   "atrium-gpu:1");
    }
}
