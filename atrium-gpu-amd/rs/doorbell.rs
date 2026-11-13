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
 * queue.rs decides who may map which doorbell and when to revoke; it holds
 * one object reference per live grant and hands out further references to
 * mmap.
 * What lives here is the part user space reaches by touching the mapping:
 * the fault handler, and the grant's lifetime. The pager only ever inserts a
 * fictitious (device) page that no other object holds, whatever the grant
 * answers.
 *
 * Lock order (queue.rs): a queue's lock, then the grant object's lock, then sc->lock.
 */

use crate::backend::Dev;
use core::ffi::c_int;
use core::sync::atomic::{AtomicBool, Ordering};
use kpi::bindings::PAGE_SIZE;
use kpi::boxed::Box;
use kpi::pager::{Backing, MgtDevHandle, VmObject};
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
    fn fault(&self, offset: u64, _prot: c_int) -> Option<Backing<'_>> {
        // One page; a revoked grant faults (SIGBUS) so the client knows.
        if self.revoked.load(Ordering::Relaxed) || offset != 0 {
            None
        } else {
            Some(Backing::Device(self.pa))
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

/// A reference to a grant's pager object. queue.rs holds one per live grant (QState::grant)
/// and hands clones to mmap.
pub type GrantObj = VmObject<Grant>;

/// A new grant for the doorbell page at `pa`. Called with the queue's lock held.
pub fn grant(dev: Dev, pa: u64, nprot: c_int) -> Result<GrantObj> {
    // Counted before the object exists, as Grant's drop undoes it; a failed allocation
    // drops the grant and so undoes it too.
    dev.grant_count(1, true);
    let g = Box::new(Grant { dev, pa, revoked: AtomicBool::new(false) }, M_WAITOK);
    let cred = unsafe { (*kpi::file::curthread()).td_ucred };
    GrantObj::mgtdev(g, PAGE_SIZE as u64, nprot, cred)
}

/// Take the doorbell page back from every mapping of the grant, and give up the queue's
/// reference (the grant itself goes when the last mapping does). Called with the queue's
/// lock held, and not the object's.
pub fn revoke(obj: GrantObj) {
    {
        let _g = obj.lock();
        obj.handle().revoked.store(true, Ordering::Relaxed);
    }
    obj.free_pages(); // pmap_remove_all and remove from the object
    // `obj` drops here: the queue's reference is gone.
}
