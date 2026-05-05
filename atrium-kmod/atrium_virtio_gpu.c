/*
 * atrium_virtio_gpu.c — D0 skeleton: native FreeBSD driver for virtio-gpu,
 * exposing the Atrium GPU ABI (docs/spec/gpu-abi.md).
 *
 * Step 1 (this file): newbus PCI attach, two cdevs (/dev/atrium-gpu0,
 * /dev/atrium-display0), all ioctl numbers wired with stubs that return
 * EOPNOTSUPP. IOC_CAPS returns minimal real data so userspace can probe.
 * No virtqueue setup, no BO allocator, no fence machinery yet — those
 * arrive in step 2.
 *
 * No DRM, no linuxkpi, no GPU vendor abstraction layer. Pure newbus +
 * cdev + kqueue + bus_dma. This module is what FreeBSD's GPU stack will
 * eventually look like across multiple vendors; virtio-gpu is the
 * proving ground for the ABI shape.
 */

#include <sys/param.h>
#include <sys/systm.h>
#include <sys/kernel.h>
#include <sys/module.h>
#include <sys/bus.h>
#include <sys/conf.h>
#include <sys/malloc.h>
#include <sys/lock.h>
#include <sys/mutex.h>
#include <sys/condvar.h>
#include <sys/rman.h>
#include <sys/sysctl.h>
#include <sys/uio.h>
#include <sys/selinfo.h>
#include <sys/event.h>
#include <sys/proc.h>
#include <sys/file.h>
#include <sys/filedesc.h>
#include <sys/capsicum.h>
#include <sys/vnode.h>
#include <sys/queue.h>
#include <sys/types.h>

#include <machine/bus.h>
#include <machine/resource.h>

#include <vm/vm.h>
#include <vm/pmap.h>

#include <dev/pci/pcireg.h>
#include <dev/pci/pcivar.h>

#include <sys/sglist.h>

#include <dev/virtio/virtio.h>
#include <dev/virtio/virtqueue.h>
#include <dev/virtio/gpu/virtio_gpu.h>

#include "atrium_gpu.h"

/* virtio device-type id for GPU (independent of PCI vendor/device). */
#define ATRIUM_VIRTIO_ID_GPU     16   /* VIRTIO_ID_GPU */
#define VIRTIO_PCI_VENDOR        0x1af4
#define VIRTIO_GPU_PCI_DEVICE    0x1050

/* Feature mask. v0.1 negotiates none; later steps add EDID and BLOB. */
#define ATRIUM_VIRTIO_GPU_FEATURES  0

static struct virtio_feature_desc atrium_virtio_gpu_feature_desc[] = {
	{ VIRTIO_GPU_F_VIRGL,         "VirGL"       },
	{ VIRTIO_GPU_F_EDID,          "EDID"        },
	{ VIRTIO_GPU_F_RESOURCE_UUID, "ResUUID"     },
	{ VIRTIO_GPU_F_RESOURCE_BLOB, "ResBlob"     },
	{ VIRTIO_GPU_F_CONTEXT_INIT,  "ContextInit" },
	{ 0, NULL }
};

MALLOC_DEFINE(M_ATRIUM_GPU, "atrium_gpu", "Atrium GPU driver memory");

/* ------------------------------------------------------------------------- */
/* Device softc                                                               */
/* ------------------------------------------------------------------------- */

/*
 * BOs.
 *
 * Each BO is a contiguously-physically-allocated region. The kva is the
 * kernel mapping (so we can later DMA it into virtio-gpu's
 * RESOURCE_ATTACH_BACKING in step 2c); the pa is the start physical
 * address, and size is page-aligned. Userspace receives:
 *   - handle (u32) — opaque ABI handle
 *   - mmap_offset = handle * ATRIUM_BO_STRIDE — synthetic; lets
 *     `d_mmap` decode the BO from the offset alone (cdev mmap doesn't
 *     get fd context).
 *
 * BO_STRIDE = 1 GiB; BO_MAX_SIZE = 1 GiB. Plenty for v0.1.
 */
#define ATRIUM_BO_STRIDE     (1ULL << 30)
#define ATRIUM_BO_MAX_SIZE   ATRIUM_BO_STRIDE

struct atrium_gpu_bo {
	TAILQ_ENTRY(atrium_gpu_bo) link;
	uint32_t                   handle;
	uint32_t                   flags;
	uint64_t                   size;
	uint64_t                   mmap_offset;
	vm_offset_t                kva;
	vm_paddr_t                 pa;
	struct atrium_gpu_file    *owner;

	/* virtio-gpu resource binding, set lazily by IOC_SET_MODE. 0 = unbound. */
	uint32_t                   virtio_resource_id;
	uint32_t                   scanout_format;
	uint32_t                   scanout_width;
	uint32_t                   scanout_height;
};

TAILQ_HEAD(atrium_gpu_bo_list, atrium_gpu_bo);

struct atrium_gpu_softc {
	device_t                       dev;
	struct mtx                     lock;       /* general softc */
	struct mtx                     ctrl_lock;  /* serialises controlq */
	struct mtx                     bo_lock;    /* BO list + next_handle */

	/* Virtio plumbing. */
	uint64_t                       features;
	struct virtqueue              *ctrl_vq;
	struct virtio_gpu_config       gpucfg;

	/* Controlq completion signalling. Submitters serialise on
	 * `ctrl_lock` (one in-flight at a time), enqueue+notify, then
	 * cv_wait on this condvar; the controlq interrupt callback
	 * dequeues + cv_signals. The `ctrl_done` flag guards against
	 * spurious wakeups + loop-on-condition. Replaces an earlier
	 * busy-poll-without-callback shortcut that deadlocked on modern
	 * MSI-X virtio plumbing (host fires the IRQ at request
	 * completion; with no callback registered the IRQ stayed pending
	 * and starved the CPU shortly after attach returned). */
	struct cv                      ctrl_done_cv;
	bool                           ctrl_done;

	/* Latest GET_DISPLAY_INFO response, cached for IOC_CAPS / debug. */
	struct virtio_gpu_resp_display_info display_info;

	/* Monotonic fence-id source for virtio-gpu protocol fences. */
	uint64_t                       next_fence;

	/* BO table. */
	struct atrium_gpu_bo_list      bos;
	uint32_t                       next_handle;
	uint32_t                       next_resource_id;  /* virtio-gpu side */

	/* Cdevs. */
	struct cdev                   *gpu_cdev;
	struct cdev                   *display_cdev;

	/* Inferred capability snapshot for IOC_CAPS. Filled in attach. */
	struct atrium_gpu_caps         caps;
};

/* ------------------------------------------------------------------------- */
/* Per-fd state — one per open(/dev/atrium-gpu0).                             */
/*                                                                            */
/* Step 1: just a pointer back to the softc. Step 2 grows: BO handle table,   */
/* per-context fence counters, mmap-offset allocator, fd reference for the    */
/* display cdev's IOC_BIND_GPU.                                               */
/* ------------------------------------------------------------------------- */

struct atrium_gpu_file {
	struct atrium_gpu_softc *sc;
};

/* Forward declarations: virtio-gpu helpers used by display ioctl handlers
 * which appear textually before the helpers in this file. */
static int atrium_vgpu_resource_create_2d(struct atrium_gpu_softc *,
    uint32_t, uint32_t, uint32_t, uint32_t);
static int atrium_vgpu_attach_backing_single(struct atrium_gpu_softc *,
    uint32_t, vm_paddr_t, uint32_t);
static int atrium_vgpu_set_scanout(struct atrium_gpu_softc *, uint32_t,
    uint32_t, uint32_t, uint32_t);
static int atrium_vgpu_transfer_to_host_2d(struct atrium_gpu_softc *,
    uint32_t, uint32_t, uint32_t, uint32_t, uint32_t, uint64_t);
static int atrium_vgpu_resource_flush(struct atrium_gpu_softc *,
    uint32_t, uint32_t, uint32_t, uint32_t, uint32_t);

struct atrium_display_file {
	struct atrium_gpu_softc *sc;
	bool                     bound;       /* IOC_BIND_GPU completed */
	/*
	 * v0.1 caveat: we don't store the gpu_file pointer here. BIND_GPU
	 * only validates that the supplied fd points at our gpu cdev;
	 * subsequent display ops resolve BO handles against the softc-wide
	 * BO table. This is sufficient for fresco-server (single process,
	 * single gpu cdev). Step 4 will add per-fp namespace enforcement.
	 */
};

/* ------------------------------------------------------------------------- */
/* BO helpers                                                                 */
/* ------------------------------------------------------------------------- */

/* Find a BO by handle. Caller holds bo_lock. */
static struct atrium_gpu_bo *
atrium_bo_find_locked(struct atrium_gpu_softc *sc, uint32_t handle)
{
	struct atrium_gpu_bo *bo;

	TAILQ_FOREACH(bo, &sc->bos, link) {
		if (bo->handle == handle)
			return (bo);
	}
	return (NULL);
}

/* Free the underlying memory + the descriptor. Caller must have already
 * removed the BO from the list. */
static void
atrium_bo_free(struct atrium_gpu_bo *bo)
{
	if (bo->kva != 0)
		free((void *)bo->kva, M_ATRIUM_GPU);
	free(bo, M_ATRIUM_GPU);
}

/* On fd close, drop every BO this fd owned. */
static void
atrium_gpu_file_dtor(void *arg)
{
	struct atrium_gpu_file *f = arg;
	struct atrium_gpu_softc *sc = f->sc;
	struct atrium_gpu_bo *bo, *tmp;
	struct atrium_gpu_bo_list orphans;

	TAILQ_INIT(&orphans);
	mtx_lock(&sc->bo_lock);
	TAILQ_FOREACH_SAFE(bo, &sc->bos, link, tmp) {
		if (bo->owner == f) {
			TAILQ_REMOVE(&sc->bos, bo, link);
			TAILQ_INSERT_TAIL(&orphans, bo, link);
		}
	}
	mtx_unlock(&sc->bo_lock);

	while ((bo = TAILQ_FIRST(&orphans)) != NULL) {
		TAILQ_REMOVE(&orphans, bo, link);
		atrium_bo_free(bo);
	}
	free(f, M_ATRIUM_GPU);
}

/* Display files have no per-fd state to track today (BIND_GPU is a
 * no-op in v0.1). Trivial dtor. */
static void
atrium_display_file_dtor(void *arg)
{
	free(arg, M_ATRIUM_GPU);
}

/* ------------------------------------------------------------------------- */
/* /dev/atrium-gpu0 cdevsw                                                    */
/* ------------------------------------------------------------------------- */

static d_open_t  atrium_gpu_open;
static d_close_t atrium_gpu_close;
static d_ioctl_t atrium_gpu_ioctl;
static d_mmap_t  atrium_gpu_mmap;

static struct cdevsw atrium_gpu_cdevsw = {
	.d_version = D_VERSION,
	.d_open    = atrium_gpu_open,
	.d_close   = atrium_gpu_close,
	.d_ioctl   = atrium_gpu_ioctl,
	.d_mmap    = atrium_gpu_mmap,
	.d_name    = "atrium-gpu",
};

static int
atrium_gpu_open(struct cdev *cdev, int oflags __unused, int devtype __unused,
    struct thread *td __unused)
{
	struct atrium_gpu_softc *sc = cdev->si_drv1;
	struct atrium_gpu_file *f;

	f = malloc(sizeof(*f), M_ATRIUM_GPU, M_WAITOK | M_ZERO);
	f->sc = sc;
	devfs_set_cdevpriv(f, atrium_gpu_file_dtor);
	return (0);
}

static int
atrium_gpu_close(struct cdev *cdev __unused, int fflag __unused,
    int devtype __unused, struct thread *td __unused)
{
	/* devfs invokes the priv destructor; nothing else to do here. */
	return (0);
}

static int
atrium_gpu_ioc_alloc(struct atrium_gpu_softc *sc, struct atrium_gpu_file *f,
    struct atrium_gpu_alloc *args)
{
	uint64_t aligned;
	struct atrium_gpu_bo *bo;
	void *mem;

	if (args->size == 0 || args->size > ATRIUM_BO_MAX_SIZE)
		return (EINVAL);
	aligned = roundup2(args->size, PAGE_SIZE);

	bo = malloc(sizeof(*bo), M_ATRIUM_GPU, M_WAITOK | M_ZERO);
	mem = contigmalloc(aligned, M_ATRIUM_GPU, M_WAITOK | M_ZERO,
	    0, ~0UL, PAGE_SIZE, 0);
	if (mem == NULL) {
		free(bo, M_ATRIUM_GPU);
		return (ENOMEM);
	}
	bo->kva   = (vm_offset_t)mem;
	bo->pa    = pmap_kextract(bo->kva);
	bo->size  = aligned;
	bo->flags = args->flags;
	bo->owner = f;

	mtx_lock(&sc->bo_lock);
	bo->handle      = ++sc->next_handle;   /* never returns 0 */
	bo->mmap_offset = (uint64_t)bo->handle * ATRIUM_BO_STRIDE;
	TAILQ_INSERT_TAIL(&sc->bos, bo, link);
	mtx_unlock(&sc->bo_lock);

	args->handle      = bo->handle;
	args->mmap_offset = bo->mmap_offset;
	return (0);
}

static int
atrium_gpu_ioc_free(struct atrium_gpu_softc *sc, struct atrium_gpu_file *f,
    uint32_t handle)
{
	struct atrium_gpu_bo *bo;

	mtx_lock(&sc->bo_lock);
	bo = atrium_bo_find_locked(sc, handle);
	if (bo == NULL || bo->owner != f) {
		mtx_unlock(&sc->bo_lock);
		return (ENOENT);
	}
	TAILQ_REMOVE(&sc->bos, bo, link);
	mtx_unlock(&sc->bo_lock);
	atrium_bo_free(bo);
	return (0);
}

/*
 * IOC_SUBMIT — push a virtio-gpu protocol command at cmd_handle:cmd_offset
 * onto the controlq, wait for the response, and return a synthetic
 * monotonic fence.
 *
 * v0.1 limitations:
 *  - Synchronous: the fence is already retired by the time we return.
 *    `IOC_FENCE_WAIT` is therefore a no-op. Async machinery (interrupt-
 *    driven fence retirement + per-fd kqueue) arrives in step 2d.
 *  - One command per submit (the request bytes are interpreted as a
 *    single virtio-gpu protocol message).
 *  - No `wait_fences` (returns EINVAL if any specified).
 *  - No response data is surfaced to userspace; we only check the status
 *    word. `RESP_OK_NODATA` and `RESP_OK_DISPLAY_INFO` are accepted; any
 *    error from the device returns EIO. Server is responsible for
 *    consistency of resource_ids, formats, etc.
 *  - Concurrent `IOC_FREE` on `cmd_handle` while submit is in flight is
 *    UB; the server MUST serialise.
 */
static int
atrium_gpu_ioc_submit(struct atrium_gpu_softc *sc, struct atrium_gpu_file *f,
    struct atrium_gpu_submit *args)
{
	struct atrium_gpu_bo *cmd_bo;
	struct virtio_gpu_ctrl_hdr resp;
	struct sglist sg;
	struct sglist_seg segs[2];
	void *cmd_va;
	uint32_t resp_type;
	int err;

	if (args->engine != FRESCO_ENGINE_GRAPHICS)
		return (EINVAL);
	if ((args->flags & ~(uint32_t)FRESCO_SUBMIT_HIGH_PRIORITY) != 0)
		return (EINVAL);
	if (args->wait_fence_count != 0)
		return (EINVAL);
	if (args->cmd_size == 0 || args->cmd_size > 4096)
		return (EINVAL);

	mtx_lock(&sc->bo_lock);
	cmd_bo = atrium_bo_find_locked(sc, args->cmd_handle);
	if (cmd_bo == NULL || cmd_bo->owner != f) {
		mtx_unlock(&sc->bo_lock);
		return (ENOENT);
	}
	if (args->cmd_offset > cmd_bo->size ||
	    args->cmd_offset + args->cmd_size > cmd_bo->size) {
		mtx_unlock(&sc->bo_lock);
		return (EINVAL);
	}
	cmd_va = (void *)(cmd_bo->kva + (vm_offset_t)args->cmd_offset);
	mtx_unlock(&sc->bo_lock);

	bzero(&resp, sizeof(resp));
	sglist_init(&sg, 2, segs);
	if ((err = sglist_append(&sg, cmd_va, args->cmd_size)) != 0)
		return (err);
	if ((err = sglist_append(&sg, &resp, sizeof(resp))) != 0)
		return (err);

	mtx_lock(&sc->ctrl_lock);
	err = virtqueue_enqueue(sc->ctrl_vq, &resp, &sg, 1, 1);
	if (err == 0) {
		virtqueue_notify(sc->ctrl_vq);
		virtqueue_poll(sc->ctrl_vq, NULL);
	}
	mtx_unlock(&sc->ctrl_lock);
	if (err != 0)
		return (err);

	resp_type = le32toh(resp.type);
	if (resp_type != VIRTIO_GPU_RESP_OK_NODATA &&
	    resp_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO) {
		device_printf(sc->dev, "submit: virtio-gpu resp 0x%x\n",
		    resp_type);
		return (EIO);
	}

	args->fence_out = atomic_fetchadd_64(&sc->next_fence, 1);
	return (0);
}

static int
atrium_gpu_ioctl(struct cdev *cdev, u_long cmd, caddr_t data,
    int fflag __unused, struct thread *td __unused)
{
	struct atrium_gpu_softc *sc = cdev->si_drv1;
	struct atrium_gpu_file *f;
	int err;

	err = devfs_get_cdevpriv((void **)&f);
	if (err != 0)
		return (err);

	switch (cmd) {
	case ATRIUM_GPU_IOC_CAPS: {
		struct atrium_gpu_caps *ucaps = (struct atrium_gpu_caps *)data;
		*ucaps = sc->caps;
		return (0);
	}
	case ATRIUM_GPU_IOC_ALLOC:
		return (atrium_gpu_ioc_alloc(sc, f,
		    (struct atrium_gpu_alloc *)data));
	case ATRIUM_GPU_IOC_FREE:
		return (atrium_gpu_ioc_free(sc, f, *(uint32_t *)data));

	case ATRIUM_GPU_IOC_SYNC:
		/* Coherent (snooped) BOs only in v0.1; sync is a no-op. */
		return (0);

	case ATRIUM_GPU_IOC_SUBMIT:
		return (atrium_gpu_ioc_submit(sc, f,
		    (struct atrium_gpu_submit *)data));
	case ATRIUM_GPU_IOC_FENCE_WAIT:
		/* v0.1 IOC_SUBMIT is synchronous, so any fence we ever
		 * returned is already retired. */
		return (0);
	case ATRIUM_GPU_IOC_FENCE_QUERY: {
		struct atrium_gpu_fence_query *q =
		    (struct atrium_gpu_fence_query *)data;
		if (q->engine != FRESCO_ENGINE_GRAPHICS)
			return (EINVAL);
		q->latest_retired = atomic_load_64(&sc->next_fence) - 1;
		return (0);
	}

	default:
		return (ENOTTY);
	}
}

static int
atrium_gpu_mmap(struct cdev *cdev, vm_ooffset_t offset, vm_paddr_t *paddr,
    int nprot __unused, vm_memattr_t *memattr __unused)
{
	struct atrium_gpu_softc *sc = cdev->si_drv1;
	struct atrium_gpu_bo *bo;
	uint64_t bo_off;

	mtx_lock(&sc->bo_lock);
	TAILQ_FOREACH(bo, &sc->bos, link) {
		if ((uint64_t)offset < bo->mmap_offset)
			continue;
		bo_off = (uint64_t)offset - bo->mmap_offset;
		if (bo_off >= bo->size)
			continue;
		*paddr = bo->pa + bo_off;
		mtx_unlock(&sc->bo_lock);
		return (0);
	}
	mtx_unlock(&sc->bo_lock);
	return (EINVAL);
}

/* ------------------------------------------------------------------------- */
/* /dev/atrium-display0 cdevsw                                                */
/* ------------------------------------------------------------------------- */

static d_open_t  atrium_display_open;
static d_close_t atrium_display_close;
static d_ioctl_t atrium_display_ioctl;

static struct cdevsw atrium_display_cdevsw = {
	.d_version = D_VERSION,
	.d_open    = atrium_display_open,
	.d_close   = atrium_display_close,
	.d_ioctl   = atrium_display_ioctl,
	.d_name    = "atrium-display",
};

static int
atrium_display_open(struct cdev *cdev, int oflags __unused, int devtype __unused,
    struct thread *td __unused)
{
	struct atrium_gpu_softc *sc = cdev->si_drv1;
	struct atrium_display_file *f;

	f = malloc(sizeof(*f), M_ATRIUM_GPU, M_WAITOK | M_ZERO);
	f->sc = sc;
	devfs_set_cdevpriv(f, atrium_display_file_dtor);
	return (0);
}

static int
atrium_display_close(struct cdev *cdev __unused, int fflag __unused,
    int devtype __unused, struct thread *td __unused)
{
	return (0);
}

/*
 * IOC_BIND_GPU resolves an integer fd to its kernel `file *`, walks
 * the cdevsw, and verifies it points at /dev/atrium-gpu0 from the same
 * softc. This is capsicum-safe — no /dev path lookup, just an fd table
 * dereference the caller already has rights to.
 */
static int
atrium_display_bind_gpu(struct atrium_display_file *df,
    struct atrium_display_bind_gpu *args, struct thread *td)
{
	struct file *fp;
	struct cdev *gpu_cdev;
	cap_rights_t rights;
	int err;

	cap_rights_init(&rights, CAP_IOCTL);
	err = fget(td, args->gpu_fd, &rights, &fp);
	if (err != 0)
		return (err);

	if (fp->f_type != DTYPE_VNODE || fp->f_vnode == NULL ||
	    fp->f_vnode->v_type != VCHR) {
		fdrop(fp, td);
		return (EINVAL);
	}
	gpu_cdev = fp->f_vnode->v_rdev;
	if (gpu_cdev == NULL || gpu_cdev->si_devsw != &atrium_gpu_cdevsw ||
	    gpu_cdev->si_drv1 != df->sc) {
		fdrop(fp, td);
		return (EINVAL);
	}

	/*
	 * Step 2 will resolve fp's cdev priv to the atrium_gpu_file and
	 * take a real reference. For step 1 we accept the bind once we've
	 * verified the fd points at our own gpu cdev — that's enough for
	 * userspace to exercise the IOC_BIND_GPU success path.
	 */
	df->bound = true;
	fdrop(fp, td);
	return (0);
}

/* Promote a CPU-allocated BO to a virtio-gpu resource (CREATE_2D +
 * ATTACH_BACKING). Idempotent: if already promoted at the same
 * dimensions/format, returns immediately. Caller MUST NOT hold bo_lock
 * (helpers take ctrl_lock and may sleep). */
static int
atrium_promote_bo_to_resource(struct atrium_gpu_softc *sc,
    struct atrium_gpu_bo *bo, uint32_t format, uint32_t w, uint32_t h)
{
	uint32_t rid;
	uint64_t need;
	int err;

	need = (uint64_t)w * h * 4;  /* 4 bytes/pixel */
	if (need == 0 || need > bo->size)
		return (EINVAL);

	if (bo->virtio_resource_id != 0 &&
	    bo->scanout_format == format &&
	    bo->scanout_width  == w &&
	    bo->scanout_height == h)
		return (0);
	if (bo->virtio_resource_id != 0)
		return (EBUSY);  /* re-bind not supported in v0.1 */

	mtx_lock(&sc->bo_lock);
	rid = sc->next_resource_id++;
	mtx_unlock(&sc->bo_lock);

	if ((err = atrium_vgpu_resource_create_2d(sc, rid, format, w, h)) != 0)
		return (err);
	if ((err = atrium_vgpu_attach_backing_single(sc, rid, bo->pa,
	    (uint32_t)need)) != 0)
		return (err);

	bo->virtio_resource_id = rid;
	bo->scanout_format     = format;
	bo->scanout_width      = w;
	bo->scanout_height     = h;
	return (0);
}

static int
atrium_display_enum_connectors(struct atrium_gpu_softc *sc,
    struct atrium_display_enum *args)
{
	struct atrium_display_connector kc;
	uint32_t out = 0, i;
	int err;

	for (i = 0; i < sc->gpucfg.num_scanouts &&
	    i < VIRTIO_GPU_MAX_SCANOUTS; i++) {
		if (sc->display_info.pmodes[i].enabled == 0)
			continue;
		if (out < args->count_in) {
			bzero(&kc, sizeof(kc));
			kc.id    = i;
			kc.type  = FRESCO_CONNECTOR_VIRTUAL;
			kc.flags = FRESCO_CONNECTOR_FLAG_CONNECTED;
			kc.edid_size = 0;  /* virtio-gpu w/o F_EDID = no EDID */
			err = copyout(&kc,
			    (void *)((char *)args->connectors_ptr +
			        out * sizeof(kc)),
			    sizeof(kc));
			if (err != 0)
				return (err);
		}
		out++;
	}
	args->count_out = out;
	return (0);
}

static int
atrium_display_modes(struct atrium_gpu_softc *sc,
    struct atrium_display_modes_query *args)
{
	struct atrium_display_mode km;
	uint32_t i = args->connector_id;
	int err;

	if (i >= sc->gpucfg.num_scanouts || i >= VIRTIO_GPU_MAX_SCANOUTS ||
	    sc->display_info.pmodes[i].enabled == 0)
		return (ENOENT);

	bzero(&km, sizeof(km));
	km.width           = le32toh(sc->display_info.pmodes[i].r.width);
	km.height          = le32toh(sc->display_info.pmodes[i].r.height);
	km.refresh_mhz     = 60000;
	km.pixel_clock_khz = (km.width * km.height * 60) / 1000;
	km.flags           = FRESCO_MODE_FLAG_PREFERRED;

	if (args->count_in > 0) {
		err = copyout(&km, (void *)args->modes_ptr, sizeof(km));
		if (err != 0)
			return (err);
	}
	args->count_out = 1;
	return (0);
}

static int
atrium_display_set_mode(struct atrium_gpu_softc *sc,
    struct atrium_display_set_mode *args)
{
	struct atrium_gpu_bo *bo;
	uint32_t w, h, rid;
	int err;

	if (args->connector_id >= sc->gpucfg.num_scanouts)
		return (ENOENT);
	w = args->mode.width;
	h = args->mode.height;
	if (w == 0 || h == 0)
		return (EINVAL);

	mtx_lock(&sc->bo_lock);
	bo = atrium_bo_find_locked(sc, args->scanout_handle);
	mtx_unlock(&sc->bo_lock);
	if (bo == NULL)
		return (ENOENT);

	if ((err = atrium_promote_bo_to_resource(sc, bo,
	    VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM, w, h)) != 0)
		return (err);
	rid = bo->virtio_resource_id;

	return (atrium_vgpu_set_scanout(sc, args->connector_id, rid, w, h));
}

static int
atrium_display_page_flip(struct atrium_gpu_softc *sc,
    struct atrium_display_page_flip *args)
{
	struct atrium_gpu_bo *bo;
	uint32_t rid, w, h;
	int err;

	if (args->flags & FRESCO_PAGE_FLIP_INCLUDE_CURSOR)
		return (EINVAL);  /* reserved for v0.2 */

	mtx_lock(&sc->bo_lock);
	bo = atrium_bo_find_locked(sc, args->scanout_handle);
	mtx_unlock(&sc->bo_lock);
	if (bo == NULL || bo->virtio_resource_id == 0)
		return (ENOENT);

	rid = bo->virtio_resource_id;
	w   = bo->scanout_width;
	h   = bo->scanout_height;

	if ((err = atrium_vgpu_transfer_to_host_2d(sc, rid, 0, 0, w, h, 0)) != 0)
		return (err);
	return (atrium_vgpu_resource_flush(sc, rid, 0, 0, w, h));
}

static int
atrium_display_ioctl(struct cdev *cdev __unused, u_long cmd, caddr_t data,
    int fflag __unused, struct thread *td)
{
	struct atrium_display_file *df;
	int err;

	err = devfs_get_cdevpriv((void **)&df);
	if (err != 0)
		return (err);

	switch (cmd) {
	case ATRIUM_DISPLAY_IOC_BIND_GPU:
		return (atrium_display_bind_gpu(df,
		    (struct atrium_display_bind_gpu *)data, td));

	/* Read-only enumeration is allowed pre-bind, per spec §6.1. */
	case ATRIUM_DISPLAY_IOC_ENUM_CONNECTORS:
		return (atrium_display_enum_connectors(df->sc,
		    (struct atrium_display_enum *)data));
	case ATRIUM_DISPLAY_IOC_MODES:
		return (atrium_display_modes(df->sc,
		    (struct atrium_display_modes_query *)data));

	/* Anything that touches a BO requires BIND_GPU first. */
	case ATRIUM_DISPLAY_IOC_SET_MODE:
		if (!df->bound)
			return (EINVAL);
		return (atrium_display_set_mode(df->sc,
		    (struct atrium_display_set_mode *)data));
	case ATRIUM_DISPLAY_IOC_PAGE_FLIP:
		if (!df->bound)
			return (EINVAL);
		return (atrium_display_page_flip(df->sc,
		    (struct atrium_display_page_flip *)data));
	case ATRIUM_DISPLAY_IOC_CURSOR:
		return (EOPNOTSUPP);  /* cursorq deferred */

	default:
		return (ENOTTY);
	}
}

/* ------------------------------------------------------------------------- */
/* PCI attach                                                                 */
/* ------------------------------------------------------------------------- */

/* ------------------------------------------------------------------------- */
/* Virtio-gpu controlq helpers                                                */
/* ------------------------------------------------------------------------- */

/*
 * Controlq interrupt callback. Drains every completed request from the
 * used ring, then signals waiters on `ctrl_done_cv`. With one in-flight
 * request at a time (enforced by `ctrl_lock`), the dequeue loop runs
 * exactly once per host completion; the loop form is defensive against
 * batched completions if we later allow multiple in-flight.
 */
static void
atrium_vgpu_ctrl_intr(void *xsc)
{
	struct atrium_gpu_softc *sc = xsc;

	mtx_lock(&sc->ctrl_lock);
	while (virtqueue_dequeue(sc->ctrl_vq, NULL) != NULL)
		;
	sc->ctrl_done = true;
	cv_signal(&sc->ctrl_done_cv);
	mtx_unlock(&sc->ctrl_lock);
}

/*
 * Synchronous request/response on the controlq. The caller supplies a
 * filled-in request struct and an out-buffer for the response. We build
 * a 2-segment scatter-gather list, enqueue under `ctrl_lock`, kick the
 * device, then `cv_wait` until the controlq IRQ callback dequeues our
 * response and sets `ctrl_done`. Serialised by `ctrl_lock` so we have
 * exactly one in-flight request at a time — that lets us use a simple
 * one-bit completion flag without per-cookie tracking.
 *
 * Earlier versions called `virtqueue_poll` here under the same lock,
 * which deadlocked on modern -CURRENT once MSI-X delivery for the
 * controlq became reliable: the host fired the IRQ but we'd registered
 * NULL as the per-VQ callback at attach time, so the IRQ stayed
 * asserted and starved the CPU shortly after attach returned. The

 * callback + cv_wait pair is the standard FreeBSD-virtio idiom; see
 * the per-driver files under sys/dev/virtio/ for analogues.
 */
static int
atrium_vgpu_req_resp(struct atrium_gpu_softc *sc,
    void *req, size_t reqlen, void *resp, size_t resplen)
{
	struct sglist sg;
	struct sglist_seg segs[2];
	int err;

	sglist_init(&sg, 2, segs);
	if ((err = sglist_append(&sg, req, reqlen)) != 0)
		return (err);
	if ((err = sglist_append(&sg, resp, resplen)) != 0)
		return (err);

	mtx_lock(&sc->ctrl_lock);
	sc->ctrl_done = false;
	err = virtqueue_enqueue(sc->ctrl_vq, resp, &sg, 1, 1);
	if (err == 0) {
		virtqueue_notify(sc->ctrl_vq);
		while (!sc->ctrl_done)
			cv_wait(&sc->ctrl_done_cv, &sc->ctrl_lock);
	}
	mtx_unlock(&sc->ctrl_lock);
	return (err);
}

static int
atrium_vgpu_get_display_info(struct atrium_gpu_softc *sc)
{
	struct {
		struct virtio_gpu_ctrl_hdr req;
		char pad;
		struct virtio_gpu_resp_display_info resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.type = htole32(VIRTIO_GPU_CMD_GET_DISPLAY_INFO);
	s.req.flags = htole32(VIRTIO_GPU_FLAG_FENCE);
	s.req.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);

	sc->display_info = s.resp;

	for (uint32_t i = 0; i < sc->gpucfg.num_scanouts &&
	    i < VIRTIO_GPU_MAX_SCANOUTS; i++) {
		if (s.resp.pmodes[i].enabled == 0)
			continue;
		device_printf(sc->dev,
		    "scanout %u: %ux%u enabled\n", i,
		    le32toh(s.resp.pmodes[i].r.width),
		    le32toh(s.resp.pmodes[i].r.height));
	}
	return (0);
}

/*
 * Driver-issued virtio-gpu commands. Each helper builds a request on
 * the stack, calls atrium_vgpu_req_resp(), and verifies the host
 * returned RESP_OK_NODATA. The response is read into a stack buffer
 * and discarded since these commands carry no useful return data.
 */

static int
atrium_vgpu_check_ok(struct atrium_gpu_softc *sc, const char *what,
    struct virtio_gpu_ctrl_hdr *resp)
{
	uint32_t t = le32toh(resp->type);
	if (t == VIRTIO_GPU_RESP_OK_NODATA)
		return (0);
	device_printf(sc->dev, "%s: virtio-gpu resp 0x%x\n", what, t);
	return (EIO);
}

static int
atrium_vgpu_resource_create_2d(struct atrium_gpu_softc *sc,
    uint32_t resource_id, uint32_t format, uint32_t w, uint32_t h)
{
	struct {
		struct virtio_gpu_resource_create_2d req;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.resource_id  = htole32(resource_id);
	s.req.format       = htole32(format);
	s.req.width        = htole32(w);
	s.req.height       = htole32(h);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "RESOURCE_CREATE_2D", &s.resp));
}

static int
atrium_vgpu_attach_backing_single(struct atrium_gpu_softc *sc,
    uint32_t resource_id, vm_paddr_t pa, uint32_t length)
{
	struct {
		struct virtio_gpu_resource_attach_backing req;
		struct virtio_gpu_mem_entry              ent;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.resource_id  = htole32(resource_id);
	s.req.nr_entries   = htole32(1);
	s.ent.addr         = htole64((uint64_t)pa);
	s.ent.length       = htole32(length);

	/* Request is the header struct + one mem_entry, contiguous. */
	err = atrium_vgpu_req_resp(sc,
	    &s.req, sizeof(s.req) + sizeof(s.ent),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "RESOURCE_ATTACH_BACKING", &s.resp));
}

static int
atrium_vgpu_set_scanout(struct atrium_gpu_softc *sc, uint32_t scanout_id,
    uint32_t resource_id, uint32_t w, uint32_t h)
{
	struct {
		struct virtio_gpu_set_scanout req;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_SET_SCANOUT);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.r.x          = 0;
	s.req.r.y          = 0;
	s.req.r.width      = htole32(w);
	s.req.r.height     = htole32(h);
	s.req.scanout_id   = htole32(scanout_id);
	s.req.resource_id  = htole32(resource_id);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "SET_SCANOUT", &s.resp));
}

static int
atrium_vgpu_transfer_to_host_2d(struct atrium_gpu_softc *sc,
    uint32_t resource_id, uint32_t x, uint32_t y, uint32_t w, uint32_t h,
    uint64_t offset)
{
	struct {
		struct virtio_gpu_transfer_to_host_2d req;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.r.x          = htole32(x);
	s.req.r.y          = htole32(y);
	s.req.r.width      = htole32(w);
	s.req.r.height     = htole32(h);
	s.req.offset       = htole64(offset);
	s.req.resource_id  = htole32(resource_id);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "TRANSFER_TO_HOST_2D", &s.resp));
}

static int
atrium_vgpu_resource_flush(struct atrium_gpu_softc *sc, uint32_t resource_id,
    uint32_t x, uint32_t y, uint32_t w, uint32_t h)
{
	struct {
		struct virtio_gpu_resource_flush req;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_RESOURCE_FLUSH);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.r.x          = htole32(x);
	s.req.r.y          = htole32(y);
	s.req.r.width      = htole32(w);
	s.req.r.height     = htole32(h);
	s.req.resource_id  = htole32(resource_id);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "RESOURCE_FLUSH", &s.resp));
}

/* ------------------------------------------------------------------------- */
/* Probe / attach                                                              */
/* ------------------------------------------------------------------------- */

static int
atrium_virtio_gpu_probe(device_t dev)
{
	if (virtio_get_device_type(dev) != ATRIUM_VIRTIO_ID_GPU)
		return (ENXIO);
	device_set_desc(dev, "Atrium virtio-gpu (native, no linuxkpi)");
	return (BUS_PROBE_DEFAULT);
}

static void
atrium_fill_caps(struct atrium_gpu_softc *sc)
{
	struct atrium_gpu_caps *c = &sc->caps;

	c->version_major = ATRIUM_GPU_ABI_VERSION_MAJOR;
	c->version_minor = ATRIUM_GPU_ABI_VERSION_MINOR;
	c->vendor_id     = VIRTIO_PCI_VENDOR;
	c->device_id     = VIRTIO_GPU_PCI_DEVICE;
	strlcpy(c->family, "virtio-gpu", sizeof(c->family));

	c->vram_total_bytes              = 0;          /* virtio-gpu has no dedicated VRAM */
	c->system_memory_visible_bytes   = 0;          /* not bounded; allocator-driven */
	c->max_texture_2d                = 16384;      /* virtio-gpu commonly accepts up to 16k */
	c->max_texture_3d                = 0;
	c->max_buffer_size_log2          = 30;         /* 1 GiB hint */
	c->engine_mask                   = 1u << FRESCO_ENGINE_GRAPHICS;
	c->feature_flags                 = 0;

	/* Surface a couple of features we negotiated for diagnostics. */
	if (sc->features & (1ULL << VIRTIO_GPU_F_VIRGL))
		c->feature_flags |= FRESCO_FEAT_COMPUTE;     /* coarse */
}

static int
atrium_virtio_gpu_attach(device_t dev)
{
	struct atrium_gpu_softc *sc = device_get_softc(dev);
	struct make_dev_args args;
	int err;

	sc->dev = dev;
	sc->next_fence = 1;
	sc->next_handle = 0;
	sc->next_resource_id = 1;
	TAILQ_INIT(&sc->bos);
	mtx_init(&sc->lock, "atrium-gpu", NULL, MTX_DEF);
	mtx_init(&sc->ctrl_lock, "atrium-gpu ctrl", NULL, MTX_DEF);
	mtx_init(&sc->bo_lock, "atrium-gpu bos", NULL, MTX_DEF);
	cv_init(&sc->ctrl_done_cv, "atrium-gpu ctrl done");
	sc->ctrl_done = false;

	/* virtio handshake. */
	virtio_set_feature_desc(dev, atrium_virtio_gpu_feature_desc);
	sc->features = virtio_negotiate_features(dev, ATRIUM_VIRTIO_GPU_FEATURES);
	if ((err = virtio_finalize_features(dev)) != 0) {
		device_printf(dev, "virtio_finalize_features failed: %d\n", err);
		goto fail_locks;
	}

	/* Read GPU device config (num_scanouts, num_capsets, events_*). */
	bzero(&sc->gpucfg, sizeof(sc->gpucfg));
#define ATRIUM_VGPU_GET_CFG(_field)                                       \
	virtio_read_device_config(dev,                                    \
	    offsetof(struct virtio_gpu_config, _field),                   \
	    &sc->gpucfg._field, sizeof(sc->gpucfg._field))
	ATRIUM_VGPU_GET_CFG(events_read);
	ATRIUM_VGPU_GET_CFG(events_clear);
	ATRIUM_VGPU_GET_CFG(num_scanouts);
	ATRIUM_VGPU_GET_CFG(num_capsets);
#undef ATRIUM_VGPU_GET_CFG

	/* Allocate controlq. cursorq comes in step 3 with hw cursor. */
	{
		struct vq_alloc_info vq_info[1];
		VQ_ALLOC_INFO_INIT(&vq_info[0], 0, atrium_vgpu_ctrl_intr,
		    sc, &sc->ctrl_vq,
		    "%s control", device_get_nameunit(dev));
		if ((err = virtio_alloc_virtqueues(dev, 1, vq_info)) != 0) {
			device_printf(dev, "virtio_alloc_virtqueues: %d\n", err);
			goto fail_locks;
		}
	}

	if ((err = virtio_setup_intr(dev, INTR_TYPE_TTY)) != 0) {
		device_printf(dev, "virtio_setup_intr: %d\n", err);
		goto fail_locks;
	}

	if ((err = atrium_vgpu_get_display_info(sc)) != 0) {
		device_printf(dev, "GET_DISPLAY_INFO failed: %d\n", err);
		goto fail_locks;
	}

	atrium_fill_caps(sc);

	/* /dev/atrium-gpu0 */
	make_dev_args_init(&args);
	args.mda_devsw = &atrium_gpu_cdevsw;
	args.mda_uid   = UID_ROOT;
	args.mda_gid   = GID_WHEEL;
	args.mda_mode  = 0600;
	args.mda_si_drv1 = sc;
	err = make_dev_s(&args, &sc->gpu_cdev, "atrium-gpu0");
	if (err != 0) {
		device_printf(dev, "make_dev_s(atrium-gpu0) failed: %d\n", err);
		goto fail_lock;
	}

	/* /dev/atrium-display0 */
	make_dev_args_init(&args);
	args.mda_devsw = &atrium_display_cdevsw;
	args.mda_uid   = UID_ROOT;
	args.mda_gid   = GID_WHEEL;
	args.mda_mode  = 0600;
	args.mda_si_drv1 = sc;
	err = make_dev_s(&args, &sc->display_cdev, "atrium-display0");
	if (err != 0) {
		device_printf(dev, "make_dev_s(atrium-display0) failed: %d\n",
		    err);
		goto fail_gpu_cdev;
	}

	device_printf(dev,
	    "attached: /dev/atrium-gpu0 /dev/atrium-display0 "
	    "(ABI %u.%u, %s, %u scanout%s, features=0x%lx)\n",
	    sc->caps.version_major, sc->caps.version_minor, sc->caps.family,
	    sc->gpucfg.num_scanouts, sc->gpucfg.num_scanouts == 1 ? "" : "s",
	    (unsigned long)sc->features);
	return (0);

fail_gpu_cdev:
	destroy_dev(sc->gpu_cdev);
fail_lock:
fail_locks:
	cv_destroy(&sc->ctrl_done_cv);
	mtx_destroy(&sc->bo_lock);
	mtx_destroy(&sc->ctrl_lock);
	mtx_destroy(&sc->lock);
	return (err);
}

static int
atrium_virtio_gpu_detach(device_t dev)
{
	struct atrium_gpu_softc *sc = device_get_softc(dev);

	struct atrium_gpu_bo *bo;

	if (sc->display_cdev != NULL)
		destroy_dev(sc->display_cdev);
	if (sc->gpu_cdev != NULL)
		destroy_dev(sc->gpu_cdev);
	while ((bo = TAILQ_FIRST(&sc->bos)) != NULL) {
		TAILQ_REMOVE(&sc->bos, bo, link);
		atrium_bo_free(bo);
	}
	cv_destroy(&sc->ctrl_done_cv);
	mtx_destroy(&sc->bo_lock);
	mtx_destroy(&sc->ctrl_lock);
	mtx_destroy(&sc->lock);
	return (0);
}

/* ------------------------------------------------------------------------- */
/* Driver glue                                                                */
/* ------------------------------------------------------------------------- */

static device_method_t atrium_virtio_gpu_methods[] = {
	DEVMETHOD(device_probe,  atrium_virtio_gpu_probe),
	DEVMETHOD(device_attach, atrium_virtio_gpu_attach),
	DEVMETHOD(device_detach, atrium_virtio_gpu_detach),
	DEVMETHOD_END
};

static driver_t atrium_virtio_gpu_driver = {
	"atrium_virtio_gpu",
	atrium_virtio_gpu_methods,
	sizeof(struct atrium_gpu_softc),
};

VIRTIO_DRIVER_MODULE(atrium_virtio_gpu, atrium_virtio_gpu_driver, NULL, NULL);
MODULE_VERSION(atrium_virtio_gpu, 1);
MODULE_DEPEND(atrium_virtio_gpu, virtio, 1, 1, 1);
VIRTIO_SIMPLE_PNPINFO(atrium_virtio_gpu, ATRIUM_VIRTIO_ID_GPU,
    "Atrium virtio-gpu (native)");
