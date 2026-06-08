/*
 * atrium-gpu-amd — public userspace ABI for /dev/atrium-gpu0.
 *
 * Shared verbatim between the kernel driver and userspace (the Rust/C
 * user-mode driver, and the in-tree test). This is the kernel/userspace
 * boundary: a tiny buffer-object + submit interface. Userspace allocates BOs
 * (each gets a GPU-VA the kernel mapped into GPUVM), fills them (a PM4 command
 * ring, compute inputs), and submits a ring on an engine; the kernel programs
 * the queue + rings the doorbell. PM4 contents are built in userspace — the
 * kernel only owns the privileged MMIO (queue map, doorbell, compute state).
 *
 * Struct layout rule: 64-bit fields first, then 32-bit, with explicit pad to a
 * multiple of 8 — so the ABI is identical for 32- and 64-bit userspace and has
 * no implementation-defined padding.
 */
#ifndef _ATRIUM_GPU_AMD_ABI_H_
#define _ATRIUM_GPU_AMD_ABI_H_

#include <sys/ioccom.h>
#include <sys/types.h>

/* Engines a ring can be submitted on (mirrors the model's gfx vs compute path). */
#define ATRIUM_GPU_ENGINE_GFX		0	/* CP_RB0 graphics ring (queue 0) */
#define ATRIUM_GPU_ENGINE_COMPUTE	1	/* MEC HQD compute queue (queue 1) */

/*
 * Allocate a buffer object. The kernel maps it into GPUVM and returns it as a
 * file descriptor (fd-as-handle): the BO lives as long as the fd is open, is
 * reclaimed on the last close, and is transportable via SCM_RIGHTS.
 */
struct atrium_gpu_bo_alloc {
	uint64_t	size;		/* in:  bytes (<= one page for now) */
	uint64_t	gpu_va;		/* out: GPU virtual address of the BO */
	uint32_t	bo_fd;		/* out: file descriptor naming this BO */
	uint32_t	pad;
};

/* Copy a byte range between userspace and a BO (write = into BO, read = out). */
struct atrium_gpu_bo_xfer {
	uint64_t	offset;		/* in: byte offset within the BO */
	uint64_t	len;		/* in: byte count */
	uint64_t	user_ptr;	/* in: userspace buffer (cast of void *) */
	uint32_t	bo_fd;		/* in: which BO */
	uint32_t	pad;
};

/* Program the compute state the SoftwareBackend reads at DISPATCH time. */
struct atrium_gpu_set_compute {
	uint64_t	src_va;		/* in: source buffer GPU-VA */
	uint64_t	dst_va;		/* in: dest buffer GPU-VA */
	uint32_t	kernel;		/* in: built-in kernel selector */
	uint32_t	pad;
};

/* Submit a PM4 ring (already laid into the ring BO) on an engine. */
struct atrium_gpu_submit {
	uint32_t	ring_fd;	/* in: BO fd holding the PM4 ring */
	uint32_t	n_dwords;	/* in: ring length in dwords (the wptr) */
	uint32_t	engine;		/* in: ATRIUM_GPU_ENGINE_* */
	uint32_t	pad;
};

/* Program graphics DRAW state read by a DRAW_INDEX_AUTO packet on the gfx ring. */
struct atrium_gpu_set_draw {
	uint64_t	vtx_va;		/* in: vertex buffer GPU-VA (24B verts) */
	uint64_t	rt_va;		/* in: render-target GPU-VA (RGBA8) */
	uint32_t	width;		/* in: RT width  */
	uint32_t	height;		/* in: RT height */
};

/* Query interrupt state: how many completions the ISR has serviced. */
struct atrium_gpu_irqs {
	uint64_t	count;		/* out: interrupts serviced so far */
	uint32_t	msix_enabled;	/* out: 1 = interrupt mode, 0 = poll mode */
	uint32_t	pad;
};

/* Block until a 64-bit fence word in a BO reaches `value`, or time out. */
struct atrium_gpu_wait_fence {
	uint64_t	value;		/* in: fence value to wait for */
	uint32_t	fence_fd;	/* in: BO fd holding the fence word */
	uint32_t	offset;		/* in: byte offset of the u64 fence */
	uint32_t	timeout_ms;	/* in: max wait (0 = check once) */
	uint32_t	pad;
};

#define ATRIUM_GPU_IOC_BO_ALLOC		_IOWR('A', 0, struct atrium_gpu_bo_alloc)
#define ATRIUM_GPU_IOC_BO_WRITE		_IOW('A', 1, struct atrium_gpu_bo_xfer)
#define ATRIUM_GPU_IOC_BO_READ		_IOW('A', 2, struct atrium_gpu_bo_xfer)
#define ATRIUM_GPU_IOC_SET_COMPUTE	_IOW('A', 3, struct atrium_gpu_set_compute)
#define ATRIUM_GPU_IOC_SUBMIT		_IOW('A', 4, struct atrium_gpu_submit)
#define ATRIUM_GPU_IOC_SET_DRAW		_IOW('A', 5, struct atrium_gpu_set_draw)
#define ATRIUM_GPU_IOC_GET_IRQS		_IOR('A', 6, struct atrium_gpu_irqs)
#define ATRIUM_GPU_IOC_WAIT_FENCE	_IOW('A', 7, struct atrium_gpu_wait_fence)

#endif /* _ATRIUM_GPU_AMD_ABI_H_ */
