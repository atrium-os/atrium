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
		int fd;
		uint64_t va;

		err = amd_bo_create_fd(sc, td, a->size, &fd, &va);
		if (err != 0)
			return (err);
		a->bo_fd = fd;
		a->gpu_va = va;
		return (0);
	}

	case ATRIUM_GPU_IOC_BO_WRITE: {
		struct atrium_gpu_bo_xfer *x = (struct atrium_gpu_bo_xfer *)data;
		struct file *fp;

		err = amd_bo_fget(td, x->bo_fd, &fp, &bo);
		if (err != 0)
			return (err);
		err = amd_xfer_bounds(bo, x->offset, x->len);
		if (err == 0)
			err = copyin((const void *)(uintptr_t)x->user_ptr,
			    (char *)bo->kva + x->offset, x->len);
		fdrop(fp, td);
		return (err);
	}

	case ATRIUM_GPU_IOC_BO_READ: {
		struct atrium_gpu_bo_xfer *x = (struct atrium_gpu_bo_xfer *)data;
		struct file *fp;

		err = amd_bo_fget(td, x->bo_fd, &fp, &bo);
		if (err != 0)
			return (err);
		err = amd_xfer_bounds(bo, x->offset, x->len);
		if (err == 0)
			err = copyout((char *)bo->kva + x->offset,
			    (void *)(uintptr_t)x->user_ptr, x->len);
		fdrop(fp, td);
		return (err);
	}

	case ATRIUM_GPU_IOC_SET_COMPUTE: {
		struct atrium_gpu_set_compute *c =
		    (struct atrium_gpu_set_compute *)data;

		amd_set_compute(sc, c->kernel, c->src_va, c->dst_va);
		return (0);
	}

	case ATRIUM_GPU_IOC_WAIT_FENCE: {
		struct atrium_gpu_wait_fence *w =
		    (struct atrium_gpu_wait_fence *)data;
		volatile uint64_t *fence;
		struct file *fp;
		int deadline, slice, recheck;

		err = amd_bo_fget(td, w->fence_fd, &fp, &bo);
		if (err != 0)
			return (err);
		err = amd_xfer_bounds(bo, w->offset, sizeof(uint64_t));
		if (err != 0) {
			fdrop(fp, td);
			return (err);
		}
		fence = (volatile uint64_t *)((char *)bo->kva + w->offset);

		/*
		 * Sleep until the GPU's RELEASE_MEM writes `value`, woken by the
		 * ISR. The fence is set by DMA (not under our lock), so a wakeup
		 * can race ahead of the sleep; bound each sleep to a recheck
		 * slice so a missed wakeup still re-tests rather than hanging.
		 * deadline/slice are in ticks (hz/sec).
		 */
		recheck = hz / 100;		/* re-test at least every ~10ms */
		if (recheck < 1)
			recheck = 1;
		deadline = ticks + (int)(((uint64_t)w->timeout_ms * hz) / 1000);
		mtx_lock(&sc->lock);
		while (*fence != w->value) {
			slice = deadline - ticks;
			if (slice <= 0)
				break;
			if (slice > recheck)
				slice = recheck;
			msleep(&sc->irq_count, &sc->lock, 0, "amdfence", slice);
		}
		err = (*fence == w->value) ? 0 : EWOULDBLOCK;
		mtx_unlock(&sc->lock);
		fdrop(fp, td);
		return (err);
	}

	case ATRIUM_GPU_IOC_GET_IRQS: {
		struct atrium_gpu_irqs *q = (struct atrium_gpu_irqs *)data;

		q->count = sc->irq_count;
		q->msix_enabled = sc->msix_enabled;
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
		struct file *fp;

		err = amd_bo_fget(td, s->ring_fd, &fp, &bo);
		if (err != 0)
			return (err);
		/* The ring must fit the BO (each dword is 4 bytes). */
		if ((uint64_t)s->n_dwords * 4 > bo->size)
			err = EINVAL;
		else
			err = amd_submit(sc, bo, s->n_dwords, s->engine);
		fdrop(fp, td);
		return (err);
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
