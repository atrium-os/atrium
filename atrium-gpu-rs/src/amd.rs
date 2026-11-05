//! Canonical Atrium GPU ABI **v2** binding — ioctl group `'A'` (GPU) + `'D'`
//! (display), per `docs/spec/gpu-abi-reconciliation.md`.
//!
//! This is the surface every backend converges to (the from-scratch
//! `atrium-gpu-amd` driver — run against the gpusim functional model — today;
//! virtio and Carillon later). It supersedes the older handle-based `'G'`
//! binding in `lib.rs`. The model here is:
//!
//!   * Every kernel object is an fd: the device (`/dev/atrium-gpu0`), each VM
//!     ([`Vm`]), buffer object ([`Bo`]) and timeline syncobj ([`Syncobj`]).
//!   * **Bind apart from create**: [`Vm::alloc`] allocates a BO then binds it
//!     into the VM's GPU-VA space; submissions reference the bound VA.
//!   * **Opaque PM4-ring submit**: the caller lays a command ring into a BO and
//!     submits it on an engine; the ring bytes are vendor-opaque.
//!   * **Timeline syncobjs are themselves kqueue-able**: the fd is
//!     `EVFILT_READ`-able once the counter reaches the registered threshold —
//!     no separate eventfd (a v2 amendment from the working implementation).
//!   * **Display is decoupled** (`/dev/atrium-display0`, display ABI v2): a
//!     scanout buffer crosses GPU→display as its BO fd in a [`Display::commit`],
//!     with optional in/out fences. Userspace never sees a VRAM address.
//!
//! All ioctls are issued on the device fd; `vm`/`bo`/`syncobj` fds travel as
//! integers inside the request structs (and own their lifetime via the fd).

use std::ffi::CString;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

use libc::{c_int, c_void, ioctl};

// ---------------------------------------------------------------------------
// ioctl encoding (FreeBSD <sys/ioccom.h>)
// ---------------------------------------------------------------------------

const IOC_VOID: u64 = 0x2000_0000;
const IOC_OUT: u64 = 0x4000_0000;
const IOC_IN: u64 = 0x8000_0000;
const IOC_INOUT: u64 = IOC_OUT | IOC_IN;

const fn ioc(dir: u64, group: u8, num: u64, size: usize) -> u64 {
    dir | (((size as u64) & 0x1fff) << 16) | ((group as u64) << 8) | num
}
const fn iow(group: u8, num: u64, size: usize) -> u64 { ioc(IOC_IN, group, num, size) }
const fn ior(group: u8, num: u64, size: usize) -> u64 { ioc(IOC_OUT, group, num, size) }
const fn iowr(group: u8, num: u64, size: usize) -> u64 { ioc(IOC_INOUT, group, num, size) }
/* _IO: void — must carry IOC_VOID like the kernel's _IO(), else the encoded
 * command never matches the handler (ENOTTY). */
const fn io(group: u8, num: u64) -> u64 { IOC_VOID | ((group as u64) << 8) | num }

const G: u8 = b'A'; // GPU ioctl group
const D: u8 = b'D'; // display ioctl group

// ---------------------------------------------------------------------------
// request structs (mirror atrium-gpu-amd/atrium_gpu_amd_abi.h + atrium_display_abi.h)
// ---------------------------------------------------------------------------

/// BO placement: device VRAM (else System/GTT). VRAM BOs are GPU-only (no CPU
/// map): populate via a GPU copy from a System staging BO.
pub const BO_VRAM: u32 = 0x1;
pub const ENGINE_GFX: u32 = 0;
pub const ENGINE_COMPUTE: u32 = 1;

// PM4 type-3 header + the one opcode the offset-model scanout path needs:
// IT_DMA_DATA (CP DMA copy, memory<->memory) to blit a System staging BO into
// the VRAM scanout BO (VRAM is GPU-only; only VRAM is scannable).
const IT_DMA_DATA: u32 = 0x50;
// RELEASE_MEM end-of-pipe with an interrupt: the kernel signals a submit's
// syncobj from that interrupt, so a ring that should signal must end with one.
const IT_RELEASE_MEM: u32 = 0x49;
fn pm4_type3(opcode: u32, body_dwords: u32) -> u32 {
    (3 << 30) | (((body_dwords - 1) & 0x3fff) << 16) | (opcode << 8)
}

#[repr(C)]
#[derive(Default)]
struct BoAlloc { size: u64, bo_fd: u32, flags: u32 }
#[repr(C)]
#[derive(Default)]
struct BoXfer { offset: u64, len: u64, user_ptr: u64, bo_fd: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct VmCreate { out_fd: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct VmBind { va: u64, vm_fd: u32, bo_fd: u32 }
#[repr(C)]
#[derive(Default)]
struct VmBind2 { va: u64, vm_fd: u32, bo_fd: u32, flags: u32, pad: u32 }
/// `VM_BIND2` flag: map without GPU write access (driver ABI rev 3).
pub const BIND_READONLY: u32 = 0x1;
#[repr(C)]
#[derive(Default)]
struct Submit {
    signal_value: u64,
    vm_fd: u32,
    ring_fd: u32,
    n_dwords: u32,
    engine: u32,
    signal_syncobj_fd: i32,
    deadline_ns: u32,
}
#[repr(C)]
#[derive(Default)]
struct SyncobjCreate { out_fd: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct Irqs { count: u64, msix_enabled: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct Sched {
    op: u32, arg: u32, ops: u32, bytes: u32, level: u32,
    energy_uj: u32, runs: u32, busy_us: u32, count: u32, deadline_ns: u32,
}
#[repr(C)]
#[derive(Default)]
struct SyncobjOp { value: u64, syncobj_fd: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct SyncobjWait { value: u64, syncobj_fd: u32, timeout_ms: u32 }
#[repr(C)]
#[derive(Default)]
struct CapsQuery { caps_ptr: u64, caps_size: u64 }

// display
#[repr(C)]
struct DConnector {
    connected: u32,
    connector_type: u32,
    usbc_lanes: u32,
    edid_len: u32,
    edid: [u8; 128],
}
impl Default for DConnector {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct Mode { pub width: u32, pub height: u32, pub refresh_mhz: u32, pad: u32 }
#[repr(C)]
struct DModes { count: u32, pad: u32, modes: [Mode; 8] }
impl Default for DModes {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}
#[repr(C)]
#[derive(Default)]
struct DStatus { vblank_count: u64, dropped_flips: u32, tear_line: u32 }

// display ABI v2 (include/atrium/display_abi_v2.h)
#[repr(C)]
#[derive(Default)]
struct DCommit {
    struct_size: u32,
    flags: u32,
    mode_index: u32,
    format: u32,
    bo_fd: i32,
    pitch: u32,
    bo_offset: u64,
    modifier: u64,
    in_fence_fd: i32,
    out_fence_fd: i32,
    in_fence_value: u64,
    out_fence_value: u64,
    out_fault: u32,
    mode_generation: u32,
}
#[repr(C)]
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeV2 {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub pixel_clock_khz: u32,
    pub flags: u32,
    pad: u32,
}
#[repr(C)]
struct DConnectorV2 {
    struct_size: u32,
    out_flags: u32,
    out_connector_type: u32,
    out_generation: u32,
    out_edid_len: u32,
    out_mode_count: u32,
    out_current_mode: u32,
    out_vrr_min_hz: u16,
    out_vrr_max_hz: u16,
    out_edid: [u8; 512],
    out_modes: [ModeV2; 16],
}
#[repr(C)]
struct DConfig { connector_type: u32, plug_mode: u32 }

// display ABI 3.0: several outputs
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DOutputInfo {
    identity: u64,
    flags: u32,
    connector_type: u32,
    generation: u32,
    width: u32,
    height: u32,
    refresh_mhz: u32,
    pixel_clock_khz: u32,
    _pad: u32,
}
#[repr(C)]
#[derive(Default)]
struct DOutputs {
    struct_size: u32,
    out_count: u32,
    out_topology_generation: u32,
    out_bandwidth_khz: u32,
    out_bandwidth_used_khz: u32,
    _pad: u32,
    out: [DOutputInfo; 8],
}
#[repr(C)]
struct DConnectorAt { struct_size: u32, output: u32, out_identity: u64, conn: DConnectorV2 }
#[repr(C)]
#[derive(Default)]
struct DStatusAt { struct_size: u32, output: u32, status: DStatusV2 }
#[repr(C)]
#[derive(Default)]
struct DOutputCommit { output: u32, _pad: u32, c: DCommit }
#[repr(C)]
#[derive(Default)]
struct DCommitMulti {
    struct_size: u32,
    flags: u32,
    count: u32,
    out_failed: u32,
    out_reason: u32,
    _pad: u32,
    commits: u64,
}
#[repr(C)]
#[derive(Default)]
struct DStatusV2 {
    struct_size: u32,
    out_flags: u32,
    out_vblank_count: u64,
    out_flips_done: u64,
    out_dropped_flips: u32,
    out_tear_line: u32,
    out_refresh_mhz: u32,
    out_last_fault: u32,
}

/// Commit flags (`ATRIUM_DISPLAY_COMMIT_*`).
pub const COMMIT_MODESET: u32 = 0x1;
pub const COMMIT_ASYNC: u32 = 0x2;
pub const COMMIT_TEST_ONLY: u32 = 0x4;
/// (2.2) With MODESET: variable refresh across the monitor's range.
pub const COMMIT_VRR: u32 = 0x8;
/// CONNECTOR flags (`ATRIUM_DISPLAY_CONNECTOR_*`).
pub const CONNECTOR_CONNECTED: u32 = 0x1;
pub const CONNECTOR_VRR: u32 = 0x2;
pub const CONNECTOR_VRR_ACTIVE: u32 = 0x4;
/// `ATRIUM_DISPLAY_FORMAT_XRGB8888`: B,G,R,X in memory.
pub const FORMAT_XRGB8888: u32 = 0;
/// (2.3) `ATRIUM_DISPLAY_FORMAT_XRGB2101010`: LE words, B 9:0, G 19:10, R 29:20.
pub const FORMAT_XRGB2101010: u32 = 1;
/// (2.3) CONNECTOR: the monitor declares HDR / an HDR EOTF is signalled.
pub const CONNECTOR_HDR: u32 = 0x8;
pub const CONNECTOR_HDR_ACTIVE: u32 = 0x10;
/// (2.3) HDR EOTFs (`ATRIUM_DISPLAY_EOTF_*`) and colorimetries.
pub const EOTF_SDR: u32 = 0;
pub const EOTF_TRAD_HDR: u32 = 1;
pub const EOTF_PQ: u32 = 2;
pub const EOTF_HLG: u32 = 3;
pub const COLORIMETRY_DEFAULT: u32 = 0;
pub const COLORIMETRY_BT2020: u32 = 1;
const HDR_SET: u32 = 0x1;

#[repr(C)]
#[derive(Default)]
struct DHdr {
    struct_size: u32,
    flags: u32,
    eotf: u32,
    colorimetry: u32,
    primaries_x: [u16; 3],
    primaries_y: [u16; 3],
    white_x: u16,
    white_y: u16,
    max_mastering_nits: u16,
    min_mastering: u16,
    max_cll: u16,
    max_fall: u16,
    out_eotfs: u32,
    out_colorimetry: u32,
    out_max_nits: u32,
    out_max_fall_nits: u32,
    out_min_nits_x10000: u32,
    output: u32,
}

/// What the stream is signalled with (display ABI 2.3 HDR): EOTF,
/// colorimetry and CTA-861.3 static metadata type 1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hdr {
    pub eotf: u32,
    pub colorimetry: u32,
    /// R, G, B chromaticity, units of 0.00002.
    pub primaries_x: [u16; 3],
    pub primaries_y: [u16; 3],
    pub white_x: u16,
    pub white_y: u16,
    pub max_mastering_nits: u16,
    /// Units of 0.0001 cd/m².
    pub min_mastering: u16,
    pub max_cll: u16,
    pub max_fall: u16,
}

impl Hdr {
    /// HDR10: PQ in BT.2020 with a D65 white point, mastered on a display of
    /// `max_mastering_nits` (0.005 cd/m² black), content light as given.
    pub fn hdr10(max_mastering_nits: u16, max_cll: u16, max_fall: u16) -> Self {
        Hdr {
            eotf: EOTF_PQ,
            colorimetry: COLORIMETRY_BT2020,
            primaries_x: [35400, 8500, 6550],
            primaries_y: [14600, 39850, 2300],
            white_x: 15635,
            white_y: 16450,
            max_mastering_nits,
            min_mastering: 50,
            max_cll,
            max_fall,
        }
    }
}

/// The monitor's HDR side, from its EDID.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HdrCaps {
    /// Bit n = EOTF n offered (SDR always).
    pub eotfs: u32,
    /// Bit n = colorimetry n offered (default always).
    pub colorimetry: u32,
    pub max_nits: u32,
    pub max_fall_nits: u32,
    pub min_nits_x10000: u32,
}
/// Display referee faults (`ATRIUM_DISPLAY_FAULT_*`).
pub const FAULT_NO_MODE: u32 = 1;
pub const FAULT_NOT_RESIDENT: u32 = 3;
pub const FAULT_TOO_SMALL: u32 = 4;
pub const FAULT_BAD_PITCH: u32 = 6;
/// STATUS flags.
pub const STATUS_COMMIT_PENDING: u32 = 0x1;
pub const STATUS_MODE_SET: u32 = 0x2;
/// A device reset may have wiped the screen; cleared when a commit made after
/// it reaches the screen (GPU ABI v2.1 §10.3).
pub const STATUS_CONTENT_LOST: u32 = 0x4;
/// kqueue fflags on the display fd: reset-only registration / reset seen.
pub const NOTE_RESET: u32 = 0x1;
/// (2.1) A monitor was plugged, unplugged or swapped.
pub const NOTE_HOTPLUG: u32 = 0x2;
/// (2.1) The monitor doesn't offer the programmed timing.
pub const FAULT_MODE_UNSUPPORTED: u32 = 8;
/// `ModeV2::flags`: the monitor's preferred mode.
pub const MODE_PREFERRED: u32 = 0x1;
/// Interim CONFIG test knob: the simulated monitors (`plug`), and unplug.
pub const PLUG_VGA: u32 = 0;
pub const PLUG_1080P: u32 = 1;
pub const PLUG_4K: u32 = 2;
pub const PLUG_DESKTOP: u32 = 3;
pub const UNPLUG: u32 = 0xffff_ffff;
/// (3.0) gpusim monitors for the other outputs: an office monitor (1440p60,
/// SDR, product 2) and a twin of the desktop monitor (another serial).
pub const PLUG_OFFICE: u32 = 4;
pub const PLUG_DESKTOP_TWIN: u32 = 5;
/// (3.0) COMMIT_MULTI: turn an output off.
pub const COMMIT_OFF: u32 = 0x10;
/// (3.0) OUTPUTS flag: the output scans out a mode.
pub const OUTPUT_LIT: u32 = 0x100;
/// (3.0) Faults: the lit outputs would need more pixel rate than the engine
/// has; no such output (or named twice).
pub const FAULT_BANDWIDTH: u32 = 9;
pub const FAULT_NO_OUTPUT: u32 = 10;
/// (3.0) kqueue fflags: count output `n`'s vblanks.
pub const fn note_output(n: u32) -> u32 { 0x100 << n }
/// Interim connector type code for HDMI 2.1 (the power-on cable).
pub const CONN_HDMI21: u32 = 2;

fn open_rdwr(path: &str) -> io::Result<RawFd> {
    let c = CString::new(path).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(fd) }
}

unsafe fn call<T>(fd: RawFd, req: u64, arg: &mut T) -> io::Result<()> {
    if ioctl(fd, req as _, arg as *mut T as *mut c_void) != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Gpu — /dev/atrium-gpu0
// ---------------------------------------------------------------------------

pub struct Gpu { fd: RawFd }

/// Decoded device capabilities (the subset the TLV currently carries).
#[derive(Debug, Default, Clone)]
pub struct Caps {
    pub abi_major: u32,
    pub abi_minor: u32,
    pub vendor: String,
    pub features: u32,
    /// Per-VM GPU virtual-address window (CAP_ADDRESS_SPACE): bind VAs live in
    /// `va_base .. va_base+va_size`, `va_align`-aligned. Size allocations to this
    /// rather than discovering the limit via ENOSPC.
    pub va_base: u64,
    pub va_size: u64,
    pub va_align: u64,
    /// Device-local VRAM heap size in bytes (CAP_HEAPS, the DEVICE heap).
    pub vram_bytes: u64,
    /// CAP_BIND_FEATURES (`BIND_FEAT_*`).
    pub bind_features: u64,
    /// CAP_ISA: the instruction set family (`ISA_*`), the architecture's own
    /// revision number (0x1201 for gfx1201) and its name. Pick compilers
    /// and packet builders by this, not by the vendor string. 0 = the
    /// driver does not report one (before ABI rev 4).
    pub isa_family: u32,
    pub isa_revision: u32,
    pub isa_name: String,
}

/// CAP_ISA families.
pub const ISA_AMD_RDNA4: u32 = 1;
pub const ISA_AMD_RDNA3: u32 = 2;
pub const ISA_INTEL_XE2: u32 = 3;
/// CAP_BIND_FEATURES: read-only binds (`Vm::alloc_readonly`).
pub const BIND_FEAT_READ_ONLY: u64 = 0x2;

impl Gpu {
    pub fn open() -> io::Result<Self> {
        Ok(Self { fd: open_rdwr("/dev/atrium-gpu0")? })
    }

    /// Walk the QUERY_CAPS TLV (two-phase: size probe, then fill).
    pub fn caps(&self) -> io::Result<Caps> {
        // A size-probe (caps_size=0) is unusable here: the driver reports the
        // needed size only via ENOMEM, and FreeBSD skips the IOC_OUT copyout when
        // the handler errors — so the probe can never learn the size. Pass a
        // generous fixed buffer up front (driver TLV is <128 bytes), like the C test.
        let mut buf = vec![0u8; 512];
        let mut q = CapsQuery { caps_ptr: buf.as_mut_ptr() as u64, caps_size: buf.len() as u64 };
        unsafe { call(self.fd, iowr(G, 15, std::mem::size_of::<CapsQuery>()), &mut q)? };
        buf.truncate(q.caps_size as usize);
        let mut caps = Caps::default();
        let mut i = 0usize;
        while i + 8 <= buf.len() {
            let cap_id = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
            let cap_size = u32::from_le_bytes(buf[i + 4..i + 8].try_into().unwrap()) as usize;
            let data = &buf[i + 8..(i + 8 + cap_size).min(buf.len())];
            match cap_id {
                1 if data.len() >= 8 => {
                    caps.abi_major = u32::from_le_bytes(data[0..4].try_into().unwrap());
                    caps.abi_minor = u32::from_le_bytes(data[4..8].try_into().unwrap());
                }
                2 => {
                    let n = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                    caps.vendor = String::from_utf8_lossy(&data[..n]).into_owned();
                }
                // Ids as include/atrium/gpu_abi_v2.h numbers them (ABI rev 4).
                0x100 if data.len() >= 4 => {
                    caps.features = u32::from_le_bytes(data[0..4].try_into().unwrap());
                }
                5 if data.len() >= 24 => {
                    caps.va_base = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    caps.va_size = u64::from_le_bytes(data[8..16].try_into().unwrap());
                    caps.va_align = u64::from_le_bytes(data[16..24].try_into().unwrap());
                }
                3 => {
                    // array of heap_info { u32 kind, u32 flags, u64 size }; take
                    // the DEVICE (VRAM) heap's size.
                    for h in data.chunks_exact(16) {
                        let kind = u32::from_le_bytes(h[0..4].try_into().unwrap());
                        if kind == 0 {
                            caps.vram_bytes = u64::from_le_bytes(h[8..16].try_into().unwrap());
                        }
                    }
                }
                9 if data.len() >= 8 => {
                    caps.bind_features = u64::from_le_bytes(data[0..8].try_into().unwrap());
                }
                18 if data.len() >= 32 => {
                    caps.isa_family = u32::from_le_bytes(data[0..4].try_into().unwrap());
                    caps.isa_revision = u32::from_le_bytes(data[4..8].try_into().unwrap());
                    let name = &data[8..32];
                    let n = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                    caps.isa_name = String::from_utf8_lossy(&name[..n]).into_owned();
                }
                _ => {}
            }
            i += 8 + ((cap_size + 3) & !3); // pad to 4 bytes
        }
        Ok(caps)
    }

    /// Create a per-process address space (VMID + page tables).
    pub fn create_vm(&self) -> io::Result<Vm<'_>> {
        let mut v = VmCreate::default();
        unsafe { call(self.fd, iowr(G, 12, std::mem::size_of::<VmCreate>()), &mut v)? };
        Ok(Vm { gpu: self, fd: v.out_fd as RawFd })
    }

    /// Create a timeline syncobj (its fd is kqueue-able, see [`Syncobj`]).
    pub fn create_syncobj(&self) -> io::Result<Syncobj<'_>> {
        let mut c = SyncobjCreate::default();
        unsafe { call(self.fd, iowr(G, 8, std::mem::size_of::<SyncobjCreate>()), &mut c)? };
        Ok(Syncobj { gpu: self, fd: c.out_fd as RawFd })
    }

    /// Total interrupts the device's IH ISR has serviced (GET_IRQS). The ISR
    /// bumps this on every interrupt, so with no submits in flight it rises only
    /// on vblank IRQs — the observable that the DCN-like vblank interrupt fires.
    pub fn irq_count(&self) -> io::Result<u64> {
        let mut q = Irqs::default();
        unsafe { call(self.fd, ior(G, 6, std::mem::size_of::<Irqs>()), &mut q)? };
        Ok(q.count)
    }

}

impl AsRawFd for Gpu { fn as_raw_fd(&self) -> RawFd { self.fd } }
impl Drop for Gpu { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

// ---------------------------------------------------------------------------
// GpuCtl — /dev/atrium-gpu0ctl: device-wide control (reset, firmware scheduler,
// power gating). Root-only and never in a jail; the client node refuses these
// ioctls with EPERM (GPU isolation, review 2026-11-03 §A2.4).
// ---------------------------------------------------------------------------

pub struct GpuCtl { fd: RawFd }

impl GpuCtl {
    pub fn open() -> io::Result<Self> {
        Ok(Self { fd: open_rdwr("/dev/atrium-gpu0ctl")? })
    }

    // --- firmware scheduler (regSCHED via the 'A' SCHED ioctl) ---

    fn sched(&self, mut s: Sched) -> io::Result<Sched> {
        unsafe { call(self.fd, iowr(G, 25, std::mem::size_of::<Sched>()), &mut s)? };
        Ok(s)
    }
    /// Register a queue with a weight + per-round kernel; returns the queue count.
    pub fn sched_add_queue(&self, weight: u32, ops: u32, bytes: u32, level: u32) -> io::Result<u32> {
        Ok(self.sched(Sched { op: 0, arg: weight, ops, bytes, level, ..Default::default() })?.count)
    }
    /// Run `rounds` scheduling rounds (deadline-aware iff a window is set).
    pub fn sched_run(&self, rounds: u32) -> io::Result<()> {
        self.sched(Sched { op: 1, arg: rounds, ..Default::default() }).map(|_| ())
    }
    /// Query queue `q`: (runs, engine-time µs, energy µJ).
    pub fn sched_query(&self, q: u32) -> io::Result<(u32, u32, u32)> {
        let r = self.sched(Sched { op: 2, arg: q, ..Default::default() })?;
        Ok((r.runs, r.busy_us, r.energy_uj))
    }
    /// Set the deadline window (ns); 0 = deadline-blind (fair).
    pub fn sched_set_window(&self, window_ns: u32) -> io::Result<()> {
        self.sched(Sched { op: 3, arg: window_ns, ..Default::default() }).map(|_| ())
    }
    /// Stamp queue `q` with a deadline `deadline_ns` from now (0 = clear) — the
    /// frame-pacing path (a compositor's target vblank).
    pub fn sched_set_deadline(&self, q: u32, deadline_ns: u32) -> io::Result<()> {
        self.sched(Sched { op: 4, arg: q, deadline_ns, ..Default::default() }).map(|_| ())
    }
}

impl AsRawFd for GpuCtl { fn as_raw_fd(&self) -> RawFd { self.fd } }
impl Drop for GpuCtl { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

// ---------------------------------------------------------------------------
// Vm — a per-process GPU address space
// ---------------------------------------------------------------------------

pub struct Vm<'a> { gpu: &'a Gpu, fd: RawFd }

impl<'a> Vm<'a> {
    /// The device this address space belongs to.
    pub fn gpu(&self) -> &'a Gpu { self.gpu }

    /// Allocate a BO and bind it into this VM at an auto-assigned GPU-VA.
    /// Allocate a BO mapped read-only for the GPU in this VM (`VM_BIND2`,
    /// `BIND_READONLY`): a GPU store to it is a VM fault. The CPU still fills
    /// it through [`Bo::write`]. For an app's code and constants (F6).
    pub fn alloc_readonly(&self, size: u64, flags: u32) -> io::Result<Bo<'a>> {
        let mut a = BoAlloc { size, flags, ..Default::default() };
        unsafe { call(self.gpu.fd, iowr(G, 0, std::mem::size_of::<BoAlloc>()), &mut a)? };
        let mut b = VmBind2 { vm_fd: self.fd as u32, bo_fd: a.bo_fd, flags: BIND_READONLY, ..Default::default() };
        if let Err(e) = unsafe { call(self.gpu.fd, iowr(G, 27, std::mem::size_of::<VmBind2>()), &mut b) } {
            unsafe { libc::close(a.bo_fd as RawFd) };
            return Err(e);
        }
        Ok(Bo { gpu: self.gpu, fd: a.bo_fd as RawFd, gpu_va: b.va, size })
    }

    /// Map `bo`, which another VM of the same device allocated, into this
    /// one as well: read-only to the GPU here if `readonly` (VM_BIND2,
    /// ABI rev 3). The returned [`Bo`] holds its own fd (a dup), so either
    /// handle may be dropped first; the pages live until both are gone and
    /// no queue can still touch them.
    pub fn share(&self, bo: &Bo<'_>, readonly: bool) -> io::Result<Bo<'a>> {
        let fd = unsafe { libc::fcntl(bo.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let flags = if readonly { BIND_READONLY } else { 0 };
        let mut b = VmBind2 { vm_fd: self.fd as u32, bo_fd: fd as u32, flags, ..Default::default() };
        if let Err(e) = unsafe { call(self.gpu.fd, iowr(G, 27, std::mem::size_of::<VmBind2>()), &mut b) } {
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(Bo { gpu: self.gpu, fd, gpu_va: b.va, size: bo.size })
    }

    pub fn alloc(&self, size: u64, flags: u32) -> io::Result<Bo<'a>> {
        let mut a = BoAlloc { size, flags, ..Default::default() };
        unsafe { call(self.gpu.fd, iowr(G, 0, std::mem::size_of::<BoAlloc>()), &mut a)? };
        let mut b = VmBind { va: 0, vm_fd: self.fd as u32, bo_fd: a.bo_fd };
        if let Err(e) = unsafe { call(self.gpu.fd, iowr(G, 13, std::mem::size_of::<VmBind>()), &mut b) } {
            unsafe { libc::close(a.bo_fd as RawFd) };
            return Err(e);
        }
        Ok(Bo { gpu: self.gpu, fd: a.bo_fd as RawFd, gpu_va: b.va, size })
    }

    /// Bind a BO obtained from elsewhere — a `bo_fd` passed in via SCM_RIGHTS
    /// (BOs are `DFLAG_PASSABLE`) or `dup`'d locally — into this VM, sharing the
    /// same underlying memory. This is the cross-address-space sharing path
    /// (e.g. a compositor importing a client's buffer): the BO is independent of
    /// any VM (ABI-v2), so the same object maps into many VMs at independent
    /// GPU-VAs. Takes ownership of `bo_fd` (closed on drop).
    pub fn import(&self, bo_fd: RawFd, size: u64) -> io::Result<Bo<'a>> {
        let mut b = VmBind { va: 0, vm_fd: self.fd as u32, bo_fd: bo_fd as u32 };
        unsafe { call(self.gpu.fd, iowr(G, 13, std::mem::size_of::<VmBind>()), &mut b)? };
        Ok(Bo { gpu: self.gpu, fd: bo_fd, gpu_va: b.va, size })
    }

    /// Submit a PM4 ring (already laid into `ring`) on an engine, optionally
    /// signalling `signal` (a syncobj + value) on completion.
    pub fn submit(
        &self,
        ring: &Bo<'_>,
        n_dwords: u32,
        engine: u32,
        signal: Option<(&Syncobj<'_>, u64)>,
    ) -> io::Result<()> {
        self.submit_deadline(ring, n_dwords, engine, signal, 0)
    }

    /// Submit, stamping this submit's queue with a frame `deadline_ns` (ns; 0 =
    /// none). The firmware scheduler drains queues earliest-deadline-first, so a
    /// compositor's frame beats a background hog under contention (frame-pacing).
    pub fn submit_deadline(
        &self,
        ring: &Bo<'_>,
        n_dwords: u32,
        engine: u32,
        signal: Option<(&Syncobj<'_>, u64)>,
        deadline_ns: u32,
    ) -> io::Result<()> {
        let (sfd, sval) = signal.map_or((-1i32, 0u64), |(s, v)| (s.fd, v));
        let mut s = Submit {
            signal_value: sval,
            vm_fd: self.fd as u32,
            ring_fd: ring.fd as u32,
            n_dwords,
            engine,
            signal_syncobj_fd: sfd,
            deadline_ns,
        };
        unsafe { call(self.gpu.fd, iow(G, 4, std::mem::size_of::<Submit>()), &mut s) }
    }
}

impl Drop for Vm<'_> { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

// ---------------------------------------------------------------------------
// Bo — a buffer object (fd-as-handle)
// ---------------------------------------------------------------------------

pub struct Bo<'a> { gpu: &'a Gpu, fd: RawFd, gpu_va: u64, size: u64 }

impl<'a> Bo<'a> {
    pub fn gpu_va(&self) -> u64 { self.gpu_va }
    pub fn size(&self) -> u64 { self.size }

    /// The BO's fd — pass it via SCM_RIGHTS (or `dup`) to share this buffer with
    /// another VM/process, which binds it with [`Vm::import`]. Borrowed: this Bo
    /// keeps ownership.
    pub fn as_raw_fd(&self) -> RawFd { self.fd }

    /// Copy `data` into the BO at `offset` (System BOs; VRAM BOs are GPU-only).
    pub fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut x = BoXfer {
            offset,
            len: data.len() as u64,
            user_ptr: data.as_ptr() as u64,
            bo_fd: self.fd as u32,
            pad: 0,
        };
        unsafe { call(self.gpu.fd, iow(G, 1, std::mem::size_of::<BoXfer>()), &mut x) }
    }

    /// Copy `out.len()` bytes from the BO at `offset` into `out`.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        let mut x = BoXfer {
            offset,
            len: out.len() as u64,
            user_ptr: out.as_mut_ptr() as u64,
            bo_fd: self.fd as u32,
            pad: 0,
        };
        unsafe { call(self.gpu.fd, iow(G, 2, std::mem::size_of::<BoXfer>()), &mut x) }
    }
}

impl Drop for Bo<'_> { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

// ---------------------------------------------------------------------------
// Scanout — the offset-model "CPU pixels -> onscreen" surface
// ---------------------------------------------------------------------------

/// A set of presentable framebuffers for the display.
///
/// CPU-rendered pixels can't be written into a scannable VRAM BO directly
/// (VRAM is GPU-only), so a `Scanout` owns a System **staging** BO
/// (CPU-writable via `BO_WRITE`), one or more VRAM **scanout** BOs (the only
/// kind the display can scan) and a small **ring** BO. [`Scanout::upload`]
/// copies pixels into staging, then a CP `DMA_DATA` blit moves them into one of
/// the VRAM buffers. The display is then given that buffer's BO
/// ([`Scanout::buffer`]) in a [`Display::commit`].
///
/// With two or more buffers, render into one that is not on screen: the
/// display scans one out while the next frame is copied into another.
pub struct Scanout<'a> {
    vm: &'a Vm<'a>,
    staging: Bo<'a>,
    buffers: Vec<Bo<'a>>,
    ring: Bo<'a>,
    size: u64,
}

impl<'a> Scanout<'a> {
    /// One VRAM buffer of `size` bytes.
    pub fn new(vm: &'a Vm<'a>, size: u64) -> io::Result<Self> {
        Self::with_buffers(vm, size, 1)
    }

    /// `n` VRAM buffers of `size` bytes each, sharing one staging BO.
    pub fn with_buffers(vm: &'a Vm<'a>, size: u64, n: usize) -> io::Result<Self> {
        let staging = vm.alloc(size, 0)?; // System: CPU-writable
        let mut buffers = Vec::with_capacity(n);
        for _ in 0..n.max(1) {
            buffers.push(vm.alloc(size, BO_VRAM)?); // VRAM: scannable
        }
        let ring = vm.alloc(4096, 0)?;
        Ok(Self { vm, staging, buffers, ring, size })
    }

    /// Buffer `i`, to name in a [`Display::commit`].
    pub fn buffer(&self, i: usize) -> &Bo<'a> { &self.buffers[i] }
    pub fn len(&self) -> usize { self.buffers.len() }
    pub fn is_empty(&self) -> bool { self.buffers.is_empty() }
    pub fn size(&self) -> u64 { self.size }

    /// Upload CPU `pixels` (a full frame) into buffer 0.
    pub fn update(&self, pixels: &[u8]) -> io::Result<()> {
        self.upload(0, pixels, 0, None)
    }

    /// Upload CPU `pixels` into buffer `i` with a CP DMA copy, stamped with a
    /// frame `deadline_ns` (0 = none) so the firmware scheduler serves it ahead
    /// of background GPU work. With `signal`, that syncobj reaches the value
    /// when the copy is done: pass it as the commit's in-fence.
    pub fn upload(
        &self,
        i: usize,
        pixels: &[u8],
        deadline_ns: u32,
        signal: Option<(&Syncobj<'_>, u64)>,
    ) -> io::Result<()> {
        self.staging.write(0, pixels)?;
        let src = self.staging.gpu_va();
        let dst = self.buffers[i].gpu_va();
        // The EOP fence lands in the ring BO's second half (never executed).
        let fence = self.ring.gpu_va() + 2048;
        let words: [u32; 14] = [
            pm4_type3(IT_DMA_DATA, 6),
            0,
            (src & 0xffff_ffff) as u32,
            (src >> 32) as u32,
            (dst & 0xffff_ffff) as u32,
            (dst >> 32) as u32,
            pixels.len() as u32,
            pm4_type3(IT_RELEASE_MEM, 6),
            5 << 8,                 // EOP event
            (2 << 29) | (2 << 24),  // 64-bit data, interrupt after write
            (fence & 0xffff_ffff) as u32,
            (fence >> 32) as u32,
            0x0000_0001,
            0,
        ];
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for w in words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        self.ring.write(0, &bytes)?;
        self.vm.submit_deadline(&self.ring, words.len() as u32, ENGINE_GFX, signal, deadline_ns)
    }

    /// Like [`Scanout::update`], with a frame deadline.
    pub fn update_deadline(&self, pixels: &[u8], deadline_ns: u32) -> io::Result<()> {
        self.upload(0, pixels, deadline_ns, None)
    }
}

// ---------------------------------------------------------------------------
// Syncobj — a timeline (its fd is kqueue-able)
// ---------------------------------------------------------------------------

pub struct Syncobj<'a> { gpu: &'a Gpu, fd: RawFd }

impl<'a> Syncobj<'a> {
    pub fn signal(&self, value: u64) -> io::Result<()> {
        let mut o = SyncobjOp { value, syncobj_fd: self.fd as u32, pad: 0 };
        unsafe { call(self.gpu.fd, iow(G, 9, std::mem::size_of::<SyncobjOp>()), &mut o) }
    }
    pub fn query(&self) -> io::Result<u64> {
        let mut o = SyncobjOp { value: 0, syncobj_fd: self.fd as u32, pad: 0 };
        unsafe { call(self.gpu.fd, iowr(G, 10, std::mem::size_of::<SyncobjOp>()), &mut o)? };
        Ok(o.value)
    }
    pub fn wait(&self, value: u64, timeout_ms: u32) -> io::Result<()> {
        let mut w = SyncobjWait { value, syncobj_fd: self.fd as u32, timeout_ms };
        unsafe { call(self.gpu.fd, iow(G, 11, std::mem::size_of::<SyncobjWait>()), &mut w) }
    }
}

/// The syncobj fd is `EVFILT_READ`-able — register it on a kqueue with the wait
/// threshold in the kevent `data` field; it fires when the counter reaches it.
impl AsRawFd for Syncobj<'_> { fn as_raw_fd(&self) -> RawFd { self.fd } }
impl Drop for Syncobj<'_> { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

// ---------------------------------------------------------------------------
// Display — /dev/atrium-display0 (decoupled, dma-buf-style scanout)
// ---------------------------------------------------------------------------

pub struct Display {
    fd: RawFd,
    /// Pixel format of the buffers committed (`FORMAT_*`), see [`Display::set_format`].
    format: std::cell::Cell<u32>,
}

/// [`Display::status_v2`] result.
#[derive(Debug, Clone, Copy)]
pub struct StatusV2 {
    pub flags: u32,
    pub vblank_count: u64,
    pub flips_done: u64,
    pub dropped_flips: u32,
    pub tear_line: u32,
    pub refresh_mhz: u32,
    pub last_fault: u32,
}

/// [`Display::connector_v2`] result (display ABI 2.1 CONNECTOR).
#[derive(Debug, Clone)]
pub struct ConnectorV2 {
    pub connected: bool,
    pub connector_type: u32,
    /// Hotplug generation; pass it to [`Display::modeset_at`].
    pub generation: u32,
    pub edid: Vec<u8>,
    pub modes: Vec<ModeV2>,
    /// Index in `modes` of the lit mode, if any.
    pub current: Option<usize>,
    /// The variable refresh range `(min_hz, max_hz)`, if the monitor has one.
    pub vrr: Option<(u32, u32)>,
    /// The lit mode runs with variable refresh.
    pub vrr_active: bool,
    /// (2.3) The monitor declares HDR; an HDR EOTF is being signalled.
    pub hdr: bool,
    pub hdr_active: bool,
}

/// (3.0) One output in [`Display::outputs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputInfo {
    /// The monitor's identity (0 = none): the same monitor reads the same on
    /// any output and any boot.
    pub identity: u64,
    /// `CONNECTOR_*` | [`OUTPUT_LIT`].
    pub flags: u32,
    pub connector_type: u32,
    pub generation: u32,
    /// The lit mode (0 when off) and its pixel clock.
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub pixel_clock_khz: u32,
}

impl OutputInfo {
    pub fn connected(&self) -> bool { self.flags & CONNECTOR_CONNECTED != 0 }
    pub fn lit(&self) -> bool { self.flags & OUTPUT_LIT != 0 }
}

/// (3.0) [`Display::outputs`]: every output, one snapshot.
#[derive(Debug, Clone)]
pub struct Outputs {
    pub topology_generation: u32,
    pub bandwidth_khz: u32,
    pub bandwidth_used_khz: u32,
    pub out: Vec<OutputInfo>,
}

/// (3.0) One output's part of [`Display::commit_multi`].
pub struct OutputCommit<'b, 'a> {
    pub output: u32,
    /// [`COMMIT_MODESET`] (with [`COMMIT_VRR`]), [`COMMIT_OFF`], or 0 /
    /// [`COMMIT_ASYNC`] for a flip.
    pub flags: u32,
    pub mode_index: u32,
    pub generation: u32,
    pub bo: Option<&'b Bo<'a>>,
    pub pitch: u32,
    pub format: u32,
    pub in_fence: Option<(&'b Syncobj<'a>, u64)>,
    pub out_fence: Option<(&'b Syncobj<'a>, u64)>,
}

impl<'b, 'a> OutputCommit<'b, 'a> {
    /// Light `output` at its connector's `modes[mode_index]` with `bo` up.
    pub fn modeset(output: u32, bo: &'b Bo<'a>, mode_index: u32, generation: u32) -> Self {
        OutputCommit {
            output,
            flags: COMMIT_MODESET,
            mode_index,
            generation,
            bo: Some(bo),
            pitch: 0,
            format: FORMAT_XRGB8888,
            in_fence: None,
            out_fence: None,
        }
    }
    /// Turn `output` off.
    pub fn off(output: u32) -> Self {
        OutputCommit { flags: COMMIT_OFF, bo: None, ..Self::flip(output, None, true) }
    }
    /// Flip `output` to `bo` (vsync or at once).
    pub fn flip(output: u32, bo: Option<&'b Bo<'a>>, vsync: bool) -> Self {
        OutputCommit {
            output,
            flags: if vsync { 0 } else { COMMIT_ASYNC },
            mode_index: 0,
            generation: 0,
            bo,
            pitch: 0,
            format: FORMAT_XRGB8888,
            in_fence: None,
            out_fence: None,
        }
    }
}

/// (3.0) What [`Display::commit_multi`] made of a commit: the first output
/// (index into the slice) that could not take its part, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultiResult {
    pub failed: Option<usize>,
    pub reason: u32,
}

#[derive(Debug, Clone)]
pub struct Connector {
    pub connected: bool,
    pub connector_type: u32,
    pub usbc_lanes: u32,
    pub edid: Vec<u8>,
}

impl Display {
    pub fn open() -> io::Result<Self> {
        Ok(Self { fd: open_rdwr("/dev/atrium-display0")?, format: std::cell::Cell::new(FORMAT_XRGB8888) })
    }

    pub fn connector(&self) -> io::Result<Connector> {
        let mut c = DConnector::default();
        unsafe { call(self.fd, ior(D, 1, std::mem::size_of::<DConnector>()), &mut c)? };
        Ok(Connector {
            connected: c.connected != 0,
            connector_type: c.connector_type,
            usbc_lanes: c.usbc_lanes,
            edid: c.edid[..(c.edid_len as usize).min(128)].to_vec(),
        })
    }

    pub fn modes(&self) -> io::Result<Vec<Mode>> {
        let mut m = DModes::default();
        unsafe { call(self.fd, ior(D, 2, std::mem::size_of::<DModes>()), &mut m)? };
        Ok(m.modes[..(m.count as usize).min(8)].to_vec())
    }

    /// The monitor, its raw EDID and every mode it offers (display ABI 2.1).
    pub fn connector_v2(&self) -> io::Result<ConnectorV2> {
        let mut c: DConnectorV2 = unsafe { std::mem::zeroed() };
        c.struct_size = std::mem::size_of::<DConnectorV2>() as u32;
        unsafe { call(self.fd, iowr(D, 66, std::mem::size_of::<DConnectorV2>()), &mut c)? };
        Ok(Self::conn_v2(&c))
    }

    /// (3.0) [`Self::connector_v2`] for any output, with the monitor's identity.
    pub fn connector_at(&self, output: u32) -> io::Result<(ConnectorV2, u64)> {
        let mut c: DConnectorAt = unsafe { std::mem::zeroed() };
        c.struct_size = std::mem::size_of::<DConnectorAt>() as u32;
        c.output = output;
        c.conn.struct_size = std::mem::size_of::<DConnectorV2>() as u32;
        unsafe { call(self.fd, iowr(D, 69, std::mem::size_of::<DConnectorAt>()), &mut c)? };
        Ok((Self::conn_v2(&c.conn), c.out_identity))
    }

    /// (3.0) Every output at a glance.
    pub fn outputs(&self) -> io::Result<Outputs> {
        let mut v = DOutputs { struct_size: std::mem::size_of::<DOutputs>() as u32, ..Default::default() };
        unsafe { call(self.fd, iowr(D, 68, std::mem::size_of::<DOutputs>()), &mut v)? };
        let n = (v.out_count as usize).min(8);
        Ok(Outputs {
            topology_generation: v.out_topology_generation,
            bandwidth_khz: v.out_bandwidth_khz,
            bandwidth_used_khz: v.out_bandwidth_used_khz,
            out: v.out[..n]
                .iter()
                .map(|o| OutputInfo {
                    identity: o.identity,
                    flags: o.flags,
                    connector_type: o.connector_type,
                    generation: o.generation,
                    width: o.width,
                    height: o.height,
                    refresh_mhz: o.refresh_mhz,
                    pixel_clock_khz: o.pixel_clock_khz,
                })
                .collect(),
        })
    }

    /// (3.0) Change several outputs in one commit: atomic for mode sets and
    /// turn-offs, per output for flips. `test` checks without applying.
    pub fn commit_multi(&self, parts: &[OutputCommit<'_, '_>], test: bool) -> io::Result<MultiResult> {
        let mut oc: Vec<DOutputCommit> = parts
            .iter()
            .map(|p| {
                let (in_fd, in_v) = p.in_fence.map_or((-1, 0), |(s, v)| (s.fd, v));
                let (out_fd, out_v) = p.out_fence.map_or((-1, 0), |(s, v)| (s.fd, v));
                DOutputCommit {
                    output: p.output,
                    _pad: 0,
                    c: DCommit {
                        struct_size: std::mem::size_of::<DCommit>() as u32,
                        flags: p.flags,
                        mode_index: p.mode_index,
                        format: p.format,
                        bo_fd: p.bo.map_or(-1, |b| b.fd),
                        pitch: p.pitch,
                        in_fence_fd: in_fd,
                        out_fence_fd: out_fd,
                        in_fence_value: in_v,
                        out_fence_value: out_v,
                        mode_generation: p.generation,
                        ..Default::default()
                    },
                }
            })
            .collect();
        let mut m = DCommitMulti {
            struct_size: std::mem::size_of::<DCommitMulti>() as u32,
            flags: if test { COMMIT_TEST_ONLY } else { 0 },
            count: oc.len() as u32,
            commits: oc.as_mut_ptr() as u64,
            ..Default::default()
        };
        unsafe { call(self.fd, iowr(D, 71, std::mem::size_of::<DCommitMulti>()), &mut m)? };
        Ok(MultiResult {
            failed: (m.out_failed != u32::MAX).then_some(m.out_failed as usize),
            reason: m.out_reason,
        })
    }

    /// (3.0) [`Self::status_v2`] for any output.
    pub fn status_at(&self, output: u32) -> io::Result<StatusV2> {
        let mut st = DStatusAt { struct_size: std::mem::size_of::<DStatusAt>() as u32, output, ..Default::default() };
        st.status.struct_size = std::mem::size_of::<DStatusV2>() as u32;
        unsafe { call(self.fd, iowr(D, 70, std::mem::size_of::<DStatusAt>()), &mut st)? };
        let s = &st.status;
        Ok(StatusV2 {
            flags: s.out_flags,
            vblank_count: s.out_vblank_count,
            flips_done: s.out_flips_done,
            dropped_flips: s.out_dropped_flips,
            tear_line: s.out_tear_line,
            refresh_mhz: s.out_refresh_mhz,
            last_fault: s.out_last_fault,
        })
    }

    fn conn_v2(c: &DConnectorV2) -> ConnectorV2 {
        let n = (c.out_mode_count as usize).min(16);
        ConnectorV2 {
            connected: c.out_flags & 1 != 0,
            connector_type: c.out_connector_type,
            generation: c.out_generation,
            edid: c.out_edid[..(c.out_edid_len as usize).min(512)].to_vec(),
            modes: c.out_modes[..n].to_vec(),
            current: (c.out_current_mode as usize).lt(&n).then_some(c.out_current_mode as usize),
            vrr: (c.out_flags & CONNECTOR_VRR != 0)
                .then_some((u32::from(c.out_vrr_min_hz), u32::from(c.out_vrr_max_hz))),
            vrr_active: c.out_flags & CONNECTOR_VRR_ACTIVE != 0,
            hdr: c.out_flags & CONNECTOR_HDR != 0,
            hdr_active: c.out_flags & CONNECTOR_HDR_ACTIVE != 0,
        }
    }

    /// The pixel format of the buffers this display commits from now on
    /// (`FORMAT_XRGB8888` until set).
    pub fn set_format(&self, format: u32) { self.format.set(format); }

    fn hdr_call(&self, d: &mut DHdr) -> io::Result<HdrCaps> {
        d.struct_size = std::mem::size_of::<DHdr>() as u32;
        unsafe { call(self.fd, iowr(D, 67, std::mem::size_of::<DHdr>()), d)? };
        Ok(HdrCaps {
            eotfs: d.out_eotfs,
            colorimetry: d.out_colorimetry,
            max_nits: d.out_max_nits,
            max_fall_nits: d.out_max_fall_nits,
            min_nits_x10000: d.out_min_nits_x10000,
        })
    }

    /// (2.3) What is signalled now, and what the monitor offers.
    pub fn hdr(&self) -> io::Result<(Hdr, HdrCaps)> {
        let mut d = DHdr::default();
        let caps = self.hdr_call(&mut d)?;
        Ok((
            Hdr {
                eotf: d.eotf,
                colorimetry: d.colorimetry,
                primaries_x: d.primaries_x,
                primaries_y: d.primaries_y,
                white_x: d.white_x,
                white_y: d.white_y,
                max_mastering_nits: d.max_mastering_nits,
                min_mastering: d.min_mastering,
                max_cll: d.max_cll,
                max_fall: d.max_fall,
            },
            caps,
        ))
    }

    /// (2.3) Signal `h` from the next commit on (`EINVAL` if the monitor
    /// doesn't offer its EOTF or colorimetry). [`Hdr::default`] is SDR.
    pub fn set_hdr(&self, h: &Hdr) -> io::Result<HdrCaps> {
        self.set_hdr_at(0, h)
    }

    /// (3.0) [`Self::set_hdr`] for any output.
    pub fn set_hdr_at(&self, output: u32, h: &Hdr) -> io::Result<HdrCaps> {
        let mut d = DHdr {
            output,
            flags: HDR_SET,
            eotf: h.eotf,
            colorimetry: h.colorimetry,
            primaries_x: h.primaries_x,
            primaries_y: h.primaries_y,
            white_x: h.white_x,
            white_y: h.white_y,
            max_mastering_nits: h.max_mastering_nits,
            min_mastering: h.min_mastering,
            max_cll: h.max_cll,
            max_fall: h.max_fall,
            ..Default::default()
        };
        self.hdr_call(&mut d)
    }

    /// Test knob (interim CONFIG): re-plug the simulated monitor (`PLUG_*`)
    /// on a connector type, or unplug it ([`UNPLUG`]).
    pub fn sim_plug(&self, connector_type: u32, plug: u32) -> io::Result<()> {
        let mut c = DConfig { connector_type, plug_mode: plug };
        unsafe { call(self.fd, iow(D, 6, std::mem::size_of::<DConfig>()), &mut c) }
    }

    /// MODESET at `connector_v2().modes[index]`, checked against the hotplug
    /// `generation` it was read from (`ESTALE` if the monitor changed since;
    /// 0 skips the check). `pitch` 0 = packed.
    pub fn modeset_at(&self, bo: &Bo<'_>, index: u32, generation: u32, pitch: u32) -> io::Result<u32> {
        self.modeset_fenced(bo, index, generation, pitch, None, None)
    }

    /// [`Self::modeset_at`] with fences: the in-fence must already be
    /// signalled (`EAGAIN` otherwise); the out-fence fires with the mode lit.
    pub fn modeset_fenced(
        &self,
        bo: &Bo<'_>,
        index: u32,
        generation: u32,
        pitch: u32,
        in_fence: Option<(&Syncobj<'_>, u64)>,
        out_fence: Option<(&Syncobj<'_>, u64)>,
    ) -> io::Result<u32> {
        self.modeset_vrr(bo, index, generation, pitch, false, in_fence, out_fence)
    }

    /// [`Self::modeset_fenced`], with variable refresh on (`vrr`) or off
    /// (display ABI 2.2; `EINVAL` if the monitor has no VRR range or the
    /// mode's refresh is outside it).
    #[allow(clippy::too_many_arguments)]
    pub fn modeset_vrr(
        &self,
        bo: &Bo<'_>,
        index: u32,
        generation: u32,
        pitch: u32,
        vrr: bool,
        in_fence: Option<(&Syncobj<'_>, u64)>,
        out_fence: Option<(&Syncobj<'_>, u64)>,
    ) -> io::Result<u32> {
        let (in_fd, in_v) = in_fence.map_or((-1, 0), |(s, v)| (s.fd, v));
        let (out_fd, out_v) = out_fence.map_or((-1, 0), |(s, v)| (s.fd, v));
        let mut c = DCommit {
            struct_size: std::mem::size_of::<DCommit>() as u32,
            flags: COMMIT_MODESET | if vrr { COMMIT_VRR } else { 0 },
            mode_index: index,
            format: self.format.get(),
            bo_fd: bo.fd,
            pitch,
            in_fence_fd: in_fd,
            out_fence_fd: out_fd,
            in_fence_value: in_v,
            out_fence_value: out_v,
            mode_generation: generation,
            ..Default::default()
        };
        unsafe { call(self.fd, iowr(D, 64, std::mem::size_of::<DCommit>()), &mut c) }?;
        Ok(c.out_fault)
    }

    /// Put `bo` on the primary plane (display ABI v2 `COMMIT`).
    ///
    /// `flags` is a mix of [`COMMIT_MODESET`], [`COMMIT_ASYNC`] and
    /// [`COMMIT_TEST_ONLY`]; `pitch` 0 means packed. The flip waits for
    /// `in_fence` and signals `out_fence` once `bo` is on screen (which is
    /// when the previous buffer is free again). Returns the referee fault
    /// (0 = ok); an outstanding commit gives `EBUSY`.
    pub fn commit(
        &self,
        bo: &Bo<'_>,
        flags: u32,
        pitch: u32,
        in_fence: Option<(&Syncobj<'_>, u64)>,
        out_fence: Option<(&Syncobj<'_>, u64)>,
    ) -> io::Result<u32> {
        let (in_fd, in_v) = in_fence.map_or((-1, 0), |(s, v)| (s.fd, v));
        let (out_fd, out_v) = out_fence.map_or((-1, 0), |(s, v)| (s.fd, v));
        let mut c = DCommit {
            struct_size: std::mem::size_of::<DCommit>() as u32,
            flags,
            format: self.format.get(),
            bo_fd: bo.fd,
            pitch,
            in_fence_fd: in_fd,
            out_fence_fd: out_fd,
            in_fence_value: in_v,
            out_fence_value: out_v,
            ..Default::default()
        };
        // A referee fault comes back as success with out_fault set.
        unsafe { call(self.fd, iowr(D, 64, std::mem::size_of::<DCommit>()), &mut c) }?;
        Ok(c.out_fault)
    }

    /// Light the CRTC with `bo` on screen from the first frame.
    pub fn modeset(&self, bo: &Bo<'_>) -> io::Result<u32> {
        self.commit(bo, COMMIT_MODESET, 0, None, None)
    }

    /// Flip to `bo` (vsync = latch at the next vblank, else at once).
    pub fn flip(&self, bo: &Bo<'_>, vsync: bool) -> io::Result<u32> {
        self.commit(bo, if vsync { 0 } else { COMMIT_ASYNC }, 0, None, None)
    }

    /// Wait until no commit is outstanding (the last one reached the screen),
    /// polling STATUS. For tests and simple tools; a compositor waits on the
    /// commit's out-fence instead.
    pub fn wait_idle(&self, timeout_ms: u64) -> io::Result<()> {
        let t0 = std::time::Instant::now();
        loop {
            if self.status_v2()?.flags & STATUS_COMMIT_PENDING == 0 {
                return Ok(());
            }
            if t0.elapsed().as_millis() as u64 > timeout_ms {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "display commit still pending"));
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Display ABI v2 status.
    pub fn status_v2(&self) -> io::Result<StatusV2> {
        let mut st = DStatusV2 {
            struct_size: std::mem::size_of::<DStatusV2>() as u32,
            ..Default::default()
        };
        unsafe { call(self.fd, iowr(D, 65, std::mem::size_of::<DStatusV2>()), &mut st)? };
        Ok(StatusV2 {
            flags: st.out_flags,
            vblank_count: st.out_vblank_count,
            flips_done: st.out_flips_done,
            dropped_flips: st.out_dropped_flips,
            tear_line: st.out_tear_line,
            refresh_mhz: st.out_refresh_mhz,
            last_fault: st.out_last_fault,
        })
    }

    /// (vblank_count, dropped_flips, tear_line).
    pub fn status(&self) -> io::Result<(u64, u32, u32)> {
        let mut st = DStatus::default();
        unsafe { call(self.fd, ior(D, 5, std::mem::size_of::<DStatus>()), &mut st)? };
        Ok((st.vblank_count, st.dropped_flips, st.tear_line))
    }
}

impl AsRawFd for Display { fn as_raw_fd(&self) -> RawFd { self.fd } }

/// A kqueue that wakes only on device resets of a [`Display`] (registered with
/// `NOTE_RESET`, so vblanks never wake it). Borrows the display's fd: keep the
/// `Display` open for as long as this is used.
pub struct ResetWatch { kq: RawFd }

/// What [`EventWatch::wait`] saw since the last wait. `data` is the
/// generation of the last event (the reset generation or the connector
/// generation); read CONNECTOR / STATUS for both when both are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayEvents {
    pub reset: bool,
    pub hotplug: bool,
    pub data: u64,
}

/// A kqueue that wakes on display events (`NOTE_RESET` and/or
/// `NOTE_HOTPLUG`), never on vblanks. Borrows the display's fd.
pub struct EventWatch { kq: RawFd, notes: u32 }

impl EventWatch {
    pub fn new(dpy: &Display, notes: u32) -> io::Result<Self> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let chg = libc::kevent {
            ident: dpy.fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: notes,
            data: 0,
            udata: std::ptr::null_mut(),
            ext: [0; 4],
        };
        if unsafe { libc::kevent(kq, &chg, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(kq) };
            return Err(e);
        }
        Ok(Self { kq, notes })
    }

    /// Block until the next event (or `timeout`, then `None`).
    pub fn wait(&self, timeout: Option<std::time::Duration>) -> io::Result<Option<DisplayEvents>> {
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut ev, 1, tsp) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(e);
            }
            if r == 0 { return Ok(None); }
            let f = ev.fflags & self.notes;
            if f != 0 {
                return Ok(Some(DisplayEvents {
                    reset: f & NOTE_RESET != 0,
                    hotplug: f & NOTE_HOTPLUG != 0,
                    data: ev.data as u64,
                }));
            }
        }
    }
}

impl Drop for EventWatch { fn drop(&mut self) { unsafe { libc::close(self.kq) }; } }

/// (3.0) A kqueue counting the vblanks of the outputs in `outputs` (a mask
/// of [`note_output`] bits). Borrows the display's fd.
pub struct VblankWatch { kq: RawFd }

impl VblankWatch {
    pub fn new(dpy: &Display, outputs: u32) -> io::Result<Self> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let chg = libc::kevent {
            ident: dpy.fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: outputs,
            data: 0,
            udata: std::ptr::null_mut(),
            ext: [0; 4],
        };
        if unsafe { libc::kevent(kq, &chg, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(kq) };
            return Err(e);
        }
        Ok(Self { kq })
    }

    /// Block until a vblank (or `timeout`, then `None`): the vblanks since the
    /// last wait, and the [`note_output`] bits of the outputs they came from.
    pub fn wait(&self, timeout: Option<std::time::Duration>) -> io::Result<Option<(u64, u32)>> {
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut ev, 1, tsp) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(e);
            }
            if r == 0 { return Ok(None); }
            return Ok(Some((ev.data as u64, ev.fflags)));
        }
    }
}

impl Drop for VblankWatch { fn drop(&mut self) { unsafe { libc::close(self.kq) }; } }

impl ResetWatch {
    pub fn new(dpy: &Display) -> io::Result<Self> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(io::Error::last_os_error());
        }
        let chg = libc::kevent {
            ident: dpy.fd as libc::uintptr_t,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: NOTE_RESET,
            data: 0,
            udata: std::ptr::null_mut(),
            ext: [0; 4],
        };
        if unsafe { libc::kevent(kq, &chg, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(kq) };
            return Err(e);
        }
        Ok(Self { kq })
    }

    /// Block until the next device reset (or `timeout`, if given); returns the
    /// device's reset generation, or None on timeout.
    pub fn wait(&self, timeout: Option<std::time::Duration>) -> io::Result<Option<u64>> {
        let ts = timeout.map(|t| libc::timespec {
            tv_sec: t.as_secs() as libc::time_t,
            tv_nsec: t.subsec_nanos() as libc::c_long,
        });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe { libc::kevent(self.kq, std::ptr::null(), 0, &mut ev, 1, tsp) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted { continue; }
                return Err(e);
            }
            if r == 0 { return Ok(None); }
            if ev.fflags & NOTE_RESET != 0 { return Ok(Some(ev.data as u64)); }
        }
    }
}

impl Drop for ResetWatch { fn drop(&mut self) { unsafe { libc::close(self.kq) }; } }

impl Drop for Display { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

/// GPU reset (`_IO('A',16)`) — exposed for harness/recovery paths.
pub fn gpu_reset(ctl: &GpuCtl) -> io::Result<()> {
    let r = unsafe { ioctl(ctl.fd, io(G, 16) as _, 0 as c_int) };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
