/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * ioctl.rs — the /dev/atrium-gpuN client node and the /dev/atrium-gpuNctl
 * control node. Was ioctl.c.
 *
 * The user-space ABI is atrium_gpu_amd_abi.h: create VMs, BOs and syncobjs
 * (each an fd), bind BOs into VMs, copy data in and out of System BOs, submit
 * a PM4 ring, map a queue's doorbell. Everything device-specific goes through
 * the backend vtable (sc->backend); this file is transport-neutral.
 *
 * Every argument comes from an unprivileged, possibly hostile client, so each
 * is read as a value with its size checked against the ioctl number, every fd
 * is resolved to a typed, held reference (released on every path by drop),
 * and every length or offset is checked before it touches memory.
 */

use crate::backend::Dev;
use crate::bo::{self, BoFile};
use crate::syncobj::{self, SyncObj};
use crate::sys;
use crate::vm::{self, Vm};
use core::ffi::{CStr, c_int};
use kpi::bindings::{self, caddr_t, cdev, thread, u_long, vm_object_t, vm_ooffset_t, vm_size_t};
use kpi::boxed::Box;
use kpi::cdev::{CdevPriv, set_cdevpriv, with_cdevpriv};
use kpi::file::FileRef;
use kpi::kobj::AsCType;
use kpi::prelude::*;
use kpi::usercopy::{AsBytes, FromBytes, IoctlData, UserSlice};
use kpi::ErrCode;

/// The ioctl argument structs: plain integers with no padding, so any bytes are a valid
/// value and a copy back out leaks nothing. The sizes are the ABI's, checked at build time.
macro_rules! abi_structs {
    ($($t:ident = $size:expr;)*) => {
        $(
            const _: () = assert!(core::mem::size_of::<sys::$t>() == $size);
            // SAFETY: integer fields only, and the size assert above shows no padding (each
            // struct's fields add up to $size).
            unsafe impl FromBytes for sys::$t {}
            unsafe impl AsBytes for sys::$t {}
        )*
    };
}
abi_structs! {
    atrium_gpu_bo_alloc = 16;
    atrium_gpu_bo_xfer = 32;
    atrium_gpu_submit = 32;
    atrium_gpu_irqs = 16;
    atrium_gpu_syncobj_create = 8;
    atrium_gpu_syncobj_op = 16;
    atrium_gpu_syncobj_wait = 16;
    atrium_gpu_vm_create = 8;
    atrium_gpu_vm_bind = 16;
    atrium_gpu_vm_bind2 = 24;
    atrium_gpu_queue_map = 32;
    atrium_gpu_caps_query = 16;
    atrium_gpu_sched = 40;
    atrium_gpu_powergate = 24;
}

/// One open of the client node. Queue ownership and doorbell grants (queue.c) are keyed by
/// its address, and are given back when its last fd closes.
pub struct Client {
    dev: Dev,
}

// SAFETY: the client has no state of its own.
unsafe impl Send for Client {}
unsafe impl Sync for Client {}

// SAFETY: amd_rs_open is the only d_open of the client node and sets a Client.
unsafe impl CdevPriv for Client {}

impl Client {
    fn as_c(&self) -> *mut sys::atrium_amd_client {
        core::ptr::from_ref(self).cast_mut().cast()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Give back any queue this client owns and revoke its doorbell mappings.
        self.dev.client_close(self.as_c());
    }
}

fn errno(r: Result<()>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(e) => e.as_c_type(),
    }
}

/// An fd field from the ABI: u32 on the wire, an int to the kernel.
fn fd(v: u32) -> c_int {
    v as c_int
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_open(
    dev: *mut cdev,
    _oflags: c_int,
    _devtype: c_int,
    _td: *mut thread,
) -> c_int {
    let dev = unsafe { Dev::from_cdev(dev) };
    errno(set_cdevpriv(Box::new(Client { dev }, M_WAITOK)))
}

/// mmap of the client node: only a doorbell page of a queue this client owns, as a revocable
/// managed pager object (queue.c). Anything else is refused there.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_mmap_single(
    dev: *mut cdev,
    offset: *mut vm_ooffset_t,
    size: vm_size_t,
    object: *mut vm_object_t,
    nprot: c_int,
) -> c_int {
    let dev = unsafe { Dev::from_cdev(dev) };
    match with_cdevpriv(|c: &Client| unsafe { dev.mmap_single(c.as_c(), offset, size, object, nprot) }) {
        Ok(e) => e,
        Err(e) => e.as_c_type(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_ioctl(
    dev: *mut cdev,
    cmd: u_long,
    data: caddr_t,
    _fflag: c_int,
    _td: *mut thread,
) -> c_int {
    let dev = unsafe { Dev::from_cdev(dev) };
    // SAFETY: the kernel's argument buffer for this command.
    let mut data = unsafe { IoctlData::from_raw(cmd, data) };
    match with_cdevpriv(|c: &Client| client_ioctl(dev, c, cmd, &mut data)) {
        Ok(e) => e,
        Err(e) => e.as_c_type(),
    }
}

/// Read the argument, run `f` on it, write it back (for IOC_OUT commands the kernel then
/// copies it out). `f` returns an errno so a raw ERESTART can pass through.
fn with_arg<T: FromBytes + AsBytes>(data: &mut IoctlData<'_>, f: impl FnOnce(&mut T) -> c_int) -> c_int {
    let mut a = match data.read::<T>() {
        Ok(a) => a,
        Err(e) => return e.as_c_type(),
    };
    let e = f(&mut a);
    if e == 0 {
        if let Err(e) = data.write(&a) {
            return e.as_c_type();
        }
    }
    e
}

fn client_ioctl(dev: Dev, client: &Client, cmd: u_long, data: &mut IoctlData<'_>) -> c_int {
    match cmd {
        sys::ATRIUM_RS_IOC_VM_CREATE => with_arg(data, |a: &mut sys::atrium_gpu_vm_create| {
            errno(vm::create_fd(dev).map(|fd| a.out_fd = fd as u32))
        }),

        sys::ATRIUM_RS_IOC_BO_ALLOC => with_arg(data, |a: &mut sys::atrium_gpu_bo_alloc| {
            errno(bo::create_fd(dev, a.size, a.flags).map(|fd| a.bo_fd = fd as u32))
        }),

        sys::ATRIUM_RS_IOC_VM_BIND => with_arg(data, |a: &mut sys::atrium_gpu_vm_bind| {
            errno(vm_bind(a.vm_fd, a.bo_fd, a.va, 0).map(|va| a.va = va))
        }),

        sys::ATRIUM_RS_IOC_VM_BIND2 => with_arg(data, |a: &mut sys::atrium_gpu_vm_bind2| {
            if a.pad != 0 {
                return EINVAL.as_c_type();
            }
            errno(vm_bind(a.vm_fd, a.bo_fd, a.va, a.flags).map(|va| a.va = va))
        }),

        sys::ATRIUM_RS_IOC_QUEUE_MAP => with_arg(data, |a: &mut sys::atrium_gpu_queue_map| {
            errno(queue_map(dev, client, a))
        }),

        sys::ATRIUM_RS_IOC_BO_WRITE => with_arg(data, |a: &mut sys::atrium_gpu_bo_xfer| {
            errno(bo_xfer(a, true))
        }),

        sys::ATRIUM_RS_IOC_BO_READ => with_arg(data, |a: &mut sys::atrium_gpu_bo_xfer| {
            errno(bo_xfer(a, false))
        }),

        sys::ATRIUM_RS_IOC_SYNCOBJ_CREATE => {
            with_arg(data, |a: &mut sys::atrium_gpu_syncobj_create| {
                errno(syncobj::create_fd(dev).map(|fd| a.out_fd = fd as u32))
            })
        }

        sys::ATRIUM_RS_IOC_SYNCOBJ_SIGNAL => with_arg(data, |a: &mut sys::atrium_gpu_syncobj_op| {
            errno(FileRef::<SyncObj>::get(fd(a.syncobj_fd)).map(|so| so.signal(a.value)))
        }),

        sys::ATRIUM_RS_IOC_SYNCOBJ_QUERY => with_arg(data, |a: &mut sys::atrium_gpu_syncobj_op| {
            errno(FileRef::<SyncObj>::get(fd(a.syncobj_fd)).map(|so| a.value = so.value()))
        }),

        sys::ATRIUM_RS_IOC_SYNCOBJ_WAIT => with_arg(data, |a: &mut sys::atrium_gpu_syncobj_wait| {
            match FileRef::<SyncObj>::get(fd(a.syncobj_fd)) {
                Ok(so) => so.wait(a.value, a.timeout_ms),
                Err(e) => e.as_c_type(),
            }
        }),

        sys::ATRIUM_RS_IOC_GET_IRQS => with_arg(data, |a: &mut sys::atrium_gpu_irqs| {
            (a.count, a.msix_enabled) = dev.irqs();
            0
        }),

        sys::ATRIUM_RS_IOC_SUBMIT => with_arg(data, |a: &mut sys::atrium_gpu_submit| {
            errno(submit(dev, client, a))
        }),

        sys::ATRIUM_RS_IOC_QUERY_CAPS => with_arg(data, |a: &mut sys::atrium_gpu_caps_query| {
            errno(query_caps(dev, a))
        }),

        // Device-wide authority lives on the control node only (isolation break 5).
        sys::ATRIUM_RS_IOC_GPU_RESET | sys::ATRIUM_RS_IOC_SCHED | sys::ATRIUM_RS_IOC_POWERGATE => {
            EPERM.as_c_type()
        }

        // Display ioctls are /dev/atrium-display0's (atrium_gpu_amd_display.ko).
        _ => ENOTTY.as_c_type(),
    }
}

fn vm_bind(vm_fd: u32, bo_fd: u32, va: u64, flags: u32) -> Result<u64> {
    let vm = FileRef::<Vm>::get(fd(vm_fd))?;
    let bo = FileRef::<BoFile>::get(fd(bo_fd))?;
    bo.bind(&vm, va, flags)
}

fn queue_map(dev: Dev, client: &Client, a: &mut sys::atrium_gpu_queue_map) -> Result<()> {
    let vm = FileRef::<Vm>::get(fd(a.vm_fd))?;
    let ring = FileRef::<BoFile>::get(fd(a.ring_fd))?;
    // The ring must be bound in THIS vm, for a GPU-VA.
    let ring_va = ring.gpu_va(&vm).ok_or(EINVAL)?;
    // Exclusive: EBUSY if another client owns the queue; EOPNOTSUPP without user queues.
    let doorbell_off = dev.queue_map(client.as_c(), vm.as_c(), ring_va, a.engine)?;
    // doorbell_off is page-aligned (one page per queue), so it IS the mmap offset of this
    // queue's own doorbell page; the doorbell word is at its start. Only this client can
    // mmap it, and only while it owns the queue.
    a.doorbell_mmap_offset = doorbell_off as u64;
    a.doorbell_size = bindings::PAGE_SIZE as u64;
    a.doorbell_word_offset = 0;
    Ok(())
}

/// BO_WRITE / BO_READ: copy between user memory and a System BO's CPU mapping.
fn bo_xfer(a: &sys::atrium_gpu_bo_xfer, write: bool) -> Result<()> {
    let bo = FileRef::<BoFile>::get(fd(a.bo_fd))?;
    let kva = bo.kva().ok_or(EINVAL)?; // VRAM BO: GPU-only, no CPU copy path
    let end = a.offset.checked_add(a.len).ok_or(EINVAL)?;
    if end > bo.size() {
        return Err(EINVAL);
    }
    let len = usize::try_from(a.len).map_err(|_| EINVAL)?;
    let user = UserSlice::new(a.user_ptr, len);
    // SAFETY: [offset, offset + len) lies inside the BO's CPU mapping (checked above), which
    // stays mapped while we hold the file. The GPU may access the same memory concurrently;
    // that is the ABI (user space orders its copies against its submits with syncobjs), and
    // it is why this is a raw copy rather than a Rust slice.
    unsafe {
        let p = kva.as_ptr().add(a.offset as usize);
        if write { user.copy_in_raw(p) } else { user.copy_out_raw(p) }
    }
}

fn submit(dev: Dev, client: &Client, a: &sys::atrium_gpu_submit) -> Result<()> {
    let vm = FileRef::<Vm>::get(fd(a.vm_fd))?;
    let ring = FileRef::<BoFile>::get(fd(a.ring_fd))?;
    // The ring must fit the BO (each dword is 4 bytes).
    if a.n_dwords as u64 * 4 > ring.size() {
        return Err(EINVAL);
    }
    let so = if a.signal_syncobj_fd >= 0 {
        Some(FileRef::<SyncObj>::get(a.signal_syncobj_fd)?)
    } else {
        None
    };
    let so_ptr = so.as_ref().map_or(core::ptr::null_mut(), |s| s.as_c());
    // The backend stages the frame deadline, programs the queue, registers the completion on
    // that queue's list BEFORE ringing the doorbell (the ISR signals it on the end-of-pipe
    // interrupt, which for a ring parked on a cross-queue WAIT comes on a later doorbell), and
    // rings it, all under the queue's lock.
    dev.submit(client.as_c(), ring.as_c(), a.n_dwords, a.engine, vm.as_c(), a.deadline_ns, so_ptr, a.signal_value)
}

/// A TLV writer over a fixed buffer: every record is bounds-checked.
struct Caps {
    buf: [u8; 256],
    off: usize,
}

impl Caps {
    /// Append a record (header + data), padded to 4 bytes.
    fn put(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let size = u32::try_from(data.len()).map_err(|_| EINVAL)?;
        let end = self.off + 8 + data.len();
        let padded = (end + 3) & !3;
        let rec = self.buf.get_mut(self.off..padded).ok_or(ENOMEM)?;
        rec[0..4].copy_from_slice(&id.to_ne_bytes());
        rec[4..8].copy_from_slice(&size.to_ne_bytes());
        rec[8..8 + data.len()].copy_from_slice(data);
        rec[8 + data.len()..].fill(0);
        self.off = padded;
        Ok(())
    }

    fn put_obj<T: AsBytes>(&mut self, id: u32, v: &T) -> Result<()> {
        // SAFETY: T: AsBytes has no uninitialised bytes.
        let b = unsafe { core::slice::from_raw_parts(core::ptr::from_ref(v).cast::<u8>(), core::mem::size_of::<T>()) };
        self.put(id, b)
    }
}

/// The capability records the ABI sends. Sizes as in atrium_gpu_amd_abi.h.
#[repr(C)]
struct CapIsa {
    family: u32,
    revision: u32,
    name: [u8; 24],
}
#[repr(C)]
struct CapAddressSpace {
    va_base: u64,
    va_size: u64,
    va_align: u64,
}
#[repr(C)]
struct CapHeap {
    kind: u32,
    flags: u32,
    size: u64,
}
const _: () = assert!(core::mem::size_of::<CapIsa>() == core::mem::size_of::<sys::atrium_gpu_isa_info>());
const _: () = assert!(core::mem::size_of::<CapAddressSpace>() == core::mem::size_of::<sys::atrium_gpu_cap_address_space>());
const _: () = assert!(core::mem::size_of::<CapHeap>() == core::mem::size_of::<sys::atrium_gpu_heap_info>());
// SAFETY: integer fields, sizes above show no padding.
unsafe impl AsBytes for CapIsa {}
unsafe impl AsBytes for CapAddressSpace {}
unsafe impl AsBytes for CapHeap {}

fn query_caps(dev: Dev, q: &mut sys::atrium_gpu_caps_query) -> Result<()> {
    // The backend supplies the device-specific values; the front-end assembles the
    // (forward-compatible) TLV.
    let bc = dev.caps();
    let mut c = Caps { buf: [0; 256], off: 0 };

    let ver: [u32; 2] = [1, 0]; // ABI major.minor (the front-end's)
    c.put_obj(sys::ATRIUM_GPU_CAP_ABI_VERSION as u32, &ver)?;
    let vendor = if bc.vendor.is_null() { c"" } else { unsafe { CStr::from_ptr(bc.vendor) } };
    c.put(sys::ATRIUM_GPU_CAP_VENDOR as u32, vendor.to_bytes_with_nul())?;
    c.put_obj(sys::ATRIUM_GPU_CAP_FEATURES_V1 as u32, &bc.features)?;
    c.put_obj(
        sys::ATRIUM_GPU_CAP_ADDRESS_SPACE as u32,
        &CapAddressSpace { va_base: bc.va_base, va_size: bc.va_size, va_align: bc.va_align },
    )?;
    let heaps = [
        CapHeap { kind: sys::ATRIUM_GPU_HEAP_DEVICE as u32, flags: 0, size: bc.vram_bytes },
        CapHeap { kind: sys::ATRIUM_GPU_HEAP_SYSTEM as u32, flags: 0, size: 0 },
    ];
    c.put_obj(sys::ATRIUM_GPU_CAP_HEAPS as u32, &heaps)?;
    c.put_obj(sys::ATRIUM_GPU_CAP_BIND_FEATURES as u32, &bc.bind_features)?;
    let mut isa = CapIsa { family: bc.isa_family, revision: bc.isa_revision, name: [0; 24] };
    if !bc.isa_name.is_null() {
        // strlcpy: at most 23 bytes and a NUL.
        let n = unsafe { CStr::from_ptr(bc.isa_name) }.to_bytes();
        let k = n.len().min(isa.name.len() - 1);
        isa.name[..k].copy_from_slice(&n[..k]);
    }
    c.put_obj(sys::ATRIUM_GPU_CAP_ISA as u32, &isa)?;

    if q.caps_size < c.off as u64 {
        q.caps_size = c.off as u64; // tell user space the size it needs
        return Err(ENOMEM);
    }
    UserSlice::new(q.caps_ptr, c.off).copy_out(&c.buf[..c.off])?;
    q.caps_size = c.off as u64;
    Ok(())
}

/// The control node: reset, scheduler and power gating affect every client, so they are
/// here, root-only and hidden from every jail, not on the client node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_rs_ctl_ioctl(
    dev: *mut cdev,
    cmd: u_long,
    data: caddr_t,
    _fflag: c_int,
    _td: *mut thread,
) -> c_int {
    let dev = unsafe { Dev::from_cdev(dev) };
    // SAFETY: the kernel's argument buffer for this command.
    let mut data = unsafe { IoctlData::from_raw(cmd, data) };
    match cmd {
        // Recover a wedged engine: the reset's prepare hook stops every queue, which fails
        // the completions they owed (waiters get EIO, not a timeout); GPUVM page tables
        // survive, so open VMs and BOs stay valid.
        sys::ATRIUM_RS_IOC_GPU_RESET => errno(dev.gpu_reset()),
        sys::ATRIUM_RS_IOC_SCHED => with_arg(&mut data, |a: &mut sys::atrium_gpu_sched| errno(dev.sched(a))),
        sys::ATRIUM_RS_IOC_POWERGATE => {
            with_arg(&mut data, |a: &mut sys::atrium_gpu_powergate| errno(dev.powergate(a)))
        }
        _ => ENOTTY.as_c_type(),
    }
}
