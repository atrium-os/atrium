/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * uqueue.rs — queue fds (GPU ABI v2 §5.5, §5.7; stage 5c-2b,
 * docs/spec/r9700-bare-metal.md §4.5.1).
 *
 * A queue is made on the VM it runs in (QUEUE_CREATE, an ioctl on the VM fd)
 * and is itself an fd: the right to submit. It holds a reference on its VM,
 * so the VM's page tables outlive the queue. Each one is a MES user queue of
 * its own on silicon (si_client.c, mes12.c); a backend without them refuses
 * with EOPNOTSUPP.
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
 */

use crate::backend::Dev;
use crate::syncobj::SyncObj;
use crate::sys;
use crate::vm::Vm;
use core::ffi::{c_int, c_void};
use core::mem::{ManuallyDrop, size_of};
use kpi::bindings::{self, file, thread, u_long, ucred};
use kpi::boxed::Box;
use kpi::file::{self as kfile, FileRef};
use kpi::kobj::AsCType;
use kpi::prelude::*;
use kpi::usercopy::UserSlice;

/// The most dwords one INDIRECT_BUFFER packet can name.
const IB_MAX_DW: u64 = 0xfffff;

pub struct Queue {
    dev: Dev,
    /// Held while the queue may run; leaked (never dropped) if the hardware kept it.
    vm: ManuallyDrop<FileRef<Vm>>,
    /// The backend's queue (si_client.c's slot).
    slot: c_int,
}

// SAFETY: the backend serialises its queue state (sc->lock, its own sx).
unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

kpi::file_ops!(Queue, |ops| {
    ops.fo_ioctl = Some(queue_ioctl);
});

impl Drop for Queue {
    fn drop(&mut self) {
        match self.dev.uq_destroy(self.slot) {
            // SAFETY: dropped once, here; the queue no longer runs.
            Ok(()) => unsafe { ManuallyDrop::drop(&mut self.vm) },
            // The VM stays referenced for good (si_client.c logged why).
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
    if a.family_id != sys::ATRIUM_GPU_QUEUE_FAMILY_COMPUTE as u32 {
        return Err(EINVAL);
    }
    if a.flags & sys::ATRIUM_GPU_QUEUE_FLAG_USER_MODE as u32 != 0 {
        return Err(EOPNOTSUPP); // 5c-2b step 3
    }
    if a.flags != 0 || a.priority != 0 {
        return Err(EINVAL); // one class (NORMAL) until the priority mapping lands
    }
    // SAFETY: the kernel holds vm_fp for the duration of its fo_ioctl.
    let vm = unsafe { FileRef::<Vm>::from_file(vm_fp)? };
    let dev = vm.dev();
    let slot = dev.uq_create(vm.as_c(), a.priority)?;
    // From here the Box owns the queue: on any failure its drop takes it off the GPU.
    let q: Box<Queue> = Box::new(Queue { dev, vm: ManuallyDrop::new(vm), slot }, M_WAITOK);
    let fd = kfile::install(q, (bindings::FREAD | bindings::FWRITE) as c_int)?;
    a.out_queue_fd = fd;
    Ok(())
}

impl Queue {
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
        a.out_seqno = self.dev.uq_submit(self.slot, a.blob_va, (a.blob_size / 4) as u32, so, value)?;
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
        _ => ENOTTY.as_c_type(),
    }
}
