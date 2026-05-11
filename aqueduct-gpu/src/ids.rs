//! Partitioned resource-ID namespace.
//!
//! Every resource handle (pipelines, images, buffers, samplers,
//! fences) is a u32 carrying a 4-bit namespace tag in the top bits:
//!
//! | Tag (high 4 bits) | Allocator                              | Use |
//! |-------------------|----------------------------------------|-----|
//! | `0x0`             | Atrium build process                   | Built-in pipelines/resources shipped in atrium-core / atrium-text bundles |
//! | `0x1`–`0xE`       | Host endpoint at `OP_GPU_BUNDLE_LOAD`  | Per-third-party-bundle namespace; up to 14 bundles loaded concurrently |
//! | `0xF`             | ICD-runtime (monotonic)                | App-created resources via Vulkan API or direct aqueduct-gpu |
//!
//! The low 28 bits carry the ID-within-namespace. This gives 2^28
//! (≈ 268M) IDs per namespace, which is sufficient for any realistic
//! workload (a frame with 1k draws using 100 distinct pipelines uses
//! ~100 IDs in the ICD-runtime namespace per launch).
//!
//! ## Why this is a wire-protocol concern, not just an implementation
//! detail
//!
//! The host needs to know which namespace an ID belongs to in order
//! to look it up in the right table. Per-connection state for the
//! built-in and ICD-runtime namespaces; cross-connection-shared
//! state for bundle namespaces (a bundle loaded by connection A
//! can be referenced by connection B if both have the same bundle
//! materialised). The tag encoding is the wire-level discriminant.
//!
//! See `docs/spec/aqueduct-gpu.md` §3 (partitioned ID namespace
//! principle) and §7.3 (how bundles use this).

use std::fmt;

/// Atrium-built-in resources (atrium-core, atrium-text pre-shipped
/// pipelines + render-target formats).
pub const BUILTIN_NAMESPACE: u8 = 0x0;

/// Third-party bundle namespaces, assigned by the host endpoint at
/// `OP_GPU_BUNDLE_LOAD` time. Up to 14 distinct bundles loaded
/// concurrently per host endpoint.
pub const BUNDLE_NAMESPACE_RANGE: std::ops::RangeInclusive<u8> = 0x1..=0xE;

/// ICD-runtime allocations — apps creating resources via the
/// Vulkan API or directly through aqueduct-gpu's Rust API.
/// Monotonic per connection.
pub const ICD_RUNTIME_NAMESPACE: u8 = 0xF;

/// Discriminated namespace tag carried in the top 4 bits of every
/// `ResourceId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdNamespace {
    /// Atrium build-time-shipped resource.
    Builtin,
    /// Third-party bundle, identified by its allocated namespace
    /// index in `0x1..=0xE`.
    Bundle(u8),
    /// App-runtime allocation (e.g., Vulkan ICD).
    IcdRuntime,
}

impl IdNamespace {
    /// The 4-bit tag this namespace uses on the wire.
    pub const fn tag(self) -> u8 {
        match self {
            IdNamespace::Builtin       => BUILTIN_NAMESPACE,
            IdNamespace::Bundle(i)     => i,
            IdNamespace::IcdRuntime    => ICD_RUNTIME_NAMESPACE,
        }
    }

    /// Convert a 4-bit tag (high nibble of a `ResourceId`) back into
    /// a typed namespace. Tags outside the documented ranges return
    /// `None` — the host should treat that as a wire-protocol error.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0x0 => Some(IdNamespace::Builtin),
            0x1..=0xE => Some(IdNamespace::Bundle(tag)),
            0xF => Some(IdNamespace::IcdRuntime),
            _ => None, // tag is 4 bits, so this is unreachable in practice
        }
    }
}

impl fmt::Display for IdNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdNamespace::Builtin    => write!(f, "builtin"),
            IdNamespace::Bundle(i)  => write!(f, "bundle/{i:#x}"),
            IdNamespace::IcdRuntime => write!(f, "icd-runtime"),
        }
    }
}

/// A `u32` resource handle with a partitioned namespace tag.
///
/// Wire layout: bits 31..28 = namespace tag, bits 27..0 = local-ID.
/// Cheap to construct and decompose; no allocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct ResourceId(pub u32);

impl ResourceId {
    /// Maximum local-ID within a namespace (2^28 - 1).
    pub const LOCAL_MAX: u32 = 0x0FFF_FFFF;

    /// Construct a `ResourceId` from a namespace + local-ID.
    /// Panics if `local_id > LOCAL_MAX`; debug-build only.
    pub const fn new(namespace: IdNamespace, local_id: u32) -> Self {
        debug_assert!(local_id <= Self::LOCAL_MAX,
            "local_id exceeds the 28-bit field width");
        ResourceId(((namespace.tag() as u32) << 28) | (local_id & Self::LOCAL_MAX))
    }

    /// Extract the namespace tag. Returns `None` only for the
    /// theoretically-impossible "tag is out of 4-bit range" case,
    /// which the bit-arithmetic prevents.
    pub fn namespace(self) -> Option<IdNamespace> {
        IdNamespace::from_tag(((self.0 >> 28) & 0xF) as u8)
    }

    /// Extract the local-ID-within-namespace.
    pub const fn local_id(self) -> u32 {
        self.0 & Self::LOCAL_MAX
    }

    /// Raw u32 value, for wire serialisation.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.namespace() {
            Some(ns) => write!(f, "id({ns}, {:#x})", self.local_id()),
            None     => write!(f, "id({:#010x}!invalid_namespace)", self.0),
        }
    }
}

// Serde transparent over u32 — wire stays compact.
impl serde::Serialize for ResourceId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<'de> serde::Deserialize<'de> for ResourceId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(ResourceId(u32::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrip_each_namespace() {
        let cases = [
            (IdNamespace::Builtin,     0x0000_0001),
            (IdNamespace::Bundle(0x1), 0x0000_0042),
            (IdNamespace::Bundle(0xE), 0x0FFF_FFFF),
            (IdNamespace::IcdRuntime,  0x0000_0100),
        ];
        for (ns, lid) in cases {
            let id = ResourceId::new(ns, lid);
            assert_eq!(id.namespace(), Some(ns));
            assert_eq!(id.local_id(), lid);
        }
    }

    #[test]
    fn id_namespaces_dont_collide() {
        let a = ResourceId::new(IdNamespace::Builtin,    0x42);
        let b = ResourceId::new(IdNamespace::Bundle(1),  0x42);
        let c = ResourceId::new(IdNamespace::IcdRuntime, 0x42);
        assert_ne!(a.raw(), b.raw());
        assert_ne!(b.raw(), c.raw());
        assert_ne!(a.raw(), c.raw());
    }

    #[test]
    fn display_uses_named_namespace() {
        assert_eq!(format!("{}", ResourceId::new(IdNamespace::Builtin, 0x42)),
                   "id(builtin, 0x42)");
        assert_eq!(format!("{}", ResourceId::new(IdNamespace::Bundle(0x3), 0x10)),
                   "id(bundle/0x3, 0x10)");
        assert_eq!(format!("{}", ResourceId::new(IdNamespace::IcdRuntime, 0x1)),
                   "id(icd-runtime, 0x1)");
    }

    #[test]
    fn raw_encodes_tag_in_top_nibble() {
        let id = ResourceId::new(IdNamespace::Builtin, 0xABCDEF);
        assert_eq!(id.raw() >> 28, 0x0);
        let id = ResourceId::new(IdNamespace::IcdRuntime, 0xABCDEF);
        assert_eq!(id.raw() >> 28, 0xF);
        let id = ResourceId::new(IdNamespace::Bundle(0x7), 0xABCDEF);
        assert_eq!(id.raw() >> 28, 0x7);
    }
}
