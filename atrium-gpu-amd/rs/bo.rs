/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * bo.rs — buffer objects as fd objects, and their bindings into VMs. Was the
 * front-end half of bo.c.
 *
 * A BO is independent of any VM (ABI-v2 principle 4): the same object can be
 * bound into up to ATRIUM_AMD_BO_MAX_BIND VMs at once, each at its own GPU-VA
 * (a compositor importing a client's buffer). Each binding holds a file
 * reference on its VM, so every VM a BO is mapped into outlives the BO.
 *
 * The backing store (bus_dma System pages or VRAM) is the backend's
 * `struct atrium_amd_bo`, this object's first field.
 *
 * Freeing has to respect the GPU (isolation break 2): on the last close the
 * BO's PTEs are cleared in every VM, and if a queue that ran under one of
 * those VMs may still be using the pages, the BO goes to the deferred-free
 * list (queue.c), whose reaper calls the backend's bo_free and then
 * amd_bo_free_struct once the queues have drained. So the BO is not the file's
 * object: the file holds a `BoFile`, whose drop decides between the two.
 */

use crate::backend::Dev;
use crate::defer;
use crate::quota::Charge;
use crate::sys;
use crate::vm::Vm;
use core::ffi::c_int;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use kpi::bindings::{self, file, thread};
use kpi::boxed::Box;
use kpi::file::{self as kfile, FileRef};
use kpi::kobj::AsCType;
use kpi::prelude::*;
use kpi::sync::sx::{SxLock, sx_init, sx_slock, sx_xlock};
use kpi::vmem::Fit;

const MAX_BIND: usize = sys::ATRIUM_AMD_BO_MAX_BIND as usize;
const PAGE_SIZE: u64 = bindings::PAGE_SIZE as u64;

/// One mapping of the BO into a VM: the VM (held), and the range it occupies there.
struct Binding {
    vm: FileRef<Vm>,
    gpu_va: u64,
}

#[repr(C)]
pub struct Bo {
    /// The backend's part. Must stay the first field (C casts between the two).
    pub(crate) hw: sys::atrium_amd_bo,
    /// The bindings. A BO fd can be used by two threads (or passed to another process) at
    /// once, so two VM_BINDs of one BO race; in C both passed the "slot free" check and the
    /// second wrote one past the array (kmod audit 2026-11-07, G1). Here the slots can only
    /// be reached through the exclusive guard, and a full table is an error, not an index.
    /// Exclusive in bind (which may sleep in map_page), shared in gpu_va.
    binds: SxLock<[Option<Binding>; MAX_BIND]>,
    /// What the BO holds against its allocator's uid (quota.rs); `None` until charged.
    charge: Option<Charge>,
}

// SAFETY: the backing store is written only by the backend at alloc/free (no other user then)
// and by the GPU; the bindings are behind the sx lock.
unsafe impl Send for Bo {}
unsafe impl Sync for Bo {}

impl Bo {
    pub fn as_c(&self) -> *mut sys::atrium_amd_bo {
        core::ptr::from_ref(&self.hw).cast_mut()
    }

    /// # Safety
    ///
    /// `bo` came from `as_c` on a BO that is still alive.
    unsafe fn from_c<'a>(bo: *mut sys::atrium_amd_bo) -> &'a Bo {
        unsafe { &*bo.cast::<Bo>() }
    }

    fn dev(&self) -> Dev {
        // SAFETY: set at creation from a live device, which outlives its BOs.
        unsafe { Dev::from_raw(self.hw.sc) }
    }

    pub fn size(&self) -> u64 {
        self.hw.size
    }

    /// The bytes the BO spans in a VM: whole pages.
    fn span(&self) -> u64 {
        self.hw.npages as u64 * PAGE_SIZE
    }

    /// The CPU mapping of a System BO, `None` for VRAM (GPU-only, no CPU copy path).
    pub fn kva(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.hw.kva.cast::<u8>())
    }

    /// The BO's pages as the backend's array (bus addresses, or VRAM offsets).
    fn pages(&self) -> &[bindings::bus_addr_t] {
        // SAFETY: the backend allocated `npages` entries and frees them only in bo_free, which
        // runs after every binding is gone.
        unsafe { core::slice::from_raw_parts(self.hw.pages, self.hw.npages as usize) }
    }

    /// The BO's base GPU-VA in `vm`, if it is bound there.
    pub fn gpu_va(&self, vm: &Vm) -> Option<u64> {
        let binds = sx_slock(&self.binds);
        binds
            .iter()
            .flatten()
            .find(|b| core::ptr::eq(&*b.vm, vm))
            .map(|b| b.gpu_va)
    }

    /// Bind into `vm` at `va` (0: the arena picks one). Takes its own reference on the VM.
    /// Returns the VA.
    pub fn bind(&self, vm: &FileRef<Vm>, va: u64, flags: u32) -> Result<u64> {
        if flags & !(sys::ATRIUM_GPU_BIND_READONLY as u32) != 0 {
            return Err(EINVAL);
        }
        let writable = flags & sys::ATRIUM_GPU_BIND_READONLY as u32 == 0;
        let span = self.span();
        let mut binds = sx_xlock(&self.binds);

        // A BO maps at most once per VM: a second bind would alias it at two VAs.
        if binds.iter().flatten().any(|b| core::ptr::eq(&*b.vm, &**vm)) {
            return Err(EBUSY);
        }
        let slot = binds.iter().position(Option::is_none).ok_or(ENOSPC)?; // shared too widely

        // Reserve the range. A caller-chosen VA must be page-aligned, inside the window and
        // clear of every other binding (review §A2.5); otherwise next-fit picks one.
        let addr = if va != 0 {
            if va & (PAGE_SIZE - 1) != 0 || va.checked_add(span).is_none() {
                return Err(EINVAL);
            }
            vm.arena.alloc_at(va, span)?; // EEXIST: overlaps a binding or off-window
            va
        } else {
            vm.arena.alloc(span, Fit::Next).map_err(|_| ENOSPC)?
        };

        // One PTE per page: the GPUVM gathers the BO's (possibly scattered) pages into one
        // contiguous GPU-VA range, separately in each VM's own page tables.
        let dev = self.dev();
        let vram = self.hw.vram != 0;
        for (i, &page) in self.pages().iter().enumerate() {
            let pva = addr + i as u64 * PAGE_SIZE;
            if let Err(e) = dev.map_page(vm.as_c(), pva, page as u64, vram, writable) {
                for j in 0..i {
                    dev.unmap_page(vm.as_c(), addr + j as u64 * PAGE_SIZE);
                }
                // SAFETY: [addr, addr + span) was reserved above and nothing maps it.
                unsafe { vm.arena.free(addr, span) };
                return Err(e);
            }
        }
        binds[slot] = Some(Binding { vm: vm.clone(), gpu_va: addr });
        Ok(addr)
    }
}

/// What a BO fd's file holds. Its drop is the BO's last close.
pub struct BoFile(NonNull<Bo>);

// SAFETY: as Bo.
unsafe impl Send for BoFile {}
unsafe impl Sync for BoFile {}

kpi::file_ops!(BoFile);

impl core::ops::Deref for BoFile {
    type Target = Bo;
    fn deref(&self) -> &Bo {
        // SAFETY: the BO lives until this file's drop hands it on.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for BoFile {
    fn drop(&mut self) {
        // SAFETY: the last reference: no thread can reach the BO through the fd any more, and
        // nothing else holds it (queue.c's submit path holds the file while it uses the ring).
        let bo = unsafe { self.0.as_mut() };
        let dev = bo.dev();
        let span = bo.span();
        let npages = bo.hw.npages as u64;
        let mut vmids = [0u16; MAX_BIND];
        let mut n = 0;

        // Clear the BO's PTEs in every VM it is bound into (each unmap flushes the TLB), so
        // no new access can reach its pages, and give the ranges back to the arenas.
        let binds = bo.binds.get_mut();
        for b in binds.iter().flatten() {
            for i in 0..npages {
                dev.unmap_page(b.vm.as_c(), b.gpu_va + i * PAGE_SIZE);
            }
            // SAFETY: the binding's reserved range, unmapped above.
            unsafe { b.vm.arena.free(b.gpu_va, span) };
            // SAFETY: the VM is held by the binding.
            vmids[n] = unsafe { (*b.vm.as_c()).vmid };
            n += 1;
        }
        // Work already queued under one of those VMs may still be using the pages (isolation
        // break 2): note which queues are busy now, before dropping the VM references (the
        // last one tears the VM down, which stops its queues).
        let at = dev.busy_snapshot(&vmids[..n]);
        for b in binds.iter_mut() {
            *b = None; // drops the VM reference
        }
        // SAFETY: created by create_fd with Box; this was the last user and nothing else
        // refers to it (no bindings left).
        let mut bo = unsafe { Box::<Bo>::from_raw(self.0.as_ptr()) };
        if at.mask != 0 {
            // Freed by the reaper once those queues have moved on (defer.rs).
            defer::defer(dev, bo, at);
            return;
        }
        free_backing(dev, &mut bo);
    }
}

/// Free a BO's backing store (bo_count--) and give its charge back. Only once no queue can
/// still reach it: at the last close, or from the deferred-free reaper.
pub(crate) fn free_backing(dev: Dev, bo: &mut Bo) {
    dev.bo_free(&mut bo.hw);
    if let Some(c) = bo.charge.take() {
        dev.state().quotas.uncharge(c);
    }
}

/// BO_ALLOC: a BO of `size` bytes with placement `flags`, as an fd.
pub fn create_fd(dev: Dev, size: u64, flags: u32) -> Result<c_int> {
    // SAFETY: the backend struct is pointers and integers; all-zero is "no backing yet",
    // which bo_alloc fills in (the C front-end used M_ZERO).
    let mut hw: sys::atrium_amd_bo = unsafe { MaybeUninit::zeroed().assume_init() };
    hw.sc = dev.raw();
    // Charged first, so two threads of one uid can't both pass the cap; whole pages, as
    // allocated. The size check is bo_alloc's too, made here first so that a bad size is
    // EINVAL whatever the uid has charged.
    if size == 0 || size > sys::ATRIUM_AMD_BO_MAX_PAGES as u64 * PAGE_SIZE {
        return Err(EINVAL);
    }
    let vram = flags & sys::ATRIUM_GPU_BO_VRAM as u32 != 0;
    let bytes = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    // A device with no VRAM heap (silicon before stage 5d) has nothing to place it in: not
    // supported, rather than a quota of 0 % of nothing (EDQUOT).
    if vram && dev.caps().vram_bytes == 0 {
        return Err(EOPNOTSUPP);
    }
    let charge = dev.state().quotas.charge(dev, vram, bytes)?;
    let mut bo: Box<Bo> =
        Box::new(Bo { hw, binds: SxLock::new([const { None }; MAX_BIND]), charge: None }, M_WAITOK);
    // SAFETY: boxed, never moves again.
    unsafe { sx_init(Pin::new_unchecked(&bo.binds), c"amdbobind") };
    // Self-cleaning on failure; the Box then frees the struct.
    if let Err(e) = dev.bo_alloc(&mut bo.hw, size, flags) {
        dev.state().quotas.uncharge(charge);
        return Err(e);
    }
    bo.charge = Some(charge);
    // From here the BoFile owns the BO: if install fails, its drop frees the backing.
    let raw = Box::into_raw(bo);
    // SAFETY: Box::into_raw is never null.
    let f = Box::new(BoFile(unsafe { NonNull::new_unchecked(raw) }), M_WAITOK);
    kfile::install(f, (bindings::FREAD | bindings::FWRITE) as c_int)
}

// --- exports for C ---

/// For the display module (through the vtable): resolve a BO fd. The caller fdrop()s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn amd_bo_fget(
    _td: *mut thread,
    fd: c_int,
    out_fp: *mut *mut file,
    out_bo: *mut *mut sys::atrium_amd_bo,
) -> c_int {
    match FileRef::<BoFile>::get(fd) {
        Ok(r) => {
            unsafe {
                *out_bo = r.as_c();
                *out_fp = r.into_raw();
            }
            0
        }
        Err(e) => e.as_c_type(),
    }
}

