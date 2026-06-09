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
 * Device discovery (ABI-v2 §5.1): QUERY_CAPS fills a user buffer with a
 * sequence of TLV records so userspace can skip caps it does not know. Old
 * userspace walks past unrecognized cap_ids; new caps are appended over time.
 */
struct atrium_gpu_caps_query {
	uint64_t	caps_ptr;	/* in: user buffer to fill */
	uint64_t	caps_size;	/* in: buffer bytes; out: bytes needed */
};

struct atrium_gpu_cap_record {
	uint32_t	cap_id;		/* ATRIUM_GPU_CAP_* */
	uint32_t	cap_size;	/* bytes of cap_data that follow */
	/* uint8_t cap_data[cap_size]; then padded to a 4-byte boundary */
};

#define ATRIUM_GPU_CAP_ABI_VERSION	1	/* data: u32 major, u32 minor */
#define ATRIUM_GPU_CAP_VENDOR		2	/* data: NUL-terminated string */
#define ATRIUM_GPU_CAP_FEATURES		3	/* data: u32 ATRIUM_GPU_FEAT_* bitmap */

#define ATRIUM_GPU_FEAT_GRAPHICS	(1u << 0)
#define ATRIUM_GPU_FEAT_COMPUTE		(1u << 1)
#define ATRIUM_GPU_FEAT_USER_QUEUES	(1u << 2)	/* mmap'd doorbell submit */
#define ATRIUM_GPU_FEAT_SYNCOBJ		(1u << 3)	/* kqueue-able timeline */
#define ATRIUM_GPU_FEAT_VM_BIND		(1u << 4)	/* bind apart from submit */

/* Create a per-process GPU address space; the kernel returns it as an fd. */
struct atrium_gpu_vm_create {
	uint32_t	out_fd;		/* out: the vm fd */
	uint32_t	pad;
};

/*
 * Allocate a buffer object: just memory, not yet in any address space (ABI-v2
 * principle 4 — bind apart from submit). Returned as a file descriptor; map it
 * into a VM with VM_BIND before the GPU can reach it.
 */
struct atrium_gpu_bo_alloc {
	uint64_t	size;		/* in:  bytes (<= one page for now) */
	uint32_t	bo_fd;		/* out: file descriptor naming this BO */
	uint32_t	pad;
};

/* Map a BO into a VM at a GPU virtual address (0 = let the kernel pick one). */
struct atrium_gpu_vm_bind {
	uint64_t	va;		/* in: GPU-VA (0 = auto); out: actual VA */
	uint32_t	vm_fd;		/* in: address space to map into */
	uint32_t	bo_fd;		/* in: buffer object to map */
};

/*
 * Set up a user-mode queue: the kernel programs the queue onto `ring_fd` under
 * `vm_fd`'s context (the privileged part) and returns the doorbell. Userspace
 * then mmap()s the device fd at doorbell_mmap_offset and rings the doorbell
 * directly (write the dword write-pointer at doorbell_word_offset) — no submit
 * ioctl on the hot path. The doorbell page is the capability (ABI-v2 §5.9).
 */
struct atrium_gpu_queue_map {
	uint64_t	doorbell_mmap_offset;	/* out: mmap() offset on the dev fd */
	uint64_t	doorbell_size;		/* out: bytes to mmap (one page) */
	uint32_t	doorbell_word_offset;	/* out: this queue's doorbell offset */
	uint32_t	vm_fd;			/* in: address space */
	uint32_t	ring_fd;		/* in: BO holding the ring */
	uint32_t	engine;			/* in: ATRIUM_GPU_ENGINE_* */
};

/* Copy a byte range between userspace and a BO (write = into BO, read = out). */
struct atrium_gpu_bo_xfer {
	uint64_t	offset;		/* in: byte offset within the BO */
	uint64_t	len;		/* in: byte count */
	uint64_t	user_ptr;	/* in: userspace buffer (cast of void *) */
	uint32_t	bo_fd;		/* in: which BO */
	uint32_t	pad;
};

/*
 * Submit a PM4 ring (already laid into the ring BO) on an engine, optionally
 * signalling a syncobj timeline to `signal_value` on completion.
 */
struct atrium_gpu_submit {
	uint64_t	signal_value;	/* in: timeline value to signal on done */
	uint32_t	vm_fd;		/* in: address space to submit under */
	uint32_t	ring_fd;	/* in: BO fd holding the PM4 ring */
	uint32_t	n_dwords;	/* in: ring length in dwords (the wptr) */
	uint32_t	engine;		/* in: ATRIUM_GPU_ENGINE_* */
	int32_t		signal_syncobj_fd; /* in: syncobj to signal (-1 = none) */
	uint32_t	pad;
};

/* Query interrupt state: how many completions the ISR has serviced. */
struct atrium_gpu_irqs {
	uint64_t	count;		/* out: interrupts serviced so far */
	uint32_t	msix_enabled;	/* out: 1 = interrupt mode, 0 = poll mode */
	uint32_t	pad;
};

/* Create a timeline syncobj; the kernel returns it as a (kqueue-able) fd. */
struct atrium_gpu_syncobj_create {
	uint32_t	out_fd;		/* out: the syncobj fd */
	uint32_t	pad;
};

/* Host-side signal (set counter) or query (read counter) of a syncobj. */
struct atrium_gpu_syncobj_op {
	uint64_t	value;		/* signal: in (set to); query: out (current) */
	uint32_t	syncobj_fd;	/* in: which syncobj */
	uint32_t	pad;
};

/* Block until a syncobj's counter reaches `value`, or time out. */
struct atrium_gpu_syncobj_wait {
	uint64_t	value;		/* in: wait until counter >= value */
	uint32_t	syncobj_fd;	/* in: which syncobj */
	uint32_t	timeout_ms;	/* in: max wait (0 = check once) */
};

#define ATRIUM_GPU_IOC_BO_ALLOC		_IOWR('A', 0, struct atrium_gpu_bo_alloc)
#define ATRIUM_GPU_IOC_BO_WRITE		_IOW('A', 1, struct atrium_gpu_bo_xfer)
#define ATRIUM_GPU_IOC_BO_READ		_IOW('A', 2, struct atrium_gpu_bo_xfer)
/* 'A',3 (SET_COMPUTE) and 'A',5 (SET_DRAW) retired: compute/draw state now
 * travels in the ring as SET_SH_REG packets (opaque-blob submit, ABI-v2). */
#define ATRIUM_GPU_IOC_SUBMIT		_IOW('A', 4, struct atrium_gpu_submit)
#define ATRIUM_GPU_IOC_GET_IRQS		_IOR('A', 6, struct atrium_gpu_irqs)
#define ATRIUM_GPU_IOC_SYNCOBJ_CREATE	_IOWR('A', 8, struct atrium_gpu_syncobj_create)
#define ATRIUM_GPU_IOC_SYNCOBJ_SIGNAL	_IOW('A', 9, struct atrium_gpu_syncobj_op)
#define ATRIUM_GPU_IOC_SYNCOBJ_QUERY	_IOWR('A', 10, struct atrium_gpu_syncobj_op)
#define ATRIUM_GPU_IOC_SYNCOBJ_WAIT	_IOW('A', 11, struct atrium_gpu_syncobj_wait)
#define ATRIUM_GPU_IOC_VM_CREATE	_IOWR('A', 12, struct atrium_gpu_vm_create)
#define ATRIUM_GPU_IOC_VM_BIND		_IOWR('A', 13, struct atrium_gpu_vm_bind)
#define ATRIUM_GPU_IOC_QUEUE_MAP	_IOWR('A', 14, struct atrium_gpu_queue_map)
#define ATRIUM_GPU_IOC_QUERY_CAPS	_IOWR('A', 15, struct atrium_gpu_caps_query)
/*
 * Recover a wedged engine: full GPU reset (tears down the rings), reload CP
 * firmware, re-init the MES, and abandon in-flight completions. The
 * timeout -> reset -> resubmit recovery a driver runs when a submission is lost
 * (a forever-unsatisfied cross-queue WAIT, a hang). GPUVM page tables survive,
 * so open VMs/BOs stay valid. No arguments.
 */
#define ATRIUM_GPU_IOC_GPU_RESET	_IO('A', 16)

#endif /* _ATRIUM_GPU_AMD_ABI_H_ */
