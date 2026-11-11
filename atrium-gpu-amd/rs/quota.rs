/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * quota.rs — per-uid BO accounting (kmod audit 2026-11-07, G2).
 *
 * Without it, anyone holding a GPU fd could allocate VRAM (or wired System
 * memory, which bus_dma BOs are) until every other client's BO_ALLOC failed.
 * Now every BO is charged to the real uid of the thread that allocated it, and
 * an allocation that would take that uid over its cap fails with EDQUOT. The
 * caps are a share of the device's VRAM and of RAM: sysctls
 * dev.atrium_gpu_amd.N.isolation.vram_limit_pct (default 75) and
 * sys_limit_pct (default 50), read at each allocation.
 *
 * Why the uid and not the open: a client can open the node as often as it
 * likes, so a per-open cap is no cap. The uid is what separates the parties
 * that matter: frescod (_frescod) and the renderers (_fresco-render) hold
 * their budgets apart, so app renderers together can never starve the
 * compositor. A BO fd may be passed to another process; it stays charged to
 * the uid that allocated it until its backing is freed, which for a BO a
 * queue may still be using is when the deferred free reaps it (defer.rs).
 */

use crate::backend::Dev;
use core::pin::Pin;
use kpi::boxed::Box;
use kpi::prelude::*;
use kpi::sync::sx::{SxLock, sx_init, sx_xlock};

/// One uid's charges, in bytes (whole pages).
struct Account {
    uid: u32,
    vram: u64,
    sys: u64,
    next: Option<Box<Account>>,
}

/// The accounts with something charged (an account goes when it is back to zero).
pub struct Quotas(SxLock<Option<Box<Account>>>);

/// What a BO holds against its owner's account; given back by `uncharge`.
#[derive(Copy, Clone)]
pub struct Charge {
    uid: u32,
    vram: bool,
    bytes: u64,
}

impl Quotas {
    pub fn new() -> Self {
        Quotas(SxLock::new(None))
    }

    /// # Safety
    ///
    /// `self` does not move again.
    pub unsafe fn init(&self) {
        unsafe { sx_init(Pin::new_unchecked(&self.0), c"amd_quota") };
    }

    /// Charge `bytes` of VRAM or System memory to the calling thread's uid, or EDQUOT if
    /// that would take it over its cap.
    pub fn charge(&self, dev: Dev, vram: bool, bytes: u64) -> Result<Charge> {
        let uid = dev.cur_ruid();
        let limit = dev.quota_limit(vram);
        // Allocated before the lock (M_WAITOK); dropped unused if the uid has an account.
        let mut fresh = Some(Box::new(Account { uid, vram: 0, sys: 0, next: None }, M_WAITOK));
        let mut head = sx_xlock(&self.0);
        if find(&mut head, uid).is_none() {
            let mut a = fresh.take().unwrap();
            a.next = head.take();
            *head = Some(a);
        }
        let acct = find(&mut head, uid).unwrap();
        let used = if vram { &mut acct.vram } else { &mut acct.sys };
        match used.checked_add(bytes) {
            Some(n) if n <= limit => {
                *used = n;
                Ok(Charge { uid, vram, bytes })
            }
            _ => {
                prune(&mut head);
                drop(head);
                dev.quota_refused();
                Err(EDQUOT)
            }
        }
    }

    /// Give a charge back (the BO's backing has been freed).
    pub fn uncharge(&self, c: Charge) {
        let mut head = sx_xlock(&self.0);
        if let Some(a) = find(&mut *head, c.uid) {
            let used = if c.vram { &mut a.vram } else { &mut a.sys };
            debug_assert!(*used >= c.bytes);
            *used = used.saturating_sub(c.bytes);
        } else {
            debug_assert!(false, "uncharge without an account");
        }
        prune(&mut head);
    }
}

impl Drop for Quotas {
    fn drop(&mut self) {
        // Detach waits for every BO to be freed, so nothing is charged any more.
        debug_assert!(self.0.get_mut().is_none());
    }
}

fn find(head: &mut Option<Box<Account>>, uid: u32) -> Option<&mut Account> {
    let mut cur = head.as_deref_mut();
    while let Some(a) = cur {
        if a.uid == uid {
            return Some(a);
        }
        cur = a.next.as_deref_mut();
    }
    None
}

/// Drop the accounts that are back to zero.
fn prune(head: &mut Option<Box<Account>>) {
    let mut keep: Option<Box<Account>> = None;
    let mut cur = head.take();
    while let Some(mut a) = cur {
        cur = a.next.take();
        if a.vram != 0 || a.sys != 0 {
            a.next = keep.take();
            keep = Some(a);
        }
    }
    *head = keep;
}
