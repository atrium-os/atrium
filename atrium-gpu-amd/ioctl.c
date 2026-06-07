/*
 * ioctl.c — the /dev/atrium-gpu0 character-device interface.
 *
 * The userspace ABI (atrium_gpu_amd_abi.h): allocate buffer objects, copy data
 * in/out of them, set compute state, and submit a PM4 ring on an engine. This
 * is the seam where the user-mode driver builds command rings and the kernel
 * owns the privileged MMIO (queue map, doorbell, compute registers).
 */
#include "atrium_gpu_amd.h"
#include "atrium_gpu_amd_abi.h"

static int
atrium_amd_open(struct cdev *cdev, int oflags, int devtype, struct thread *td)
{
	return (0);
}

/* Bounds-check a BO byte range [offset, offset+len) against the BO size. */
static int
amd_xfer_bounds(struct atrium_amd_bo *bo, uint64_t offset, uint64_t len)
{
	if (bo == NULL)
		return (ENXIO);
	if (len > bo->size || offset > bo->size - len)
		return (EINVAL);
	return (0);
}

static int
atrium_amd_ioctl(struct cdev *cdev, u_long cmd, caddr_t data, int fflag,
    struct thread *td)
{
	struct atrium_amd_softc *sc = cdev->si_drv1;
	struct atrium_amd_bo *bo;
	int err;

	switch (cmd) {
	case ATRIUM_GPU_IOC_BO_ALLOC: {
		struct atrium_gpu_bo_alloc *a = (struct atrium_gpu_bo_alloc *)data;

		return (amd_bo_alloc(sc, a->size, &a->handle, &a->gpu_va));
	}

	case ATRIUM_GPU_IOC_BO_WRITE: {
		struct atrium_gpu_bo_xfer *x = (struct atrium_gpu_bo_xfer *)data;

		bo = amd_bo_lookup(sc, x->handle);
		err = amd_xfer_bounds(bo, x->offset, x->len);
		if (err != 0)
			return (err);
		return (copyin((const void *)(uintptr_t)x->user_ptr,
		    (char *)bo->kva + x->offset, x->len));
	}

	case ATRIUM_GPU_IOC_BO_READ: {
		struct atrium_gpu_bo_xfer *x = (struct atrium_gpu_bo_xfer *)data;

		bo = amd_bo_lookup(sc, x->handle);
		err = amd_xfer_bounds(bo, x->offset, x->len);
		if (err != 0)
			return (err);
		return (copyout((char *)bo->kva + x->offset,
		    (void *)(uintptr_t)x->user_ptr, x->len));
	}

	case ATRIUM_GPU_IOC_SET_COMPUTE: {
		struct atrium_gpu_set_compute *c =
		    (struct atrium_gpu_set_compute *)data;

		amd_set_compute(sc, c->kernel, c->src_va, c->dst_va);
		return (0);
	}

	case ATRIUM_GPU_IOC_SET_DRAW: {
		struct atrium_gpu_set_draw *d =
		    (struct atrium_gpu_set_draw *)data;

		amd_set_draw(sc, d->vtx_va, d->rt_va, d->width, d->height);
		return (0);
	}

	case ATRIUM_GPU_IOC_SUBMIT: {
		struct atrium_gpu_submit *s = (struct atrium_gpu_submit *)data;

		bo = amd_bo_lookup(sc, s->ring_handle);
		if (bo == NULL)
			return (ENXIO);
		/* The ring must fit the BO (each dword is 4 bytes). */
		if ((uint64_t)s->n_dwords * 4 > bo->size)
			return (EINVAL);
		return (amd_submit(sc, bo, s->n_dwords, s->engine));
	}

	default:
		return (ENOTTY);
	}
}

struct cdevsw atrium_amd_cdevsw = {
	.d_version =	D_VERSION,
	.d_name =	"atrium-gpu",
	.d_open =	atrium_amd_open,
	.d_ioctl =	atrium_amd_ioctl,
};
