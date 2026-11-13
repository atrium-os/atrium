/*
 * Atrium GPU ABI v2.1 — normative header.
 *
 * This header and docs/spec/atrium-gpu-abi-v2.md are one specification: where
 * they disagree it is a bug in one of them, to be fixed in both. Every vendor
 * module (atrium-gpu-amd, atrium-virtio-gpu, ...) and every userspace consumer
 * (atrium-gpu-rs, atrium-vk-icd) uses these definitions. The intermediate
 * atrium-gpu-amd ABI (atrium-gpu-amd/atrium_gpu_amd_abi.h, 'A' 0..27) converges
 * to this one; v2.1 ioctls are numbered from 64 so both can coexist during the
 * migration.
 *
 * Rules (spec §5, §8):
 *  - Every argument struct begins with uint32_t struct_size. The kernel uses
 *    min(struct_size, sizeof known) and rejects sizes below the 2.1 layout.
 *  - _pad/_reserved fields and unknown flag bits must be zero (EINVAL).
 *  - Every fd field is int32_t; -1 means none.
 *  - Every pointer field is a uint64_t user address of a flat array of POD
 *    with an explicit count (transport-clean, §6.4 rule 2).
 *  - All multi-byte fields are little-endian.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */
#ifndef _ATRIUM_GPU_ABI_V2_H_
#define _ATRIUM_GPU_ABI_V2_H_

#include <stdint.h>
#ifdef __FreeBSD__
#include <sys/ioccom.h>
#else
#include <sys/ioctl.h>
#endif

#define ATRIUM_GPU_ABI_MAJOR	2
#define ATRIUM_GPU_ABI_MINOR	1

/* ------------------------------------------------------------------ */
/* Error codes recorded on errored signal points (§5.6, §10)           */
/* ------------------------------------------------------------------ */
#define ATRIUM_GPU_ERR_NONE		0
#define ATRIUM_GPU_ERR_FAULT		1  /* GPU page fault or shader trap in this queue */
#define ATRIUM_GPU_ERR_HANG		2  /* watchdog expired on this queue */
#define ATRIUM_GPU_ERR_RESET		3  /* innocent: lost to a device reset (rung 1+) */
#define ATRIUM_GPU_ERR_DEVICE_GONE	4  /* device detached (rung 3) or hot-removed */
#define ATRIUM_GPU_ERR_CANCELLED	5  /* queue closed or reset before the work ran */
#define ATRIUM_GPU_ERR_DEPENDENCY	6  /* an in-dependency was itself errored */

/* kevent fflags on a syncobj fd (§5.6) */
#define ATRIUM_GPU_NOTE_ERROR		0x00000001u

/* ------------------------------------------------------------------ */
/* Capabilities (§5.1)                                                 */
/* ------------------------------------------------------------------ */
struct atrium_gpu_caps_query {
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t out_caps_ptr;		/* user buffer of TLV records */
	uint64_t out_caps_size;		/* in: buffer bytes; out: bytes needed */
};

struct atrium_gpu_cap_record {
	uint32_t cap_id;
	uint32_t cap_size;		/* payload bytes, multiple of 8 */
	/* uint8_t cap_data[cap_size]; */
};

#define ATRIUM_GPU_CAP_ABI_VERSION	1   /* uint32 major, minor */
#define ATRIUM_GPU_CAP_VENDOR		2   /* NUL-padded UTF-8 */
#define ATRIUM_GPU_CAP_HEAPS		3   /* atrium_gpu_heap_info[] */
#define ATRIUM_GPU_CAP_QUEUE_FAMILIES	4   /* atrium_gpu_queue_family_info[] */
#define ATRIUM_GPU_CAP_ADDRESS_SPACE	5   /* uint64 va_base, va_size, va_align */
#define ATRIUM_GPU_CAP_FORMATS		6   /* bitmap */
#define ATRIUM_GPU_CAP_MODIFIERS	7   /* (uint32 format, uint32 _pad, uint64 modifier)[] */
#define ATRIUM_GPU_CAP_SYNC_FEATURES	8   /* uint64 bitmap */
#define ATRIUM_GPU_CAP_BIND_FEATURES	9   /* uint64 bitmap, ATRIUM_GPU_BIND_FEAT_* */
#define ATRIUM_GPU_CAP_SUBMIT_FEATURES	10  /* uint64 bitmap, ATRIUM_GPU_SUBMIT_FEAT_* */
#define ATRIUM_GPU_CAP_VULKAN_EXTENSIONS 11 /* NUL-separated strings */
#define ATRIUM_GPU_CAP_TELEMETRY	12  /* uint64 bitmap */
#define ATRIUM_GPU_CAP_LOST_GENERATION	13  /* uint64 */
#define ATRIUM_GPU_CAP_LIMITS		14  /* atrium_gpu_limits */
#define ATRIUM_GPU_CAP_CLOCK		15  /* atrium_gpu_clock_info */
#define ATRIUM_GPU_CAP_PREEMPTION	16  /* atrium_gpu_preemption_info */
#define ATRIUM_GPU_CAP_PERF_COUNTERS	17  /* atrium_gpu_perf_counter_info[] */
#define ATRIUM_GPU_CAP_ISA		18  /* atrium_gpu_isa_info */
/* Ids from 0x100 are a driver's own and transitional (atrium-gpu-amd's
 * FEATURES_V1 is 0x100); clients skip ids they do not know. */

/*
 * ISA: the instruction set the device runs. Userspace picks its compiler
 * backend and packet builder by family, never by the vendor string, so a
 * new GPU family slots in behind the same interfaces (the app path's
 * atrium_gpu_app::session::Arch, Fresco's fresco_compute::gpu::Arch).
 * revision is the family's own number (0x1201 for gfx1201); name the same
 * as text, NUL padded.
 */
#define ATRIUM_GPU_ISA_AMD_RDNA4	1
#define ATRIUM_GPU_ISA_AMD_RDNA3	2
#define ATRIUM_GPU_ISA_INTEL_XE2	3
struct atrium_gpu_isa_info {
	uint32_t family;
	uint32_t revision;
	char     name[24];
};

#define ATRIUM_GPU_HEAP_DEVICE_LOCAL	0x1u
#define ATRIUM_GPU_HEAP_CPU_VISIBLE	0x2u  /* mappable with BO_MMAP_INFO */
#define ATRIUM_GPU_HEAP_CPU_CACHED	0x4u
#define ATRIUM_GPU_HEAP_LOST_ON_RESET	0x8u  /* contents do not survive rung 2 */

struct atrium_gpu_heap_info {
	uint32_t heap_index;
	uint32_t flags;			/* ATRIUM_GPU_HEAP_* */
	uint64_t size;			/* bytes */
	uint64_t min_alignment;
};

#define ATRIUM_GPU_ENGINE_GFX		1
#define ATRIUM_GPU_ENGINE_COMPUTE	2
#define ATRIUM_GPU_ENGINE_COPY		3

#define ATRIUM_GPU_QFAM_USER_MODE	0x1u  /* supports QUEUE_FLAG_USER_MODE */
#define ATRIUM_GPU_QFAM_TIMESTAMPS	0x2u
#define ATRIUM_GPU_QFAM_PREEMPT_MID	0x4u  /* mid-command preemption */

struct atrium_gpu_queue_family_info {
	uint32_t family_id;
	uint32_t engine;		/* ATRIUM_GPU_ENGINE_* */
	uint32_t flags;			/* ATRIUM_GPU_QFAM_* */
	uint32_t priority_levels;	/* client-usable priorities: 0..n-1 */
};

struct atrium_gpu_quota {
	uint64_t vram_bytes;
	uint64_t system_bytes;
	uint64_t va_bytes;
	uint32_t max_vms;
	uint32_t max_queues;
	uint32_t max_syncobjs;
	uint32_t max_inflight;
};

struct atrium_gpu_limits {
	uint64_t max_blob;		/* SUBMIT blob_size limit */
	uint32_t max_bind_ops;		/* per VM_BIND call */
	uint32_t max_deps;		/* per SUBMIT, in and out each */
	struct atrium_gpu_quota quota;	/* this context's quota (§4.2) */
};

struct atrium_gpu_clock_info {
	uint64_t timestamp_hz;		/* GPU timestamp tick rate */
	uint32_t timestamp_valid_bits;
	uint32_t _pad;
};

#define ATRIUM_GPU_PREEMPT_NONE		0  /* runs to completion */
#define ATRIUM_GPU_PREEMPT_DRAW		1  /* between draws/dispatches */
#define ATRIUM_GPU_PREEMPT_WAVE		2  /* between waves/primitives */
#define ATRIUM_GPU_PREEMPT_INSTRUCTION	3  /* mid-shader */

struct atrium_gpu_preemption_info {
	uint32_t family_id;
	uint32_t granularity;		/* ATRIUM_GPU_PREEMPT_* */
	uint64_t worst_case_ns;		/* 0 = vendor states no bound */
};

#define ATRIUM_GPU_PERF_PER_CONTEXT	0x1u  /* counts only the opening context's work */

struct atrium_gpu_perf_counter_info {
	uint32_t counter_id;
	uint32_t flags;			/* ATRIUM_GPU_PERF_PER_CONTEXT */
	char     name[24];		/* NUL-padded */
};

#define ATRIUM_GPU_BIND_FEAT_SPARSE	0x1u
#define ATRIUM_GPU_BIND_FEAT_READ_ONLY	0x2u  /* mandatory in 2.1 */
#define ATRIUM_GPU_SUBMIT_FEAT_USER_QUEUE 0x1u
#define ATRIUM_GPU_SUBMIT_FEAT_GPU_WAIT	0x2u  /* same-device deps waited on the GPU */

/* ------------------------------------------------------------------ */
/* Authority and accounting (§4.1, §4.2)                               */
/* ------------------------------------------------------------------ */
#define ATRIUM_GPU_CLIENT_ALLOW_USER_QUEUES 0x1u  /* §5.9 */
#define ATRIUM_GPU_CLIENT_ALLOW_PERF	    0x2u  /* §5.15; test/dev only */

struct atrium_gpu_client_open {			/* on control_fd */
	uint32_t struct_size;
	uint32_t flags;			/* ATRIUM_GPU_CLIENT_* */
	int32_t  jail_id;		/* 0 = the broker's own */
	uint32_t sched_weight;		/* 0 = default */
	struct atrium_gpu_quota quota;	/* zero fields = device default */
	int32_t  out_device_fd;
	uint32_t _pad;
};

struct atrium_gpu_set_quota {			/* on control_fd */
	uint32_t struct_size;
	int32_t  jail_id;
	struct atrium_gpu_quota quota;
};

struct atrium_gpu_query_usage {			/* device_fd: own jail; control_fd: any */
	uint32_t struct_size;
	int32_t  jail_id;		/* ignored on device_fd */
	struct atrium_gpu_quota out_used;
	struct atrium_gpu_quota out_limit;
};

/* ------------------------------------------------------------------ */
/* VM, BO, bind (§5.2-5.4, §5.13)                                      */
/* ------------------------------------------------------------------ */
struct atrium_gpu_vm_create {			/* on device_fd */
	uint32_t struct_size;
	uint32_t flags;			/* must be 0 in 2.1 */
	uint64_t va_size;		/* 0 = device default */
	int32_t  out_vm_fd;
	uint32_t _pad;
};

#define ATRIUM_GPU_BO_CPU_ACCESS	0x1u
#define ATRIUM_GPU_BO_SCANOUT		0x2u

struct atrium_gpu_bo_create {			/* on device_fd */
	uint32_t struct_size;
	uint32_t flags;			/* ATRIUM_GPU_BO_* */
	uint64_t size;
	uint64_t alignment;		/* 0 = device default */
	uint32_t heap_mask;		/* bit i = heap_index i acceptable */
	uint32_t _pad;
	int32_t  out_bo_fd;
	uint32_t _pad2;
};

struct atrium_gpu_bo_mmap_info {		/* on bo_fd */
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t out_offset;		/* mmap(device_fd, ..., offset) */
	uint64_t out_size;
};

#define ATRIUM_GPU_BOQ_SEALED		0x1u
#define ATRIUM_GPU_BOQ_LOST		0x2u  /* contents lost to a reset */
#define ATRIUM_GPU_BOQ_SCANOUT		0x4u
#define ATRIUM_GPU_BOQ_CPU_ACCESS	0x8u
#define ATRIUM_GPU_BOQ_IMPORTED		0x10u

struct atrium_gpu_bo_query {			/* on bo_fd */
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t out_size;
	uint32_t out_heap_index;
	uint32_t out_flags;		/* ATRIUM_GPU_BOQ_* */
};

struct atrium_gpu_bo_xfer {			/* on bo_fd; bring-up and tests only */
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t bo_offset;
	uint64_t user_ptr;
	uint64_t size;
};

struct atrium_gpu_bo_seal {			/* on bo_fd */
	uint32_t struct_size;
	uint32_t flags;			/* must be 0 */
	int32_t  wait_syncobj_fd;	/* -1 = seal now */
	uint32_t _pad;
	uint64_t wait_value;
};

struct atrium_gpu_bo_export_share {		/* on bo_fd */
	uint32_t struct_size;
	uint32_t flags;
	int32_t  out_share_fd;
	uint32_t _pad;
};

struct atrium_gpu_bo_import_share {		/* on device_fd */
	uint32_t struct_size;
	uint32_t flags;
	int32_t  share_fd;
	int32_t  out_bo_fd;
};

#define ATRIUM_GPU_BIND_MAP		1
#define ATRIUM_GPU_BIND_UNMAP		2
#define ATRIUM_GPU_BIND_REMAP		3

#define ATRIUM_GPU_BIND_READ_ONLY	0x1u  /* GPU PTEs read-only; required for sealed BOs */
#define ATRIUM_GPU_BIND_SPARSE		0x2u  /* needs BIND_FEAT_SPARSE */

struct atrium_gpu_vm_bind_op {
	uint32_t op;			/* ATRIUM_GPU_BIND_* */
	uint32_t flags;			/* ATRIUM_GPU_BIND_READ_ONLY, ... */
	int32_t  bo_fd;			/* -1 for UNMAP */
	uint32_t _pad;
	uint64_t bo_offset;
	uint64_t va;			/* 0 = kernel picks (MAP); out: actual VA */
	uint64_t length;
};

struct atrium_gpu_syncobj_point {
	int32_t  syncobj_fd;
	uint32_t _pad;
	uint64_t value;
};

struct atrium_gpu_vm_bind {			/* on vm_fd */
	uint32_t struct_size;
	uint32_t op_count;
	uint64_t ops_ptr;		/* atrium_gpu_vm_bind_op[op_count] */
	int32_t  signal_syncobj_fd;	/* -1 = synchronous */
	uint32_t _pad;
	uint64_t signal_value;
	uint32_t wait_count;
	uint32_t out_failed_op;		/* index of the op that failed, on error */
	uint64_t wait_ptr;		/* atrium_gpu_syncobj_point[wait_count] */
};

/* ------------------------------------------------------------------ */
/* Queues and submission (§5.5, §5.7, §5.9)                            */
/* ------------------------------------------------------------------ */
#define ATRIUM_GPU_QUEUE_FLAG_USER_MODE	0x1u

struct atrium_gpu_queue_create {		/* on vm_fd */
	uint32_t struct_size;
	uint32_t family_id;
	uint32_t priority;		/* 0..priority_levels-1 */
	uint32_t flags;			/* ATRIUM_GPU_QUEUE_FLAG_* */
	int32_t  out_queue_fd;
	uint32_t _pad;
};

struct atrium_gpu_queue_reset {			/* on queue_fd */
	uint32_t struct_size;
	uint32_t flags;			/* must be 0 */
};

#define ATRIUM_GPU_FAULT_READ		0x1u
#define ATRIUM_GPU_FAULT_WRITE		0x2u
#define ATRIUM_GPU_FAULT_EXEC		0x4u

struct atrium_gpu_queue_query_fault {		/* on queue_fd */
	uint32_t struct_size;
	uint32_t out_error;		/* ATRIUM_GPU_ERR_*, 0 if healthy */
	uint64_t out_seqno;		/* submission that faulted */
	uint64_t out_fault_va;
	uint32_t out_fault_flags;	/* ATRIUM_GPU_FAULT_* */
	uint32_t out_engine;		/* ATRIUM_GPU_ENGINE_* */
};

struct atrium_gpu_submit {			/* on queue_fd */
	uint32_t struct_size;
	uint32_t flags;			/* must be 0 in 2.1 */
	uint64_t blob_va;		/* bound in this queue's VM */
	uint64_t blob_size;
	uint64_t in_dep_ptr;		/* atrium_gpu_syncobj_point[] */
	uint32_t in_dep_count;
	uint32_t deadline_ns;		/* 0 = none */
	uint64_t out_dep_ptr;		/* atrium_gpu_syncobj_point[] */
	uint32_t out_dep_count;
	uint32_t _pad2;
	uint64_t out_seqno;
};

struct atrium_gpu_queue_map {			/* on queue_fd */
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t out_ring_offset;	/* mmap(queue_fd) offsets */
	uint64_t out_ring_size;
	uint64_t out_doorbell_offset;
	uint64_t out_doorbell_size;
	uint64_t out_wptr_offset;
	uint64_t out_fence_gpu_va;	/* 2.2: advisory, client-written (§5.9) */
};

/* ------------------------------------------------------------------ */
/* Syncobjs (§5.6)                                                     */
/* ------------------------------------------------------------------ */
struct atrium_gpu_syncobj_create {		/* on device_fd */
	uint32_t struct_size;
	uint32_t flags;
	uint64_t initial_value;
	int32_t  out_syncobj_fd;
	uint32_t _pad;
};

struct atrium_gpu_syncobj_signal {		/* on syncobj_fd */
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t value;
};

struct atrium_gpu_syncobj_wait {		/* on syncobj_fd */
	uint32_t struct_size;
	uint32_t flags;
	uint64_t value;
	int64_t  timeout_ns;		/* -1 forever, 0 poll */
};

struct atrium_gpu_syncobj_query {		/* on syncobj_fd */
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t out_current_value;
	uint32_t out_error;		/* ATRIUM_GPU_ERR_* */
	uint32_t _pad;
	uint64_t out_error_value;
};

/* ------------------------------------------------------------------ */
/* Telemetry, clocks, perf counters (§5.10, §5.15)                     */
/* ------------------------------------------------------------------ */
struct atrium_gpu_telemetry {			/* device_fd: own; control_fd: device */
	uint32_t struct_size;
	uint32_t _reserved;
	uint64_t busy_ns;
	uint64_t sample_ns;
	uint32_t queue_depth;
	uint32_t power_state;		/* control_fd only; 0 on device_fd */
	uint32_t tier_cost_hint;
	uint32_t flags;
};

struct atrium_gpu_clock_calibrate {		/* on device_fd */
	uint32_t struct_size;
	uint32_t flags;			/* must be 0: CPU side is CLOCK_MONOTONIC */
	uint64_t out_gpu_timestamp;
	uint64_t out_cpu_ns;
	uint64_t out_max_deviation_ns;
};

#define ATRIUM_GPU_PERF_DEVICE_WIDE	0x1u  /* control_fd only */

struct atrium_gpu_perf_open {			/* device_fd (ALLOW_PERF) or control_fd */
	uint32_t struct_size;
	uint32_t flags;			/* ATRIUM_GPU_PERF_DEVICE_WIDE */
	uint32_t counter_count;
	uint32_t _pad;
	uint64_t counters_ptr;		/* uint32 counter_id[counter_count] */
	int32_t  out_perf_fd;		/* read(2): uint64 value[counter_count] */
	uint32_t _pad2;
};

/* ------------------------------------------------------------------ */
/* Control-only device ioctls (§5.10, §10)                             */
/* ------------------------------------------------------------------ */
#define ATRIUM_GPU_SCHED_SET_WEIGHT	1  /* target: device_fd; value: weight */
#define ATRIUM_GPU_SCHED_SET_PRIORITY	2  /* target: queue_fd; value: class (incl. reserved top) */
#define ATRIUM_GPU_SCHED_DEADLINE_WINDOW 3 /* target: -1; value: ns (0 = fair) */
#define ATRIUM_GPU_SCHED_WATCHDOG	4  /* target: device_fd or queue_fd; value: ns */
#define ATRIUM_GPU_SCHED_RESET_BUDGET	5  /* target: -1; value: (count << 32) | window_s */

struct atrium_gpu_sched {			/* on control_fd */
	uint32_t struct_size;
	uint32_t op;			/* ATRIUM_GPU_SCHED_* */
	int32_t  target_fd;
	uint32_t _pad;
	uint64_t value;
};

struct atrium_gpu_powergate {			/* on control_fd */
	uint32_t struct_size;
	uint32_t busy_mask;		/* blocks the current workload uses */
	uint32_t next_busy;		/* blocks the next workload needs */
	uint32_t out_gate_mask;
	uint32_t out_power_before_mw;
	uint32_t out_power_after_mw;
	uint32_t out_wake_stall_us;
	uint32_t _pad;
};

struct atrium_gpu_device_reset {		/* on control_fd */
	uint32_t struct_size;
	uint32_t flags;			/* must be 0 */
	uint64_t out_lost_generation;
};

/* ------------------------------------------------------------------ */
/* ioctl numbers: group 'A', v2.1 range 64..127 (§19)                  */
/* ------------------------------------------------------------------ */
#define ATRIUM_GPU_QUERY_CAPS		_IOWR('A', 64, struct atrium_gpu_caps_query)
#define ATRIUM_GPU_CLIENT_OPEN		_IOWR('A', 65, struct atrium_gpu_client_open)
#define ATRIUM_GPU_SET_QUOTA		_IOW ('A', 66, struct atrium_gpu_set_quota)
#define ATRIUM_GPU_QUERY_USAGE		_IOWR('A', 67, struct atrium_gpu_query_usage)
#define ATRIUM_GPU_VM_CREATE		_IOWR('A', 68, struct atrium_gpu_vm_create)
#define ATRIUM_GPU_VM_BIND		_IOWR('A', 69, struct atrium_gpu_vm_bind)
#define ATRIUM_GPU_BO_CREATE		_IOWR('A', 70, struct atrium_gpu_bo_create)
#define ATRIUM_GPU_BO_MMAP_INFO		_IOWR('A', 71, struct atrium_gpu_bo_mmap_info)
#define ATRIUM_GPU_BO_QUERY		_IOWR('A', 72, struct atrium_gpu_bo_query)
#define ATRIUM_GPU_BO_WRITE		_IOW ('A', 73, struct atrium_gpu_bo_xfer)
#define ATRIUM_GPU_BO_READ		_IOW ('A', 74, struct atrium_gpu_bo_xfer)
#define ATRIUM_GPU_BO_SEAL		_IOW ('A', 75, struct atrium_gpu_bo_seal)
#define ATRIUM_GPU_BO_EXPORT_SHARE	_IOWR('A', 76, struct atrium_gpu_bo_export_share)
#define ATRIUM_GPU_BO_IMPORT_SHARE	_IOWR('A', 77, struct atrium_gpu_bo_import_share)
/*                                     'A', 78 reserved: BO_IMPORT_MEMORY (§6.3) */
#define ATRIUM_GPU_QUEUE_CREATE		_IOWR('A', 79, struct atrium_gpu_queue_create)
#define ATRIUM_GPU_QUEUE_RESET		_IOW ('A', 80, struct atrium_gpu_queue_reset)
#define ATRIUM_GPU_QUEUE_QUERY_FAULT	_IOWR('A', 81, struct atrium_gpu_queue_query_fault)
#define ATRIUM_GPU_QUEUE_MAP		_IOWR('A', 82, struct atrium_gpu_queue_map)
#define ATRIUM_GPU_SUBMIT		_IOWR('A', 83, struct atrium_gpu_submit)
#define ATRIUM_GPU_SYNCOBJ_CREATE	_IOWR('A', 84, struct atrium_gpu_syncobj_create)
#define ATRIUM_GPU_SYNCOBJ_SIGNAL	_IOW ('A', 85, struct atrium_gpu_syncobj_signal)
#define ATRIUM_GPU_SYNCOBJ_WAIT		_IOW ('A', 86, struct atrium_gpu_syncobj_wait)
#define ATRIUM_GPU_SYNCOBJ_QUERY	_IOWR('A', 87, struct atrium_gpu_syncobj_query)
#define ATRIUM_GPU_QUERY_TELEMETRY	_IOWR('A', 88, struct atrium_gpu_telemetry)
#define ATRIUM_GPU_CLOCK_CALIBRATE	_IOWR('A', 89, struct atrium_gpu_clock_calibrate)
#define ATRIUM_GPU_PERF_OPEN		_IOWR('A', 90, struct atrium_gpu_perf_open)
#define ATRIUM_GPU_SCHED		_IOW ('A', 91, struct atrium_gpu_sched)
#define ATRIUM_GPU_POWERGATE		_IOWR('A', 92, struct atrium_gpu_powergate)
#define ATRIUM_GPU_DEVICE_RESET		_IOWR('A', 93, struct atrium_gpu_device_reset)

#endif /* _ATRIUM_GPU_ABI_V2_H_ */
