//! Monotonic ID allocator for the client's namespace.
//!
//! Each `GpuClient` owns one [`IdAllocator`] for the
//! `IdNamespace::IcdRuntime` partition — that's where every resource
//! the client creates ends up. Built-in IDs come from the host
//! pre-shipped resources (Atrium-built bundles); bundle IDs come from
//! `OP_GPU_BUNDLE_LOAD` responses. The client's allocator only
//! produces `IcdRuntime`-namespaced handles.
//!
//! IDs are allocated monotonically. `ResourceId::LOCAL_MAX` is
//! 2^28-1 ≈ 268M; any single connection running for a realistic time
//! cannot exhaust this. On exhaustion the allocator returns `None`
//! and the [`GpuClient`] surfaces `GpuClientError::IdNamespaceExhausted`.
//!
//! [`GpuClient`]: super::GpuClient

use aqueduct_gpu::ids::{IdNamespace, ResourceId};

/// Monotonic ID allocator scoped to a single namespace.
///
/// The default constructor binds to `IdNamespace::IcdRuntime` (the
/// client's allocations). Use [`IdAllocator::with_namespace`] for
/// other namespaces — only the host endpoint typically constructs
/// allocators for `Bundle(_)` namespaces.
#[derive(Debug, Clone)]
pub struct IdAllocator {
    namespace: IdNamespace,
    next: u32,
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::with_namespace(IdNamespace::IcdRuntime)
    }
}

impl IdAllocator {
    /// Construct an allocator bound to the given namespace, starting
    /// at local-ID 1. (Local-ID 0 is reserved as a sentinel for "no
    /// resource" — matches the convention used by Vulkan `VK_NULL_HANDLE`.)
    pub fn with_namespace(namespace: IdNamespace) -> Self {
        Self { namespace, next: 1 }
    }

    /// Allocate the next ID. Returns `None` once the namespace is
    /// exhausted (in practice never; 28-bit ID space).
    pub fn next(&mut self) -> Option<ResourceId> {
        if self.next > ResourceId::LOCAL_MAX {
            return None;
        }
        let id = ResourceId::new(self.namespace, self.next);
        self.next += 1;
        Some(id)
    }

    /// Current allocator state; useful for diagnostics and
    /// "how many resources have been created" telemetry.
    pub fn count_used(&self) -> u32 {
        self.next.saturating_sub(1)
    }

    /// Namespace this allocator is bound to.
    pub fn namespace(&self) -> IdNamespace {
        self.namespace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_monotonically_in_icd_namespace() {
        let mut a = IdAllocator::default();
        let id1 = a.next().unwrap();
        let id2 = a.next().unwrap();
        let id3 = a.next().unwrap();
        assert_eq!(id1.namespace(), Some(IdNamespace::IcdRuntime));
        assert_eq!(id1.local_id(), 1);
        assert_eq!(id2.local_id(), 2);
        assert_eq!(id3.local_id(), 3);
        assert_eq!(a.count_used(), 3);
    }

    #[test]
    fn never_returns_local_id_zero() {
        // Zero is the VK_NULL_HANDLE-equivalent sentinel; the
        // allocator must skip it.
        let mut a = IdAllocator::default();
        assert_ne!(a.next().unwrap().local_id(), 0);
    }

    #[test]
    fn returns_none_when_exhausted() {
        let mut a = IdAllocator::default();
        a.next = ResourceId::LOCAL_MAX;
        assert!(a.next().is_some()); // the very last valid ID
        assert!(a.next().is_none()); // exhausted
    }

    #[test]
    fn bundle_namespace_works_too() {
        let mut a = IdAllocator::with_namespace(IdNamespace::Bundle(0x3));
        let id = a.next().unwrap();
        assert_eq!(id.namespace(), Some(IdNamespace::Bundle(0x3)));
    }
}
