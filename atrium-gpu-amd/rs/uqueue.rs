/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * uqueue.rs — queue fds (GPU ABI v2 §5.5, §5.7; stage 5c-2b,
 * docs/spec/r9700-bare-metal.md §4.5.1).
 *
 * A queue is made on the VM it runs in (QUEUE_CREATE, an ioctl on the VM fd)
 * and is itself an fd: the right to submit. It holds a reference on its VM,
 * so the VM's page tables outlive the queue. Each one is a MES user queue of
 * its own on silicon (si_client.c, mes12.c). On gpusim (no MES user queues;
 * decided 2026-11-13 so both backends share one submit path) a kernel-mode
 * queue fd runs its blobs on the device's compute queue under its VM: the
 * blob is the ring the queue is programmed with (queue.rs), so it runs one
 * submit at a time with the other queue fds of the device, and a fault or
 * hang fails what that queue owes. USER_MODE needs MES: EOPNOTSUPP there.
 *
 * QUEUE_SUBMIT (an ioctl on the queue fd) is kernel-mediated: the kernel puts
 * the client's PM4 (an indirect buffer at a VA of the queue's VM) on the
 * queue's ring, then a fence and an EOP interrupt, and rings the doorbell.
 * The completion it owes is retired by that queue's fence only.
 *
 * The last close takes the queue off the GPU; completions it still owes fail
 * with ECANCELED. If the hardware does not let go of the queue, the VM
 * reference is leaked on purpose: its page tables must never be freed under
 * a queue that may still run.
 *
 * A USER_MODE queue (step 3; root only for now) is rung by its client: the
 * queue fd's mmap is the ring and the writeback page (three RamPages, the
 * third the EOP buffer, which only the CP sees) and the queue's own doorbell
 * page, all through one managed device pager object (UqMap) the queue holds.
 * Its last close first takes every mapping back (the next touch is SIGBUS),
 * then the queue off the GPU; the pages go when the last mapping does, and
 * never while the hardware may still use them (a leaked queue leaks them).
 * A lost queue maps no new page.
 */

use crate::backend::Dev;
use crate::syncobj::SyncObj;
use crate::sys;
use crate::vm::Vm;
use core::ffi::{c_int, c_void};
use core::mem::{ManuallyDrop, size_of};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use kpi::bindings::{self, file, thread, u_long, ucred, vm_map_t, vm_offset_t, vm_ooffset_t, vm_prot_t, vm_size_t};
use kpi::boxed::Box;
use kpi::file::{self as kfile, FileRef};
use kpi::kobj::AsCType;
use kpi::pager::{Backing, MgtDevHandle, RamPages, VmObject};
use kpi::prelude::*;
use kpi::usercopy::UserSlice;
use kpi::ErrCode;

/// The most dwords one INDIRECT_BUFFER packet can name.
const IB_MAX_DW: u64 = 0xfffff;
/// `Queue::slot` of a gpusim queue fd (no backend queue of its own).
const SIM_SLOT: c_int = -1;

/// A user-mode queue's pages (mes12.c's UQ2_PAGES: ring, writeback, EOP), and its mmap
/// layout: ring, writeback page, doorbell page.
const UQ_PAGES: usize = 3;
const PAGE: u64 = bindings::PAGE_SIZE as u64;
const MAP_RING: u64 = 0;
const MAP_WB: u64 = PAGE;
const MAP_DB: u64 = 2 * PAGE;
const MAP_SIZE: u64 = 3 * PAGE;
/// The wptr's place in the writeback page (the rptr report is at its start).
const WB_WPTR: u64 = 8;
/// VM_PROT_READ | VM_PROT_WRITE (casts in C, so bindgen leaves them out).
const PROT_RW: c_int = 0x1 | 0x2;

/// The pager handle of a user-mode queue's mapping.
pub struct UqMap {
    dev: Dev,
    /// The backend slot, once the queue exists (-1 before).
    slot: AtomicI32,
    /// The queue's doorbell page, once the queue exists (0 before).
    db_pa: AtomicU64,
    ram: RamPages,
    /// Set (only) under the object's lock, so a fault sees it before or after, never during.
    revoked: AtomicBool,
}

impl MgtDevHandle for UqMap {
    fn fault(&self, offset: u64, _prot: c_int) -> Option<Backing<'_>> {
        if self.revoked.load(Ordering::Relaxed) {
            return None;
        }
        let slot = self.slot.load(Ordering::Relaxed);
        // A lost queue (or a lost card) maps nothing more. Lock order: object, then sc->lock.
        if slot < 0 || !self.dev.uq_alive(slot) {
            return None;
        }
        match offset {
            MAP_RING => Some(Backing::Ram(&self.ram, 0)),
            MAP_WB => Some(Backing::Ram(&self.ram, 1)),
            MAP_DB => match self.db_pa.load(Ordering::Relaxed) {
                0 => None,
                pa => Some(Backing::Device(pa)),
            },
            _ => None,
        }
    }
}

kpi::mgtdev_pager!(UqMap);

impl Drop for UqMap {
    fn drop(&mut self) {
        // The pager destructor: the last mapping is gone (the pages free after this, as the
        // `ram` field drops). Counted as a grant: module code until then.
        self.dev.grant_count(-1, false);
    }
}

pub struct Queue {
    dev: Dev,
    /// Held while the queue may run; leaked (never dropped) if the hardware kept it.
    vm: ManuallyDrop<FileRef<Vm>>,
    /// The backend's queue (si_client.c's slot), or SIM_SLOT.
    slot: c_int,
    /// gpusim: the engine the queue runs on (ATRIUM_GPU_ENGINE_*).
    engine: u32,
    /// gpusim: submits so far (out_seqno counts from 1).
    sim_seq: AtomicU64,
    /// A user-mode queue's pager object; leaked with the VM if the hardware kept the queue.
    map: ManuallyDrop<Option<VmObject<UqMap>>>,
}

// SAFETY: the backend serialises its queue state (sc->lock, its own sx).
unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

kpi::file_ops!(Queue, |ops| {
    ops.fo_ioctl = Some(queue_ioctl);
    ops.fo_mmap = Some(queue_mmap);
});

impl Drop for Queue {
    fn drop(&mut self) {
        // Every mapping first: the client's next touch is SIGBUS, before the queue goes.
        if let Some(map) = self.map.as_ref() {
            {
                let _g = map.lock();
                map.handle().revoked.store(true, Ordering::Relaxed);
            }
            map.free_pages();
        }
        let gone = if self.slot == SIM_SLOT {
            // gpusim: the VM's teardown (its last reference may be this one) stops what
            // still runs under it, and fails what it owes.
            Ok(())
        } else {
            self.dev.uq_destroy(self.slot)
        };
        match gone {
            // SAFETY: dropped once, here; the queue no longer runs, so neither its VM nor its
            // pages are in use.
            Ok(()) => unsafe {
                ManuallyDrop::drop(&mut self.vm);
                ManuallyDrop::drop(&mut self.map);
            },
            // The VM and the pages stay for good (si_client.c logged why).
            Err(_) => {}
        }
    }
}

fn errno(r: Result<()>) -> c_int {
    match r {
        Ok(()) => 0,
        Err(e) => e.as_c_type(),
    }
}

/// QUEUE_CREATE on `vm_fp`.
fn create(vm_fp: *mut file, a: &mut sys::atrium_gpu_queue_create) -> Result<()> {
    if a.struct_size as usize != size_of::<sys::atrium_gpu_queue_create>() || a._pad != 0 {
        return Err(EINVAL);
    }
    let gfx = a.family_id == sys::ATRIUM_GPU_QUEUE_FAMILY_GFX as u32;
    if a.family_id != sys::ATRIUM_GPU_QUEUE_FAMILY_COMPUTE as u32 && !gfx {
        return Err(EINVAL);
    }
    let user_mode = sys::ATRIUM_GPU_QUEUE_FLAG_USER_MODE as u32;
    if a.flags & !user_mode != 0 || a.priority != 0 {
        return Err(EINVAL); // one class (NORMAL) until the priority mapping lands
    }
    let user = a.flags & user_mode != 0;
    if user {
        // Interim: the ring is the client's, so only a client trusted with the hardware.
        check(unsafe { sys::amd_rs_priv_driver() })?;
    }
    // SAFETY: the kernel holds vm_fp for the duration of its fo_ioctl.
    let vm = unsafe { FileRef::<Vm>::from_file(vm_fp)? };
    let dev = vm.dev();
    let si = unsafe { sys::amd_rs_is_si(dev.raw()) } != 0;
    // GFX queues: gpusim's rasterizer only (kernel-mode, on its GFX queue).
    if gfx && (si || user) {
        return Err(EINVAL);
    }
    let map = if user { Some(user_map(dev)?) } else { None };
    let pages = map.as_ref().map(|m| (m.handle().ram.kva(), m.handle().ram.pa()));
    // On failure `map` drops: nothing mapped it yet, so its pages free.
    // No MES user queues (gpusim): a kernel-mode queue on the device's own queue.
    let slot = if si { dev.uq_create(vm.as_c(), a.priority, pages)? } else if user {
        return Err(EOPNOTSUPP);
    } else {
        SIM_SLOT
    };
    let engine = if gfx { sys::ATRIUM_GPU_ENGINE_GFX } else { sys::ATRIUM_GPU_ENGINE_COMPUTE } as u32;
    if let Some(m) = &map {
        let _g = m.lock();
        m.handle().db_pa.store(dev.uq_doorbell_pa(slot), Ordering::Relaxed);
        m.handle().slot.store(slot, Ordering::Relaxed);
    }
    // From here the Box owns the queue: on any failure its drop takes it off the GPU.
    let q: Box<Queue> =
        Box::new(
            Queue {
                dev,
                vm: ManuallyDrop::new(vm),
                slot,
                engine,
                sim_seq: AtomicU64::new(0),
                map: ManuallyDrop::new(map),
            },
            M_WAITOK,
        );
    let fd = kfile::install(q, (bindings::FREAD | bindings::FWRITE) as c_int)?;
    a.out_queue_fd = fd;
    Ok(())
}

fn check(err: c_int) -> Result<()> {
    match err {
        0 => Ok(()),
        e => Err(ErrCode::from(e)),
    }
}

/// A user-mode queue's pager object, over fresh pages.
fn user_map(dev: Dev) -> Result<VmObject<UqMap>> {
    let ram = RamPages::alloc(UQ_PAGES)?;
    // Counted before the object exists, as UqMap's drop undoes it; a failed allocation drops
    // the handle and so undoes it too.
    dev.grant_count(1, true);
    let h = Box::new(
        UqMap {
            dev,
            slot: AtomicI32::new(-1),
            db_pa: AtomicU64::new(0),
            ram,
            revoked: AtomicBool::new(false),
        },
        M_WAITOK,
    );
    let cred = unsafe { (*kfile::curthread()).td_ucred };
    VmObject::mgtdev(h, MAP_SIZE, PROT_RW, cred)
}

impl Queue {
    /// QUEUE_MAP2: the mmap layout of a user-mode queue.
    fn map_info(&self, a: &mut sys::atrium_gpu_queue_map2) -> Result<()> {
        if a.struct_size as usize != size_of::<sys::atrium_gpu_queue_map2>() || a._reserved != 0 {
            return Err(EINVAL);
        }
        if self.map.is_none() {
            return Err(EINVAL); // a kernel-mode queue: nothing to map
        }
        if self.slot == SIM_SLOT || !self.dev.uq_alive(self.slot) {
            return Err(EIO); // lost (fault, hang): its pages map no more
        }
        a.out_ring_offset = MAP_RING;
        a.out_ring_size = PAGE;
        a.out_doorbell_offset = MAP_DB;
        a.out_doorbell_size = PAGE;
        a.out_wptr_offset = MAP_WB + WB_WPTR;
        a.out_fence_gpu_va = self.dev.uq_fence_va(self.slot);
        Ok(())
    }

    /// QUEUE_SUBMIT.
    fn submit(&self, a: &mut sys::atrium_gpu_queue_submit) -> Result<()> {
        if a.struct_size as usize != size_of::<sys::atrium_gpu_queue_submit>() || a.flags != 0 || a._pad2 != 0 {
            return Err(EINVAL);
        }
        if a.in_dep_count != 0 || a.out_dep_count > 1 {
            return Err(EOPNOTSUPP); // waits and multiple signals come with the scheduler
        }
        if a.blob_size == 0 || a.blob_size % 4 != 0 || a.blob_size / 4 > IB_MAX_DW || a.blob_va & 3 != 0 {
            return Err(EINVAL);
        }
        // The IB's VA is the client's own: anything it names that is not bound faults in its
        // own VM, never another's.
        let out = if a.out_dep_count == 1 {
            let pt: sys::atrium_gpu_syncobj_point =
                UserSlice::new(a.out_dep_ptr, size_of::<sys::atrium_gpu_syncobj_point>()).read_obj()?;
            if pt._pad != 0 {
                return Err(EINVAL);
            }
            let so = FileRef::<SyncObj>::get(pt.syncobj_fd)?;
            // Only this device scrubs its owed lists when the syncobj closes.
            if so.dev().raw() != self.dev.raw() {
                return Err(EXDEV);
            }
            Some((so, pt.value))
        } else {
            None
        };
        let (so, value) = match &out {
            Some((so, v)) => (so.as_c(), *v),
            None => (core::ptr::null_mut(), 0),
        };
        // The backend records the completion before the EOP can retire it (sc->lock), so the
        // syncobj reference can go when this returns: its close scrubs the entry.
        let dw = (a.blob_size / 4) as u32;
        if self.slot == SIM_SLOT {
            // The blob is the ring (gpusim). No client: no user-mode queue owner to match.
            self.dev.submit(
                core::ptr::null_mut(),
                a.blob_va,
                dw,
                self.engine,
                self.vm.as_c(),
                a.deadline_ns,
                so,
                value,
            )?;
            a.out_seqno = self.sim_seq.fetch_add(1, Ordering::Relaxed) + 1;
            return Ok(());
        }
        a.out_seqno = self.dev.uq_submit(self.slot, a.blob_va, dw, so, value)?;
        Ok(())
    }
}

/// `fo_ioctl` of a VM fd: QUEUE_CREATE.
///
/// # Safety
///
/// Called by the kernel only, with a held VM file and the argument copied in.
pub(crate) unsafe extern "C" fn vm_ioctl(
    fp: *mut file,
    com: u_long,
    data: *mut c_void,
    _cred: *mut ucred,
    _td: *mut thread,
) -> c_int {
    match com {
        sys::ATRIUM_RS_IOC_QUEUE_CREATE => {
            // SAFETY: the kernel copied in IOCPARM_LEN(com) bytes, this struct's size (checked
            // at build time in ioctl.rs), into an aligned buffer.
            let a = unsafe { &mut *data.cast::<sys::atrium_gpu_queue_create>() };
            errno(create(fp, a))
        }
        _ => ENOTTY.as_c_type(),
    }
}

/// `fo_ioctl` of a queue fd: QUEUE_SUBMIT.
///
/// # Safety
///
/// Called by the kernel only, with a held queue file and the argument copied in.
unsafe extern "C" fn queue_ioctl(
    fp: *mut file,
    com: u_long,
    data: *mut c_void,
    _cred: *mut ucred,
    _td: *mut thread,
) -> c_int {
    // SAFETY: a held queue file (this table is Queue's only), so f_data is our Queue.
    let q = unsafe { &*(*fp).f_data.cast::<Queue>() };
    match com {
        sys::ATRIUM_RS_IOC_QUEUE_SUBMIT => {
            // SAFETY: as in vm_ioctl.
            let a = unsafe { &mut *data.cast::<sys::atrium_gpu_queue_submit>() };
            errno(q.submit(a))
        }
        sys::ATRIUM_RS_IOC_QUEUE_MAP2 => {
            // SAFETY: as in vm_ioctl.
            let a = unsafe { &mut *data.cast::<sys::atrium_gpu_queue_map2>() };
            errno(q.map_info(a))
        }
        _ => ENOTTY.as_c_type(),
    }
}

/// `fo_mmap` of a queue fd: a user-mode queue's pages (QUEUE_MAP2's layout), shared.
///
/// # Safety
///
/// Called by the kernel only (kern_mmap), with a held queue file.
unsafe extern "C" fn queue_mmap(
    fp: *mut file,
    map: vm_map_t,
    addr: *mut vm_offset_t,
    size: vm_size_t,
    prot: vm_prot_t,
    cap_maxprot: vm_prot_t,
    flags: c_int,
    foff: vm_ooffset_t,
    td: *mut thread,
) -> c_int {
    // SAFETY: a held queue file (this table is Queue's only), so f_data is our Queue.
    let q = unsafe { &*(*fp).f_data.cast::<Queue>() };
    let Some(m) = q.map.as_ref() else {
        return ENODEV.as_c_type();
    };
    let end = (foff as u64).checked_add(size as u64);
    if foff < 0 || size == 0 || end.is_none_or(|e| e > MAP_SIZE) {
        return EINVAL.as_c_type();
    }
    // A reference for the mapping; the glue consumes it whatever happens.
    let obj = m.clone().into_raw();
    // SAFETY: the kernel's arguments, passed on; `obj` is a reference we give up.
    unsafe { sys::amd_rs_mmap_object(map, addr, size, prot, cap_maxprot, flags, obj, foff, td) }
}
