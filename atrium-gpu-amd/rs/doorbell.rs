/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * doorbell.rs — doorbell mapping grants (isolation break 3, queue.c). Was the
 * grant pager in queue.c.
 *
 * A queue's owner maps its doorbell page through d_mmap_single. Each grant is a
 * managed device-pager object (kpi::pager) whose handle is a `Grant`, so the
 * kernel can take the page back out of every process that mapped it: on
 * revoke, faults fail (SIGBUS) and the page is removed from every mapping.
 *
 * Queue ownership and its lock stay in queue.c, which decides who may map
 * which doorbell and when to revoke. queue.c holds one object reference per
 * live grant (hwq->grant_obj) and hands out further references to mmap.
 * What lives here is the part user space reaches by touching the mapping:
 * the fault handler, and the grant's lifetime. The pager only ever inserts a
 * fictitious (device) page that no other object holds, whatever the grant
 * answers.
 *
 * Lock order (queue.c): hwq->lock, then the grant object's lock, then sc->lock.
 */

use crate::backend::Dev;
use crate::sys;
use core::ffi::c_int;
use core::sync::atomic::{AtomicBool, Ordering};
use kpi::bindings::{PAGE_SIZE, vm_object_t};
use kpi::boxed::Box;
use kpi::kobj::AsCType;
use kpi::pager::{MgtDevHandle, VmObject};
use kpi::prelude::*;

/// The pager handle of one owner's doorbell mapping.
pub struct Grant {
    dev: Dev,
    /// The doorbell page.
    pa: u64,
    /// Set (only) under the object's lock, so a fault sees it before or after, never during.
    revoked: AtomicBool,
}

impl MgtDevHandle for Grant {
    fn fault(&self, offset: u64, _prot: c_int) -> Option<u64> {
        // One page; a revoked grant faults (SIGBUS) so the client knows.
        if self.revoked.load(Ordering::Relaxed) || offset != 0 {
            None
        } else {
            Some(self.pa)
        }
    }
}

kpi::mgtdev_pager!(Grant);

impl Drop for Grant {
    fn drop(&mut self) {
        // The pager destructor: the last mapping of a grant is gone.
        self.dev.grant_count(-1, false);
    }
}

/// queue.c's references to grant objects.
type GrantObj = VmObject<Grant>;

/// A new grant for the doorbell page at `pa`; `*out` gets a reference to its object.
///
/// # Safety
///
/// `sc` is a live softc, `out` is writable, and the caller holds the queue's lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_db_grant_create(
    sc: *mut sys::atrium_amd_softc,
    pa: u64,
    nprot: c_int,
    out: *mut vm_object_t,
) -> c_int {
    let dev = unsafe { Dev::from_raw(sc) };
    // Counted before the object exists, as the Drop above undoes it; a failed allocation
    // drops the grant and so undoes it too.
    dev.grant_count(1, true);
    let g = Box::new(Grant { dev, pa, revoked: AtomicBool::new(false) }, M_WAITOK);
    let cred = unsafe { (*kpi::file::curthread()).td_ucred };
    match GrantObj::mgtdev(g, PAGE_SIZE as u64, nprot, cred) {
        Ok(obj) => {
            unsafe { *out = obj.into_raw() };
            0
        }
        Err(e) => e.as_c_type(),
    }
}

/// Take the doorbell page back from every mapping of the grant, and give up queue.c's
/// reference (the grant itself goes when the last mapping does).
///
/// # Safety
///
/// `obj` is queue.c's reference from `amd_db_grant_create`, not used after this; the caller
/// holds the queue's lock, and not the object's.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_db_grant_revoke(obj: vm_object_t) {
    let obj = unsafe { GrantObj::from_raw(obj) };
    {
        let _g = obj.lock();
        obj.handle().revoked.store(true, Ordering::Relaxed);
    }
    obj.free_pages(); // pmap_remove_all and remove from the object
    // `obj` drops here: queue.c's reference is gone.
}
