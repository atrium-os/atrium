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
 * Map one doorbell page of BAR2 into userspace for user-mode-queue submission.
 * The doorbell BAR is divided into per-queue pages: QUEUE_MAP hands a queue's
 * page offset to its owner, who mmap()s just that page. Mapping one queue's
 * doorbell therefore does not expose any other queue's — the page is the
 * capability (so it can be SCM_RIGHTS-granted to a single jailed client). The
 * page is device MMIO (VM_MEMATTR_DEVICE), so a store to it traps straight to
 * the device. Offsets past the BAR are refused.
 */
static int
atrium_amd_mmap(struct cdev *cdev, vm_ooffset_t offset, vm_paddr_t *paddr,
    int nprot, vm_memattr_t *memattr)
{
	struct atrium_amd_softc *sc = cdev->si_drv1;

	return (sc->backend->mmap(sc, offset, paddr, memattr));
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
	if (bo->kva == NULL)
		return (EINVAL);	/* VRAM BO: GPU-only, no CPU copy path */
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

		err = amd_bo_create_fd(sc, td, a->size, a->flags, &fd);
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
		uint64_t ring_va = amd_bo_gpu_va(bo, vm);

		if (ring_va == 0)	/* ring must be bound in THIS vm for a GPU-VA */
			err = EINVAL;
		else
			err = amd_queue_program(sc, ring_va, m->engine,
			    vm->vmid, &doorbell_off);
		fdrop(bofp, td);
		fdrop(vmfp, td);
		if (err != 0)
			return (err);
		/*
		 * doorbell_off is page-aligned (one page per queue), so it IS the
		 * mmap offset of this queue's own doorbell page; the doorbell word
		 * is at the start of that page. mmap()ing it exposes only this queue.
		 */
		m->doorbell_mmap_offset = doorbell_off;
		m->doorbell_size = PAGE_SIZE;
		m->doorbell_word_offset = 0;
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

	case ATRIUM_GPU_IOC_BO_EXPORT_SCANOUT: {
		struct atrium_gpu_bo_export_scanout *e =
		    (struct atrium_gpu_bo_export_scanout *)data;
		struct file *fp;

		/*
		 * Export a VRAM BO as a scanout handle for the display module
		 * (a separate driver with no BO table): the absolute VRAM offset
		 * + size, the dma-buf-equivalent it imports. Only VRAM is
		 * scannable — System/GTT BOs have no contiguous VRAM offset.
		 */
		err = amd_bo_fget(td, e->bo_fd, &fp, &bo);
		if (err != 0)
			return (err);
		err = sc->backend->export_scanout(bo, &e->vram_offset, &e->size);
		fdrop(fp, td);
		return (err);
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
				err = sc->backend->submit(sc, bo, s->n_dwords,
				    s->engine, vm);
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
		uint32_t ver[2] = { 1, 0 };	/* ABI major.minor (front-end's) */
		struct atrium_gpu_backend_caps bc;
		struct atrium_gpu_cap_address_space as;
		struct atrium_gpu_heap_info heaps[2];
		uint8_t buf[256];
		size_t off = 0;

		/* The backend supplies the device-specific values; the front-end
		 * assembles the (forward-compatible) TLV. */
		sc->backend->get_caps(sc, &bc);
		as.va_base = bc.va_base;
		as.va_size = bc.va_size;
		as.va_align = bc.va_align;
		heaps[0].kind = ATRIUM_GPU_HEAP_DEVICE;
		heaps[0].flags = 0;
		heaps[0].size = bc.vram_bytes;
		heaps[1].kind = ATRIUM_GPU_HEAP_SYSTEM;
		heaps[1].flags = 0;
		heaps[1].size = 0;

		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_ABI_VERSION, ver,
		    sizeof(ver));
		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_VENDOR, bc.vendor,
		    strlen(bc.vendor) + 1);
		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_FEATURES, &bc.features,
		    sizeof(bc.features));
		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_ADDRESS_SPACE, &as,
		    sizeof(as));
		off = amd_put_cap(buf, off, ATRIUM_GPU_CAP_HEAPS, heaps,
		    sizeof(heaps));

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

	case ATRIUM_GPU_IOC_SCHED: {
		struct atrium_gpu_sched *s = (struct atrium_gpu_sched *)data;

		amd_sched(sc, s);
		return (0);
	}

	case ATRIUM_GPU_IOC_POWERGATE: {
		struct atrium_gpu_powergate *p =
		    (struct atrium_gpu_powergate *)data;

		return (amd_powergate(sc, p));
	}

	/*
	 * Display ioctls moved to /dev/atrium-display0 (atrium_gpu_amd_display.ko,
	 * §4.1): the display engine is a separate driver, and the scanout FB is
	 * handed across by the dma-buf-style BO_EXPORT_SCANOUT above rather than a
	 * BO fd consumed here.
	 */

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
