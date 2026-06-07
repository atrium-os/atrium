# Atrium GPU ABI v2 — Draft for Review

> **Status.** Draft. Intended audience: FreeBSD-CURRENT contributors, OpenBSD/NetBSD/DragonFly graphics maintainers, Mesa userspace authors familiar with the libdrm vendor backends. Not yet implemented end-to-end; v0.1 (the virtio-gpu skeleton at `atrium-kmod/atrium_virtio_gpu.c`) is the only consumer of an earlier, much smaller version of this surface today. Separately, a from-scratch AMD bring-up driver (`atrium-gpu-amd`, developed against the gpusim functional model) implements a smaller intermediate ABI — integer handles, kernel-mediated submit, blocking fence-wait, MSI-X completion — that is expected to converge to this surface; that work is what grounds the user-mode-queue (§5.9) and energy-telemetry (§5.10) additions in this revision. The intent of v2 is to lock in the long-term shape *before* a second vendor ports.
>
> **Companion.** Read `drm-research-findings.md` first if you want the source-grounded background. This document is the design that falls out of those findings. See `atrium-gpu-driver-architecture.md` for the driver-architecture stance and the rationale behind the user-mode-queue (§5.9) and energy-telemetry (§5.10) additions folded into this revision.
>
> **One-line summary.** A small, BSD-native, vendor-per-cdev kernel ABI for GPU work, with Vulkan as the only userspace ABI it targets. POD-blob submits, timeline-only sync, VM_BIND from day one, kernel-mediated *or* capability-scoped user-mode-queue submission, Capsicum-friendly, optional read-only energy telemetry, no shared kernel framework, no Linux DRM concepts inherited.

---

## 1. Why this exists, and why it's BSD-native

Every existing BSD that wants accelerated graphics today imports `drm-kmod`: a port of Linux's DRM/KMS subsystem stitched onto FreeBSD via the linuxkpi shim. It works in the sense that it boots; it does not work in the sense that:

- It is GPL-licensed (BSD policy disagrees).
- It is shaped by Linux primitives all the way down (`struct file_operations`, `struct device`, work queues, `idr`, `wait_queue_head_t`, `dma_resv` with Linux waitqs). Every concept needs a shim.
- It tracks Linux's release cadence, not FreeBSD's.
- It is unstable on -CURRENT precisely because it sits on a moving shim layer over a moving kernel.

The pragmatic alternative — keep `drm-kmod` and live with it — has been the BSD position for a decade. This document proposes the alternative we have not yet tried: **specify a contract, written in BSD primitives, that any GPU vendor driver can implement, and that Mesa upstream can target as an alternative `winsys`**. The contract is small enough that BSDs can own it forever; the per-vendor ports remain large but inherit hardware-specific MIT-licensed code from upstream where licenses permit.

Three claims this design rests on, each derived from `drm-research-findings.md`:

1. **Most of DRM's framework exists for cross-vendor uniformity Linux historically had to provide.** A single-vendor-ABI-per-cdev model (the model FreeBSD's own driver tradition prefers) drops most of that framework with no algorithmic loss. (Findings §5a.)
2. **The hard problems any GPU stack must solve are smaller than DRM's surface implies.** Five concerns are real (memory placement, sync semantics, VM_BIND, hang recovery, KMS atomic). The rest is policy or convention. (Findings §5b.)
3. **The post-2020 design (Intel Xe, Asahi v2) already converged on the right shape**: VM_BIND, timeline-only sync, firmware scheduling, explicit-sync only, POD submit blobs. We do not have to invent a model — we adopt that consensus and translate its primitives into BSD idioms. (Findings §2.)

What we explicitly do not do:

- We do not invent a new userspace API. Mesa's `radv`, `anv`, `nvk`, `lavapipe` are unchanged. We replace `libdrm_<vendor>` (~50 KLoC of ioctl wrappers and helpers) with `libatrium_gpu` (target ~5 KLoC), and patch each Mesa backend's `winsys/` layer to call us instead of DRM.
- We do not ship a kernel-side "framework" that vendor drivers must integrate with. The vendor cdev *is* the contract. There is no `atrium_drm_*` framework. Vendor modules share *headers* (ioctl numbers and struct definitions) and a *spec* (this document). They do not share kernel code.
- We do not target non-Vulkan APIs (OpenGL, OpenCL). Mesa's gallium drivers consume `winsys` too, so OpenGL works as a side-effect, but we do not design *for* it. Compute (OpenCL via clover/rusticl) likewise.

### 1.1 Cross-BSD scope

This document specifies the ABI in terms portable across FreeBSD, OpenBSD, NetBSD, and DragonFly. Where a primitive is BSD-but-not-portable (e.g., FreeBSD's newbus, OpenBSD's `device_t` shape), we describe the role and let each BSD's driver framework satisfy it. Cross-BSD adoption is a stated goal; FreeBSD ships it first because the work is starting there.

Specifically portable: the **userspace ABI** (ioctls, structs, fd semantics, object lifecycle) is identical across BSDs. Vendor kernel modules are ports per BSD; the contract they expose is one.

---

## 2. Goals and non-goals

### Goals

- A small, stable kernel ABI Mesa Vulkan can target unchanged across BSDs.
- Per-vendor cdev with no shared framework — each vendor module is self-contained.
- BSD-native primitives only: cdev, kqueue, fd-passing via `SCM_RIGHTS`, `vm_object_t`, `mtx`/`sx`, condvars. No Linux concept inherited.
- Capsicum-clean: every operation works in capability mode; no global state lookup.
- Forward-compatible: new fields, new ops, new capabilities can be added without breaking deployed userspace.
- Wire-friendly: every submission and binding payload is plain old data (no embedded pointers); bindable to virtio-gpu transport without translation.
- Display fully separated (existing `/dev/atrium-display0` cdev, separately specified).

### Non-goals

- Backward compatibility with libdrm. We provide a Mesa winsys port; `libdrm_amdgpu` clients never see this ABI.
- A general "graphics framework" for non-Vulkan APIs. OpenGL works because Mesa internally translates to a similar shape; we don't shape the ABI around it.
- Cross-architecture portability beyond BSDs. Linux and Solaris and macOS and Windows are not targets.
- Cross-vendor *uniformity* of internal driver structure. Each vendor module is allowed to be wildly different inside as long as the cdev surface matches.
- Re-implementing hardware-specific code from scratch where MIT-licensed code already exists. We expect ports to lift as much from upstream amdgpu / Xe / Asahi / nouveau MIT bits as licensing permits.

---

## 3. Design principles

These are non-negotiable. Every concrete decision below is a consequence of one or more of these.

1. **Contract over framework.** The ABI is documentation + ioctl numbers + struct definitions. Vendors implement against it; they do not link against shared code. New vendors port without coordinating with existing ones.

2. **fd-as-handle.** Every kernel object exposed to userspace is a file descriptor. Not a "handle" indexed in a per-process table. fds are the only object type BSD has that survives `fork()`, transports across processes via `SCM_RIGHTS`, and is unambiguously refcounted by the kernel. We use them everywhere and add no parallel handle namespace.

3. **One sync primitive.** `atrium_gpu_syncobj` is a 64-bit monotonic counter (timeline). Binary semaphore is a timeline with values 0/1. Vulkan fence is a timeline. Vulkan binary semaphore is a timeline. Vulkan timeline semaphore *is* a timeline. There are no other sync primitives in this ABI.

4. **Bind separately from submit.** `VM_BIND` (async, persistent) maps a buffer object into a per-process GPU virtual address space. `SUBMIT` references already-bound addresses. The per-submit "list of buffers I touch" model is gone. (The Xe / Asahi v2 / Panthor consensus.)

5. **Submits are POD blobs.** A submission is a contiguous byte string the kernel passes opaquely to the vendor module. No embedded user pointers, no relocations, no fixups. This is what makes virtio-gpu transport free.

6. **Explicit sync only.** No implicit-sync fast path. Producers signal a syncobj timeline value when their work completes; consumers wait on that value. There is no `dma_resv`-equivalent attached to buffers.

7. **No scheduler in the kernel.** Modern hardware (AMD MES, Intel GuC, Apple ASC) schedules in firmware. The kernel surfaces submissions to hardware queues and tracks completions; it does not arbitrate. Vendors with older hardware that genuinely needs SW scheduling implement it inside their own module without involving the ABI.

8. **Capsicum-friendly.** Every ioctl operates on objects reachable from the caller's fd table. No `lookup-by-name`, no `lookup-by-pid`. A sandboxed process with `cap_enter()` and a single device fd can do everything the spec allows.

9. **Forward-compatible structs.** Every input struct's first field is `uint32_t struct_size`. Drivers check `struct_size` against the version they understand; older drivers ignore trailing fields they don't recognize, newer drivers default missing trailing fields to safe values. This is the whole versioning story for in-place evolution. Larger breaking changes go through capability bits.

10. **Display is a different device.** Modesetting, page-flip, EDID, hotplug live behind `/dev/atrium/display<N>` cdevs, specified separately (see `atrium-gpu-abi-v0.1.md` for the existing draft of the display side; v2-of-display is its own document). The GPU cdev and the display cdev are linked via shared buffer fds (one BO is allocated by the GPU side, exported via `SHARE_FD`, imported as a scanout source by the display side). They do not share state otherwise.

11. **User-mode submission is capability-scoped.** Where hardware supports user-mode queues — userspace rings a hardware doorbell directly, bypassing the `SUBMIT` ioctl (AMD MES on GFX11/GFX12, Intel GuC, Apple ASC) — the submit-side authority is expressed as a *capability*, never as ambient power. A queue's doorbell is an MMIO page scoped to that queue's VM (its own VMID + page tables), so mapping it into a process grants exactly the ability to kick *that one isolated queue* and nothing else. The kernel keeps ownership of VM_BIND, queue/MQD setup, the hand-off to scheduler firmware, preemption, and reset. This is the Capsicum/Portcullis-native expression of "let userspace submit without a syscall": passing a `queue_fd` (with its mapped doorbell) over `SCM_RIGHTS` is a scoped grant to a sandbox or jail, not a privilege escalation. Kernel-mediated `SUBMIT` (§5.7) remains the baseline and the fallback for hardware without user queues.

---

## 4. Object model

The ABI exposes seven object kinds. Each is a file descriptor.

| Object | What it represents | Created by | Lifetime ends |
|---|---|---|---|
| `device_fd` | A specific GPU device | `open("/dev/atrium/gpu/<vendor><N>")` | Last `close()` |
| `vm_fd` | A GPU virtual address space, per-process | `ATRIUM_GPU_VM_CREATE` ioctl on `device_fd` | Last `close()` |
| `bo_fd` | A buffer object resident in some heap | `ATRIUM_GPU_BO_CREATE` on `vm_fd`, *or* `ATRIUM_GPU_BO_IMPORT_FD` on `vm_fd` | Last `close()` (refcounted across cross-process sharing) |
| `queue_fd` | A submission queue (one hardware engine, one priority) | `ATRIUM_GPU_QUEUE_CREATE` on `vm_fd` | Last `close()` |
| `syncobj_fd` | A timeline semaphore (monotonic 64-bit counter) | `ATRIUM_GPU_SYNCOBJ_CREATE` on `device_fd` | Last `close()` |
| `share_fd` | A vendor-neutral handle to a buffer's backing storage, suitable for cross-device sharing | `ATRIUM_GPU_BO_EXPORT_SHARE` on `bo_fd` | Last `close()` (refcounts the underlying storage) |
| `event_fd` | A kqueue-readable fd that signals when a syncobj timeline passes a given value | `ATRIUM_GPU_SYNCOBJ_EVENTFD` on `syncobj_fd` | Last `close()` |

Lifetime rules:

- Closing `device_fd` does *not* implicitly destroy `vm_fd`s, `bo_fd`s, etc. that derive from it — those have their own refcounts. A closed `device_fd` simply cannot be used to issue new operations.
- Closing `vm_fd` releases all bindings in that VM. The bound BOs survive (refcounted via their own fds); they just stop being mapped at that VM's virtual addresses.
- Closing `bo_fd` decrements the buffer's refcount. The buffer is freed when the count reaches zero across all process refs (including imported `share_fd`s and ongoing GPU work pinning it).
- Closing `queue_fd` waits for all in-flight submissions on it to either complete or fault, then frees the queue. (Long-running compute is the open question here; see §13.)
- Closing `syncobj_fd` while submissions still wait on it is well-defined: those waits abort with `EBADF` reported via the queue's fault stream. Signaling a closed syncobj is silently dropped.

All fds are inherited by `fork()` per POSIX. All fds (other than `device_fd` and `vm_fd`, which are device- and process-bound respectively) can be transported across processes via `SCM_RIGHTS` over a Unix socket; the receiving process can use them on its own `device_fd` (subject to vendor compatibility — see §6).

---

## 5. Per-cdev ABI surface

The full ioctl table. All input structs begin with `uint32_t struct_size; uint32_t _reserved;` (the reserved word forces 8-byte alignment for u64 fields below). All multi-byte fields are little-endian. All ioctls are issued on the appropriate object's fd.

Numbers shown are illustrative and will be assigned via the FreeBSD ioctl conventions (`_IOWR('A', N, struct ...)`) when implementation begins.

### 5.1 Device discovery and capabilities

```
ATRIUM_GPU_QUERY_CAPS    (on device_fd)

struct atrium_gpu_caps_query {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t out_caps_ptr;        /* user buffer to fill */
    uint64_t out_caps_size;       /* in: buffer size; out: bytes written */
};
```

The output buffer is a sequence of TLV records:

```
struct atrium_gpu_cap_record {
    uint32_t cap_id;              /* see ATRIUM_GPU_CAP_* */
    uint32_t cap_size;            /* bytes of cap_data */
    uint8_t  cap_data[];          /* cap_id-specific payload */
};
```

Defined cap IDs (initial set; extensible):

| `cap_id` | Payload | Meaning |
|---|---|---|
| `ABI_VERSION` | `uint32_t major, minor` | ABI revision the driver implements |
| `VENDOR` | string (vendor + device name) | For UI / logging only |
| `HEAPS` | array of `heap_info` | Memory heaps available (size, properties) |
| `QUEUE_FAMILIES` | array of `queue_family_info` | Queue families and their capabilities |
| `ADDRESS_SPACE` | `uint64_t va_base, va_size, va_align` | Per-VM address space layout |
| `FORMATS` | bitmap | Pixel formats supported |
| `MODIFIERS` | array of (format, modifier) pairs | Tiling/compression modifiers per format |
| `SYNC_FEATURES` | bitmap | Timeline syncobj feature flags |
| `BIND_FEATURES` | bitmap | VM_BIND options (sparse, residency hints, etc.) |
| `SUBMIT_FEATURES` | bitmap | Submit options (user-mode queues, long-running compute, indirect dispatch, ...) |
| `VULKAN_EXTENSIONS` | array of strings | Vulkan extensions the driver can support |
| `TELEMETRY` | bitmap | Read-only energy/utilization telemetry available (§5.10) |

TLV is chosen over a fixed struct because new caps will be added over the device's lifetime and old userspace must skip them cleanly.

### 5.2 VM lifecycle

```
ATRIUM_GPU_VM_CREATE     (on device_fd, returns vm_fd)

struct atrium_gpu_vm_create {
    uint32_t struct_size;
    uint32_t flags;               /* ATRIUM_GPU_VM_FLAG_* */
    uint64_t va_size;             /* requested VA span (0 = device default) */
    int32_t  out_vm_fd;           /* set by kernel */
    uint32_t _pad;
};
```

A VM is *per-process*. Two processes that open the same device get independent VMs. A VM is *not* a Vulkan instance; one Vulkan logical device typically maps to one VM, but multiple VMs per process are allowed (e.g., one Vulkan backend driver per process can create one VM, and so on).

### 5.3 BO lifecycle

```
ATRIUM_GPU_BO_CREATE     (on vm_fd, returns bo_fd)

struct atrium_gpu_bo_create {
    uint32_t struct_size;
    uint32_t flags;               /* ATRIUM_GPU_BO_FLAG_* */
    uint64_t size;                /* bytes */
    uint64_t alignment;           /* bytes; 0 = device default */
    uint32_t heap_mask;           /* OR of acceptable heap indices */
    uint32_t _pad;
    int32_t  out_bo_fd;
    uint32_t _pad2;
};
```

`heap_mask` is a bitmap over the heap indices reported by `QUERY_CAPS`. The kernel picks any heap whose bit is set; userspace expresses preference by setting only the desired bits. Zero is invalid.

```
ATRIUM_GPU_BO_MMAP_INFO  (on bo_fd)

struct atrium_gpu_bo_mmap_info {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t out_offset;          /* pass to mmap(device_fd, ..., offset) */
    uint64_t out_size;            /* page-aligned mappable size */
};
```

Userspace then calls `mmap()` on the *device_fd* (not the bo_fd) with the returned offset. The device cdev's `d_mmap_single` resolves the offset back to the BO via a `vm_object_t` the vendor driver constructs.

This mirrors Linux's GEM mmap-via-fake-offset trick but uses BSD's `vm_object_t` directly, which is exactly the right abstraction for "object of pages with custom pager." No equivalent of Linux's `vma_offset_manager` is needed — the offset is the BO's identity within the device fd.

### 5.4 VM_BIND

```
ATRIUM_GPU_VM_BIND       (on vm_fd, async)

struct atrium_gpu_vm_bind_op {
    uint32_t op;                  /* ATRIUM_GPU_BIND_{MAP,UNMAP,REMAP} */
    uint32_t flags;               /* sparse, residency hints */
    int32_t  bo_fd;               /* MAP/REMAP only; -1 for UNMAP */
    uint32_t _pad;
    uint64_t bo_offset;           /* byte offset into BO */
    uint64_t va;                  /* GPU virtual address */
    uint64_t length;              /* bytes */
};

struct atrium_gpu_vm_bind {
    uint32_t struct_size;
    uint32_t op_count;
    uint64_t ops_ptr;             /* user array of struct atrium_gpu_vm_bind_op */
    int32_t  signal_syncobj_fd;   /* syncobj signaled when ops complete; -1 = sync */
    uint64_t signal_value;        /* timeline value to write */
    uint32_t wait_count;
    uint64_t wait_ptr;            /* user array of (syncobj_fd, value) deps */
};
```

A bind operation is async by default. The caller provides an output syncobj+value to wait on; the kernel signals it once all listed ops complete on the device side. If `signal_syncobj_fd == -1`, the call blocks until done. Multiple ops in a single ioctl execute as a transaction: either all bind / unbind / remap, or none (on per-op error, the kernel rolls back and reports which op failed).

Sparse bindings are advertised via `BIND_FEATURES` capability; if absent, sparse flags return `ENOTSUP`.

### 5.5 Queue lifecycle

```
ATRIUM_GPU_QUEUE_CREATE  (on vm_fd, returns queue_fd)

struct atrium_gpu_queue_create {
    uint32_t struct_size;
    uint32_t family_id;           /* index into QUEUE_FAMILIES caps */
    uint32_t priority;            /* device-defined; 0 = default */
    uint32_t flags;
    int32_t  out_queue_fd;
    uint32_t _pad;
};
```

A queue is bound to one VM at creation. Submissions on the queue may reference any address bound in that VM. Queues from different VMs cannot share submissions (would require cross-VM page-table coordination, which we explicitly punt to the per-vendor module's discretion if it wants to optimize).

### 5.6 Syncobj lifecycle

```
ATRIUM_GPU_SYNCOBJ_CREATE  (on device_fd, returns syncobj_fd)

struct atrium_gpu_syncobj_create {
    uint32_t struct_size;
    uint32_t flags;
    uint64_t initial_value;       /* starting counter value (typically 0) */
    int32_t  out_syncobj_fd;
    uint32_t _pad;
};

ATRIUM_GPU_SYNCOBJ_SIGNAL  (on syncobj_fd, host-side signal)

struct atrium_gpu_syncobj_signal {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t value;               /* set counter to this value (must be > current) */
};

ATRIUM_GPU_SYNCOBJ_WAIT    (on syncobj_fd, blocking host-side wait)

struct atrium_gpu_syncobj_wait {
    uint32_t struct_size;
    uint32_t flags;               /* ATRIUM_GPU_WAIT_ANY (one of N), default = all */
    uint64_t value;               /* wait until counter >= value */
    int64_t  timeout_ns;          /* -1 = forever */
};

ATRIUM_GPU_SYNCOBJ_QUERY   (on syncobj_fd, non-blocking poll)

struct atrium_gpu_syncobj_query {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t out_current_value;
};

ATRIUM_GPU_SYNCOBJ_EVENTFD (on syncobj_fd, returns event_fd)

struct atrium_gpu_syncobj_eventfd {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t value;               /* threshold to fire on */
    int32_t  out_event_fd;
    uint32_t _pad;
};
```

`event_fd` is the kqueue integration point. Userspace registers it with `EVFILT_READ`; the kernel fires the event when the syncobj's counter reaches or exceeds `value`. Reading from `event_fd` returns a `uint64_t` of the current value; subsequent reads after the threshold has been passed return immediately.

This is how Mesa's WSI integrates with a kqueue-driven compositor like frescod: each frame's "render done" signal is a syncobj `event_fd` registered alongside the socket fd and the input pipes.

### 5.7 SUBMIT

```
ATRIUM_GPU_SUBMIT        (on queue_fd)

struct atrium_gpu_submit_dep {
    int32_t  syncobj_fd;
    uint32_t _pad;
    uint64_t value;               /* timeline point */
};

struct atrium_gpu_submit {
    uint32_t struct_size;
    uint32_t flags;
    uint64_t blob_ptr;            /* user pointer to POD command blob */
    uint64_t blob_size;
    uint64_t in_dep_ptr;          /* array of dep structs to wait on */
    uint32_t in_dep_count;
    uint32_t _pad;
    uint64_t out_dep_ptr;         /* array of dep structs to signal */
    uint32_t out_dep_count;
    uint32_t _pad2;
};
```

The `blob` is opaque to the kernel. It is a vendor-specific command stream — the bytes the GPU will execute, formatted however the vendor's hardware expects. Userspace constructs the blob using vendor-specific knowledge baked into Mesa's vendor backend; the kernel's job is to validate basic structure, hand off to the hardware/firmware, and signal completion.

The blob is required to be self-contained: no pointers (only GPU virtual addresses, which were established via VM_BIND), no relocations, no fixups. This is what enables transparent virtio-gpu transport — the blob is just bytes on the wire.

The `in_dep` / `out_dep` arrays specify the sync graph. The kernel waits for all `in_dep` syncobj-timeline-value pairs to reach their value before dispatching the submission, and signals all `out_dep` pairs when the submission completes (or aborts due to fault — see §10).

### 5.8 Cross-process buffer sharing

```
ATRIUM_GPU_BO_EXPORT_SHARE (on bo_fd, returns share_fd)

struct atrium_gpu_bo_export_share {
    uint32_t struct_size;
    uint32_t flags;
    int32_t  out_share_fd;
    uint32_t _pad;
};

ATRIUM_GPU_BO_IMPORT_SHARE (on vm_fd, returns bo_fd)

struct atrium_gpu_bo_import_share {
    uint32_t struct_size;
    uint32_t _reserved;
    int32_t  share_fd;
    uint32_t _pad;
    int32_t  out_bo_fd;
    uint32_t _pad2;
};
```

`share_fd` is **vendor-neutral**. It is a handle to a buffer's backing storage with a vendor-tagged header the importing driver inspects to decide how to map. A buffer exported by `atrium-amdgpu` and imported by `atrium-amdgpu` is zero-cost (the import is a refcount bump). A buffer exported by one vendor and imported by another is supported only if both vendors agree on a common storage representation — typically dma-buf-equivalent shared physical pages — and is otherwise rejected with `ENOTSUP`.

The `share_fd` is what travels across processes via `SCM_RIGHTS` over a Unix socket. The compositor protocol (e.g., aqueduct's CLASS_DISPLAY) carries `share_fd`s as the substrate for any cross-process buffer reference. A frescod scanout BO is just a `share_fd` exported by the producer and imported by the display cdev.

This is the dma-buf concept, redesigned: smaller surface (one export, one import, no attach/detach lifecycle), explicit vendor identification, no implicit fence carriage. Synchronization is always via separate `syncobj_fd`s passed alongside.

### 5.9 User-mode queues (direct doorbell submission)

`SUBMIT` (§5.7) is the baseline: one ioctl per submission. On hardware with firmware-scheduled user-mode queues (AMD MES on GFX11/GFX12, Intel GuC, Apple ASC), userspace can instead ring a hardware doorbell directly and skip the syscall on the submit hot path. This matters for two reasons specific to Atrium: it removes per-submit syscall/scheduler overhead the Tier-2/3 energy router cannot otherwise reclaim, and — see principle 11 — it expresses GPU-submit authority as a scoped capability rather than ambient power. It is advertised by the `USER_QUEUE` bit of `SUBMIT_FEATURES`; absent it, the ioctls below return `ENOTSUP` and userspace uses §5.7.

A user-mode queue is requested at creation (`ATRIUM_GPU_QUEUE_CREATE` with `flags |= ATRIUM_GPU_QUEUE_FLAG_USER_MODE`) and then mapped:

```
ATRIUM_GPU_QUEUE_MAP     (on queue_fd)

struct atrium_gpu_queue_map {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t out_ring_offset;     /* mmap(device_fd) offset: command ring */
    uint64_t out_ring_size;       /* ring bytes */
    uint64_t out_doorbell_offset; /* mmap(device_fd) offset: doorbell page */
    uint64_t out_doorbell_size;   /* doorbell page bytes */
    uint64_t out_wptr_offset;     /* write-pointer location (ring-relative) */
    uint32_t out_doorbell_index;  /* this queue's slot within the doorbell page */
    uint32_t _pad;
};
```

Userspace `mmap`s the ring and the doorbell page off `device_fd` (the same `d_mmap_single` path as `BO_MMAP_INFO`, §5.3), writes a POD command blob (§5.7's format and opacity rules, unchanged) into the ring at the write pointer, advances the write pointer, and writes the doorbell to kick the engine. **Completion is unchanged**: the blob's trailing fence packet signals a `syncobj` timeline, observed via `SYNCOBJ_EVENTFD` → kqueue exactly as in §5.7. Only the submit *path* moves to userspace; the *completion* path stays on the existing fd/kqueue machinery, so a frescod-style compositor (§5.6, §9) is unaffected.

What stays in the kernel (non-negotiable, principle 11): VM_BIND and memory validation, queue/MQD setup and its hand-off to the scheduler firmware, preemption, and hang/reset (§10). Userspace owns only its own ring contents and write pointer.

**Security model.** The doorbell page is MMIO scoped to this queue's VM — its own VMID and page tables (§5.5). Mapping it grants exactly the authority to kick *this* queue, which can reach only addresses bound in *this* VM. So a `queue_fd` (carrying its mapped doorbell) passed to a sandboxed or jailed process via `SCM_RIGHTS` is a *scoped capability*: that process can submit GPU work in one isolated address space and nothing more. There is no ambient "submit to the GPU" authority anywhere in the ABI — which is what makes user-mode submission safe under `cap_enter()` and under Portcullis default-deny jails. The capability *is* the mapped doorbell.

### 5.10 Energy telemetry (read-only)

Atrium routes work between a software renderer (Tier-2) and the GPU (Tier-3) under an energy policy (`energy-policy.md`). For that router to decide whether using the GPU is worth its energy, it needs a cheap, read-only sample of GPU state. This is the only place the ABI exposes anything power-adjacent, and it is strictly *observational*.

```
ATRIUM_GPU_QUERY_TELEMETRY  (on device_fd)

struct atrium_gpu_telemetry {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t busy_ns;             /* monotonic engine-busy time, all queues */
    uint64_t sample_ns;           /* monotonic wall time of this sample */
    uint32_t queue_depth;         /* submissions in flight across queues */
    uint32_t power_state;         /* vendor-normalized 0=idle .. N=max */
    uint32_t tier_cost_hint;      /* normalized energy/op estimate (vs Tier-2) */
    uint32_t flags;
};
```

`busy_ns` over elapsed `sample_ns` gives a utilization fraction; `tier_cost_hint` is the vendor's normalized estimate of GPU energy-per-unit-work on the *same scale* the router uses for the Tier-2 software path, so the two are directly comparable. Advertised by the `TELEMETRY` cap; absent it, `ENOTSUP`.

This is **read-only and advisory**. Per the energy policy's "coordinated, not coupled" rule, intent flows one way only: the router *samples* telemetry to make routing decisions; there is no ioctl that lets it drive the driver's power state (that remains the vendor module's own business, §12). Telemetry is observation, not a power-management *control* surface. It exists because — unlike a Linux driver — an Atrium GPU driver lives in a system that can choose *not to use the GPU at all*, and that choice needs data.

---

## 6. Cross-process and cross-device semantics

The ABI is designed for two classes of cross-boundary scenarios:

### 6.1 Compositor-style sharing (single device, multiple processes)

A client process produces frames into a `bo_fd` it allocated; the compositor process consumes them. Flow:

1. Client: `BO_CREATE`, `VM_BIND` into its VM, render via `SUBMIT`, signal `syncobj` on completion.
2. Client: `BO_EXPORT_SHARE` produces `share_fd`; `SYNCOBJ` is already an fd.
3. Client: send `share_fd` + `syncobj_fd` + timeline value over aqueduct (via `SCM_RIGHTS`).
4. Compositor: receive both fds. `BO_IMPORT_SHARE` into its VM (zero-copy on same vendor). `SYNCOBJ_EVENTFD` → kqueue.
5. Compositor: when kqueue fires, the BO is ready for use as a texture in the next composite pass.

No dma-buf, no Wayland-style explicit-sync extension, no separate fence-fd protocol. Just two fds and a counter value.

### 6.2 Cross-device sharing (multiple GPUs, single process or multi-process)

Mesa needs this for laptops with discrete + integrated GPUs (render on discrete, display on integrated). The same `share_fd` mechanism applies, with the constraint that both vendor modules must understand the share representation. Practically this means: vendors implementing this ABI agree on a shared "physical-pages-with-vendor-tag" representation as the export format, similar to dma-buf today. The agreement is documented in this spec; vendors that opt out reject cross-vendor imports cleanly (`ENOTSUP`) rather than corrupting state.

Display cdev is treated as a "vendor" for this purpose: a BO produced by `atrium-amdgpu` and imported by `atrium-display` is a scanout buffer; the display cdev's `IMPORT_SHARE` accepts vendor-tagged buffers it can DMA from.

---

## 7. Display ABI summary

Display is on a separate cdev (`/dev/atrium/display<N>`) and is specified in its own document. The interaction with the GPU ABI is purely via `share_fd`:

- Display cdev's atomic-commit ioctl takes `share_fd`s as scanout sources.
- Display cdev signals page-flip completion via a `syncobj_fd` it owns.
- Display cdev exposes vsync/vblank events via `event_fd` (kqueue-readable).

No display configuration touches the GPU cdev. No GPU operation touches the display cdev. They share buffer fds; that is the entire interface.

---

## 8. Versioning and extension model

Three layers of evolution, in order of preference:

1. **Trailing fields in input structs.** Append new fields at the end. Old userspace passes a smaller `struct_size`; the kernel zero-fills missing fields with documented defaults. New userspace passes a larger `struct_size`; old kernels see fields they don't recognize and ignore them (the documented behavior is that the kernel's effective size is `min(struct_size, kernel_known_size)`).

2. **New cap_id values in `QUERY_CAPS`.** Drivers advertise new features by emitting new cap records. Old userspace skips unknown cap_ids (TLV walks past unrecognized records). New userspace tests for cap presence before using new features.

3. **New ioctl numbers.** When a feature is not expressible as a struct extension, add a new ioctl. Older drivers return `ENOTTY`; userspace tests via `QUERY_CAPS` for the corresponding feature flag before issuing the new ioctl.

We do not version the ABI as a single monolithic number except as a coarse compatibility shibboleth (ABI_VERSION major bumps are reserved for incompatible struct layout changes that cannot be expressed with `struct_size`; this should never happen).

The "uAPI is forever" lesson from DRM (findings §1.8) applies in full: once an ioctl ships with stable userspace consumers, its signature is permanent. New behavior goes through new mechanisms, not by redefining old ones.

---

## 9. Worked example: a Vulkan frame on this ABI

Pseudocode for a Mesa Vulkan backend rendering one frame and presenting to a Fresco compositor:

```c
/* one-time setup */
device_fd  = open("/dev/atrium/gpu/amdgpu0", O_RDWR);
ioctl(device_fd, ATRIUM_GPU_QUERY_CAPS, &caps);

vm_fd      = ioctl(device_fd, ATRIUM_GPU_VM_CREATE, &vm_args);
queue_fd   = ioctl(vm_fd, ATRIUM_GPU_QUEUE_CREATE, &q_args);

/* per-frame */
swapchain_bo_fd = ioctl(vm_fd, ATRIUM_GPU_BO_CREATE, &bo_args);
ioctl(vm_fd, ATRIUM_GPU_BO_MMAP_INFO, ...);  /* if app needs CPU access */

/* bind into VM (async, signals bind_done timeline value) */
ioctl(vm_fd, ATRIUM_GPU_VM_BIND, &bind_args);

/* build vendor command blob in user memory; reference VM addresses */
build_command_blob(blob, swapchain_bo_va, ...);

/* submit; wait on bind_done, signal frame_done on completion */
ioctl(queue_fd, ATRIUM_GPU_SUBMIT, &submit_args);

/* hand to compositor */
share_fd = ioctl(swapchain_bo_fd, ATRIUM_GPU_BO_EXPORT_SHARE, ...);
sendmsg(aqueduct_socket, SCM_RIGHTS={share_fd, frame_done_syncobj_fd}, ...);

close(share_fd);  /* compositor holds its own ref now */
```

The compositor side, in frescod-equivalent code:

```c
/* recvmsg returns share_fd + frame_done_syncobj_fd from client */

bo_fd = ioctl(vm_fd, ATRIUM_GPU_BO_IMPORT_SHARE, &import_args);

/* register completion */
event_fd = ioctl(frame_done_syncobj_fd, ATRIUM_GPU_SYNCOBJ_EVENTFD,
    &{.value = expected_value});
EV_SET(&kev, event_fd, EVFILT_READ, EV_ADD, 0, 0, ...);
kevent(kq, &kev, 1, NULL, 0, NULL);

/* in main event loop, when kqueue fires this fd: client's frame is ready */
/* compositor uses bo_fd as a texture in its next composite SUBMIT */
```

There is no Wayland surface, no dma-buf, no explicit-sync extension. The compositor protocol carries fds; the kernel does the rest.

---

## 10. Hang recovery and fault model

The hardest part of any GPU ABI. Brief sketch:

- A submission may fault (page fault, shader trap, hardware hang). The vendor module detects this via interrupts or completion polling.
- On fault: all outstanding `out_dep` syncobjs for the faulted submission are signaled with their target value but the timeline carries an *aborted* flag, queryable via a new cap.
- Subsequent submissions on the same queue are aborted with the same fault status until the queue is reset.
- Queue reset is initiated by closing `queue_fd` and creating a new one. The vendor module determines whether this requires a full device reset (some hardware does, some doesn't); userspace is told via `QUERY_CAPS` what scope of reset is implied.
- Vulkan's device-lost semantics map cleanly: a faulted queue's Vulkan device transitions to `VK_ERROR_DEVICE_LOST`; the application must recreate the device, which closes all fds and starts over.

The Asahi v2 + Xe approach to hang recovery (per-firmware-context isolation, devcoredump capture, fault-info to userspace) is the model to inherit; this section will be filled in detail when first vendor port forces the question.

---

## 11. Mesa porting effort

Estimated work to land Atrium support in Mesa upstream:

1. **`libatrium_gpu`** — userspace shim implementing the ABI calls. ~5 KLoC (compare libdrm's ~50 KLoC). Single shared library, one per system.
2. **`winsys/atrium_amdgpu`** — Mesa's amdgpu winsys layer ported to call `libatrium_gpu` instead of `libdrm_amdgpu`. The radv vendor backend itself is untouched. Estimated: 3-6 engineer-months.
3. **WSI for atrium-display** — a new `VK_ATRIUM_surface` Vulkan extension equivalent to `VK_KHR_wayland_surface`, plus the WSI common-code adapter that uses our `share_fd` + syncobj path. ~1-2 engineer-months.
4. **Per-additional-vendor winsys port** — each new vendor (anv, nvk) is similar in scope: 3-6 engineer-months.

Total to Mesa-ready first vendor: 6-12 engineer-months *of upstream-ready Mesa work*, separate from the per-vendor kernel module port (3-5 engineer-years per vendor for the kernel side, mostly hardware-specific MIT code).

---

## 12. Out of scope for this document

- **Non-Vulkan APIs.** OpenGL via gallium works because Mesa internally maps it onto `winsys`; we don't design for it. OpenCL same.
- **Power management interface.** Each vendor module handles its own (PCI runtime PM, ASC firmware, etc.). The ABI does not expose explicit "device suspend" ioctls; the existing FreeBSD `device_suspend(9)` framework handles this at the cdev layer.
- **Hot-add/hot-remove.** The cdev disappears on device removal. fds opened against it return errors on subsequent ops. No special protocol.
- **Display ABI** — see separate document.
- **Capability protocols** (HDCP, content protection). Not a v2 concern.
- **Virtual GPU partitioning** (SR-IOV, MIG). Possibly enabled by extending QUEUE_CREATE later. Not v2.

---

## 13. Open questions

These need answers before v2 is final. Each will be resolved during the first vendor port (probably AMD), with input from Mesa contributors and BSD reviewers.

a. **Long-running compute fences.** A submission that may take seconds (e.g., compute shader processing a large dataset) should not block the kernel's reclaim path on a dma-fence-style allocation discipline. Xe added a "long-running fence" annotation; we need an equivalent flag on `SUBMIT` and well-defined semantics for "wait" callers.

b. **Cross-vendor share representation.** Section 6.2 assumes vendors agree on a common "physical pages + vendor tag" format for `share_fd`. The actual byte format and validation rules need a sub-spec.

c. **Capsicum behavior under errors.** A capability-mode process that closes its `device_fd` mid-operation should see well-defined errors on derived fds, not undefined behavior. We need to enumerate every "device gone" path.

d. **Ioctl number assignment.** FreeBSD's ioctl group letter for our ABI. `'A'` is taken; need to coordinate.

e. **mmap size limits.** Some GPUs expose 64-bit BARs; the cdev `d_mmap_single` path needs to handle arbitrary `vm_object_t` sizes including >4 GiB.

f. **kqueue + SCM_RIGHTS interaction.** Does an `event_fd` survive transit across `SCM_RIGHTS`? Should a syncobj's eventfd be re-creatable after import to a different process? Need a precise answer.

g. **Vendor compatibility metadata in `share_fd`.** A vendor tag is required, but what about driver version compatibility (e.g., a buffer exported by amdgpu-driver-A and imported by amdgpu-driver-B, where B is older)? Forward-compat or reject?

h. **Memory pressure / eviction.** When is the vendor allowed to evict a BO from VRAM to system memory? Does it need a callback to userspace? (DRM's answer is "no callback, just transparent migration with appropriate fence ordering"; we likely follow.)

i. **Sparse residency.** Vulkan exposes `sparseResidency*` features. If we want to support these, `VM_BIND` needs to handle partial/sparse mappings explicitly. Cap-flagged for now.

j. **Multi-GPU presentation.** PRIME-equivalent for "render on discrete, present on integrated" — does the WSI extension handle this entirely, or does the kernel need explicit cross-device-aware paths?

k. **User-mode queue preemption and doorbell granularity (§5.9).** With userspace owning the ring write pointer, the kernel must still preempt and reset a queue that hangs. Open: doorbell-page granularity (one queue per page vs. many sharing a page, which affects what a single `mmap` grants); how the kernel revokes a mapped doorbell on reset (unmap the `vm_object_t`? fault on next ring?); and how a hung user-mode queue surfaces into the §10 fault model when the kernel never saw the submission.

l. **Telemetry units and sampling cost (§5.10).** `tier_cost_hint` must be normalized to the *same* scale the Tier-2 software renderer reports, or the router cannot compare them. Defining that shared unit (energy-per-op? a core-ms-equivalent?) is a cross-subsystem question owned jointly with the energy policy. Also: the cost of sampling itself must be negligible (no MMIO storm), or telemetry defeats its own purpose.

---

## 14. Comparison: this ABI vs DRM uAPI vs Vulkan

| Concern | DRM uAPI | Vulkan API | Atrium GPU ABI |
|---|---|---|---|
| Per-process address space | implicit per-fd | explicit `VkDevice` | explicit `vm_fd` |
| Buffer object | GEM handle (per-fd integer) | `VkDeviceMemory` (opaque) | `bo_fd` (file descriptor) |
| Cross-process sharing | dma-buf via PRIME ioctl | `VK_EXT_external_memory_*` | `share_fd` via `SCM_RIGHTS` |
| Sync primitives | sync_file + drm_syncobj + dma_resv | `VkSemaphore` + `VkFence` (timeline subsumes) | one timeline `syncobj_fd` |
| Submit shape | varies; per-driver | `vkQueueSubmit` with cmd buffers | POD blob: ioctl, or user-mode doorbell (§5.9) |
| Bind model | per-submit BO list (legacy) or VM_BIND | `vkBindBufferMemory` etc. | `VM_BIND` ioctl, async |
| Display | KMS in same fd as GPU | WSI extension | separate cdev, separate fd |
| Master concept | DRM-Master | n/a | none (filesystem perms + Capsicum) |
| Versioning | per-driver `DRM_VERSION` | `VkApplicationInfo.apiVersion` | `struct_size` + cap TLV |
| Event delivery | DRM event queue + poll | callback / fence wait | kqueue via `event_fd` |

Atrium's choices are mostly Vulkan's (the API we're serving) translated into BSD primitives (fds, kqueue). DRM was an intermediate layer designed for X11-era assumptions; we collapse it.

---

## 15. Cross-BSD portability notes

This ABI is portable across FreeBSD, OpenBSD, NetBSD, and DragonFly because it depends only on primitives all four share:

- Character device with `d_ioctl`, `d_mmap_single` (or equivalent), `d_kqfilter` — all four BSDs.
- File descriptor inheritance / `SCM_RIGHTS` over Unix sockets — POSIX, all four.
- kqueue with `EVFILT_READ` on arbitrary fds — all four.
- `vm_object_t` for backing storage — all four (interfaces differ slightly; vendor module bridges).

Where BSDs diverge:

- **Driver framework.** FreeBSD has newbus; NetBSD has its own autoconf; OpenBSD is a fork of NetBSD's; DragonFly inherits FreeBSD's. The vendor module's *internal* shape differs; its *cdev surface* is identical.
- **Kernel locking primitives.** Names differ (`mtx`/`sx` vs `mutex`/`rwlock`); semantics are equivalent.
- **PCI subsystem integration.** Each BSD has its own PCI driver registration; vendor modules handle this per-BSD.
- **Memory-map page handling.** FreeBSD's `vm_object_t` / `vm_pager_t` is the cleanest; OpenBSD/NetBSD have analogous primitives; DragonFly inherits from FreeBSD.

Cross-BSD vendor-driver porting is therefore "wrap the same hardware-specific code with each BSD's driver-framework idioms." The userspace ABI is the same on all four; Mesa's `libatrium_gpu` is one library that runs anywhere.

This is a stronger position than drm-kmod, which depends on linuxkpi shimming Linux idioms onto FreeBSD only — OpenBSD, NetBSD, DragonFly don't have that shim and so don't have modern GPU acceleration today. Atrium GPU ABI, by being framed in primitives all four BSDs share natively, opens the path for the smaller BSDs to benefit from FreeBSD's vendor-port work.

---

## 16. Acknowledgements

This design is heavily indebted to:

- **Intel's Xe RFC** and the team's deliberate decisions about what to drop from i915. Many of v2's choices (VM_BIND mandatory, explicit-sync only, firmware scheduling) match Xe's.
- **AMD's MES user-mode-queue work** (mainlined in Linux 6.16 for GFX11/GFX12 — the hardware class Atrium targets first). §5.9 adopts its firmware-scheduled direct-doorbell submission mechanism and reframes it as a Capsicum/Portcullis capability rather than a permission retrofit.
- **The Asahi Linux project**, particularly Alyssa Rosenzweig and Asahi Lina, for the v2 UAPI's POD-blob submit model and the demonstration that a clean GPU driver can fit in ~30K LoC of MIT-licensed code.
- **DRM maintainers** whose 20-year evolution produced the lessons we are learning from. Specific concepts we are inheriting under different names: `dma-buf` (→ `share_fd`), `drm_syncobj` timeline (→ `syncobj_fd`), VM_BIND (→ `ATRIUM_GPU_VM_BIND`), atomic modesetting (→ display-cdev v2), DMA-BUF feedback (→ `share_fd` modifier negotiation).
- The **Mesa community**, whose `winsys` abstraction makes a port like this possible without rewriting Vulkan backends. We are explicitly designing to fit within the existing `winsys` contract rather than asking Mesa to change.
- **FreeBSD's GPU contributors** who have maintained drm-kmod for a decade. Their work proves out the constraint we are responding to: shimming Linux is possible, but it is not where we want to be.

---

## 17. What happens next

This document is a draft for review. Expected sequence:

1. **Round 1 (this document).** Internal Atrium review; iterate on Open Questions §13.
2. **Round 2.** Share with FreeBSD `freebsd-current@` and `freebsd-arch@` for kernel-side critique; share with `mesa-dev@` for userspace-side critique. Specifically solicit input from people who have shipped GPU drivers on Linux.
3. **Round 3.** Iterate based on feedback. Reach version 2.0 declared "stable for first implementation."
4. **First vendor implementation.** AMD (RDNA2/RDNA3 baseline, well-documented hardware, MIT-bits-available). The implementation will surface unforeseen issues; some will require ABI adjustments, ideally as struct extensions or new caps rather than breaking changes.
5. **Mesa upstream port.** `libatrium_gpu` + `winsys/atrium_amdgpu`. Submitted upstream as one of Mesa's optional winsys backends.
6. **End-to-end demo.** frescod runs with a real AMD GPU on FreeBSD via Atrium GPU ABI. No drm-kmod in the stack.

Estimated calendar time to (6): 2-3 years of focused work with a small team. Estimated to subsequent vendor (Intel via Xe-architectural-port): another 2-3 years overlapping.

---

*This document is dual-licensed under MIT or Apache-2.0, matching the rest of Atrium. Discussion welcome at the Atrium project repository.*
