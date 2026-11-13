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

/*
 * Cap ids follow include/atrium/gpu_abi_v2.h, which is normative (ABI rev 4;
 * before it HEAPS was 5 and ADDRESS_SPACE 4). Ids from 0x100 are this
 * driver's own and transitional: FEATURES_V1 goes once QUEUE_FAMILIES and
 * SUBMIT_FEATURES are emitted.
 */
#define ATRIUM_GPU_CAP_ABI_VERSION	1	/* data: u32 major, u32 minor */
#define ATRIUM_GPU_CAP_VENDOR		2	/* data: NUL-terminated string */
#define ATRIUM_GPU_CAP_HEAPS		3	/* data: struct atrium_gpu_heap_info[] */
#define ATRIUM_GPU_CAP_ADDRESS_SPACE	5	/* data: struct atrium_gpu_cap_address_space */
#define ATRIUM_GPU_CAP_BIND_FEATURES	9	/* data: u64 ATRIUM_GPU_BIND_FEAT_* */
#define ATRIUM_GPU_CAP_ISA		18	/* data: struct atrium_gpu_isa_info */
#define ATRIUM_GPU_CAP_FEATURES_V1	0x100	/* data: u32 ATRIUM_GPU_FEAT_* bitmap */

#define ATRIUM_GPU_BIND_FEAT_SPARSE	0x1u
#define ATRIUM_GPU_BIND_FEAT_READ_ONLY	0x2u	/* VM_BIND2 + BIND_READONLY */

/*
 * ISA — which instruction set the device runs, so userspace picks its
 * compiler backend and packet builder from this instead of the vendor
 * string. family is ATRIUM_GPU_ISA_*; revision the architecture's own
 * number (0x1201 for gfx1201); name the same as text, NUL padded.
 */
#define ATRIUM_GPU_ISA_AMD_RDNA4	1
#define ATRIUM_GPU_ISA_AMD_RDNA3	2
#define ATRIUM_GPU_ISA_INTEL_XE2	3
struct atrium_gpu_isa_info {
	uint32_t	family;
	uint32_t	revision;
	char		name[24];
};

/*
 * ADDRESS_SPACE — the per-VM GPU virtual-address window a BO can be bound into,
 * so userspace sizes allocations to it instead of discovering the limit by
 * ENOSPC. va_base..va_base+va_size is biddable; bind VAs are va_align-aligned.
 */
struct atrium_gpu_cap_address_space {
	uint64_t	va_base;
	uint64_t	va_size;
	uint64_t	va_align;
};

/* HEAPS — one record per memory pool the device exposes. */
#define ATRIUM_GPU_HEAP_DEVICE	0	/* device-local VRAM */
#define ATRIUM_GPU_HEAP_SYSTEM	1	/* host/System (GTT) memory */
struct atrium_gpu_heap_info {
	uint32_t	kind;	/* ATRIUM_GPU_HEAP_* */
	uint32_t	flags;	/* reserved (0) */
	uint64_t	size;	/* bytes; 0 = host-bounded (System) */
};

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
	uint64_t	size;		/* in:  bytes */
	uint32_t	bo_fd;		/* out: file descriptor naming this BO */
	uint32_t	flags;		/* in:  ATRIUM_GPU_BO_* placement */
};
#define ATRIUM_GPU_BO_VRAM	0x1	/* place in device VRAM (else System/GTT).
					 * VRAM BOs are GPU-only (no CPU map) —
					 * populate them with a GPU copy from a
					 * System staging BO, and re-upload after a
					 * GPU reset (reset loses VRAM). */

/*
 * 'A',27 (BO_EXPORT_SCANOUT) is retired (GPU ABI v2.1 §7): it turned a BO into a
 * raw {vram_offset, size} that any display opener could then scan out. A
 * display commit now names the BO by its fd (include/atrium/display_abi_v2.h)
 * and the display module resolves it itself.
 */

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

/*
 * VM_BIND with flags (ABI rev 3). READONLY maps the BO without write access in
 * that VM: a GPU store to it is a VM fault. The broker maps an app's code and
 * constants this way (F6, docs/spec/gpu-app-path-f6.md). VM_BIND is VM_BIND2
 * with no flags.
 */
struct atrium_gpu_vm_bind2 {
	uint64_t	va;		/* in: GPU-VA (0 = auto); out: actual VA */
	uint32_t	vm_fd;
	uint32_t	bo_fd;
	uint32_t	flags;		/* ATRIUM_GPU_BIND_* */
	uint32_t	pad;		/* must be 0 */
};
#define ATRIUM_GPU_BIND_READONLY	0x1

/*
 * Queue fds (GPU ABI v2 §5.5, §5.7; stage 5c-2b). Layouts and numbers are
 * v2's (include/atrium/gpu_abi_v2.h: atrium_gpu_queue_create, and
 * atrium_gpu_submit, here named atrium_gpu_queue_submit after the retired
 * v1 SUBMIT, 'A',4, which held the name).
 *
 * QUEUE_CREATE is an ioctl on a VM fd: a queue that runs in that VM, as a
 * new fd holding a reference on the VM. family_id 0 is compute (the only
 * family on silicon so far; gpusim also has 1, gfx); priority 0 only; flags
 * 0 or USER_MODE (below, silicon only: gpusim refuses it, EOPNOTSUPP). On
 * gpusim a queue fd's work runs on the shared engine queue, in its VM.
 * The fd is the right to submit: it passes over SCM_RIGHTS, and its
 * last close takes the queue off the GPU, cancelling (ECANCELED) what has
 * not completed.
 *
 * QUEUE_SUBMIT is an ioctl on the queue fd: run blob_size bytes of PM4 at
 * blob_va (in the queue's VM) as one indirect buffer; on completion signal
 * the out dependency (at most one in 5c-2b; in_dep_count must be 0).
 * out_seqno is the submit's number on this queue, from 1.
 *
 * USER_MODE (5c-2b step 3; interim: PRIV_DRIVER only): the client writes
 * the ring itself, so the holder is its own packet writer. It is only for a
 * trusted renderer's own path (frescod's GPU renderer, aqueduct-gpu-host),
 * never minted for an app or a per-client renderer (2026-11-13). QUEUE_MAP2 on the queue fd gives the mmap(queue_fd)
 * layout: the ring (PM4, ring_size bytes; the CP fetches it read-only),
 * the writeback page and the queue's own doorbell page. Map MAP_SHARED,
 * read-write; never executable. The writeback page, from wptr_offset - 8:
 *   +0x00  rptr report: the CP's fetch position, dwords into the ring
 *   +0x08  wptr (client): 64 bits, dwords written since creation
 *   +0x10  fence: what the client's RELEASE_MEMs write, at fence_gpu_va
 *   +0x18  emitted (client): the last fence value it put on the ring
 * To submit: write packets, then emitted, then wptr, then the wptr (64
 * bits) at doorbell_offset. A job is done when the fence reaches its
 * value. QUEUE_SUBMIT is refused (EOPNOTSUPP). A lost queue (fault, or
 * hang: the fence short of emitted or the rptr short of the wptr, neither
 * moving for hang_ms) is taken off the GPU: QUEUE_MAP2 then fails with EIO and a page not yet touched
 * faults (SIGBUS). The last close takes every mapping back, so the next
 * touch is SIGBUS.
 */
#define ATRIUM_GPU_QUEUE_FAMILY_COMPUTE	0u
/* gpusim only (its rasterizer runs on the GFX engine); silicon: EINVAL. */
#define ATRIUM_GPU_QUEUE_FAMILY_GFX	1u
#define ATRIUM_GPU_QUEUE_FLAG_USER_MODE	0x1u

struct atrium_gpu_queue_create {	/* on vm_fd */
	uint32_t	struct_size;
	uint32_t	family_id;
	uint32_t	priority;
	uint32_t	flags;
	int32_t		out_queue_fd;
	uint32_t	_pad;
};

/* GPU ABI v2's atrium_gpu_queue_map (named apart from v1's 'A',14 one). */
struct atrium_gpu_queue_map2 {		/* on queue_fd */
	uint32_t	struct_size;
	uint32_t	_reserved;
	uint64_t	out_ring_offset;	/* mmap(queue_fd) offsets */
	uint64_t	out_ring_size;
	uint64_t	out_doorbell_offset;
	uint64_t	out_doorbell_size;
	uint64_t	out_wptr_offset;
	uint64_t	out_fence_gpu_va;	/* v2 2.2 (§5.9): advisory */
};

struct atrium_gpu_syncobj_point {
	int32_t		syncobj_fd;
	uint32_t	_pad;
	uint64_t	value;
};

struct atrium_gpu_queue_submit {	/* on queue_fd */
	uint32_t	struct_size;
	uint32_t	flags;		/* must be 0 */
	uint64_t	blob_va;
	uint64_t	blob_size;	/* bytes, a multiple of 4 */
	uint64_t	in_dep_ptr;	/* atrium_gpu_syncobj_point[] */
	uint32_t	in_dep_count;
	uint32_t	deadline_ns;
	uint64_t	out_dep_ptr;	/* atrium_gpu_syncobj_point[] */
	uint32_t	out_dep_count;
	uint32_t	_pad2;
	uint64_t	out_seqno;
};

#define ATRIUM_GPU_IOC_QUEUE_CREATE	_IOWR('A', 79, struct atrium_gpu_queue_create)
#define ATRIUM_GPU_IOC_QUEUE_MAP2	_IOWR('A', 82, struct atrium_gpu_queue_map2)
#define ATRIUM_GPU_IOC_QUEUE_SUBMIT	_IOWR('A', 83, struct atrium_gpu_queue_submit)

#define ATRIUM_GPU_IOC_BO_ALLOC		_IOWR('A', 0, struct atrium_gpu_bo_alloc)
#define ATRIUM_GPU_IOC_BO_WRITE		_IOW('A', 1, struct atrium_gpu_bo_xfer)
#define ATRIUM_GPU_IOC_BO_READ		_IOW('A', 2, struct atrium_gpu_bo_xfer)
/* 'A',3 (SET_COMPUTE) and 'A',5 (SET_DRAW) retired: compute/draw state now
 * travels in the ring as SET_SH_REG packets (opaque-blob submit, ABI-v2). */
/* 'A',4 (the v1 ring SUBMIT: a ring BO fd on a shared engine queue) retired
 * 2026-11-13: all work goes through queue fds (QUEUE_CREATE/QUEUE_SUBMIT
 * below), on gpusim and silicon alike. The number stays reserved. */
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

/*
 * Firmware progress-fair scheduler (gpu-scheduler §9, corrected per the
 * scheduler-federation doc): the kernel sets per-queue weights, the device
 * (firmware) enforces TIME-charged weighted fair queueing. Energy is telemetry
 * for the budget layer, never the charge. op 0 = add a queue with `arg`=weight
 * and the `ops`/`bytes`/`level` per-dispatch kernel; op 1 = run `arg` rounds;
 * op 2 = query queue `arg` (busy_us = the fairness readback; energy_uj = the
 * Joule telemetry; runs).
 */
struct atrium_gpu_sched {
	uint32_t op;
	uint32_t arg;
	uint32_t ops;		/* in (add): per-dispatch compute ops */
	uint32_t bytes;		/* in (add): per-dispatch bytes */
	uint32_t level;		/* in (add): memory level (0=Reg..4=Host) */
	uint32_t energy_uj;	/* out (query): accumulated energy, µJ (telemetry) */
	uint32_t runs;		/* out (query): run count */
	uint32_t busy_us;	/* out (query): engine time, µs (fairness) */
	uint32_t count;		/* out: number of queues */
	uint32_t deadline_ns;	/* in (set-deadline, op 4): ns from now (0 = clear) */
};
/*
 * sched ops (s->op): 0 add-queue, 1 run-rounds, 2 query-queue, 3 set deadline
 * WINDOW (s->arg ns; 0 = deadline-blind/fair), 4 set queue s->arg's DEADLINE
 * (s->deadline_ns from now). 3+4 are the frame-pacing path: a broker (frescod)
 * stamps the compositor's queue with its target vblank so the scheduler serves
 * it decisively as the deadline nears (atrium-gpu-scheduler §6).
 */
#define ATRIUM_GPU_IOC_SCHED		_IOWR('A', 25, struct atrium_gpu_sched)

/*
 * The display engine (connector discovery, modeset, page-flip, the §8 output
 * topology) is a SEPARATE driver — /dev/atrium-display0 (atrium_gpu_amd_display.ko),
 * with its own ABI (include/atrium/display_abi_v2.h). The scanout buffer
 * crosses from the GPU to the display as its BO fd, like a dma-buf fd into a
 * KMS plane; the display module resolves it through the gpu module. The display
 * 'A',17-24 ioctls that used to live here were retired in the §4.1 split.
 */

/*
 * Power gating: gate idle IP blocks. The caller passes which blocks the current
 * workload uses (busy_mask) and, optionally, which the NEXT workload will need
 * (next_busy, from Fresco/Lyra/GPU-sched foreknowledge). The driver gates every
 * idle block, pre-waking the foreknown ones, and reports the draw before/after
 * plus the wake stall the next workload would face (0 once pre-woken).
 */
struct atrium_gpu_powergate {
	uint32_t busy_mask;		/* in: blocks the current workload uses */
	uint32_t next_busy;		/* in: blocks the next workload needs (0 = none) */
	uint32_t gate_mask;		/* out: blocks the driver power-gated */
	uint32_t power_before_mw;	/* out: draw with idle blocks ungated */
	uint32_t power_after_mw;	/* out: draw after gating idle blocks */
	uint32_t wake_stall_us;		/* out: next workload's wake stall (0 = pre-woken) */
};
#define ATRIUM_GPU_IOC_POWERGATE	_IOWR('A', 26, struct atrium_gpu_powergate)
#define ATRIUM_GPU_IOC_VM_BIND2		_IOWR('A', 27, struct atrium_gpu_vm_bind2)

#endif /* _ATRIUM_GPU_AMD_ABI_H_ */
