/*-
 * SPDX-License-Identifier: BSD-2-Clause
 *
 * vm.rs — per-process GPU address spaces (ABI-v2 §5.2) as fd objects. Was the
 * front-end half of vm.c.
 *
 * The hardware address space (VMID, page tables, the per-context PT base) is
 * the backend's: `struct atrium_amd_vm`, set up and torn down through the
 * backend vtable (vm.c for amd). It is this object's first field, so the
 * backend's pointer and ours are the same address.
 *
 * What the front-end adds is the VM's GPU-VA window as a vmem arena, so a freed
 * BO's range is reused (a bump pointer ran a long-lived client out of VA), and
 * the lifetime: every BO bound here holds a file reference on the VM, so the
 * VM is torn down only after the last of them is gone.
 */

use crate::backend::Dev;
use crate::sys;
use core::ffi::c_int;
use core::mem::MaybeUninit;
use kpi::bindings;
use kpi::boxed::Box;
use kpi::file as kfile;
use kpi::prelude::*;
use kpi::vmem::VmemArena;

#[repr(C)]
pub struct Vm {
    /// The backend's part. Must stay the first field (C casts between the two).
    hw: sys::atrium_amd_vm,
    /// The GPU-VA window, next-fit so a just-freed range is handed out last.
    pub arena: VmemArena,
    /// vm_setup succeeded, so drop must run vm_teardown.
    live: bool,
}

// SAFETY: the backend serialises its own hardware state (sc->lock); the arena has its own lock.
unsafe impl Send for Vm {}
unsafe impl Sync for Vm {}

kpi::file_ops!(Vm);

impl Vm {
    pub fn as_c(&self) -> *mut sys::atrium_amd_vm {
        core::ptr::from_ref(&self.hw).cast_mut()
    }

    pub fn dev(&self) -> Dev {
        // SAFETY: set at creation from a live device, which outlives its VMs.
        unsafe { Dev::from_raw(self.hw.sc) }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        if self.live {
            // Stops every queue still programmed under the VMID, then frees the page tables
            // and the VMID (vm.c). Every BO bound here is gone: each held a VM reference.
            let dev = self.dev();
            dev.vm_teardown(&mut self.hw);
        }
        // The arena drops after this, empty.
    }
}

/// The GPU-VA window every VM gets: [BO_VA_BASE, BO_VA_BASE + VM_MAX_BO pages).
const VA_BASE: u64 = sys::ATRIUM_AMD_BO_VA_BASE as u64;
const VA_SIZE: u64 = sys::ATRIUM_AMD_VM_MAX_BO as u64 * bindings::PAGE_SIZE as u64;

/// VM_CREATE: stand up a hardware address space and return it as an fd.
pub fn create_fd(dev: Dev) -> Result<c_int> {
    let arena = VmemArena::new(c"atrium_amd_va", VA_BASE, VA_SIZE, bindings::PAGE_SIZE as u64)?;
    // SAFETY: the backend struct is plain pointers, integers and DMA-page records, for
    // which all-zero is the state vm_setup expects (the C front-end used M_ZERO).
    let mut hw: sys::atrium_amd_vm = unsafe { MaybeUninit::zeroed().assume_init() };
    hw.sc = dev.raw();
    let mut vm: Box<Vm> = Box::new(Vm { hw, arena, live: false }, M_WAITOK);
    // Self-cleaning on failure (vm.c), so a failed setup leaves nothing to tear down.
    dev.vm_setup(&mut vm.hw)?;
    vm.live = true;
    kfile::install(vm, (bindings::FREAD | bindings::FWRITE) as c_int)
}
