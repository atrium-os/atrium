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

/* Feature mask. CONTEXT_INIT lets us drive virtio-gpu's
 * SUBMIT_3D / CTX_CREATE machinery — required for venus paravirt
 * Vulkan. We negotiate it; if the host doesn't have it, the kmod
 * still drives the basic 2D/scanout path fine and venus ioctls
 * return ENOTSUP at runtime. */
#define ATRIUM_VIRTIO_GPU_FEATURES \
	((1ULL << VIRTIO_GPU_F_CONTEXT_INIT))

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

	/* Latest GET_DISPLAY_INFO response, cached for IOC_CAPS / debug.
	 * `display_info_valid` is set on first successful fetch; the
	 * fetch is lazy (deferred out of attach) because attach context
	 * on -CURRENT can't sleep — see atrium_vgpu_ensure_display_info. */
	struct virtio_gpu_resp_display_info display_info;
	bool                                display_info_valid;

	/* Monotonic fence-id source for virtio-gpu protocol fences. */
	uint64_t                       next_fence;

	/* BO table. */
	struct atrium_gpu_bo_list      bos;
	uint32_t                       next_handle;
	uint32_t                       next_resource_id;  /* virtio-gpu side */

	/* Monotonic per-driver context id allocator for CONTEXT_INIT.
	 * Never returns 0 (zero is reserved for "no context"). The
	 * counter is process-wide because virtio-gpu's context_id space
	 * is per-device, but it never escapes the kmod — Capsicum scope
	 * is preserved by keeping per-fd state the only addressable
	 * handle. */
	uint32_t                       next_ctx_id;

	/* Cdevs. */
	struct cdev                   *gpu_cdev;
	struct cdev                   *display_cdev;

	/* Inferred capability snapshot for IOC_CAPS. Filled in attach. */
	struct atrium_gpu_caps         caps;

	/* V5h: virtio-gpu host_visible shared-memory region (shmid=0).
	 * Populated at attach time by walking PCI vendor caps for
	 * cfg_type=VIRTIO_PCI_CAP_SHARED_MEMORY_CFG. shm_size==0 means
	 * the host didn't export a host_visible region (no -hostmem on
	 * the QEMU command line); HOST3D blob ioctls then return ENXIO
	 * and consumers must fall back to BLOB_MEM_GUEST. The BAR
	 * resource itself is allocated lazily on first HOST_BLOB request
	 * to avoid claiming MMIO we may never use. */
	uint8_t                        shm_bar;
	uint64_t                       shm_bar_offset; /* offset within BAR */
	uint64_t                       shm_pa;     /* host-visible base PA;
	                                              filled by shm_init_locked
	                                              after BAR is allocated */
	uint64_t                       shm_size;   /* 0 == no region */

	/* Lazily-acquired BAR resource + page-bitmap allocator over its
	 * window. shm_lock guards shm_bitmap; shm_res/shm_pa/shm_size are
	 * write-once at first IOC_HOST_BLOB so are read lock-free. */
	struct mtx                     shm_lock;
	uint8_t                       *shm_bitmap;
	uint32_t                       shm_n_pages;
	bool                           shm_initialized;
};

/* Internal-only BO flag: this BO is backed by a window of the
 * host_visible BAR (not contigmalloc'd guest pages). atrium_bo_free
 * uses it to skip the kva free and instead release the BAR window. */
#define ATRIUM_BO_INT_HOST_BLOB  0x10000u

/* Host-side page size for the BAR-window allocator. We pick the
 * worst-case host page (Apple Silicon = 16 KiB) regardless of the
 * guest page size — the host's mmap MAP_FIXED into a sub-range of
 * QEMU's hostmem mapping requires every BAR offset to be aligned
 * to the host's actual page size, and Darwin returns EINVAL otherwise. */
#define ATRIUM_HOST_PAGE_SHIFT 14
#define ATRIUM_HOST_PAGE_SIZE  (1u << ATRIUM_HOST_PAGE_SHIFT)

/* ------------------------------------------------------------------------- */
/* Per-fd state — one per open(/dev/atrium-gpu0).                             */
/*                                                                            */
/* Step 1: just a pointer back to the softc. Step 2 grows: BO handle table,   */
/* per-context fence counters, mmap-offset allocator, fd reference for the    */
/* display cdev's IOC_BIND_GPU.                                               */
/* ------------------------------------------------------------------------- */

struct atrium_gpu_file {
	struct atrium_gpu_softc *sc;

	/* Venus / virtio-gpu CONTEXT_INIT state. 0 = no context bound to
	 * this fd. Set by ATRIUM_GPU_IOC_CTX_INIT, torn down in the
	 * fd-priv destructor. Capsicum-shape: lookup is by fd, never by
	 * id; no other fd can address this context. */
	uint32_t                 ctx_id;
	uint32_t                 ctx_capset;

	/* Latest fence id we've enqueued on this context. Used by
	 * CTX_FENCE_WAIT to verify the requested fence belongs to this
	 * fd's stream. Coarse but sufficient for V4 (synchronous submit). */
	uint64_t                 ctx_last_fence;
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
static int atrium_vgpu_ensure_display_info(struct atrium_gpu_softc *);

/* V3: venus / context-init helpers (defined further down). */
static int atrium_vgpu_find_capset(struct atrium_gpu_softc *,
    uint32_t, uint32_t *, uint32_t *);
static int atrium_vgpu_get_capset(struct atrium_gpu_softc *,
    uint32_t, uint32_t, void *, size_t);
static int atrium_vgpu_ctx_create(struct atrium_gpu_softc *,
    uint32_t, uint32_t, const char *);
static int atrium_vgpu_ctx_destroy(struct atrium_gpu_softc *, uint32_t);

/* V4: resource attach + 3D submit helpers (defined further down). */
static int atrium_vgpu_resource_create_blob(struct atrium_gpu_softc *,
    uint32_t, uint32_t, uint32_t, uint32_t, uint64_t, uint64_t,
    vm_paddr_t, uint32_t);
static int atrium_vgpu_ctx_attach_resource(struct atrium_gpu_softc *,
    uint32_t, uint32_t);
static int atrium_vgpu_submit_3d(struct atrium_gpu_softc *,
    uint32_t, uint32_t, void *, size_t);

/* V5h: HOST3D blob + host_visible BAR allocator (defined further down). */
static int      atrium_vgpu_resource_map_blob(struct atrium_gpu_softc *,
                    uint32_t, uint32_t, uint64_t);
static int      atrium_vgpu_shm_init_locked(struct atrium_gpu_softc *);
static uint64_t atrium_vgpu_shm_alloc_locked(struct atrium_gpu_softc *,
                    uint32_t);
static void     atrium_vgpu_shm_free_locked(struct atrium_gpu_softc *,
                    uint64_t, uint32_t);

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
 * removed the BO from the list. For HOST_BLOB BOs the storage lives in
 * the host_visible BAR (host-allocated shm); we just release the BAR
 * window we reserved at allocation time. */
static void
atrium_bo_free(struct atrium_gpu_bo *bo)
{
	if (bo->flags & ATRIUM_BO_INT_HOST_BLOB) {
		struct atrium_gpu_softc *sc = bo->owner ? bo->owner->sc : NULL;
		if (sc != NULL && sc->shm_initialized) {
			uint64_t bar_offset = bo->pa - sc->shm_pa;
			uint32_t npages = bo->size / ATRIUM_HOST_PAGE_SIZE;
			mtx_lock(&sc->shm_lock);
			atrium_vgpu_shm_free_locked(sc, bar_offset, npages);
			mtx_unlock(&sc->shm_lock);
		}
	} else if (bo->kva != 0) {
		free((void *)bo->kva, M_ATRIUM_GPU);
	}
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

	/* Tear down virtio-gpu context (if any) before reclaiming BOs:
	 * the host renderer may hold references to BOs attached to this
	 * context, which won't release until CTX_DESTROY. Per-fd Capsicum
	 * scope: this is the only path that can destroy this context. */
	if (f->ctx_id != 0) {
		(void)atrium_vgpu_ctx_destroy(sc, f->ctx_id);
		f->ctx_id = 0;
	}

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

/* ATRIUM_GPU_IOC_CAPSET_QUERY: report whether `capset_id` is advertised
 * by the host renderer, plus the capset blob if data_ptr != NULL. Pure
 * read-side query — no per-fd state changed. */
static int
atrium_gpu_ioc_capset_query(struct atrium_gpu_softc *sc,
    struct atrium_gpu_file *f __unused,
    struct atrium_gpu_capset_query *args)
{
	uint32_t max_ver, max_size;
	void *blob;
	int err;

	err = atrium_vgpu_find_capset(sc, args->capset_id,
	    &max_ver, &max_size);
	if (err == ENOENT) {
		args->actual_version = 0;
		args->data_size      = 0;
		return (0);
	}
	if (err != 0)
		return (err);
	if (max_size > ATRIUM_GPU_CAPSET_DATA_MAX)
		return (E2BIG);

	args->actual_version = (args->capset_version == 0)
	    ? max_ver
	    : MIN(args->capset_version, max_ver);
	args->data_size = max_size;

	if (args->data_ptr == 0)
		return (0);  /* size-only query */

	blob = malloc(max_size + sizeof(struct virtio_gpu_ctrl_hdr),
	    M_ATRIUM_GPU, M_WAITOK | M_ZERO);
	err = atrium_vgpu_get_capset(sc, args->capset_id,
	    args->actual_version, blob,
	    max_size + sizeof(struct virtio_gpu_ctrl_hdr));
	if (err == 0) {
		struct virtio_gpu_resp_capset *r = blob;
		err = copyout(r->capset_data,
		    (void *)(uintptr_t)args->data_ptr, max_size);
	}
	free(blob, M_ATRIUM_GPU);
	return (err);
}

/* ATRIUM_GPU_IOC_CTX_INIT: bind a virtio-gpu context to this fd.
 * Capsicum-shape: the resulting context is reachable only through
 * `f`; no other fd can name or operate on it. */
static int
atrium_gpu_ioc_ctx_init(struct atrium_gpu_softc *sc,
    struct atrium_gpu_file *f, struct atrium_gpu_ctx_init *args)
{
	uint32_t ctx_id;
	int err;

	if (f->ctx_id != 0)
		return (EBUSY);
	if (args->flags != 0)
		return (EINVAL);
	if ((sc->features & (1ULL << VIRTIO_GPU_F_CONTEXT_INIT)) == 0)
		return (ENOTSUP);

	/* Confirm the host advertises the requested capset before
	 * burning a context id. */
	{
		uint32_t mv, ms;
		err = atrium_vgpu_find_capset(sc, args->capset_id, &mv, &ms);
		if (err == ENOENT)
			return (ENOTSUP);
		if (err != 0)
			return (err);
	}

	mtx_lock(&sc->lock);
	ctx_id = ++sc->next_ctx_id;
	mtx_unlock(&sc->lock);

	/* NUL-terminate debug_name defensively even though virtio_gpu
	 * uses an explicit length field. */
	args->debug_name[sizeof(args->debug_name) - 1] = '\0';
	err = atrium_vgpu_ctx_create(sc, ctx_id, args->capset_id,
	    args->debug_name);
	if (err != 0)
		return (err);

	f->ctx_id     = ctx_id;
	f->ctx_capset = args->capset_id;
	args->ctx_id_out = ctx_id;
	return (0);
}

/* ATRIUM_GPU_IOC_RESOURCE_ATTACH: bind a BO as a venus blob resource
 * on this fd's context. The BO must be ATRIUM_GPU_BO_GPU_VISIBLE.
 * Server-side resource_id is allocated from the kmod's monotonic
 * `next_resource_id` counter; userspace embeds it in subsequent
 * SUBMIT_3D command streams. */
static int
atrium_gpu_ioc_resource_attach(struct atrium_gpu_softc *sc,
    struct atrium_gpu_file *f, struct atrium_gpu_resource_attach *args)
{
	struct atrium_gpu_bo *bo;
	uint32_t resource_id;
	int err;

	if (f->ctx_id == 0)
		return (ENOTCONN);  /* call CTX_INIT first */
	if (args->blob_mem != ATRIUM_GPU_BLOB_MEM_GUEST &&
	    args->blob_mem != ATRIUM_GPU_BLOB_MEM_HOST3D)
		return (EINVAL);

	mtx_lock(&sc->bo_lock);
	bo = atrium_bo_find_locked(sc, args->bo_handle);
	if (bo == NULL || bo->owner != f) {
		mtx_unlock(&sc->bo_lock);
		return (ENOENT);
	}
	resource_id = ++sc->next_resource_id;
	bo->virtio_resource_id = resource_id;
	mtx_unlock(&sc->bo_lock);

	err = atrium_vgpu_resource_create_blob(sc, f->ctx_id, resource_id,
	    args->blob_mem, args->blob_flags, args->blob_id, bo->size,
	    bo->pa, (uint32_t)bo->size);
	if (err != 0)
		return (err);
	err = atrium_vgpu_ctx_attach_resource(sc, f->ctx_id, resource_id);
	if (err != 0)
		return (err);

	args->resource_id_out = resource_id;
	return (0);
}

/* ATRIUM_GPU_IOC_HOST_BLOB: V5h — allocate a HOST3D blob backed by a
 * window of the host_visible BAR. No guest pages are allocated; the host
 * (virglrenderer) sets up the actual storage via shm_open under
 * RESOURCE_CREATE_BLOB(blob_mem=HOST3D, num_entries=0), and we publish
 * the BAR offset to userspace so mmap() returns BAR pages directly. */
static int
atrium_gpu_ioc_host_blob(struct atrium_gpu_softc *sc,
    struct atrium_gpu_file *f, struct atrium_gpu_host_blob *args)
{
	struct atrium_gpu_bo *bo;
	uint64_t actual_size, bar_offset;
	uint32_t resource_id, npages;
	int err;

	if (f->ctx_id == 0)
		return (ENOTCONN);
	if (args->size == 0 || args->size > (1ULL << 32))
		return (EINVAL);

	/* Round up to the HOST page size, not the guest's — see
	 * ATRIUM_HOST_PAGE_SIZE comment. */
	actual_size = roundup2(args->size, ATRIUM_HOST_PAGE_SIZE);
	npages = actual_size / ATRIUM_HOST_PAGE_SIZE;

	/* Reserve a BAR window. */
	mtx_lock(&sc->shm_lock);
	err = atrium_vgpu_shm_init_locked(sc);
	if (err != 0) {
		mtx_unlock(&sc->shm_lock);
		return (err);
	}
	bar_offset = atrium_vgpu_shm_alloc_locked(sc, npages);
	mtx_unlock(&sc->shm_lock);
	if (bar_offset == (uint64_t)-1)
		return (ENOMEM);

	/* Allocate the BO descriptor. No guest pages — pa is the BAR PA
	 * plus our window offset. */
	bo = malloc(sizeof(*bo), M_ATRIUM_GPU, M_WAITOK | M_ZERO);
	bo->size        = actual_size;
	bo->flags       = ATRIUM_BO_INT_HOST_BLOB;
	bo->kva         = 0;
	bo->pa          = sc->shm_pa + bar_offset;
	bo->owner       = f;

	/* Send RESOURCE_CREATE_BLOB(HOST3D) — host allocates shm; venus
	 * sees blob_id (0 for shmem, the venus mem_id for VkDeviceMemory). */
	resource_id = atomic_fetchadd_32(&sc->next_resource_id, 1);
	device_printf(sc->dev,
	    "HOST_BLOB: CREATE res=%u sz=%lu flags=0x%x blob_id=0x%lx off=0x%lx\n",
	    resource_id, (unsigned long)actual_size, args->blob_flags,
	    (unsigned long)args->blob_id, (unsigned long)bar_offset);
	err = atrium_vgpu_resource_create_blob(sc, f->ctx_id, resource_id,
	    ATRIUM_GPU_BLOB_MEM_HOST3D, args->blob_flags, args->blob_id,
	    actual_size, 0, 0);  /* length=0 → no mem_entry */
	if (err != 0) {
		device_printf(sc->dev,
		    "HOST_BLOB: RESOURCE_CREATE_BLOB(HOST3D) failed: %d\n", err);
		goto fail;
	}
	device_printf(sc->dev, "HOST_BLOB: CREATE ok, attaching\n");
	err = atrium_vgpu_ctx_attach_resource(sc, f->ctx_id, resource_id);
	if (err != 0) {
		device_printf(sc->dev, "HOST_BLOB: ATTACH failed: %d\n", err);
		goto fail_destroy;
	}
	device_printf(sc->dev, "HOST_BLOB: ATTACH ok, mapping\n");
	err = atrium_vgpu_resource_map_blob(sc, f->ctx_id, resource_id,
	    bar_offset);
	if (err != 0) {
		device_printf(sc->dev, "HOST_BLOB: MAP failed: %d\n", err);
		goto fail_destroy;
	}
	device_printf(sc->dev, "HOST_BLOB: MAP ok\n");

	bo->virtio_resource_id = resource_id;

	/* Publish BO with a unique mmap_offset and handle. mmap_offset uses
	 * BAR offset for unique-by-construction (no two HOST blobs share a
	 * window) — userspace passes this to mmap(). */
	mtx_lock(&sc->bo_lock);
	bo->handle      = ++sc->next_handle;
	bo->mmap_offset = bar_offset; /* uniqueness inherited from window */
	TAILQ_INSERT_TAIL(&sc->bos, bo, link);
	mtx_unlock(&sc->bo_lock);

	args->bo_handle    = bo->handle;
	args->resource_id  = resource_id;
	args->mmap_offset  = bo->mmap_offset;
	args->actual_size  = actual_size;
	return (0);

fail_destroy:
	/* Best-effort: tell the host to drop the resource. We don't have a
	 * dedicated UNREF helper yet; the next CTX_DESTROY (on fd close)
	 * will collect it. */
fail:
	mtx_lock(&sc->shm_lock);
	atrium_vgpu_shm_free_locked(sc, bar_offset, npages);
	mtx_unlock(&sc->shm_lock);
	free(bo, M_ATRIUM_GPU);
	return (err);
}

/* ATRIUM_GPU_IOC_SUBMIT_3D: ship an opaque venus command stream to
 * this fd's context. Bytes are forwarded verbatim — the kernel does
 * not parse them; validation is the host renderer's responsibility. */
static int
atrium_gpu_ioc_submit_3d(struct atrium_gpu_softc *sc,
    struct atrium_gpu_file *f, struct atrium_gpu_submit_3d *args)
{
	void *cmd_buf;
	uint64_t fence_id;
	int err;

	if (f->ctx_id == 0)
		return (ENOTCONN);
	if (args->cmd_size == 0 || args->cmd_size > 1024 * 1024)
		return (EINVAL);

	device_printf(sc->dev, "SUBMIT_3D: ctx=%u cmd_size=%u flags=0x%x\n",
	    f->ctx_id, args->cmd_size, args->flags);
	cmd_buf = malloc(args->cmd_size, M_ATRIUM_GPU, M_WAITOK);
	err = copyin((const void *)(uintptr_t)args->cmd_ptr, cmd_buf,
	    args->cmd_size);
	if (err != 0) {
		free(cmd_buf, M_ATRIUM_GPU);
		return (err);
	}

	fence_id = atomic_fetchadd_64(&sc->next_fence, 1);
	err = atrium_vgpu_submit_3d(sc, f->ctx_id, (uint32_t)fence_id,
	    cmd_buf, args->cmd_size);
	free(cmd_buf, M_ATRIUM_GPU);
	if (err != 0)
		return (err);

	f->ctx_last_fence = fence_id;
	args->fence_out = (args->flags & ATRIUM_GPU_SUBMIT_3D_SIGNAL_FENCE)
	    ? fence_id : 0;
	return (0);
}

/* ATRIUM_GPU_IOC_CTX_FENCE_WAIT: synchronous waits are trivial in
 * V4 because atrium_vgpu_submit_3d blocks until the host fence
 * retires (the controlq req_resp pattern). Any fence_id ≤ ctx_last_fence
 * is therefore already signalled by definition. Async submit lands
 * later (V4-stretch) and turns this into a real wait. */
static int
atrium_gpu_ioc_ctx_fence_wait(struct atrium_gpu_softc *sc __unused,
    struct atrium_gpu_file *f, struct atrium_gpu_ctx_fence_wait *args)
{
	if (f->ctx_id == 0)
		return (ENOTCONN);
	if (args->fence > f->ctx_last_fence) {
		args->status = EBUSY;  /* unknown fence — caller bug */
		return (0);
	}
	args->status = 0;
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

	case ATRIUM_GPU_IOC_CAPSET_QUERY:
		return (atrium_gpu_ioc_capset_query(sc, f,
		    (struct atrium_gpu_capset_query *)data));
	case ATRIUM_GPU_IOC_CTX_INIT:
		return (atrium_gpu_ioc_ctx_init(sc, f,
		    (struct atrium_gpu_ctx_init *)data));
	case ATRIUM_GPU_IOC_RESOURCE_ATTACH:
		return (atrium_gpu_ioc_resource_attach(sc, f,
		    (struct atrium_gpu_resource_attach *)data));
	case ATRIUM_GPU_IOC_SUBMIT_3D:
		return (atrium_gpu_ioc_submit_3d(sc, f,
		    (struct atrium_gpu_submit_3d *)data));
	case ATRIUM_GPU_IOC_CTX_FENCE_WAIT:
		return (atrium_gpu_ioc_ctx_fence_wait(sc, f,
		    (struct atrium_gpu_ctx_fence_wait *)data));
	case ATRIUM_GPU_IOC_HOST_BLOB:
		return (atrium_gpu_ioc_host_blob(sc, f,
		    (struct atrium_gpu_host_blob *)data));

	default:
		return (ENOTTY);
	}
}

static int
atrium_gpu_mmap(struct cdev *cdev, vm_ooffset_t offset, vm_paddr_t *paddr,
    int nprot __unused, vm_memattr_t *memattr)
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
		/* HOST_BLOB pages live in the host_visible BAR. QEMU backs
		 * the BAR with shm pages (cacheable on the host); WB on the
		 * guest matches the host policy and is what venus expects
		 * for shmem-ring + VkDeviceMemory. The default (NULL = leave
		 * memattr untouched, ARM64 picks DEVICE) would make the ring
		 * uncacheable and ~100x slower, plus break some atomics. */
		if (bo->flags & ATRIUM_BO_INT_HOST_BLOB)
			*memattr = VM_MEMATTR_WRITE_BACK;
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

	if ((err = atrium_vgpu_ensure_display_info(sc)) != 0)
		return (err);

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

	if ((err = atrium_vgpu_ensure_display_info(sc)) != 0)
		return (err);

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

	if ((err = atrium_vgpu_ensure_display_info(sc)) != 0)
		return (err);

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
	int n = 0;

	mtx_lock(&sc->ctrl_lock);
	while (virtqueue_dequeue(sc->ctrl_vq, NULL) != NULL)
		n++;
	sc->ctrl_done = true;
	cv_signal(&sc->ctrl_done_cv);

	/* Re-arm the per-vq interrupt for the next request. Some
	 * FreeBSD virtio drivers (vtnet, vtblk) re-enable inside the
	 * intr handler after dequeueing; some rely on virtio framework
	 * auto-re-enable. We do it explicitly: the cost is one MMIO
	 * write per controlq round-trip, which is negligible. */
	(void)virtqueue_enable_intr(sc->ctrl_vq);

	if (bootverbose)
		device_printf(sc->dev, "ctrl_intr: dequeued %d, signaled\n", n);
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
	{
		uint32_t cmd_type = le32toh(*(uint32_t *)req);
		device_printf(sc->dev,
		    "req_resp: enqueue type=0x%x reqlen=%zu resplen=%zu\n",
		    cmd_type, reqlen, resplen);
	}
	err = virtqueue_enqueue(sc->ctrl_vq, resp, &sg, 1, 1);
	if (err != 0) {
		device_printf(sc->dev, "req_resp: virtqueue_enqueue: %d\n", err);
		mtx_unlock(&sc->ctrl_lock);
		return (err);
	}
	virtqueue_notify(sc->ctrl_vq);
	if (bootverbose)
		device_printf(sc->dev, "req_resp: notified, waiting...\n");

	/* Bounded wait so a missed interrupt doesn't deadlock the caller
	 * forever. 5 seconds is generous: a virtio-gpu controlq round-
	 * trip on a healthy host is microseconds. If we time out, we
	 * report failure and the caller's IOCTL returns EIO instead of
	 * sleeping uninterruptibly forever. */
	while (!sc->ctrl_done) {
		err = cv_timedwait(&sc->ctrl_done_cv, &sc->ctrl_lock,
		    5 * hz);
		if (err == EWOULDBLOCK && !sc->ctrl_done) {
			device_printf(sc->dev,
			    "req_resp: timeout waiting for completion "
			    "(host not responding or interrupt not delivered)\n");
			mtx_unlock(&sc->ctrl_lock);
			return (EIO);
		}
		err = 0;
	}
	mtx_unlock(&sc->ctrl_lock);
	return (0);
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
	sc->display_info_valid = true;
	return (0);
}

/*
 * Fetch display info if we haven't yet. Called from IOCTL handlers
 * (enum_connectors, modes, set_mode) on first use after attach. The
 * direct fetch in attach() was removed — on modern -CURRENT, the
 * newbus topology lock held during attach is non-sleepable, and the
 * `cv_wait` inside `atrium_vgpu_req_resp` would hit a lock-order
 * violation that manifests as a kernel deadlock right after attach
 * returns. Deferring to first IOCTL guarantees we're in a normal
 * thread context with no exceptional locks held. Stock vtgpu uses a
 * similar deferred pattern.
 */
static int
atrium_vgpu_ensure_display_info(struct atrium_gpu_softc *sc)
{
	int err;

	if (sc->display_info_valid)
		return (0);
	err = atrium_vgpu_get_display_info(sc);
	return (err);
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
/* Venus / context-init helpers (V3+).                                          */
/*                                                                              */
/* These wrap virtio-gpu's GET_CAPSET_INFO / GET_CAPSET / CTX_CREATE /         */
/* CTX_DESTROY commands. The kmod doesn't interpret capset payloads or         */
/* command streams; for venus the payloads are opaque blobs the host's          */
/* virglrenderer parses.                                                        */
/* ------------------------------------------------------------------------- */

/* Number of capsets advertised in virtio_config.num_capsets. The host
 * fills this when the device is realized; we cache it on first
 * use to avoid re-reading PCI config space. */
static uint32_t
atrium_vgpu_num_capsets(struct atrium_gpu_softc *sc)
{
	return (le32toh(sc->gpucfg.num_capsets));
}

/* GET_CAPSET_INFO walks capset indexes [0, num_capsets); each returns
 * a (capset_id, max_version, max_size) triple. We probe sequentially
 * to find the one matching the requested capset_id. */
static int
atrium_vgpu_get_capset_info_at(struct atrium_gpu_softc *sc,
    uint32_t index, uint32_t *out_id, uint32_t *out_max_ver,
    uint32_t *out_max_size)
{
	struct {
		struct virtio_gpu_get_capset_info req;
		char pad;
		struct virtio_gpu_resp_capset_info resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_GET_CAPSET_INFO);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.capset_index = htole32(index);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	if (le32toh(s.resp.hdr.type) != VIRTIO_GPU_RESP_OK_CAPSET_INFO) {
		device_printf(sc->dev,
		    "GET_CAPSET_INFO[%u] resp 0x%x\n", index,
		    le32toh(s.resp.hdr.type));
		return (EIO);
	}
	*out_id       = le32toh(s.resp.capset_id);
	*out_max_ver  = le32toh(s.resp.capset_max_version);
	*out_max_size = le32toh(s.resp.capset_max_size);
	return (0);
}

/* Lookup a capset by id (not index). Returns ENOENT if the host
 * doesn't advertise it. Result is informational; callers use it to
 * decide whether to issue a follow-up GET_CAPSET. */
static int
atrium_vgpu_find_capset(struct atrium_gpu_softc *sc, uint32_t want_id,
    uint32_t *out_max_ver, uint32_t *out_max_size)
{
	uint32_t i, n, id, mv, ms;
	int err;

	n = atrium_vgpu_num_capsets(sc);
	for (i = 0; i < n; i++) {
		err = atrium_vgpu_get_capset_info_at(sc, i, &id, &mv, &ms);
		if (err != 0)
			return (err);
		if (id == want_id) {
			*out_max_ver  = mv;
			*out_max_size = ms;
			return (0);
		}
	}
	return (ENOENT);
}

/* GET_CAPSET fetches the actual capability blob for a known capset.
 * Caller supplies the response buffer sized at max_size. */
static int
atrium_vgpu_get_capset(struct atrium_gpu_softc *sc, uint32_t capset_id,
    uint32_t version, void *resp_buf, size_t resp_size)
{
	struct virtio_gpu_get_capset req;
	struct virtio_gpu_ctrl_hdr *hdr = resp_buf;
	int err;

	bzero(&req, sizeof(req));
	bzero(resp_buf, resp_size);
	req.hdr.type     = htole32(VIRTIO_GPU_CMD_GET_CAPSET);
	req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE);
	req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	req.capset_id    = htole32(capset_id);
	req.capset_version = htole32(version);

	err = atrium_vgpu_req_resp(sc, &req, sizeof(req),
	    resp_buf, resp_size);
	if (err != 0)
		return (err);
	if (le32toh(hdr->type) != VIRTIO_GPU_RESP_OK_CAPSET) {
		device_printf(sc->dev, "GET_CAPSET resp 0x%x\n",
		    le32toh(hdr->type));
		return (EIO);
	}
	return (0);
}

static int
atrium_vgpu_ctx_create(struct atrium_gpu_softc *sc, uint32_t ctx_id,
    uint32_t capset_id, const char *debug_name)
{
	struct {
		struct virtio_gpu_ctx_create req;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	size_t nlen;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_CTX_CREATE);
	/* INFO_RING_IDX routes the fence through QEMU's per-context
	 * async-fence path (virgl_renderer_context_create_fence) instead
	 * of the legacy global path (virgl_renderer_create_fence). The
	 * legacy path requires vrend_initialized, which is FALSE under
	 * VIRGL_RENDERER_NO_VIRGL — so without this flag the fence is
	 * silently dropped on macOS hosts and the guest times out. */
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE |
	                             VIRTIO_GPU_FLAG_INFO_RING_IDX);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.hdr.ctx_id   = htole32(ctx_id);
	s.req.hdr.ring_idx = 0;  /* venus uses ring 0 for control */
	/* context_init's low byte holds the capset id (mask 0xFF). */
	s.req.context_init = htole32(capset_id &
	    VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK);
	nlen = strnlen(debug_name, sizeof(s.req.debug_name) - 1);
	memcpy(s.req.debug_name, debug_name, nlen);
	s.req.nlen = htole32((uint32_t)nlen);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "CTX_CREATE", &s.resp));
}

static int
atrium_vgpu_ctx_destroy(struct atrium_gpu_softc *sc, uint32_t ctx_id)
{
	struct {
		struct virtio_gpu_ctx_destroy req;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_CTX_DESTROY);
	/* See ctx_create — INFO_RING_IDX needed under NO_VIRGL hosts. */
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE |
	                             VIRTIO_GPU_FLAG_INFO_RING_IDX);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.hdr.ctx_id   = htole32(ctx_id);
	s.req.hdr.ring_idx = 0;

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "CTX_DESTROY", &s.resp));
}

/* ------------------------------------------------------------------------- */
/* V4: resource_create_blob, ctx_attach_resource, submit_3d helpers.            */
/*                                                                              */
/* All operate against a per-fd context bound by ATRIUM_GPU_IOC_CTX_INIT;        */
/* every command sets VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX     */
/* so the fence routes through QEMU's per-context async path (legacy global    */
/* path is broken under VIRGL_RENDERER_NO_VIRGL — the macOS host case).        */
/* ------------------------------------------------------------------------- */

static int
atrium_vgpu_resource_create_blob(struct atrium_gpu_softc *sc,
    uint32_t ctx_id, uint32_t resource_id, uint32_t blob_mem,
    uint32_t blob_flags, uint64_t blob_id, uint64_t size,
    vm_paddr_t pa, uint32_t length)
{
	struct {
		struct virtio_gpu_resource_create_blob req;
		struct virtio_gpu_mem_entry              ent;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE |
	                             VIRTIO_GPU_FLAG_INFO_RING_IDX);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.hdr.ctx_id   = htole32(ctx_id);
	s.req.hdr.ring_idx = 0;
	s.req.resource_id  = htole32(resource_id);
	s.req.blob_mem     = htole32(blob_mem);
	s.req.blob_flags   = htole32(blob_flags);
	s.req.blob_id      = htole64(blob_id);
	s.req.size         = htole64(size);
	s.req.nr_entries   = htole32(length > 0 ? 1 : 0);
	if (length > 0) {
		s.ent.addr   = htole64((uint64_t)pa);
		s.ent.length = htole32(length);
	}

	err = atrium_vgpu_req_resp(sc,
	    &s.req, sizeof(s.req) + (length > 0 ? sizeof(s.ent) : 0),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "RESOURCE_CREATE_BLOB", &s.resp));
}

static int
atrium_vgpu_ctx_attach_resource(struct atrium_gpu_softc *sc,
    uint32_t ctx_id, uint32_t resource_id)
{
	struct {
		struct virtio_gpu_ctx_resource req;
		char pad;
		struct virtio_gpu_ctrl_hdr resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE |
	                             VIRTIO_GPU_FLAG_INFO_RING_IDX);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.hdr.ctx_id   = htole32(ctx_id);
	s.req.hdr.ring_idx = 0;
	s.req.resource_id  = htole32(resource_id);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	return (atrium_vgpu_check_ok(sc, "CTX_ATTACH_RESOURCE", &s.resp));
}

/* V5h: ask the host to map a HOST3D blob resource into our host_visible
 * BAR at the requested offset. The reply's map_info field is the cache
 * mode the host picked (CACHED/UNCACHED/WC); we currently ignore it and
 * always set vm_memattr=WB on the userspace mapping (works for QEMU's
 * shmem-backed regions). */
static int
atrium_vgpu_resource_map_blob(struct atrium_gpu_softc *sc,
    uint32_t ctx_id, uint32_t resource_id, uint64_t bar_offset)
{
	struct {
		struct virtio_gpu_resource_map_blob req;
		char pad;
		struct virtio_gpu_resp_map_info     resp;
	} s;
	int err;

	bzero(&s, sizeof(s));
	s.req.hdr.type     = htole32(VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB);
	s.req.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE |
	                             VIRTIO_GPU_FLAG_INFO_RING_IDX);
	s.req.hdr.fence_id = htole64(atomic_fetchadd_64(&sc->next_fence, 1));
	s.req.hdr.ctx_id   = htole32(ctx_id);
	s.req.hdr.ring_idx = 0;
	s.req.resource_id  = htole32(resource_id);
	s.req.offset       = htole64(bar_offset);

	err = atrium_vgpu_req_resp(sc, &s.req, sizeof(s.req),
	    &s.resp, sizeof(s.resp));
	if (err != 0)
		return (err);
	/* Response type for MAP_BLOB success is OK_MAP_INFO, not OK_NODATA;
	 * check_ok would reject it. Just verify the type bucket. */
	if (le32toh(s.resp.hdr.type) != VIRTIO_GPU_RESP_OK_MAP_INFO) {
		device_printf(sc->dev,
		    "RESOURCE_MAP_BLOB: host returned 0x%x\n",
		    le32toh(s.resp.hdr.type));
		return (EIO);
	}
	return (0);
}

/* V5h: lazy first-time setup of the BAR-backed page allocator. Reads
 * the BAR's bus PA directly from PCI config space — we deliberately do
 * NOT bus_alloc_resource_any() because virtio_pci_modern shares BAR4
 * with MSI-X (table + PBA at the same BAR), and a competing alloc on
 * a non-shared resource deadlocks newbus interactions with the
 * interrupt path. The BAR is already programmed and we just need its
 * PA to compose the absolute address for d_mmap.
 *
 * Caller must hold sc->shm_lock. Returns 0 on success or already-init,
 * ENXIO if no host_visible region was advertised. */
static int
atrium_vgpu_shm_init_locked(struct atrium_gpu_softc *sc)
{
	device_t pcidev;
	uint32_t bar_lo, bar_hi;
	uint64_t bar_pa;

	if (sc->shm_initialized)
		return (0);
	if (sc->shm_size == 0)
		return (ENXIO);

	pcidev = device_get_parent(sc->dev);
	bar_lo = pci_read_config(pcidev, PCIR_BAR(sc->shm_bar), 4);
	/* Drop the low type/prefetch bits to get the base address.
	 * 64-bit BARs use the next register slot for the high half. */
	if ((bar_lo & PCIM_BAR_MEM_TYPE) == PCIM_BAR_MEM_64) {
		bar_hi = pci_read_config(pcidev,
		    PCIR_BAR(sc->shm_bar) + 4, 4);
		bar_pa = ((uint64_t)bar_hi << 32) |
		         (bar_lo & PCIM_BAR_MEM_BASE);
	} else {
		bar_pa = bar_lo & PCIM_BAR_MEM_BASE;
	}

	sc->shm_pa = bar_pa + sc->shm_bar_offset;
	/* Bitmap tracks ATRIUM_HOST_PAGE_SIZE-sized chunks, not guest
	 * PAGE_SIZE — see comment on the macro above. */
	sc->shm_n_pages = sc->shm_size / ATRIUM_HOST_PAGE_SIZE;
	sc->shm_bitmap = malloc(roundup2(sc->shm_n_pages, 8) / 8,
	    M_ATRIUM_GPU, M_WAITOK | M_ZERO);
	sc->shm_initialized = true;
	device_printf(sc->dev,
	    "HOST3D blob window armed: BAR%u pa=0x%lx, %u host-pages (%u KiB each)\n",
	    sc->shm_bar, (unsigned long)sc->shm_pa, sc->shm_n_pages,
	    ATRIUM_HOST_PAGE_SIZE >> 10);
	return (0);
}

/* V5h: bitmap allocator over the host_visible BAR. Returns the byte
 * offset of a contiguous run of `npages` pages, or (uint64_t)-1 on
 * failure (region exhausted). Caller must hold sc->shm_lock. */
static uint64_t
atrium_vgpu_shm_alloc_locked(struct atrium_gpu_softc *sc, uint32_t npages)
{
	uint32_t i, j, run;

	if (npages == 0 || npages > sc->shm_n_pages)
		return ((uint64_t)-1);
	run = 0;
	for (i = 0; i < sc->shm_n_pages; i++) {
		if (sc->shm_bitmap[i / 8] & (1u << (i % 8))) {
			run = 0;
			continue;
		}
		run++;
		if (run == npages) {
			uint32_t base = i + 1 - npages;
			for (j = 0; j < npages; j++) {
				uint32_t b = base + j;
				sc->shm_bitmap[b / 8] |= (1u << (b % 8));
			}
			return ((uint64_t)base << ATRIUM_HOST_PAGE_SHIFT);
		}
	}
	return ((uint64_t)-1);
}

static void
atrium_vgpu_shm_free_locked(struct atrium_gpu_softc *sc,
    uint64_t bar_offset, uint32_t npages)
{
	uint32_t base = bar_offset >> ATRIUM_HOST_PAGE_SHIFT;
	uint32_t j;

	for (j = 0; j < npages; j++) {
		uint32_t b = base + j;
		sc->shm_bitmap[b / 8] &= ~(1u << (b % 8));
	}
}

/* SUBMIT_3D ships an opaque venus command stream to the host. The
 * kernel does not parse the bytes; they're concatenated after the
 * header struct as a single sg entry to the controlq. */
static int
atrium_vgpu_submit_3d(struct atrium_gpu_softc *sc, uint32_t ctx_id,
    uint32_t fence_id, void *cmd, size_t cmd_size)
{
	struct sglist sg;
	struct sglist_seg segs[3];
	struct virtio_gpu_cmd_submit hdr;
	struct virtio_gpu_ctrl_hdr resp;
	int err;

	bzero(&hdr, sizeof(hdr));
	hdr.hdr.type     = htole32(VIRTIO_GPU_CMD_SUBMIT_3D);
	hdr.hdr.flags    = htole32(VIRTIO_GPU_FLAG_FENCE |
	                           VIRTIO_GPU_FLAG_INFO_RING_IDX);
	hdr.hdr.fence_id = htole64(fence_id);
	hdr.hdr.ctx_id   = htole32(ctx_id);
	hdr.hdr.ring_idx = 0;
	hdr.size         = htole32((uint32_t)cmd_size);

	sglist_init(&sg, 3, segs);
	if ((err = sglist_append(&sg, &hdr, sizeof(hdr))) != 0)
		return (err);
	if (cmd_size > 0 && (err = sglist_append(&sg, cmd, cmd_size)) != 0)
		return (err);
	if ((err = sglist_append(&sg, &resp, sizeof(resp))) != 0)
		return (err);

	mtx_lock(&sc->ctrl_lock);
	sc->ctrl_done = false;
	err = virtqueue_enqueue(sc->ctrl_vq, &resp, &sg, 2, 1);
	if (err != 0) {
		mtx_unlock(&sc->ctrl_lock);
		return (err);
	}
	virtqueue_notify(sc->ctrl_vq);
	while (!sc->ctrl_done) {
		err = cv_timedwait(&sc->ctrl_done_cv, &sc->ctrl_lock, 5 * hz);
		if (err == EWOULDBLOCK && !sc->ctrl_done) {
			device_printf(sc->dev,
			    "submit_3d: timeout waiting for completion\n");
			mtx_unlock(&sc->ctrl_lock);
			return (EIO);
		}
		err = 0;
	}
	mtx_unlock(&sc->ctrl_lock);
	return (atrium_vgpu_check_ok(sc, "SUBMIT_3D", &resp));
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
	/* BUS_PROBE_VENDOR (> BUS_PROBE_DEFAULT) so we win the probe
	 * against stock vtgpu when both are registered. Without this,
	 * vtgpu wins the tie-break, attaches as the framebuffer backend
	 * for vt(4), and the only way for atrium to take the slot is a
	 * runtime `devctl set driver -f` — which on -CURRENT panics in
	 * vt_timer because vt holds a stale callback into freed vtgpu
	 * state (panic: "Offset 0x000002 out of fb size", trace via
	 * scripts/ddb_session.py). Winning the probe at boot avoids the
	 * detach path entirely. */
	return (BUS_PROBE_VENDOR);
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

/* V5h step 1: walk the parent PCI device's vendor capability list looking
 * for a virtio shared-memory-region cap (cfg_type=8, virtio spec 1.1+) with
 * the requested shmid. virtio-gpu uses shmid=0 for the host_visible region.
 *
 * The cap layout (from virtio-spec virtio_pci_cap64):
 *   off 0:  cap_vndr   (PCIY_VENDOR=9)
 *   off 1:  cap_next
 *   off 2:  cap_len
 *   off 3:  cfg_type   (8 = SHARED_MEMORY_CFG)
 *   off 4:  bar
 *   off 5:  id         (shmid)
 *   off 6-7: padding
 *   off 8-11: offset_lo
 *   off 12-15: length_lo
 *   off 16-19: offset_hi
 *   off 20-23: length_hi
 *
 * FreeBSD's virtio_pci_modern only parses cap types 1..5 (common, notify,
 * isr, device, pci_cfg) so we have to do this ourselves. */
#define ATRIUM_VIRTIO_PCI_CAP_SHARED_MEMORY_CFG  8
static int
atrium_vgpu_find_shm_region(struct atrium_gpu_softc *sc, uint8_t shmid)
{
	device_t pcidev;
	int capreg;
	int err;

	/* virtio_pci_modern is itself the PCI device driver; it attaches as
	 * a pci0 child, then creates a virtio bus on which our atrium-vgpu
	 * lives. So device_get_parent(sc->dev) returns the virtio_pci_modern
	 * device, which is what pci_find_cap works on. */
	pcidev = device_get_parent(sc->dev);
	if (pcidev == NULL)
		return (ENXIO);

	for (err = pci_find_cap(pcidev, PCIY_VENDOR, &capreg);
	     err == 0;
	     err = pci_find_next_cap(pcidev, PCIY_VENDOR, capreg, &capreg)) {
		uint8_t cfg_type, bar, id;
		uint32_t off_lo, off_hi, len_lo, len_hi;

		cfg_type = pci_read_config(pcidev, capreg + 3, 1);
		if (cfg_type != ATRIUM_VIRTIO_PCI_CAP_SHARED_MEMORY_CFG)
			continue;
		id = pci_read_config(pcidev, capreg + 5, 1);
		if (id != shmid)
			continue;

		bar    = pci_read_config(pcidev, capreg + 4, 1);
		off_lo = pci_read_config(pcidev, capreg + 8, 4);
		len_lo = pci_read_config(pcidev, capreg + 12, 4);
		off_hi = pci_read_config(pcidev, capreg + 16, 4);
		len_hi = pci_read_config(pcidev, capreg + 20, 4);

		sc->shm_bar        = bar;
		sc->shm_bar_offset = ((uint64_t)off_hi << 32) | off_lo;
		sc->shm_size       = ((uint64_t)len_hi << 32) | len_lo;
		/* shm_pa is computed in shm_init_locked once we own the BAR. */
		return (0);
	}
	return (ENOENT);
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
	mtx_init(&sc->shm_lock, "atrium-gpu shm", NULL, MTX_DEF);
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

	/* Arm the controlq interrupt. virtio_setup_intr installs the IRQ
	 * handler at the bus level but each virtqueue's per-vq interrupt
	 * stays masked until enabled. The earlier virtqueue_poll-based
	 * design enabled it as a side effect; the cv_wait callback path
	 * needs an explicit arm. Without this, the host fires MSI-X on
	 * request completion but the vq's used-ring monitor stays off,
	 * so atrium_vgpu_ctrl_intr never runs and req_resp deadlocks
	 * waiting on ctrl_done_cv. */
	if (virtqueue_enable_intr(sc->ctrl_vq) != 0) {
		device_printf(dev, "virtqueue_enable_intr(ctrl_vq) failed\n");
		err = EIO;
		goto fail_locks;
	}

	/* GET_DISPLAY_INFO is deferred to first IOCTL via
	 * atrium_vgpu_ensure_display_info — see that helper for the
	 * lock-order rationale. attach context can't sleep on this
	 * kernel, but the cdev IOCTL path runs in normal thread
	 * context where cv_wait inside req_resp is safe. */

	atrium_fill_caps(sc);

	/* V5h step 1: probe for the host_visible shared-memory region.
	 * Optional — absence just means HOST3D blob ioctls will refuse with
	 * ENXIO and userspace falls back to BLOB_MEM_GUEST (the V5g code
	 * path). shmid 1 == VIRTIO_GPU_SHM_ID_HOST_VISIBLE per virtio-gpu
	 * spec (id 0 is "undefined"; the host_visible region is always
	 * id=1). */
	if (atrium_vgpu_find_shm_region(sc, 1) == 0) {
		device_printf(dev,
		    "host_visible shm region: BAR%u, off 0x%lx, size %lu MiB\n",
		    sc->shm_bar, (unsigned long)sc->shm_bar_offset,
		    (unsigned long)(sc->shm_size >> 20));
	} else {
		device_printf(dev,
		    "no host_visible shm region (need QEMU -hostmem); "
		    "venus HOST3D blobs unavailable\n");
	}

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
	mtx_destroy(&sc->shm_lock);
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
	if (sc->shm_initialized) {
		free(sc->shm_bitmap, M_ATRIUM_GPU);
		sc->shm_bitmap = NULL;
	}
	cv_destroy(&sc->ctrl_done_cv);
	mtx_destroy(&sc->shm_lock);
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
