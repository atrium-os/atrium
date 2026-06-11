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
	uint64_t	size;		/* in:  bytes */
	uint32_t	bo_fd;		/* out: file descriptor naming this BO */
	uint32_t	flags;		/* in:  ATRIUM_GPU_BO_* placement */
};
#define ATRIUM_GPU_BO_VRAM	0x1	/* place in device VRAM (else System/GTT).
					 * VRAM BOs are GPU-only (no CPU map) —
					 * populate them with a GPU copy from a
					 * System staging BO, and re-upload after a
					 * GPU reset (reset loses VRAM). */

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

/*
 * Display (D-display-1): one connector / one CRTC. The display block is
 * architecturally independent of the GFX/compute engine (its own registers);
 * these ioctls drive QUERY -> SET_MODE -> FLIP -> STATUS. Scanout FBs are VRAM
 * BOs (fd-as-handle), read by the display by VRAM offset.
 */
struct atrium_gpu_display_query {
	uint32_t connected;	/* out: 1 = monitor attached (HPD) */
	uint32_t connector_type; /* out: §8 interface type code (1=HDMI1.4 2=HDMI2.1
				  * 3=DP1.4 4=USB-C-DP-alt 5=eDP) */
	uint32_t usbc_lanes;	/* out: USB-C alt-mode lane count (0 = not USB-C) */
	uint32_t edid_len;	/* out: EDID bytes returned (128) */
	uint8_t  edid[128];	/* out: EDID base block read over DDC */
};

struct atrium_gpu_display_setmode {
	int32_t  fb_fd;		/* in: VRAM BO to scan out */
	uint32_t fault;		/* out: DisplayFault code (0 = ok) */
};

struct atrium_gpu_display_flip {
	int32_t  fb_fd;		/* in: VRAM BO to flip to */
	uint32_t vsync;		/* in: 1 = latch at vblank (no tear) */
	uint32_t fault;		/* out: DisplayFault code (0 = ok) */
};

struct atrium_gpu_display_status {
	uint64_t vblank_count;	/* out: vblanks elapsed */
	uint32_t dropped_flips;	/* out: flips dropped by the depth-1 queue */
	uint32_t tear_line;	/* out: first tear scanline (0xffffffff = none) */
};

/*
 * Reconfigure the simulated monitor (bring-up / test): re-cable the connector to
 * a different interface type and/or re-plug it advertising a different built-in
 * mode. Lets a test drive the §8 link-bandwidth referee (e.g. a 4K monitor on an
 * HDMI 1.4 cable -> ModeExceedsLink).
 */
struct atrium_gpu_display_config {
	uint32_t connector_type; /* in: type code (1=HDMI1.4 2=HDMI2.1 3=DP1.4 ...) */
	uint32_t plug_mode;	/* in: re-plug advertising mode (0=VGA 1=1080p 2=4K) */
};

/*
 * USB-C DisplayPort Alt Mode (§8 cross-subsystem): the display connector is
 * virtual until PD negotiates alt-mode. `lanes` = 0 puts the port in USB mode (no
 * display); 2 or 4 enters DP Alt Mode with that lane count (4 = full bandwidth,
 * 2 = half — USB takes the other pair).
 */
struct atrium_gpu_display_usbc {
	uint32_t lanes;
};

/*
 * DisplayPort MST (§8): one link fans out to a dynamic set of sinks sharing its
 * bandwidth. op 0 = enable/reset the hub; op 1 = add a sink advertising mode
 * `arg` (0=VGA 1=1080p 2=4K); op 2 = query sink `arg`. `count`/`starved` are out.
 */
struct atrium_gpu_display_mst {
	uint32_t op;
	uint32_t arg;
	uint32_t count;		/* out: number of sinks */
	uint32_t starved;	/* out: (op 2) selected sink bandwidth-starved? */
};

/*
 * DisplayPort link training (§8): negotiate against a physical cable. Usable
 * bandwidth is an outcome — a marginal cable falls back to a lower rate.
 */
struct atrium_gpu_display_dptrain {
	uint32_t cable_rate;	/* in: cable's max rate (0=RBR 1=HBR 2=HBR2 3=HBR3) */
	uint32_t cable_lanes;	/* in: cable's wired lane count */
	uint32_t bw_mbps;	/* out: trained bandwidth, MB/s */
	uint32_t trained;	/* out: 1 = a link trained (0 = dead cable) */
};

#define ATRIUM_GPU_IOC_DISPLAY_QUERY	_IOWR('A', 17, struct atrium_gpu_display_query)
#define ATRIUM_GPU_IOC_DISPLAY_SET_MODE	_IOWR('A', 18, struct atrium_gpu_display_setmode)
#define ATRIUM_GPU_IOC_DISPLAY_FLIP	_IOWR('A', 19, struct atrium_gpu_display_flip)
#define ATRIUM_GPU_IOC_DISPLAY_STATUS	_IOR('A', 20, struct atrium_gpu_display_status)
#define ATRIUM_GPU_IOC_DISPLAY_CONFIG	_IOW('A', 21, struct atrium_gpu_display_config)
#define ATRIUM_GPU_IOC_DISPLAY_USBC	_IOW('A', 22, struct atrium_gpu_display_usbc)
#define ATRIUM_GPU_IOC_DISPLAY_MST	_IOWR('A', 23, struct atrium_gpu_display_mst)
#define ATRIUM_GPU_IOC_DISPLAY_DPTRAIN	_IOWR('A', 24, struct atrium_gpu_display_dptrain)

#endif /* _ATRIUM_GPU_AMD_ABI_H_ */
