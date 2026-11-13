/*
 * glue.c — the C side of the boundary with the Rust front-end (rs/).
 *
 * The client node's ioctls, its per-open state, and the VM, BO and syncobj fd
 * objects, and the doorbell grant pager, are Rust (rs/README.md). What stays here is what Rust should not
 * reach into: the two cdevsw tables (module.c makes the devices from them),
 * and small accessors for the few softc fields the front-end needs, so the
 * softc stays a C type the Rust side only holds pointers to.
 */
#include "atrium_gpu_amd.h"

#include <sys/priv.h>
#include <sys/proc.h>
#include <sys/ucred.h>
#include <sys/mman.h>

#include <vm/vm.h>
#include <vm/vm_extern.h>
#include <vm/vm_object.h>

#include "rs/glue.h"

const struct atrium_gpu_backend_ops *
amd_rs_backend(struct atrium_amd_softc *sc)
{
	return (sc->backend);
}

void
amd_rs_irqs(struct atrium_amd_softc *sc, uint64_t *count, uint32_t *msix)
{
	*count = atomic_load_int(&sc->irq_count);
	*msix = sc->msix_enabled;
}

void
amd_rs_syncobj_count(struct atrium_amd_softc *sc, int delta)
{
	/* module.c's detach waits for this to reach 0. */
	atomic_add_int(&sc->syncobj_count, delta);
}

void
amd_rs_grant_count(struct atrium_amd_softc *sc, int live, int created)
{
	mtx_lock(&sc->lock);
	sc->iso.grants_live += live;	/* amd_hwq_busy_refs holds detach for these */
	if (created)
		sc->iso.grants++;
	mtx_unlock(&sc->lock);
}

void *
amd_rs_state(struct atrium_amd_softc *sc)
{
	return (sc->rs_state);
}

void
amd_rs_defer_count(struct atrium_amd_softc *sc, int now, int reaped)
{
	mtx_lock(&sc->lock);
	sc->iso.deferred_now += now;	/* amd_hwq_busy_refs holds detach for these */
	if (now > 0)
		sc->iso.deferred += now;
	sc->iso.reaped += reaped;
	mtx_unlock(&sc->lock);
}

uint32_t
amd_rs_cur_ruid(void)
{
	return (curthread->td_ucred->cr_ruid);
}

uint64_t
amd_rs_quota_limit(struct atrium_amd_softc *sc, int vram)
{
	uint64_t total;
	int pct;

	mtx_lock(&sc->lock);
	pct = vram ? sc->vram_limit_pct : sc->sys_limit_pct;
	mtx_unlock(&sc->lock);
	total = vram ? sc->vram.total : ptoa((uint64_t)physmem);
	return (total / 100 * (uint64_t)pct);
}

void
amd_rs_quota_refused(struct atrium_amd_softc *sc)
{
	mtx_lock(&sc->lock);
	sc->iso.quota_refused++;
	mtx_unlock(&sc->lock);
}

struct atrium_amd_softc *
amd_rs_cdev_softc(struct cdev *cdev)
{
	return (cdev->si_drv1);
}

struct cdevsw atrium_amd_cdevsw = {
	.d_version =	D_VERSION,
	.d_name =	"atrium-gpu",
	.d_open =	amd_rs_open,
	.d_ioctl =	amd_rs_ioctl,
	.d_mmap_single = amd_rs_mmap_single,	/* no d_mmap fallback */
};

/*
 * /dev/atrium-gpuNctl — device-wide control (isolation break 5, review §A2.4).
 * Reset, scheduler weights and power gating affect every client, so they are
 * not on the client node at all: the control node is root-only and no jail's
 * devfs ruleset unhides it. Only the compositor's broker holds it.
 */
struct cdevsw atrium_amd_ctl_cdevsw = {
	.d_version =	D_VERSION,
	.d_name =	"atrium-gpuctl",
	.d_ioctl =	amd_rs_ctl_ioctl,
};

/*
 * The calling thread may drive the hardware directly (interim gate for
 * user-mode queues, rs/uqueue.rs, until the GPU ABI's capability for it).
 */
int
amd_rs_priv_driver(void)
{
	return (priv_check(curthread, PRIV_DRIVER));
}

/*
 * A queue fd's fo_mmap (rs/uqueue.rs): map `obj` (a reference the caller
 * gives up, consumed either way) shared, read-write, never executable.
 */
int
amd_rs_mmap_object(vm_map_t map, vm_offset_t *addr, vm_size_t size,
    vm_prot_t prot, vm_prot_t cap_maxprot, int flags, vm_object_t obj,
    vm_ooffset_t foff, struct thread *td)
{
	vm_prot_t maxprot;
	int error;

	maxprot = (VM_PROT_READ | VM_PROT_WRITE) & cap_maxprot;
	if ((flags & MAP_SHARED) == 0 || (prot & ~maxprot) != 0)
		error = EACCES;
	else
		error = vm_mmap_object(map, addr, size, prot, maxprot, flags,
		    obj, foff, FALSE, td);
	if (error != 0)
		vm_object_deallocate(obj);
	return (error);
}
