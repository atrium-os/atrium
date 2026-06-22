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
//!   * **Display is decoupled** (`/dev/atrium-display0`): a scanout buffer
//!     crosses GPU→display as a plain `{vram_offset, size}` from
//!     [`Bo::export_scanout`] (dma-buf-style), never a handle.
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
struct BoExportScanout { bo_fd: u32, pad: u32, vram_offset: u64, size: u64 }
#[repr(C)]
#[derive(Default)]
struct VmCreate { out_fd: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct VmBind { va: u64, vm_fd: u32, bo_fd: u32 }
#[repr(C)]
#[derive(Default)]
struct Submit {
    signal_value: u64,
    vm_fd: u32,
    ring_fd: u32,
    n_dwords: u32,
    engine: u32,
    signal_syncobj_fd: i32,
    pad: u32,
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
struct DSetMode { vram_offset: u64, size: u64, fault: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct DFlip { vram_offset: u64, size: u64, vsync: u32, fault: u32 }
#[repr(C)]
#[derive(Default)]
struct DStatus { vblank_count: u64, dropped_flips: u32, tear_line: u32 }

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
}

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
                3 if data.len() >= 4 => {
                    caps.features = u32::from_le_bytes(data[0..4].try_into().unwrap());
                }
                4 if data.len() >= 24 => {
                    caps.va_base = u64::from_le_bytes(data[0..8].try_into().unwrap());
                    caps.va_size = u64::from_le_bytes(data[8..16].try_into().unwrap());
                    caps.va_align = u64::from_le_bytes(data[16..24].try_into().unwrap());
                }
                5 => {
                    // array of heap_info { u32 kind, u32 flags, u64 size }; take
                    // the DEVICE (VRAM) heap's size.
                    for h in data.chunks_exact(16) {
                        let kind = u32::from_le_bytes(h[0..4].try_into().unwrap());
                        if kind == 0 {
                            caps.vram_bytes = u64::from_le_bytes(h[8..16].try_into().unwrap());
                        }
                    }
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

impl AsRawFd for Gpu { fn as_raw_fd(&self) -> RawFd { self.fd } }
impl Drop for Gpu { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

// ---------------------------------------------------------------------------
// Vm — a per-process GPU address space
// ---------------------------------------------------------------------------

pub struct Vm<'a> { gpu: &'a Gpu, fd: RawFd }

impl<'a> Vm<'a> {
    /// Allocate a BO and bind it into this VM at an auto-assigned GPU-VA.
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
        let (sfd, sval) = signal.map_or((-1i32, 0u64), |(s, v)| (s.fd, v));
        let mut s = Submit {
            signal_value: sval,
            vm_fd: self.fd as u32,
            ring_fd: ring.fd as u32,
            n_dwords,
            engine,
            signal_syncobj_fd: sfd,
            pad: 0,
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

    /// Export a VRAM BO as a dma-buf-style scanout handle `{vram_offset, size}`
    /// for the display engine (`/dev/atrium-display0`).
    pub fn export_scanout(&self) -> io::Result<(u64, u64)> {
        let mut e = BoExportScanout { bo_fd: self.fd as u32, ..Default::default() };
        unsafe { call(self.gpu.fd, iowr(G, 27, std::mem::size_of::<BoExportScanout>()), &mut e)? };
        Ok((e.vram_offset, e.size))
    }
}

impl Drop for Bo<'_> { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

// ---------------------------------------------------------------------------
// Scanout — the offset-model "CPU pixels -> onscreen" surface
// ---------------------------------------------------------------------------

/// A presentable framebuffer for the offset-model display (`'D'`).
///
/// CPU-rendered pixels can't be written into the scannable VRAM BO directly
/// (VRAM is GPU-only), so a `Scanout` owns three BOs: a System **staging** BO
/// (CPU-writable via `BO_WRITE`), a VRAM **scanout** BO (the only kind the
/// display can scan), and a small **ring** BO. [`Scanout::update`] copies
/// pixels into staging then issues a CP `DMA_DATA` blit into VRAM; the display
/// is then driven by the exported `{vram_offset, size}` from [`Scanout::export`]
/// — a dma-buf-style handle, never a BO handle.
pub struct Scanout<'a> {
    vm: &'a Vm<'a>,
    staging: Bo<'a>,
    vram: Bo<'a>,
    ring: Bo<'a>,
    vram_offset: u64,
    size: u64,
}

impl<'a> Scanout<'a> {
    /// Allocate the staging/VRAM/ring BOs for a `size`-byte framebuffer and
    /// export the VRAM BO's scanout offset (stable for the surface's life).
    pub fn new(vm: &'a Vm<'a>, size: u64) -> io::Result<Self> {
        let staging = vm.alloc(size, 0)?; // System: CPU-writable
        let vram = vm.alloc(size, BO_VRAM)?; // VRAM: scannable
        let ring = vm.alloc(4096, 0)?;
        let (vram_offset, exported) = vram.export_scanout()?;
        Ok(Self { vm, staging, vram, ring, vram_offset, size: exported })
    }

    /// The `{vram_offset, size}` to hand to [`Display::set_mode`] / [`Display::page_flip`].
    pub fn export(&self) -> (u64, u64) { (self.vram_offset, self.size) }

    /// Upload CPU `pixels` (full framebuffer) into the VRAM scanout BO via a CP
    /// DMA copy. `submit` runs the ring synchronously, so on return VRAM holds
    /// the new frame and a subsequent `page_flip` is safe.
    pub fn update(&self, pixels: &[u8]) -> io::Result<()> {
        self.staging.write(0, pixels)?;
        let src = self.staging.gpu_va();
        let dst = self.vram.gpu_va();
        let words: [u32; 7] = [
            pm4_type3(IT_DMA_DATA, 6),
            0,
            (src & 0xffff_ffff) as u32,
            (src >> 32) as u32,
            (dst & 0xffff_ffff) as u32,
            (dst >> 32) as u32,
            pixels.len() as u32,
        ];
        let mut bytes = Vec::with_capacity(words.len() * 4);
        for w in words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        self.ring.write(0, &bytes)?;
        self.vm.submit(&self.ring, words.len() as u32, ENGINE_GFX, None)
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

pub struct Display { fd: RawFd }

#[derive(Debug, Clone)]
pub struct Connector {
    pub connected: bool,
    pub connector_type: u32,
    pub usbc_lanes: u32,
    pub edid: Vec<u8>,
}

impl Display {
    pub fn open() -> io::Result<Self> {
        Ok(Self { fd: open_rdwr("/dev/atrium-display0")? })
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

    /// Program the mode with `{vram_offset, size}` (from [`Bo::export_scanout`])
    /// as the scanout FB. Returns the DisplayFault code (0 = ok).
    pub fn set_mode(&self, vram_offset: u64, size: u64) -> io::Result<u32> {
        let mut s = DSetMode { vram_offset, size, ..Default::default() };
        unsafe { call(self.fd, iowr(D, 3, std::mem::size_of::<DSetMode>()), &mut s)? };
        Ok(s.fault)
    }

    /// Page-flip to a new scanout FB (vsync = latch at vblank). Fault code out.
    pub fn page_flip(&self, vram_offset: u64, size: u64, vsync: bool) -> io::Result<u32> {
        let mut f = DFlip { vram_offset, size, vsync: vsync as u32, fault: 0 };
        unsafe { call(self.fd, iowr(D, 4, std::mem::size_of::<DFlip>()), &mut f)? };
        Ok(f.fault)
    }

    /// (vblank_count, dropped_flips, tear_line).
    pub fn status(&self) -> io::Result<(u64, u32, u32)> {
        let mut st = DStatus::default();
        unsafe { call(self.fd, ior(D, 5, std::mem::size_of::<DStatus>()), &mut st)? };
        Ok((st.vblank_count, st.dropped_flips, st.tear_line))
    }
}

impl AsRawFd for Display { fn as_raw_fd(&self) -> RawFd { self.fd } }

impl Drop for Display { fn drop(&mut self) { unsafe { libc::close(self.fd) }; } }

/// GPU reset (`_IO('A',16)`) — exposed for harness/recovery paths.
pub fn gpu_reset(gpu: &Gpu) -> io::Result<()> {
    let r = unsafe { ioctl(gpu.fd, io(G, 16) as _, 0 as c_int) };
    if r != 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
