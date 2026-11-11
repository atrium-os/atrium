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
