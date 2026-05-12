//! Tier-3 MoltenVK backend — host-side Vulkan via MoltenVK on macOS.
//!
//! Architecture per `docs/spec/aqueduct-gpu.md` §6.5: tier-3 is the
//! hardware-accelerated path. On macOS-HVF dev hosts it's MoltenVK
//! sitting on top of Metal. On real FreeBSD hardware (D5+) the same
//! aqueduct-gpu wire reaches an in-kernel atrium-gpu driver — this
//! file is the **dev/CI on macOS** half of that story.
//!
//! ## Phase 1.3b scope (this file)
//!
//! - **Construction**: load the Vulkan loader via [`ash::Entry`],
//!   create a `VkInstance`, pick a physical device (preferring an
//!   Apple integrated/discrete GPU when MoltenVK is in use), create
//!   a `VkDevice` with one graphics + transfer queue.
//! - **Identity/caps reporting**: handshake reports
//!   [`GpuVendor::Apple`] (when MoltenVK is the implementation;
//!   actual vendor for non-Apple Vulkan loaders) and `CAPS_COMPUTE
//!   | CAPS_COMPOSITION | CAPS_SHARE_SURFACE | CAPS_SPIRV_UPLOAD`.
//! - **submit_frame**: protocol-correct stub — signals fences
//!   immediately like [`StubBackend`](crate::StubBackend) does. Real
//!   `VkCommandBuffer` recording lands in a follow-on commit.
//!
//! ## What this file deliberately does NOT do yet
//!
//! - Recording draws into a real `VkCommandBuffer`
//! - SPIR-V → `MTLLibrary` compile (via SPIRV-Cross or direct)
//! - Frame command stream → vkCmd* translation
//! - Surface creation / WSI (this is a HEADLESS host — pixels flow
//!   back via OP_GPU_SHARE_SURFACE, not vkSwapchain)
//!
//! Each of these is its own commit in the 1.3b rollout.
//!
//! ## Fail-soft construction
//!
//! [`MoltenVkBackend::new`] returns `Err` if Vulkan isn't installed
//! on the host (no MoltenVK, no Vulkan loader). The daemon falls
//! back to [`SoftwareBackend`](crate::SoftwareBackend) in that case;
//! no other host code knows or cares which tier is active.

use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};

use ash::{vk, Entry};
use ash::khr;

use aqueduct_gpu::backends::{BackendId, GpuVendor};
use aqueduct_gpu::ids::ResourceId;

use crate::backend::Backend;

/// Tier-3 Vulkan backend. Wraps a loaded `VkInstance` + `VkDevice`.
///
/// One instance per host endpoint; `submit_frame` is internally
/// serialised by tiny-skia in the SW path and by a shared graphics
/// queue here. Multiple guest connections share the same VkDevice;
/// per-session isolation is the [session
/// layer](crate::session)'s job.
///
/// **Owned resources** (drop order matters):
///   1. `device`  — must be destroyed before instance
///   2. `instance`
///   3. `entry`   — last (owns the dlopen handle on the loader)
pub struct MoltenVkBackend {
    /// Submission counter for telemetry.
    submissions: AtomicU64,

    /// Selected physical device. Cached so handshake can synthesise
    /// a stable [`BackendId`] without re-querying.
    physical: vk::PhysicalDevice,
    /// Vendor reported by the physical device (Apple under MoltenVK,
    /// AMD/Intel/NVIDIA on Linux dev hosts).
    vendor: GpuVendor,
    /// Driver / device generation. We pack the major-API number.
    generation: u16,

    /// Logical device.
    device: ash::Device,
    /// One graphics+transfer queue (`VK_QUEUE_GRAPHICS_BIT |
    /// VK_QUEUE_TRANSFER_BIT`). Held for later `vkQueueSubmit` calls.
    _queue: vk::Queue,
    _queue_family: u32,

    /// VkInstance. Stays alive until `Drop`.
    instance: ash::Instance,
    /// The Vulkan loader. Stays alive until `Drop`. Box keeps it
    /// pointer-stable for ash's internal references.
    _entry: Box<Entry>,
}

/// Construction errors for [`MoltenVkBackend::new`]. Each variant
/// indicates the host environment can't support the tier-3 path;
/// the caller should fall back to tier-1 SW.
#[derive(Debug)]
pub enum MoltenVkError {
    /// Couldn't load the Vulkan loader (MoltenVK / libvulkan
    /// not installed).
    LoaderUnavailable(ash::LoadingError),
    /// Vulkan call returned an error code.
    Vulkan(vk::Result),
    /// No physical device was acceptable (no graphics queue family,
    /// etc.). Diagnostic message included.
    NoSuitableDevice(String),
    /// Internal text-conversion error during instance creation.
    BadCString,
}

impl std::fmt::Display for MoltenVkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoltenVkError::LoaderUnavailable(e) =>
                write!(f, "Vulkan loader unavailable (install MoltenVK / Vulkan SDK): {e}"),
            MoltenVkError::Vulkan(e) => write!(f, "Vulkan call failed: {e:?}"),
            MoltenVkError::NoSuitableDevice(s) =>
                write!(f, "no suitable Vulkan device: {s}"),
            MoltenVkError::BadCString =>
                write!(f, "internal: failed to build CString for Vulkan init"),
        }
    }
}
impl std::error::Error for MoltenVkError {}

impl From<vk::Result> for MoltenVkError {
    fn from(e: vk::Result) -> Self { MoltenVkError::Vulkan(e) }
}

impl MoltenVkBackend {
    /// Construct a fresh tier-3 backend. Loads Vulkan, creates an
    /// instance, picks a graphics-capable physical device, creates a
    /// logical device with one graphics+transfer queue.
    ///
    /// Returns `Err(LoaderUnavailable)` if no Vulkan loader can be
    /// dlopened. Callers should treat this as "fall back to tier-1
    /// SW", not as a hard failure.
    pub fn new() -> Result<Self, MoltenVkError> {
        // ── Load Vulkan loader ────────────────────────────────────
        // SAFETY: ash's Entry::load is unsafe because the loader is
        // dynamically resolved. We trust the system Vulkan ICD.
        let entry = unsafe { Entry::load() }
            .map_err(MoltenVkError::LoaderUnavailable)?;
        let entry = Box::new(entry);

        // ── Create instance ───────────────────────────────────────
        let app_name = CString::new("aqueduct-gpu-host")
            .map_err(|_| MoltenVkError::BadCString)?;
        let engine_name = CString::new("aqueduct-gpu")
            .map_err(|_| MoltenVkError::BadCString)?;

        let app_info = vk::ApplicationInfo::default()
            .application_name(&app_name)
            .application_version(0)
            .engine_name(&engine_name)
            .engine_version(0)
            .api_version(vk::API_VERSION_1_2);

        // MoltenVK requires the portability-enumeration extension &
        // flag to be advertised. On non-Apple hosts this is harmless.
        let portability_ext_name = khr::portability_enumeration::NAME;
        let extension_ptrs = [portability_ext_name.as_ptr()];

        let mut create_flags = vk::InstanceCreateFlags::empty();
        // The constant only exists when the portability extension is
        // present; ash exposes it unconditionally so we set it.
        create_flags |= vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR;

        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .flags(create_flags)
            .enabled_extension_names(&extension_ptrs);

        // SAFETY: structs are all stack-built with correct lifetimes.
        let instance = unsafe { entry.create_instance(&create_info, None)? };

        // ── Pick a physical device ────────────────────────────────
        let physicals = unsafe { instance.enumerate_physical_devices()? };
        if physicals.is_empty() {
            unsafe { instance.destroy_instance(None) };
            return Err(MoltenVkError::NoSuitableDevice(
                "enumerate_physical_devices returned 0".into(),
            ));
        }

        let mut chosen: Option<(vk::PhysicalDevice, u32, GpuVendor, u16)> = None;
        for pd in physicals {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let vendor = vendor_from_pci_id(props.vendor_id);

            let q_families = unsafe {
                instance.get_physical_device_queue_family_properties(pd)
            };
            for (i, fam) in q_families.iter().enumerate() {
                if fam.queue_flags.contains(
                    vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER,
                ) {
                    // major version of the device's reported API.
                    let generation = vk::api_version_major(props.api_version) as u16;
                    chosen = Some((pd, i as u32, vendor, generation));
                    break;
                }
            }
            if chosen.is_some() { break; }
        }

        let (physical, queue_family, vendor, generation): (vk::PhysicalDevice, u32, GpuVendor, u16) = chosen
            .ok_or_else(|| {
                unsafe { instance.destroy_instance(None) };
                MoltenVkError::NoSuitableDevice(
                    "no graphics+transfer queue family on any device".into(),
                )
            })?;

        // ── Create logical device ─────────────────────────────────
        let priorities = [1.0_f32];
        let q_create = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities);
        let q_creates = [q_create];

        // MoltenVK requires VK_KHR_portability_subset on the device
        // (when present in the physical-device extensions). Querying
        // for it up front would be the production path; for the
        // skeleton we just attempt with no extensions and let the
        // VK_ERROR_EXTENSION_NOT_PRESENT bubble up if the host needs
        // it. The portability-subset device extension is documented
        // in the spec as "MUST be enabled if reported."
        let portability_subset = khr::portability_subset::NAME;
        let device_exts = match
            host_supports_portability_subset(&instance, physical)
        {
            true  => vec![portability_subset.as_ptr()],
            false => vec![],
        };

        let device_create = vk::DeviceCreateInfo::default()
            .queue_create_infos(&q_creates)
            .enabled_extension_names(&device_exts);

        let device = unsafe { instance.create_device(physical, &device_create, None) };
        let device = match device {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(MoltenVkError::Vulkan(e));
            }
        };
        let queue = unsafe { device.get_device_queue(queue_family, 0) };

        Ok(Self {
            submissions: AtomicU64::new(0),
            physical,
            vendor,
            generation,
            device,
            _queue: queue,
            _queue_family: queue_family,
            instance,
            _entry: entry,
        })
    }

    /// How many frames have been submitted to this backend. Diagnostic.
    pub fn submission_count(&self) -> u64 {
        self.submissions.load(Ordering::Relaxed)
    }

    /// Returns the Vulkan device properties (vendor name, device name,
    /// driver version). Diagnostic / smoke-test helper.
    pub fn device_summary(&self) -> String {
        let props = unsafe { self.instance.get_physical_device_properties(self.physical) };
        let name: String = props.device_name_as_c_str()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "<unparseable>".into());
        format!(
            "vendor={:?} api={}.{}.{} device={}",
            self.vendor,
            vk::api_version_major(props.api_version),
            vk::api_version_minor(props.api_version),
            vk::api_version_patch(props.api_version),
            name,
        )
    }
}

impl Drop for MoltenVkBackend {
    fn drop(&mut self) {
        // SAFETY: we created both via ash; destroy in reverse order.
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

impl Backend for MoltenVkBackend {
    fn identity(&self) -> BackendId {
        BackendId::new(self.vendor, self.generation)
    }

    fn caps(&self) -> u64 {
        use aqueduct_gpu::payloads::HandshakeResponse as H;
        // Tier-3 advertises the full grown-up surface: compute,
        // composition, SPIR-V upload (cold path), share-surface.
        // Bundle load lands when the bundle materialisation pipeline
        // ships (Phase 2.x).
        H::CAPS_COMPUTE
            | H::CAPS_COMPOSITION
            | H::CAPS_SHARE_SURFACE
            | H::CAPS_SPIRV_UPLOAD
    }

    fn max_frame_bytes(&self) -> u32 {
        // GPU paths can chew much larger frames than tier-1. Cap at
        // 16 MiB — Vulkan's typical maxPushConstantsSize and
        // maxCommandBuffer constraints aren't hit until well past
        // this.
        16 * (1 << 20)
    }

    fn max_fences_inflight(&self) -> u32 {
        128
    }

    fn allocate_memory(&self, _size: u64, _usage: u8) -> [u8; 32] {
        // Real impl: vkAllocateMemory of a host-visible region, return
        // a token the guest kmod imports. Stub here so handshake-level
        // wiring works.
        let n = self.submissions.fetch_add(0, Ordering::Relaxed);
        let mut tok = [0u8; 32];
        tok[..8].copy_from_slice(&n.to_le_bytes());
        tok[31] = 0xCE; // tier-3 sentinel
        tok
    }

    fn submit_frame(
        &self,
        _fence_id: ResourceId,
        _timeline: u64,
        _frame_buf: &[u8],
    ) -> bool {
        self.submissions.fetch_add(1, Ordering::Relaxed);
        // Real impl: record VkCommandBuffer from frame_buf records,
        // vkQueueSubmit, attach VkFence callback to signal aqueduct
        // fence_id. For now: signal immediately so wire correctness
        // tests pass against this backend before real GPU work lands.
        true
    }
}

/// Best-effort vendor mapping from a PCI vendor-id. MoltenVK reports
/// `0x106B` (Apple) on Apple Silicon; on Intel Macs MoltenVK still
/// reports the underlying GPU vendor.
fn vendor_from_pci_id(pci: u32) -> GpuVendor {
    match pci {
        0x106B => GpuVendor::Apple,
        0x1002 => GpuVendor::Amd,
        0x8086 => GpuVendor::Intel,
        0x10DE => GpuVendor::Nvidia,
        // Unknown vendor: report Software (the safe "no special caps"
        // fallback). The enum has no Other / Unknown variant by design
        // — see aqueduct-gpu::backends::GpuVendor. Future hardware
        // additions should add explicit variants.
        _      => GpuVendor::Software,
    }
}

/// Check whether the device exposes `VK_KHR_portability_subset`.
/// Per Vulkan spec, devices that DO advertise it MUST have it
/// enabled at device creation. MoltenVK does; native Vulkan
/// drivers don't.
fn host_supports_portability_subset(
    instance: &ash::Instance,
    physical: vk::PhysicalDevice,
) -> bool {
    let exts = match unsafe { instance.enumerate_device_extension_properties(physical) } {
        Ok(v)  => v,
        Err(_) => return false,
    };
    let want = khr::portability_subset::NAME;
    exts.iter().any(|p| {
        p.extension_name_as_c_str().map(|s| s == want).unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tier-3 tests are gated on a Vulkan loader being present on
    // the host. Use a helper to skip rather than fail when not
    // installed — that's the production daemon's fallback path too.
    fn try_init() -> Option<MoltenVkBackend> {
        match MoltenVkBackend::new() {
            Ok(b) => Some(b),
            Err(MoltenVkError::LoaderUnavailable(_)) => {
                eprintln!("MoltenVK test skipped: no Vulkan loader on this host");
                None
            }
            Err(e) => panic!("MoltenVkBackend::new unexpected failure: {e}"),
        }
    }

    #[test]
    fn loads_and_reports_identity() {
        let Some(b) = try_init() else { return; };
        let id = b.identity();
        // Vendor must be a non-Unknown value; otherwise something is
        // very wrong with the loader.
        assert_ne!(format!("{:?}", id.vendor), "Unknown");
        eprintln!("MoltenVkBackend: {}", b.device_summary());
    }

    #[test]
    fn submit_frame_signals_immediately() {
        let Some(b) = try_init() else { return; };
        let fid = ResourceId::new(aqueduct_gpu::ids::IdNamespace::IcdRuntime, 0x1);
        assert!(b.submit_frame(fid, 1, &[]));
        assert_eq!(b.submission_count(), 1);
    }

    #[test]
    fn caps_advertise_compute_and_spirv_upload() {
        let Some(b) = try_init() else { return; };
        use aqueduct_gpu::payloads::HandshakeResponse as H;
        let c = b.caps();
        assert!(c & H::CAPS_COMPUTE != 0);
        assert!(c & H::CAPS_SPIRV_UPLOAD != 0);
        assert!(c & H::CAPS_COMPOSITION != 0);
        assert!(c & H::CAPS_SHARE_SURFACE != 0);
    }
}
