//! Mirror of `atrium-kmod/atrium_gpu.h` — `repr(C)` structs and ioctl
//! numbers for the Atrium GPU ABI v0.1.0.
//!
//! This module is the only place that should know the wire layout.
//! Everything else in the crate goes through the safe wrappers in
//! [`crate::gpu`] and [`crate::display`].

#![allow(non_camel_case_types, dead_code)]

use libc::{c_char, c_int};

pub const ATRIUM_GPU_BO_GPU_VISIBLE:    u32 = 0x01;
pub const ATRIUM_GPU_BO_CPU_VISIBLE:    u32 = 0x02;
pub const ATRIUM_GPU_BO_COHERENT:       u32 = 0x04;
pub const ATRIUM_GPU_BO_SCANOUT:        u32 = 0x08;
pub const ATRIUM_GPU_BO_COMPUTE_INPUT:  u32 = 0x10;
pub const ATRIUM_GPU_BO_COMPUTE_OUTPUT: u32 = 0x20;
pub const ATRIUM_GPU_BO_RT_AS:          u32 = 0x40;

pub const FRESCO_ENGINE_GRAPHICS: u32 = 0;

pub const FRESCO_CONNECTOR_VIRTUAL:        u16 = 5;
pub const FRESCO_CONNECTOR_FLAG_CONNECTED: u16 = 0x01;

pub const FRESCO_MODE_FLAG_PREFERRED: u16 = 0x0040;

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_gpu_alloc {
    pub size: u64,
    pub flags: u32,
    pub alignment: u32,
    pub handle: u32,
    pub _pad0: u32,
    pub mmap_offset: u64,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_gpu_submit {
    pub cmd_handle: u32,
    pub cmd_offset: u64,
    pub cmd_size: u64,
    pub bo_count: u32,
    pub bo_handles_ptr: u64,
    pub wait_fence_count: u32,
    pub wait_fences_ptr: u64,
    pub fence_out: u64,
    pub engine: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_gpu_fence_wait {
    pub fence: u64,
    pub timeout_ns: i64,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_gpu_fence_query {
    pub engine: u32,
    pub _pad0: u32,
    pub latest_retired: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct atrium_gpu_caps {
    pub version_major: u32,
    pub version_minor: u32,
    pub vendor_id: u32,
    pub device_id: u32,
    pub family: [c_char; 64],
    pub vram_total_bytes: u64,
    pub system_memory_visible_bytes: u64,
    pub max_texture_2d: u32,
    pub max_texture_3d: u32,
    pub max_buffer_size_log2: u32,
    pub engine_mask: u32,
    pub feature_flags: u32,
    pub _pad0: u32,
    pub reserved: [u64; 8],
}
impl Default for atrium_gpu_caps {
    fn default() -> Self {
        // SAFETY: zeroing a `repr(C)` struct of POD fields is well-defined.
        unsafe { std::mem::zeroed() }
    }
}

/// Backend descriptor surfaced by `IOC_GPU_LIST_BACKENDS`. Matches
/// `struct atrium_gpu_backend_info` in `atrium_gpu.h`. Today the kmod
/// reports exactly one entry (`atrium-gpu-v1` over virtio-gpu); the
/// ABI is shaped for the multi-backend future.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct atrium_gpu_backend_info {
    pub vendor_id:     u32,
    pub generation_id: u32,
    pub name:          [c_char; 32],
    pub feature_flags: u32,
    pub _pad0:         u32,
    pub _reserved:     [u64; 4],
}
impl Default for atrium_gpu_backend_info {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

/// Argument struct for `IOC_GPU_LIST_BACKENDS`. Two-phase call:
/// invoke once with `count_in=0` to learn `count_out`, then again
/// with a buffer of that capacity.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_gpu_list_backends {
    pub count_in:     u32,
    pub count_out:    u32,
    pub backends_ptr: u64,
}

/// Known backend (`vendor_id`, `generation_id`) pairs.
pub const ATRIUM_GPU_BACKEND_V1_VENDOR: u32 = 0xA710;
pub const ATRIUM_GPU_BACKEND_V1_GEN:    u32 = 1;

/// Cross-process region-sharing token length. See atrium_gpu.h
/// §"Cross-process region sharing" for the full design.
pub const ATRIUM_GPU_TOKEN_LEN: usize = 32;

/// Mint an unguessable token for a BO this fd owns.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct atrium_gpu_mint_token {
    pub handle: u32,
    pub _pad0:  u32,
    pub token:  [u8; ATRIUM_GPU_TOKEN_LEN],
}
impl Default for atrium_gpu_mint_token {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}

/// Resolve a token to a BO and register it with this fd.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct atrium_gpu_import_region {
    pub token:       [u8; ATRIUM_GPU_TOKEN_LEN],
    pub handle:      u32,
    pub _pad0:       u32,
    pub size:        u64,
    pub mmap_offset: u64,
    pub flags:       u32,
    pub _pad1:       u32,
    pub _reserved:   [u64; 4],
}
impl Default for atrium_gpu_import_region {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_bind_gpu {
    pub gpu_fd: c_int,
    pub _pad0: c_int,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_connector {
    pub id: u32,
    pub r#type: u16,
    pub flags: u16,
    pub edid_size: u32,
    pub _pad0: u32,
    pub edid_ptr: u64,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_enum {
    pub count_in: u32,
    pub count_out: u32,
    pub connectors_ptr: u64,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_mode {
    pub width: u32,
    pub height: u32,
    pub pixel_clock_khz: u32,
    pub refresh_mhz: u32,
    pub h_sync_start: u16,
    pub h_sync_end: u16,
    pub h_total: u16,
    pub h_skew: u16,
    pub v_sync_start: u16,
    pub v_sync_end: u16,
    pub v_total: u16,
    pub v_scan: u16,
    pub flags: u16,
    pub _pad0: u16,
    pub _reserved: [u64; 2],
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_modes_query {
    pub connector_id: u32,
    pub count_in: u32,
    pub count_out: u32,
    pub _pad0: u32,
    pub modes_ptr: u64,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_set_mode {
    pub connector_id: u32,
    pub scanout_handle: u32,
    pub mode: atrium_display_mode,
}

#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_page_flip {
    pub connector_id: u32,
    pub scanout_handle: u32,
    pub wait_fence: u64,
    pub flip_id: u64,
    pub flags: u32,
    pub _pad0: u32,
}

/// `flags` bit: defer the actual `SET_SCANOUT_BLOB` / `RESOURCE_FLUSH`
/// to the next vblank tick on `connector_id`. The ioctl validates the
/// BO (and auto-promotes it if necessary) then returns immediately;
/// the kmod's taskqueue worker performs the real virtio commands at
/// panel-refresh boundary. See `docs/spec/aqueduct-gpu.md` §6.5.5.c.
///
/// Single-deep queue: a second queued flip arriving before the first
/// fires replaces it (newer frame wins). `flip_id` is the caller's
/// monotonic frame ID so coalesced frames can be detected upstream.
pub const ATRIUM_PAGE_FLIP_QUEUE_VBLANK: u32 = 0x04;

/// `IOC_DISPLAY_WAIT_VBLANK` — block until the next vblank tick for
/// `connector_id`, then return the post-wait sequence counter.
///
/// Today the kmod emulates vblank with a `callout(9)` firing at the
/// connector's mode refresh interval (see `atrium-virtio-gpu.c` /
/// `atrium_display_vblank_tick`). On D5+ native hardware the
/// callout source is replaced by a real GPU IRQ; the userspace ABI
/// does not change.
///
/// Caller pattern (frescod-style):
///
/// ```rust
/// loop {
///     dpy.wait_vblank(connector_id)?;
///     // render + page_flip ...
/// }
/// ```
///
/// `seq` is post-wait so callers can detect dropped vblanks across
/// long render runs: `seq[N] - seq[N-1] > 1` ⇒ missed vblanks.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct atrium_display_wait_vblank {
    pub connector_id: u32,
    pub _pad0: u32,
    /// Out: sequence count after the wait returns.
    pub seq: u64,
}

// ioctl numbers — matches `_IO[WR]+('G', n, ...)` and ('D', n, ...) macros
// from atrium_gpu.h. FreeBSD's _IOC encoding:
//   bits 28..29: dir (1=void, 2=out, 3=in, 4=inout — see <sys/ioccom.h>)
//   bits 16..27: size of arg
//   bits  8..15: group
//   bits  0..7:  cmd number
const IOC_VOID:  u32 = 0x2000_0000;
const IOC_OUT:   u32 = 0x4000_0000;
const IOC_IN:    u32 = 0x8000_0000;
const IOC_INOUT: u32 = IOC_IN | IOC_OUT;

const fn ioc(dir: u32, group: u32, num: u32, size: u32) -> u64 {
    (dir | ((size & 0x1fff) << 16) | (group << 8) | num) as u64
}
const fn iow(group: u32, num: u32, size: u32) -> u64  { ioc(IOC_IN, group, num, size) }
const fn ior(group: u32, num: u32, size: u32) -> u64  { ioc(IOC_OUT, group, num, size) }
const fn iowr(group: u32, num: u32, size: u32) -> u64 { ioc(IOC_INOUT, group, num, size) }

const G: u32 = b'G' as u32;
const D: u32 = b'D' as u32;

pub const ATRIUM_GPU_IOC_ALLOC: u64 = iowr(G, 1, std::mem::size_of::<atrium_gpu_alloc>() as u32);
pub const ATRIUM_GPU_IOC_FREE:  u64 = iow (G, 2, std::mem::size_of::<u32>()                as u32);
pub const ATRIUM_GPU_IOC_SYNC:  u64 = iow (G, 3, 24); // size of atrium_gpu_sync; not used yet
pub const ATRIUM_GPU_IOC_SUBMIT:       u64 = iowr(G, 4, std::mem::size_of::<atrium_gpu_submit>()       as u32);
pub const ATRIUM_GPU_IOC_FENCE_WAIT:   u64 = iow (G, 5, std::mem::size_of::<atrium_gpu_fence_wait>()   as u32);
pub const ATRIUM_GPU_IOC_FENCE_QUERY:  u64 = iowr(G, 6, std::mem::size_of::<atrium_gpu_fence_query>()  as u32);
pub const ATRIUM_GPU_IOC_CAPS:         u64 = ior (G, 7, std::mem::size_of::<atrium_gpu_caps>()         as u32);
pub const ATRIUM_GPU_IOC_LIST_BACKENDS:u64 = iowr(G, 0x46, std::mem::size_of::<atrium_gpu_list_backends>() as u32);
pub const ATRIUM_GPU_IOC_MINT_TOKEN:   u64 = iowr(G, 0x47, std::mem::size_of::<atrium_gpu_mint_token>()    as u32);
pub const ATRIUM_GPU_IOC_IMPORT_REGION:u64 = iowr(G, 0x48, std::mem::size_of::<atrium_gpu_import_region>() as u32);

pub const ATRIUM_DISPLAY_IOC_BIND_GPU:        u64 = iow (D, 0, std::mem::size_of::<atrium_display_bind_gpu>()        as u32);
pub const ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS: u64 = iowr(D, 1, std::mem::size_of::<atrium_display_enum>()            as u32);
pub const ATRIUM_DISPLAY_IOC_MODES:           u64 = iowr(D, 2, std::mem::size_of::<atrium_display_modes_query>()     as u32);
pub const ATRIUM_DISPLAY_IOC_SET_MODE:        u64 = iow (D, 3, std::mem::size_of::<atrium_display_set_mode>()        as u32);
pub const ATRIUM_DISPLAY_IOC_PAGE_FLIP:       u64 = iow (D, 4, std::mem::size_of::<atrium_display_page_flip>()       as u32);
pub const ATRIUM_DISPLAY_IOC_WAIT_VBLANK:     u64 = iowr(D, 5, std::mem::size_of::<atrium_display_wait_vblank>()     as u32);
