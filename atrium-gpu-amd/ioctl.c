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

/*
 * Map the BAR2 doorbell page into userspace for user-mode-queue submission.
 * The page is device MMIO (VM_MEMATTR_DEVICE — uncacheable, no write
 * combining), so a userspace store to the queue's doorbell traps straight to
 * the device. This is the single mappable region; any other offset is refused.
 */
static int
atrium_amd_mmap(struct cdev *cdev, vm_ooffset_t offset, vm_paddr_t *paddr,
    int nprot, vm_memattr_t *memattr)
{
	struct atrium_amd_softc *sc = cdev->si_drv1;

	if (offset >= PAGE_SIZE)
		return (EINVAL);
	*paddr = rman_get_start(sc->doorbell) + offset;
	*memattr = VM_MEMATTR_DEVICE;
	return (0);
}

/* Append a TLV cap record (header + data, padded to 4 bytes) to a buffer. */
static size_t
amd_put_cap(uint8_t *buf, size_t off, uint32_t id, const void *data,
    uint32_t size)
{
	struct atrium_gpu_cap_record r;

	r.cap_id = id;
	r.cap_size = size;
	memcpy(buf + off, &r, sizeof(r));
	off += sizeof(r);
	memcpy(buf + off, data, size);
	off += size;
	while ((off & 3) != 0)
		buf[off++] = 0;
	return (off);
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
	case ATRIUM_GPU_IOC_VM_CREATE: {
		struct atrium_gpu_vm_create *v =
		    (struct atrium_gpu_vm_create *)data;
		int fd;

		err = amd_vm_create_fd(sc, td, &fd);
		if (err != 0)
			return (err);
		v->out_fd = fd;
		return (0);
	}

	case ATRIUM_GPU_IOC_BO_ALLOC: {
		struct atrium_gpu_bo_alloc *a = (struct atrium_gpu_bo_alloc *)data;
		int fd;

		err = amd_bo_create_fd(sc, td, a->size, &fd);
		if (err != 0)
			return (err);
		a->bo_fd = fd;
		return (0);
	}

	case ATRIUM_GPU_IOC_VM_BIND: {
		struct atrium_gpu_vm_bind *b = (struct atrium_gpu_vm_bind *)data;
		struct atrium_amd_vm *vm;
		struct file *vmfp, *bofp;
		uint64_t va = b->va;

		err = amd_vm_fget(td, b->vm_fd, &vmfp, &vm);
		if (err != 0)
			return (err);
		err = amd_bo_fget(td, b->bo_fd, &bofp, &bo);
		if (err != 0) {
			fdrop(vmfp, td);
			return (err);
		}
		err = amd_bo_bind(bo, vm, vmfp, &va);
		fdrop(bofp, td);
		if (err != 0) {
			fdrop(vmfp, td);	/* bind failed: BO did not take it */
			return (err);
		}
		/* Success: the BO now owns vmfp — do not drop it here. */
		b->va = va;
		return (0);
	}

	case ATRIUM_GPU_IOC_QUEUE_MAP: {
		struct atrium_gpu_queue_map *m =
		    (struct atrium_gpu_queue_map *)data;
		struct atrium_amd_vm *vm;
		struct file *vmfp, *bofp;
		uint32_t doorbell_off;

		err = amd_vm_fget(td, m->vm_fd, &vmfp, &vm);
		if (err != 0)
			return (err);
		err = amd_bo_fget(td, m->ring_fd, &bofp, &bo);
		if (err != 0) {
			fdrop(vmfp, td);
			return (err);
		}
		if (bo->vm == NULL)	/* ring must be bound to have a GPU-VA */
			err = EINVAL;
		else
			err = amd_queue_program(sc, bo->gpu_va, m->engine,
			    vm->vmid, &doorbell_off);
		fdrop(bofp, td);
		fdrop(vmfp, td);
		if (err != 0)
			return (err);
		m->doorbell_mmap_offset = ATRIUM_AMD_DOORBELL_MMAP_OFF;
		m->doorbell_size = PAGE_SIZE;
		m->doorbell_word_offset = doorbell_off;
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

	case ATRIUM_GPU_IOC_SYNCOBJ_CREATE: {
		struct atrium_gpu_syncobj_create *c =
		    (struct atrium_gpu_syncobj_create *)data;
		int fd;

		err = amd_syncobj_create_fd(sc, td, &fd);
		if (err != 0)
			return (err);
		c->out_fd = fd;
		return (0);
	}

	case ATRIUM_GPU_IOC_SYNCOBJ_SIGNAL: {
		struct atrium_gpu_syncobj_op *o =
		    (struct atrium_gpu_syncobj_op *)data;
		struct atrium_amd_syncobj *so;
		struct file *fp;

		err = amd_syncobj_fget(td, o->syncobj_fd, &fp, &so);
		if (err != 0)
			return (err);
		amd_syncobj_signal(so, o->value);
		fdrop(fp, td);
		return (0);
	}

	case ATRIUM_GPU_IOC_SYNCOBJ_QUERY: {
		struct atrium_gpu_syncobj_op *o =
		    (struct atrium_gpu_syncobj_op *)data;
		struct atrium_amd_syncobj *so;
		struct file *fp;

		err = amd_syncobj_fget(td, o->syncobj_fd, &fp, &so);
		if (err != 0)
			return (err);
		mtx_lock(&so->lock);
		o->value = so->value;
		mtx_unlock(&so->lock);
		fdrop(fp, td);
		return (0);
	}

	case ATRIUM_GPU_IOC_SYNCOBJ_WAIT: {
		struct atrium_gpu_syncobj_wait *w =
		    (struct atrium_gpu_syncobj_wait *)data;
		struct atrium_amd_syncobj *so;
		struct file *fp;
		int deadline, slice;

		err = amd_syncobj_fget(td, w->syncobj_fd, &fp, &so);
		if (err != 0)
			return (err);
		deadline = ticks + (int)(((uint64_t)w->timeout_ms * hz) / 1000);
		mtx_lock(&so->lock);
		while (so->value < w->value) {
			slice = deadline - ticks;
			if (slice <= 0)
				break;
			msleep(&so->value, &so->lock, 0, "amdsyncw", slice);
		}
		err = (so->value >= w->value) ? 0 : EWOULDBLOCK;
		mtx_unlock(&so->lock);
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

		amd_set_draw(sc, d->vtx_va, d->rt_va, d->width, d->height,
		    d->tex_va, d->tex_w, d->tex_h, d->tex_filter, d->blend,
		    d->depth_va);
		return (0);
	}

	case ATRIUM_GPU_IOC_SUBMIT: {
		struct atrium_gpu_submit *s = (struct atrium_gpu_submit *)data;
		struct atrium_amd_syncobj *so = NULL;
		struct atrium_amd_vm *vm;
		struct file *fp, *vmfp, *sfp = NULL;

		err = amd_vm_fget(td, s->vm_fd, &vmfp, &vm);
		if (err != 0)
			return (err);
		err = amd_bo_fget(td, s->ring_fd, &fp, &bo);
		if (err != 0) {
			fdrop(vmfp, td);
			return (err);
		}
		/* The ring must fit the BO (each dword is 4 bytes). */
		if ((uint64_t)s->n_dwords * 4 > bo->size) {
			err = EINVAL;
		} else {
			/*
			 * Register the completion syncobj BEFORE ringing the
			 * doorbell — the ISR signals it on the end-of-pipe
			 * interrupt, not here. For a ring that parks on a
			 * cross-queue WAIT that interrupt arrives on a *later*
			 * doorbell (another queue writes the awaited fence),
			 * so completion is genuinely asynchronous w.r.t. this
			 * submit. For a ring that drains synchronously the IRQ
			 * fires inside amd_submit; the entry is already queued,
			 * so the ISR still finds it. Pushing first is what makes
			 * both cases correct.
			 */
			if (s->signal_syncobj_fd >= 0) {
				err = amd_syncobj_fget(td,
				    s->signal_syncobj_fd, &sfp, &so);
				if (err == 0)
					amd_pending_push(sc, so,
					    s->signal_value);
			}
			if (err == 0)
				err = amd_submit(sc, bo, s->n_dwords,
				    s->engine, vm->vmid);
			/* Submit never reached the GPU -> reclaim the entry. */
			if (err != 0 && so != NULL)
				amd_pending_scrub(sc, so);
			if (sfp != NULL)
				fdrop(sfp, td);
		}
		fdrop(fp, td);
		fdrop(vmfp, td);
		return (err);
	}

	case ATRIUM_GPU_IOC_QUERY_CAPS: {
		struct atrium_gpu_caps_query *q =
		    (struct atrium_gpu_caps_query *)data;
		static const char vendor[] = "Atrium AMD RDNA4 (gpusim)";
		uint32_t ver[2] = { 1, 0 };	/* ABI major.minor */
		uint32_t feat = ATRIUM_GPU_FEAT_GRAPHICS |
		    ATRIUM_GPU_FEAT_COMPUTE | ATRIUM_GPU_FEAT_USER_QUEUES |
		    ATRIUM_GPU_FEAT_SYNCOBJ | ATRIUM_GPU_FEAT_VM_BIND;
		uint8_t buf[128];
		size_t off = 0;

		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_ABI_VERSION, ver,
		    sizeof(ver));
		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_VENDOR, vendor,
		    sizeof(vendor));
		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_FEATURES, &feat,
		    sizeof(feat));

		if (q->caps_size < off) {
			q->caps_size = off;	/* tell userspace the size needed */
			return (ENOMEM);
		}
		err = copyout(buf, (void *)(uintptr_t)q->caps_ptr, off);
		if (err == 0)
			q->caps_size = off;
		return (err);
	}

	case ATRIUM_GPU_IOC_GPU_RESET: {
		/*
		 * Recover a wedged engine. A full GPU reset tears down the rings
		 * (the model drops every queue), then reload CP firmware and
		 * re-init the MES — the timeout -> reset -> resubmit path a driver
		 * runs when a submission is lost (a forever-unsatisfied cross-queue
		 * WAIT, a hang). GPUVM page tables survive the reset, so open VMs
		 * and their BOs stay valid; the next submit re-maps its queue onto
		 * the clean engine. Drop any pending completions — the reset
		 * abandoned the work that would have signalled them, so they must
		 * not be mis-attributed to a later interrupt.
		 */
		err = amd_reset(sc);
		if (err == 0) {
			amd_firmware_load(sc);
			amd_mes_init(sc);
			mtx_lock(&sc->lock);
			sc->n_pending = 0;
			mtx_unlock(&sc->lock);
		}
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
	.d_mmap =	atrium_amd_mmap,
};
