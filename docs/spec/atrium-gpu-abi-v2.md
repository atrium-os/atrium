# Atrium GPU ABI v2.3

> **CANONICAL.** This is the canonical GPU ABI. All backends
> (`atrium-gpu-amd`, `atrium-virtio-gpu`, Carillon) and `atrium-gpu-rs` converge
> to it. See [`gpu-abi-reconciliation.md`](gpu-abi-reconciliation.md) for the
> lineage and migration plan. Display lives in
> [`atrium-display-architecture.md`](atrium-display-architecture.md); §7 here
> fixes how buffers and authority cross into it.

> **Revision 2.1 (2026-11-03).** Folds in the reconciliation decisions
> (D-1: the syncobj fd itself is the kqueue source, `SYNCOBJ_EVENTFD` is gone;
> D-2: BOs are created on the device fd and are VM-agnostic until `VM_BIND`;
> ioctl group `'A'`; kqueue-only vblank). It also settles the design gaps found
> by [`../reviews/2026-11-03-gpu-kernel-and-tier2-review.md`](../reviews/2026-11-03-gpu-kernel-and-tier2-review.md)
> §A2. New or rewritten in 2.1: principles 12–14 (§3), the access and authority
> model (§4.1), resource accounting per jail (§4.2), process contexts instead of
> fixed VMIDs (§5.2), validated `VM_BIND` (§5.4), real queue objects with
> per-queue fences (§5.5, §5.7), syncobj error state (§5.6), revocable doorbells
> (§5.9), the memory-lifetime rule (§5.11), the kernel's validation duties
> (§5.12), sealed buffers for content-addressed sharing (§5.13), scanout by fd and the display lease (§7), the concrete fault model
> (§10, including the escalation ladder, broker crash, display reset and
> suspend/resume in §10.1–10.4), the remaining calls and error codes (§5.14),
> timestamps and performance counters (§5.15), userspace consumers (§11), the
> work order (§17), and the implementation-status table (§18). Sections marked
> *(unchanged in 2.1)* carry over from v2 as written.

> **Revision 2.3 (2026-11-13).** Settles where the spec and `atrium-gpu-amd`
> disagreed (`gpu-security-model.md` §9, §11): the node-based authority profile and
> per-uid accounting become the conforming single-broker profile (principles 13 and 14,
> §4.1, §4.2), `CLIENT_OPEN` minting is required only before a second broker or a
> production direct grant; GPU W^X: executable PTEs only on read-only binds
> (`BIND_EXEC`, §5.4); user-mode queues stay host-root-only until §5.9's open question is
> decided; §18 is brought up to date.

> **Normative header.** [`include/atrium/gpu_abi_v2.h`](../../include/atrium/gpu_abi_v2.h)
> carries every struct, flag, error code, cap payload and ioctl number; §19
> lists the calls. The header and this text are one specification.

> **Status.** Specification. `atrium-gpu-amd` (developed against the gpusim
> functional model) is the reference implementation and implements part of it;
> §18 maps what exists against this text. Intended audience: Atrium kernel and
> userspace authors first, then FreeBSD-CURRENT contributors and the other BSDs'
> graphics maintainers. Atrium's own userspace (our RDNA4 compiler and command
> builders, §11, §13p) is the consumer; a Mesa winsys port is a possible fallback, not
> the plan.
>
> **Companion.** `drm-research-findings.md` (background),
> `atrium-gpu-driver-architecture.md` (driver stance), `atrium-gpu-amd-design.md`
> (the AMD module), `gpu-isolation.md` (the isolation invariants and the
> attestation gate this ABI must satisfy).
>
> **One-line summary.** A small, BSD-native, vendor-per-cdev kernel ABI for GPU work, with Vulkan as the only userspace API it targets. POD-blob submits, timeline-only sync, VM_BIND from day one, per-queue fences with deferred frees, kernel-mediated *or* capability-scoped user-mode-queue submission, a control/client authority split, per-principal resource accounting, GPU W^X, Capsicum-friendly, optional read-only energy telemetry, no shared kernel framework, no Linux DRM concepts inherited.

---

## 1. Why this exists, and why it's BSD-native

*(unchanged in 2.1)*

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

What we explicitly do not do (as first written; since 2.1 Atrium's own userspace is the first consumer and Mesa a fallback, §11, §13p):

- We do not invent a new userspace API. Mesa's `radv`, `anv`, `nvk`, `lavapipe` are unchanged. We replace `libdrm_<vendor>` (~50 KLoC of ioctl wrappers and helpers) with `libatrium_gpu` (target ~5 KLoC), and patch each Mesa backend's `winsys/` layer to call us instead of DRM.
- We do not ship a kernel-side "framework" that vendor drivers must integrate with. The vendor cdev *is* the contract. There is no `atrium_drm_*` framework. Vendor modules share *headers* (ioctl numbers and struct definitions) and a *spec* (this document). They do not share kernel code.
- We do not target non-Vulkan APIs (OpenGL, OpenCL). Mesa's gallium drivers consume `winsys` too, so OpenGL works as a side-effect, but we do not design *for* it. Compute (OpenCL via clover/rusticl) likewise.

### 1.1 Cross-BSD scope

This document specifies the ABI in terms portable across FreeBSD, OpenBSD, NetBSD, and DragonFly. Where a primitive is BSD-but-not-portable (e.g., FreeBSD's newbus, OpenBSD's `device_t` shape), we describe the role and let each BSD's driver framework satisfy it. Cross-BSD adoption is a stated goal; FreeBSD ships it first because the work is starting there.

Specifically portable: the **userspace ABI** (ioctls, structs, fd semantics, object lifecycle) is identical across BSDs. Vendor kernel modules are ports per BSD; the contract they expose is one.

---

## 2. Goals and non-goals

*(unchanged in 2.1)*

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



12. **The kernel validates everything it programs into hardware.** Every address, size, offset, engine id, mode and buffer reference that the kernel writes into a register, a page table, a queue descriptor or a firmware message is checked by the kernel first. A device model's referee (gpusim) or a firmware check is a second line, never the first: on silicon nothing else is checking. The opaque *contents* of a submit blob are the one deliberate exception (principle 5), and they are contained by the VM (I1–I3 of `gpu-isolation.md`), not by inspection.

13. **Authority is brokered and split.** *Control* authority (device reset, scheduler weights, power gating, quotas, minting clients) is separate from *client* authority (running GPU work), and the kernel enforces the split itself; Capsicum's `cap_ioctls_limit` narrows further but is not what makes it safe. Two conforming profiles (§4.1): **node-based** (2.3): a client node and a root-only control node whose ioctls check privilege on every call, so a passed control fd carries nothing; and **minted**: a broker holding the control fd mints restricted client fds with `CLIENT_OPEN` and hands them over `SCM_RIGHTS`. The minted profile is required before a second GPU broker exists or any app receives a client fd outside test and development. **The default deployment gives no app a client fd at all**: apps reach the GPU through the aqueduct-gpu protocol to a trusted daemon (frescod, `aqueduct-gpu-host`), and jails plus devfs rulesets keep them off the device node. A direct client fd for an app exists only for testing and development, never in production (§4.1).

14. **A principal is the accounting unit; a VM is the isolation unit.** Isolation between GPU contexts is a property of the driver and the GPU MMU: jail boundaries do not apply to device DMA. What jails *are* good for is deciding who may hold a client fd and charging resource use. In the node-based profile the principal is the allocating uid (Atrium gives every app and every service its own uid, so a uid is one app or one service); in the minted profile it is the jail id the client was minted for (§4.2).

---

## 4. Object model

The ABI exposes eight object kinds. Each is a file descriptor.

| Object | What it represents | Created by | Lifetime ends |
|---|---|---|---|
| `control_fd` | Control authority over one GPU device | `open("/dev/atrium/gpu/<vendor><N>")` by the broker only (devfs-restricted) | Last `close()` |
| `device_fd` | A *client* context on one GPU device: the handle every client op goes through, carrying the jail id and quotas it was minted with | `ATRIUM_GPU_CLIENT_OPEN` on `control_fd` | Last `close()` |
| `vm_fd` | A GPU virtual address space | `ATRIUM_GPU_VM_CREATE` on `device_fd` | Last `close()`, then deferred until its queues' work retires (§5.11) |
| `bo_fd` | A buffer object resident in some heap; VM-agnostic until bound | `ATRIUM_GPU_BO_CREATE` on `device_fd`, *or* `ATRIUM_GPU_BO_IMPORT_SHARE` on `device_fd` | Last `close()`, then deferred until no in-flight work can reach it (§5.11) |
| `queue_fd` | A submission queue (one engine, one priority) in one VM, with its own fence slot and, for user-mode queues, its own doorbell page | `ATRIUM_GPU_QUEUE_CREATE` on `vm_fd` | Last `close()`: the queue is unmapped from the hardware/firmware, outstanding signal points are completed or errored (§10), doorbell mappings are revoked |
| `syncobj_fd` | A timeline semaphore (monotonic 64-bit counter plus an error word). Itself `EVFILT_READ`-able | `ATRIUM_GPU_SYNCOBJ_CREATE` on `device_fd` | Last `close()` |
| `perf_fd` | A set of performance counters, readable with `read(2)` | `ATRIUM_GPU_PERF_OPEN` on `device_fd` (needs `ALLOW_PERF`) or `control_fd` | Last `close()` |
| `share_fd` | A vendor-neutral handle to a buffer's backing storage, for cross-process, cross-device and display import | `ATRIUM_GPU_BO_EXPORT_SHARE` on `bo_fd` | Last `close()` (refcounts the underlying storage) |

*2.1 changes:* `event_fd` is removed (D-1: the syncobj fd is the kqueue source, with the threshold in the kevent `data` field). `BO_CREATE` and `BO_IMPORT_SHARE` move to `device_fd` (D-2). `control_fd` is new and the old "open the node" `device_fd` becomes a minted client context (§4.1).

Lifetime rules:

- Closing `device_fd` does *not* destroy objects derived from it; they hold their own references to the client context. A context's quotas stay charged until its last object is gone.
- Closing `vm_fd` releases all bindings in that VM once in-flight work on its queues has retired (§5.11). The bound BOs survive through their own fds.
- Closing `bo_fd` drops a reference. Backing storage is freed when the count reaches zero across all fds, imports and display scanouts **and** no submitted work can still reach it (§5.11).
- Closing `queue_fd` unmaps the queue, signals or errors its outstanding signal points (§10), and revokes its doorbell mapping (§5.9).
- Closing `syncobj_fd` while submissions still signal it is well-defined: the signal is dropped. Waiters on other fds of the same syncobj are unaffected.

All fds are inherited by `fork()` per POSIX. All fds except `control_fd` can be transported across processes via `SCM_RIGHTS`. A received fd keeps the authority and accounting it was minted with: it does not become the receiver's jail's resource. `control_fd` is not `DFLAG_PASSABLE`: control authority cannot be delegated by fd passing, only by opening the node, which devfs restricts.

### 4.1 Access and authority

*2.3: two profiles.* **Node-based** (what `atrium-gpu-amd` implements, conforming for a single broker): the client node (`/dev/atrium-gpu0`) carries client authority; the control node (`/dev/atrium-gpu0ctl`, root 0600 on the host, never unhidden in a jail) carries reset, scheduler and power gating, and checks privilege (`PRIV_DRIVER`) on every ioctl so that a control fd passed over `SCM_RIGHTS` grants nothing. Control ioctls on the client node fail with `EPERM`. Who may open the client node is decided by devfs rulesets and jaild's pins: in production only frescod's jail (`gpu-security-model.md` §2). Accounting is per uid (§4.2). **Minted** (the text below): required before a second broker process holds GPU fds, or before any app holds a client fd outside test and development; until then `CLIENT_OPEN`, `SET_QUOTA` and `QUERY_USAGE` may return `ENOTSUP`.

Three levels of authority, enforced by the kernel on every ioctl:

| Level | Holder | Can do |
|---|---|---|
| **Control** | The GPU broker: the process that owns the device node (frescod's broker in the single-GPU desktop; `aqueduct-gpu-host` for headless) | `CLIENT_OPEN`, `SET_QUOTA`, `DEVICE_RESET`, scheduler weights and deadline windows (`SCHED`), power gating (`POWERGATE`), device-wide telemetry, plus everything a client can do |
| **Client** | Holders of a minted `device_fd` | VM, BO, queue, syncobj, bind, submit, share, per-context telemetry, queue reset of *its own* queues |
| **None** | Everyone else | Nothing: they cannot open the node |

```
ATRIUM_GPU_CLIENT_OPEN   (on control_fd, returns device_fd)

struct atrium_gpu_client_open {
    uint32_t struct_size;
    uint32_t flags;               /* ATRIUM_GPU_CLIENT_* (e.g. ALLOW_USER_QUEUES) */
    int32_t  jail_id;             /* accounting jail; 0 = the broker's own */
    uint32_t sched_weight;        /* firmware time-slice weight (0 = default) */
    struct atrium_gpu_quota quota;/* §4.2; zero fields = device default */
    int32_t  out_device_fd;
    uint32_t _pad;
};
```

Rules:

- A control-only ioctl issued on a client `device_fd` fails with `EPERM`. The check is a field on the kernel's context object, not a Capsicum right, so it holds whether or not the receiver entered capability mode.
- The broker may further narrow a client fd with `cap_ioctls_limit()` before handing it out (for example, removing `QUEUE_MAP` from a client that must not use user-mode queues). That is defence in depth.
- **Default deployment (no direct grants):** apps hold no `device_fd`. They speak aqueduct-gpu to the broker, which is the only GPU client. Jails, devfs rulesets (`atrium.devfs.rules`) and Portcullis keep apps off the node. In this mode the kernel ABI's attack surface is the broker, and per-app isolation inside the broker is the shader sandbox (`aqueduct-gpu.md` §11) plus one VM per app inside the broker's context.
- **Direct grant: test and development only.** A client `device_fd` handed to an app is for driver bring-up, conformance runs and Tier-3 experiments, never a production path. It exists only when the broker runs with an explicit test policy (`[gpu] direct_grant = "test"` in `jaild.policy.toml`, refused when the policy is `production`) or inside a dev jail (`atrium-devjail.md`), and Portcullis still refuses it unless the bound driver passes `gpu-isolation.md`'s attestation gate. Production images ship with no path that mints a client fd for an app.

### 4.2 Resource accounting per jail

*2.3, node-based profile:* each allocation is charged to the real uid of the allocating thread before the backing is allocated, and given back when the backing is actually freed (after §5.11's deferred free). A uid over its cap gets `EDQUOT`. Caps are device-wide percentages (`atrium-gpu-amd`: 75 % of VRAM, 50 % of RAM, sysctl-tunable), plus a cap on live queues per uid. A BO fd passed to another process stays charged to the uid that allocated it. In production frescod allocates for every app, so the kernel bounds frescod as a whole (and keeps it apart from its renderers, a separate uid); per-app limits inside it are frescod's (≤ 4 GPU contexts per uid, its own memory budget). The record below applies to the minted profile.

Every client context carries the `jail_id` it was minted for. All contexts with the same `jail_id` on a device share one accounting record.

```
struct atrium_gpu_quota {
    uint64_t vram_bytes;          /* device-local heap */
    uint64_t system_bytes;        /* host (GTT) heap */
    uint64_t va_bytes;            /* total VA reservable across this jail's VMs */
    uint32_t max_vms;
    uint32_t max_queues;
    uint32_t max_syncobjs;
    uint32_t max_inflight;        /* outstanding signal points across queues */
};

ATRIUM_GPU_SET_QUOTA     (on control_fd)
ATRIUM_GPU_QUERY_USAGE   (on device_fd: own jail; on control_fd: any jail_id)
```

- Allocation beyond quota fails with `EDQUOT`. Nothing is reclaimed from a jail behind its back (no eviction in V1, §13h).
- GPU time is charged through the scheduler weight given at `CLIENT_OPEN`, enforced by firmware time-slicing (principle 7). The kernel does not meter per-submit time.
- When a jail is destroyed, its contexts keep working until their fds close: the kernel does not revoke on jail teardown. The broker is responsible for closing what it minted, and `QUERY_USAGE` lets it audit.

---

## 5. Per-cdev ABI surface

The full ioctl table. All input structs begin with `uint32_t struct_size; uint32_t _reserved;` (the reserved word forces 8-byte alignment for u64 fields below). All multi-byte fields are little-endian. All ioctls are issued on the appropriate object's fd.

Struct listings in this section are the layouts of the normative header `include/atrium/gpu_abi_v2.h`; ioctl numbers are assigned there (group `'A'`, 64–127, so the intermediate `atrium-gpu-amd` ABI at 0–27 can coexist during migration) and tabulated in §19. Every struct size is a multiple of 8 and has no implicit padding; the header is compile-checked for that.

### 5.1 Device discovery and capabilities

```
ATRIUM_GPU_QUERY_CAPS    (on device_fd or control_fd)

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
| `LOST_GENERATION` | `uint64_t` | Device reset generation; a change means `VK_ERROR_DEVICE_LOST` (§10). *New in 2.1* |
| `LIMITS` | struct | `max_blob`, per-call array limits, the context's quota (§4.2, §5.12). *New in 2.1* |

Payload layouts (`atrium_gpu_heap_info`, `atrium_gpu_queue_family_info`, `atrium_gpu_limits`, `atrium_gpu_clock_info`, `atrium_gpu_preemption_info`, `atrium_gpu_perf_counter_info`) and the remaining cap ids (`CLOCK`, `PREEMPTION`, `PERF_COUNTERS`) are in the header. Every payload is a multiple of 8 bytes.

TLV is chosen over a fixed struct because new caps will be added over the device's lifetime and old userspace must skip them cleanly.

### 5.2 VM lifecycle

```
ATRIUM_GPU_VM_CREATE     (on device_fd, returns vm_fd)

struct atrium_gpu_vm_create {
    uint32_t struct_size;
    uint32_t flags;               /* ATRIUM_GPU_VM_FLAG_* */
    uint64_t va_size;             /* requested VA span (0 = device default); charged to va_bytes */
    int32_t  out_vm_fd;           /* set by kernel */
    uint32_t _pad;
};
```

A VM is an isolated GPU address space. One client may create several (up to its `max_vms` quota); the broker in the default deployment creates one per app it serves.

**A VM is not a hardware VMID.** Hardware has a small fixed number of address-space slots (16 VMIDs on AMD, a handful of contexts on others). The ABI never ties a VM to one permanently, so the number of VMs is bounded by quota and memory, not by hardware slots:

- On firmware-scheduled hardware (AMD MES, Intel GuC, Apple ASC) a VM is registered with the firmware as a *process context* (page-table base, process quantum, priority) when its first queue is created, and the firmware assigns and switches hardware slots itself.
- On hardware without that, the vendor module assigns a VMID when a queue is made runnable and may steal an idle one (with a TLB flush) when it needs it. This is the vendor module's business and invisible to the ABI.

Whatever the mechanism, a VM's queues only ever execute under that VM's page tables (I1).

### 5.3 BO lifecycle

```
ATRIUM_GPU_BO_CREATE     (on device_fd, returns bo_fd)

struct atrium_gpu_bo_create {
    uint32_t struct_size;
    uint32_t flags;               /* ATRIUM_GPU_BO_FLAG_* (CPU_ACCESS, SCANOUT, ...) */
    uint64_t size;                /* bytes */
    uint64_t alignment;           /* bytes; 0 = device default */
    uint32_t heap_mask;           /* OR of acceptable heap indices */
    uint32_t _pad;
    int32_t  out_bo_fd;
    uint32_t _pad2;
};
```

A BO is created on the client context (D-2) and belongs to no VM until `VM_BIND`. `heap_mask` is a bitmap over the heap indices reported by `QUERY_CAPS`; the kernel picks any permitted heap with room under the jail's quota and fails with `EDQUOT` or `ENOMEM` otherwise. Zero is invalid. New BOs are zero-filled (I5).

```
ATRIUM_GPU_BO_MMAP_INFO  (on bo_fd)

struct atrium_gpu_bo_mmap_info {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t out_offset;          /* pass to mmap(device_fd, ..., offset) */
    uint64_t out_size;            /* page-aligned mappable size */
};
```

Userspace then calls `mmap()` on the *device_fd* with the returned offset. The cdev's `d_mmap_single` resolves the offset to the BO's `vm_object_t` (a device-pager object), so the kernel can **revoke** CPU mappings (invalidate the pager's pages) when the BO's backing moves or is lost on device reset. System-heap BOs are always mappable; device-local BOs are mappable only when the device exposes them through a CPU-visible BAR and the BO was created with `CPU_ACCESS` (`ENOTSUP` otherwise; populate them with a GPU copy from a System staging BO).

Copy ioctls (`BO_WRITE`/`BO_READ`) remain as a bring-up convenience and for tests. Production paths use the mapping.

**Compression (2.1, normative).** Lossless render-target compression (AMD DCC/HTILE and equivalents) is **off**: the kernel never sets a compression page-table bit or allocates compression metadata, and no modifier advertised in `QUERY_CAPS` is a compressed layout. A later revision defines it (§13o). Block-compressed texture *formats* (BC, ASTC, ETC2) are ordinary data to the kernel and are unaffected.

**Allocator requirements (normative for every vendor module):** device-local memory and each VM's VA space are managed with an allocator that frees and reuses (FreeBSD `vmem(9)` arenas are the reference choice). Bump-only allocators are not conformant: a long-running compositor must not exhaust VRAM or VA by allocating per frame.

### 5.4 VM_BIND

```
ATRIUM_GPU_VM_BIND       (on vm_fd, async)

struct atrium_gpu_vm_bind_op {
    uint32_t op;                  /* ATRIUM_GPU_BIND_{MAP,UNMAP,REMAP} */
    uint32_t flags;               /* ATRIUM_GPU_BIND_READ_ONLY, ATRIUM_GPU_BIND_SPARSE */
    int32_t  bo_fd;               /* MAP/REMAP only; -1 for UNMAP */
    uint32_t _pad;
    uint64_t bo_offset;           /* byte offset into BO */
    uint64_t va;                  /* GPU VA; 0 = kernel picks (MAP only); out: actual VA */
    uint64_t length;              /* bytes */
};

struct atrium_gpu_vm_bind {
    uint32_t struct_size;
    uint32_t op_count;
    uint64_t ops_ptr;             /* user array of struct atrium_gpu_vm_bind_op */
    int32_t  signal_syncobj_fd;   /* syncobj signaled when ops complete; -1 = sync */
    uint32_t _pad;
    uint64_t signal_value;        /* timeline value to write */
    uint32_t wait_count;
    uint32_t out_failed_op;       /* index of the failing op, on error */
    uint64_t wait_ptr;            /* atrium_gpu_syncobj_point[wait_count] */
};
```

A bind operation is async by default. The caller provides an output syncobj+value to wait on; the kernel signals it once all listed ops complete on the device side. If `signal_syncobj_fd == -1`, the call blocks until done. Ops in one ioctl are a transaction: all apply or none (the kernel rolls back and reports the failing op index).

**Validation (2.1, normative).** For each op the kernel checks, before touching a page table:

- `va`, `bo_offset` and `length` are multiples of the `ADDRESS_SPACE` cap's `va_align`, `length > 0`, and `[va, va+length)` lies inside the VM's address window. Otherwise `EINVAL`.
- `[bo_offset, bo_offset+length)` lies inside the BO. Otherwise `EINVAL`.
- `MAP` does not overlap an existing mapping in this VM (`EEXIST`); replacing a mapping is `REMAP`, which is explicit.
- `UNMAP`/`REMAP` cover exactly existing mappings (no partial unmaps in V1; `EINVAL`).
- `va == 0` on `MAP` allocates from the VM's VA arena (§5.3 allocator rule) and returns the VA in the op.
- *2.3, GPU W^X:* PTEs are executable only for a `MAP`/`REMAP` with `ATRIUM_GPU_BIND_EXEC`, and `BIND_EXEC` requires `BIND_READ_ONLY` (`EINVAL` otherwise). So no binding is both GPU-writable and GPU-executable, in any VM. Shader code, and command buffers the hardware fetches as instructions (on gfx12 the CP's ring fetch is an execute access), are bound `READ_ONLY | EXEC`; everything a dispatch may write is bound without `EXEC`. Kernel-owned pages in a client VM follow the same rule: the kernel-mediated ring is `READ | EXEC`; pages the CP writes (EOP buffer, writeback) are bound without `EXEC` unless silicon shows a fetch needs it, in which case the driver documents the exception at the mapping. `BIND_FEATURES` reports `BIND_FEAT_EXEC` once the driver honours this.

Unmapped VA and the BO's binding reference are released only after the VM's in-flight work has retired (§5.11), so a running submission never sees its translation pulled away or reused.

The kernel batches page-table updates and flushes the TLB once per bind call, not per page.

Sparse bindings are advertised via `BIND_FEATURES`; if absent, sparse flags return `ENOTSUP`.

### 5.5 Queue lifecycle

```
ATRIUM_GPU_QUEUE_CREATE  (on vm_fd, returns queue_fd)

struct atrium_gpu_queue_create {
    uint32_t struct_size;
    uint32_t family_id;           /* index into QUEUE_FAMILIES caps */
    uint32_t priority;            /* device-defined; 0 = default */
    uint32_t flags;               /* ATRIUM_GPU_QUEUE_FLAG_USER_MODE, ... */
    int32_t  out_queue_fd;
    uint32_t _pad;
};

ATRIUM_GPU_QUEUE_RESET   (on queue_fd)   /* recover this queue only, §10 */
```

A queue belongs to one VM for its whole life. 2.1 makes the queue a real, private object:

- **Its own hardware state.** Each queue has its own queue descriptor (MQD on AMD), ring and, for user-mode queues, its own doorbell page. Creating or submitting to one queue never reprograms another queue's descriptor, base or VM. On firmware-scheduled hardware the kernel registers the queue with the firmware (MES `ADD_QUEUE` on AMD) under the VM's process context (§5.2) and removes it on close.
- **Its own fence.** Each queue has a kernel-owned *fence slot*: a 64-bit monotonic sequence number in memory that the GPU writes when work completes (§5.7). Completion is identified by `(queue, seqno)`, never by counting interrupts.
- **Its own lock.** The kernel serializes everything that touches one queue's hardware state, its signal-point list and its doorbell under a per-queue lock. Different queues proceed in parallel.
- **Counted.** Creation is charged to `max_queues` (§4.2). There is no fixed device-wide queue count in the ABI; hardware limits are the vendor module's to multiplex.

Queues from different VMs cannot share submissions.

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
    uint64_t value;               /* raise the counter to this value; lower values are ignored */
};

ATRIUM_GPU_SYNCOBJ_WAIT    (on syncobj_fd, blocking host-side wait)

struct atrium_gpu_syncobj_wait {
    uint32_t struct_size;
    uint32_t flags;
    uint64_t value;               /* wait until counter >= value */
    int64_t  timeout_ns;          /* -1 = forever; 0 = poll */
};

ATRIUM_GPU_SYNCOBJ_QUERY   (on syncobj_fd, non-blocking)

struct atrium_gpu_syncobj_query {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t out_current_value;
    uint32_t out_error;           /* 0, or ATRIUM_GPU_ERR_* of the first errored point */
    uint32_t _pad;
    uint64_t out_error_value;     /* timeline value at which that error occurred */
};
```

**kqueue (D-1).** The syncobj fd is itself the kqueue source. Register it with `EVFILT_READ` and put the threshold in the kevent `data` field; the event fires when the counter reaches or passes it. On return, `data` holds the current value and `fflags` carries `ATRIUM_GPU_NOTE_ERROR` if an error has been recorded at or below the threshold. kqueue registrations are per-process, so a syncobj fd received over `SCM_RIGHTS` is registered fresh by the receiver (resolves v2 §13f).

**Error state (2.1).** When work that would signal a point is aborted (a fault, a hang, a queue or device reset; §10), the kernel still advances the counter to that value, so no waiter hangs, and records the error and the value. `SYNCOBJ_WAIT` then returns `EIO` for any `value >= out_error_value`. The error is sticky for the life of the syncobj.

**Waits are interruptible.** `SYNCOBJ_WAIT` sleeps with signal catching and returns `EINTR`/`ERESTART` normally. Timeouts are 64-bit nanoseconds; no tick arithmetic that can overflow.

This is how a kqueue-driven compositor like frescod folds GPU completion, socket traffic, input and vblank into one `kevent()`.

### 5.7 SUBMIT

```
ATRIUM_GPU_SUBMIT        (on queue_fd)

struct atrium_gpu_syncobj_point {   /* also used by VM_BIND waits */
    int32_t  syncobj_fd;
    uint32_t _pad;
    uint64_t value;               /* timeline point */
};

struct atrium_gpu_submit {
    uint32_t struct_size;
    uint32_t flags;
    uint64_t blob_va;             /* GPU VA of the command blob, bound in this queue's VM */
    uint64_t blob_size;           /* bytes; <= SUBMIT_FEATURES.max_blob */
    uint64_t in_dep_ptr;          /* atrium_gpu_syncobj_point[] to wait on */
    uint32_t in_dep_count;
    uint32_t deadline_ns;         /* frame deadline hint for the firmware scheduler; 0 = none */
    uint64_t out_dep_ptr;         /* atrium_gpu_syncobj_point[] to signal */
    uint32_t out_dep_count;
    uint32_t _pad2;
    uint64_t out_seqno;           /* the (queue, seqno) this submission retires at */
};
```

The blob is opaque to the kernel: a vendor-specific command stream built by userspace. 2.1 fixes *how* it reaches the hardware so that opacity, completion and isolation all hold:

- **The blob is executed by reference.** Userspace writes the blob into a BO bound in the queue's VM and passes its VA. The kernel validates that `[blob_va, blob_va+blob_size)` is mapped in this VM, then appends to the queue's **kernel-owned ring** exactly two things: a reference to the blob (an indirect-buffer packet on AMD) and a fence write of the next sequence number to the queue's fence slot, with an interrupt. The kernel never parses the blob and userspace never writes the kernel ring. The blob executes under the queue's VM, so it can reach only that VM's mappings (I1–I3); privileged state is unreachable from an indirect buffer (I4).
- **Completion is identified.** The submission's signal points (`out_dep`s) are recorded against `(queue, seqno)`. On the interrupt the kernel reads each signalling queue's fence slot and retires *every* signal point with `seqno <= fence`, in any order across queues. There is no fixed-size global completion list; `max_inflight` (§4.2) bounds it and a submit beyond it fails with `EAGAIN` rather than being accepted and never signalled.
- **Dependencies.** In-deps from the same device whose producer is already submitted become GPU-side waits on the producer queue's fence slot where the hardware supports it; any other in-dep is waited for by the kernel before the blob is placed in the ring. Either way the queue lock is not held while waiting. If an in-dep is errored (§5.6), the submission does not run: its own signal points are errored with `ATRIUM_GPU_ERR_DEPENDENCY`, so failure propagates down a dependency chain instead of executing on bad input.
- **The deadline is per submission**, carried in the queue's descriptor or the firmware message for that submit. It is never staged in a device-global register.

On firmware-scheduled hardware the ring above belongs to the queue's descriptor that the firmware runs; on simpler hardware it is the vendor module's per-queue ring. The ABI is the same.

### 5.8 Cross-process buffer sharing

```
ATRIUM_GPU_BO_EXPORT_SHARE (on bo_fd, returns share_fd)

struct atrium_gpu_bo_export_share {
    uint32_t struct_size;
    uint32_t flags;
    int32_t  out_share_fd;
    uint32_t _pad;
};

ATRIUM_GPU_BO_IMPORT_SHARE (on device_fd, returns bo_fd)

struct atrium_gpu_bo_import_share {
    uint32_t struct_size;
    uint32_t flags;
    int32_t  share_fd;
    int32_t  out_bo_fd;
};
```

`share_fd` is **vendor-neutral**. It is a handle to a buffer's backing storage with a vendor-tagged header the importing driver inspects to decide how to map. A buffer exported and imported by the same vendor module is zero-cost (a refcount bump). A buffer exported by one vendor and imported by another is supported only if both agree on a common storage representation (§6.2, §13b) and is otherwise rejected with `ENOTSUP`.

*2.1 changes:* import moves from `vm_fd` to `device_fd` (D-2): an imported BO is VM-agnostic like a created one and is bound with `VM_BIND`. An imported BO is charged to the **exporter's** accounting record, not the importer's (§4.2), so a compositor importing every client's frames does not exhaust its own quota; the importer's reference still delays the free (§5.11). The intermediate `BO_EXPORT_SCANOUT` ioctl of `atrium-gpu-amd` is retired: the display device imports a `share_fd` (or a `bo_fd`) like any other importer (§7). No ioctl anywhere in the ABI takes a raw VRAM offset.

The `share_fd` is what travels across processes via `SCM_RIGHTS`. The compositor protocol (aqueduct CLASS_DISPLAY) carries `share_fd`s as the substrate for any cross-process buffer reference. Synchronization is always via separate `syncobj_fd`s passed alongside; there is no implicit fence carriage.

### 5.9 User-mode queues (direct doorbell submission)

`SUBMIT` (§5.7) is the baseline: one ioctl per submission. On hardware with firmware-scheduled user-mode queues (AMD MES on GFX11/GFX12, Intel GuC, Apple ASC), userspace can instead ring a hardware doorbell directly and skip the syscall on the submit hot path. It is advertised by the `USER_QUEUE` bit of `SUBMIT_FEATURES` **and** requires the client context to have been minted with `ATRIUM_GPU_CLIENT_ALLOW_USER_QUEUES` (§4.1); otherwise the ioctls below return `ENOTSUP` or `EPERM` and userspace uses §5.7. Because apps hold no client fd in production (§4.1), in production this is a broker-internal fast path.

*2.3:* in the node-based profile (§4.1) there is no minted flag, so `QUEUE_CREATE` with `USER_MODE` requires `PRIV_DRIVER` (host root; jailed root is refused). A user-mode queue's holder writes its own ring, which gives up the "broker writes every packet" property of the app path (`gpu-app-path-f6.md`). So a user-mode queue is never handed to an app or a per-client renderer, and frescod (non-root) gets one only after that is decided for its own trusted renderer. Kernel-mediated queue fds (§5.7) are the production path.

A user-mode queue is requested at creation (`ATRIUM_GPU_QUEUE_CREATE` with `flags |= ATRIUM_GPU_QUEUE_FLAG_USER_MODE`) and then mapped:

```
ATRIUM_GPU_QUEUE_MAP     (on queue_fd)

struct atrium_gpu_queue_map {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t out_ring_offset;     /* mmap(queue_fd) offset: command ring */
    uint64_t out_ring_size;
    uint64_t out_doorbell_offset; /* mmap(queue_fd) offset: this queue's doorbell page */
    uint64_t out_doorbell_size;
    uint64_t out_wptr_offset;     /* write-pointer location (ring-relative) */
    uint64_t out_fence_gpu_va;    /* 2.2: GPU VA of the queue's fence slot */
};
```

*2.1 changes (resolves §13k):*

- **Mapped from `queue_fd`, not `device_fd`.** Each queue has its own `d_mmap_single` device pager. The only doorbell offset `mmap` accepts is that queue's own page; any other offset fails with `EACCES`. Raw doorbell-BAR mapping through `device_fd` does not exist.
- **One queue per doorbell page.** The hardware doorbell page is the unit of authority, so one page grants exactly one queue. Vendors whose hardware packs several doorbells per page assign at most one *user-mode* queue per page and leave the rest unused.
- **Revocable.** On `QUEUE_RESET`, device reset or `close(queue_fd)` the kernel removes the queue from the firmware, then invalidates the pager's pages (`cdev_pager_free_page`/`pmap_remove`). A later access faults, and the fault handler returns `SIGBUS`; a queue re-created after reset is mapped afresh.
- **Hang detection without seeing submissions.** The kernel cannot count user-mode submissions, so the watchdog (§10) watches the queue's fence slot against its write pointer: a queue whose write pointer is ahead of its fence for longer than the watchdog period is hung. Its signal points are errored like a kernel-submitted queue's.
- **Frees never trust userspace fences.** A user-mode queue's fence slot is writable by work the user built, so it cannot gate memory reuse. Freeing VA or backing storage reachable from a user-mode queue's VM waits for the firmware to preempt or remove the queue (§5.11).

*2.2 changes:*

- **Fence slot address.** `out_fence_gpu_va` (appended; a 2.1-sized struct still works and gets no address) is the GPU VA, in the queue's own VM, of the queue's fence slot: the address the client's end-of-work packets (AMD: `RELEASE_MEM`) write their fence value to. The slot sits in a kernel-allocated writeback page the client also maps: rptr report, write pointer, fence, and an *emitted* word in which the client publishes the last fence value it put on the ring. Without the address a client would have to hard-code the layout.
- **Advisory and client-written.** The slot and the emitted word are for the client's own waits and for the kernel's hang watchdog (fence short of emitted, or the hardware read pointer short of the write pointer, neither moving for the watchdog period; the read pointer alone is not enough because it is a fetch pointer and runs ahead of execution). The kernel never frees, reuses or signals anything on them: the free rule stays firmware preemption or removal (§5.11), and kqueue completion for a user-mode queue comes from the queue's own end-of-pipe interrupt, never from reading the slot. A client can fake progress to dodge the watchdog; that only keeps its own queue alive, which the firmware still time-slices against everyone else.

What stays in the kernel (principle 11): VM_BIND and memory validation, queue/MQD setup and its hand-off to the scheduler firmware, preemption, and hang/reset (§10). Userspace owns only its own ring contents and write pointer. Completion is unchanged: signal points on syncobjs, observed via kqueue on the syncobj fd (§5.6).

**Security model.** The doorbell page is scoped to one queue, and the queue runs in one VM. Mapping it grants exactly the authority to kick *this* queue, which can reach only addresses bound in *this* VM. A `queue_fd` passed over `SCM_RIGHTS` is a scoped capability, and it stops working when the kernel revokes it.

### 5.10 Energy telemetry and control-only device ioctls

```
ATRIUM_GPU_QUERY_TELEMETRY  (on device_fd: own context; on control_fd: whole device)

struct atrium_gpu_telemetry {
    uint32_t struct_size;
    uint32_t _reserved;
    uint64_t busy_ns;             /* monotonic engine-busy time for the scope */
    uint64_t sample_ns;           /* monotonic wall time of this sample */
    uint32_t queue_depth;         /* submissions in flight in the scope */
    uint32_t power_state;         /* vendor-normalized 0=idle .. N=max (control_fd only; 0 on device_fd) */
    uint32_t tier_cost_hint;      /* normalized energy/op estimate (vs Tier-2) */
    uint32_t flags;
};
```

`busy_ns` over elapsed `sample_ns` gives a utilization fraction; `tier_cost_hint` is on the same scale the Tier-2 software path reports, so the energy router (`energy-policy.md`) can compare them. Advertised by the `TELEMETRY` cap; absent it, `ENOTSUP`. Telemetry is read-only and advisory: the router samples it and drives nothing.

*2.1 change:* a client sees its own context's numbers only. Device-wide numbers and the power state are control-only, because another tenant's busy time is a side channel.

The device-level knobs that `atrium-gpu-amd` exposes today as unprivileged ioctls are **control-only** (`EPERM` on a client `device_fd`):

| ioctl | On | Purpose |
|---|---|---|
| `ATRIUM_GPU_DEVICE_RESET` | `control_fd` | Full device reset (§10) |
| `ATRIUM_GPU_SCHED` | `control_fd` | Scheduler policy: weights, deadline window, priorities per context |
| `ATRIUM_GPU_POWERGATE` | `control_fd` | Power gating policy; the broker's channel for the energy policy |
| `ATRIUM_GPU_SET_QUOTA` | `control_fd` | §4.2 |

A client can reset only its own queues (`QUEUE_RESET` on its `queue_fd`).

### 5.11 Memory lifetime versus in-flight work (new in 2.1)

The kernel never frees or reuses memory that submitted work can still reach. This is the rule `atrium-gpu-amd` breaks today (a BO freed while a submission still references it), and the one every allocator in the vendor module must follow.

- **Per-VM high-water mark.** Each VM records, per queue, the last `seqno` submitted on it. That pair set is the VM's *retire point*.
- **Deferred free.** When the last reference to a BO, a binding or a VM goes away, the kernel moves the backing (VRAM range, system pages, VA range, page-table pages) to a deferred list tagged with the current retire point of every VM it was bound in. The fence-retire path (§5.7) releases entries whose retire point has passed. Nothing is returned to an allocator before that.
- **Unbind.** `VM_BIND` with `UNMAP` updates the page tables and flushes the TLB immediately, so new work cannot reach the range, but the VA range and the backing are released only at the retire point (§5.4).
- **User-mode queues.** For a VM with a user-mode queue, the retire point is "the firmware has preempted or removed every user-mode queue of this VM", never a value read from a user-writable fence slot (§5.9).
- **Reset.** After a queue or device reset the kernel knows nothing more will execute from the reset queues, so their pending deferred entries are released at once.
- **Accounting.** Deferred memory stays charged to its jail until it is actually released, so a client cannot exceed its quota by free-and-reallocate churn.

### 5.12 What the kernel validates (new in 2.1)

Principle 12 in list form. Every vendor module must check these itself; a simulator referee or an IOMMU is a second line, never the first. Each line is a test case in `tests/gpu/` (§18).

| Input | Check | Error |
|---|---|---|
| Every struct | `struct_size` ≥ the 2.0 size and ≤ a page; `_pad`/`_reserved` zero; unknown `flags` bits zero | `EINVAL` |
| Every fd field | is an fd of the expected object kind, on the same device (or a valid `share_fd`) | `EBADF` / `EINVAL` |
| Counts × sizes | no overflow; arrays fit the per-call limits in `QUERY_CAPS` | `EINVAL` / `E2BIG` |
| `VM_BIND` | §5.4 list (alignment, window, BO range, overlap, exact unmap) | `EINVAL` / `EEXIST` / `ENOENT` |
| `SUBMIT` | blob range mapped in this VM and readable by the GPU; size ≤ `max_blob`; in-flight under `max_inflight` | `EFAULT` / `E2BIG` / `EAGAIN` |
| `mmap` | offset is one this fd handed out, length within that object | `EACCES` / `EINVAL` |
| Display commit | buffer is a BO/share of a size, pitch, format and tiling the plane accepts; BO pinned for scanout (§7) | `EINVAL` |
| Sealed BO | bind only `READ_ONLY`; no writable mmap or `BO_WRITE`; no seal with live writable bindings | `EACCES` / `EBUSY` |
| Authority | control-only ioctl on a client context | `EPERM` |
| Quota | allocation within the jail's record | `EDQUOT` |

### 5.13 Sealed buffers and content-addressed sharing (new in 2.1)

Isolation means no context can write to, or reach, memory that is not its own. It does not require a private copy per context. One physical buffer may be bound into many VMs if no binding can change it. Sealing makes that a kernel guarantee, so content-addressed dedup (Tessera, the aqueduct CAS) can share GPU memory across apps without weakening I1–I5.

```
ATRIUM_GPU_BO_SEAL       (on bo_fd)

struct atrium_gpu_bo_seal {
    uint32_t struct_size;
    uint32_t flags;               /* must be 0 */
    int32_t  wait_syncobj_fd;     /* -1, or seal once this point signals */
    uint32_t _pad;
    uint64_t wait_value;
};
```

- **Seal once, forever.** After `BO_SEAL` the BO's contents are immutable for the life of the storage. The seal travels with the storage, so every `bo_fd` and `share_fd` that refers to it sees it, and `BO_QUERY` reports `SEALED`. There is no unseal.
- **What the kernel enforces.** A sealed BO can be bound only with the `READ_ONLY` bind flag, and the kernel writes read-only GPU page-table entries for it (`EACCES` on a writable bind). CPU mappings are read-only (`EACCES` on `PROT_WRITE`); existing writable mappings are revoked through the device pager at seal time. `BO_WRITE` fails with `EACCES`. A BO cannot be sealed while it has a writable binding (`EBUSY`): unbind it first, or pass a wait point so the seal applies after the producing work retires (§5.11).
- **Not scanout, not shared with writers.** A BO created with `SCANOUT`, or one still imported writable elsewhere, cannot be sealed (`EBUSY`).
- **Hashing stays in userspace.** The kernel never hashes, never compares contents and never merges pages on its own (no KSM-style scanning). The broker or Tessera keys sealed BOs by content hash and hands out `share_fd`s for them. The kernel's only job is to make the seal hold.
- **Accounting.** A sealed shared BO is charged once, to the record of the context that created it, normally the broker's shared pool (§4.2), not to every importer.

What is allowed to be deduplicated is a policy for the sharing service, and it is normative:

- **Public, immutable content only.** Shader binaries and pipeline caches, fonts and glyph atlases, icons, engine and package assets. Never framebuffers, render targets, or any buffer whose contents an app produced from its own data.
- **Proven possession.** A context may be handed the shared copy of H only if its trust domain has already proven it holds H (the `aqueduct.md` §6.6 rule). Otherwise it gets a private copy, and the load time must not depend on whether H was already resident. This closes the dedup timing oracle (one app learning what another is using).
- **Physical side channels remain.** A read-only page table does not stop Rowhammer-style bit flips in shared DRAM, or cache-timing observation of another tenant's reads. That is why the content rule above excludes anything secret. Platforms with ECC or TRR reduce the first; nothing in this ABI removes the second.

### 5.14 Remaining calls, flags and error codes (new in 2.1)

The calls below are referenced elsewhere in this document; their full layouts are in the header.

| Call | On | Purpose |
|---|---|---|
| `BO_QUERY` | `bo_fd` | Size, heap, and state flags: `SEALED`, `LOST` (contents lost to a reset, §10), `SCANOUT`, `CPU_ACCESS`, `IMPORTED` |
| `QUEUE_QUERY_FAULT` | `queue_fd` | The queue's error, the faulting submission's `seqno`, faulting VA, access type (read/write/exec) and engine. Only the owner of the queue sees its fault record |
| `QUEUE_RESET` | `queue_fd` | Clear a queue's error state after §10 recovery; outstanding points not yet errored are cancelled (`ERR_CANCELLED`) |
| `SET_QUOTA` | `control_fd` | `{jail_id, atrium_gpu_quota}`; lowering a quota below current use does not reclaim, it only blocks further allocation |
| `QUERY_USAGE` | `device_fd` (own jail) or `control_fd` (any `jail_id`) | Current use and limit, both as `atrium_gpu_quota` |
| `SCHED` | `control_fd` | One op per call on a target fd the broker holds: `SET_WEIGHT` (a context), `SET_PRIORITY` (a queue; includes the top class reserved for the control plane, §6.4), `DEADLINE_WINDOW`, `WATCHDOG` (period in ns, §10), `RESET_BUDGET` (§10.1) |
| `POWERGATE` | `control_fd` | The broker's power-gating hint: blocks in use now and next; returns what was gated and the expected wake stall. Advisory, per the energy policy |
| `DEVICE_RESET` | `control_fd` | Rung 1 of §10.1 on demand; returns the new lost generation |

Flags used by more than one section: client flags `ALLOW_USER_QUEUES`, `ALLOW_PERF` (§4.1); BO flags `CPU_ACCESS`, `SCANOUT` (§5.3); bind flags `READ_ONLY`, `SPARSE` (§5.4; `READ_ONLY` support is mandatory in 2.1); queue flag `USER_MODE` (§5.9).

**Error codes** recorded on errored signal points (§5.6) and in fault records:

| Code | Meaning |
|---|---|
| `ERR_FAULT` | GPU page fault or shader trap in this queue's work |
| `ERR_HANG` | This queue's watchdog expired |
| `ERR_RESET` | Innocent work lost to a device reset (rung 1 or worse) |
| `ERR_DEVICE_GONE` | Device detached (rung 3) or hot-removed |
| `ERR_CANCELLED` | Queue closed or reset before the work ran |
| `ERR_DEPENDENCY` | An in-dependency was itself errored (§5.7) |

### 5.15 Timestamps and performance counters (new in 2.1)

**Timestamps.** GPU timestamps are written by the command stream itself (in the blob), so they need no ioctl. What userspace cannot do alone is relate them to CPU time, which Vulkan's calibrated timestamps require:

```
ATRIUM_GPU_CLOCK_CALIBRATE   (on device_fd)
  -> out_gpu_timestamp, out_cpu_ns (CLOCK_MONOTONIC), out_max_deviation_ns
```

The kernel samples both clocks as close together as the hardware allows and reports the uncertainty. Tick rate and valid bits come from the `CLOCK` cap. A client's timestamps measure only its own work, so they are not a cross-tenant channel beyond what any timer gives.

**Performance counters are a side channel** (cache, memory and engine counters reveal what other tenants run), so access is gated:

```
ATRIUM_GPU_PERF_OPEN   (device_fd with ALLOW_PERF, or control_fd; returns perf_fd)
  counters: array of counter_id from the PERF_COUNTERS cap
  read(perf_fd) -> uint64 value[counter_count]
```

- On a client `device_fd` it requires `ATRIUM_GPU_CLIENT_ALLOW_PERF`, which, like a direct grant, is for test and development only (§4.1). Only counters the vendor marks `PER_CONTEXT` (hardware filters them to this context's VM) may be opened there; others fail with `EPERM`.
- Device-wide counters (`PERF_DEVICE_WIDE`) are control-only. The broker may use them for its own profiling and energy decisions and must not relay them to apps.
- Opening counters never changes scheduling or power state visible to other contexts.

---

## 6. Cross-process and cross-device semantics

The ABI is designed for two classes of cross-boundary scenarios:

### 6.1 Compositor-style sharing (single device, multiple processes)

A client process produces frames into a `bo_fd` it allocated; the compositor process consumes them. Flow:

1. Client: `BO_CREATE`, `VM_BIND` into its VM, render via `SUBMIT`, signal `syncobj` on completion.
2. Client: `BO_EXPORT_SHARE` produces `share_fd`; `SYNCOBJ` is already an fd.
3. Client: send `share_fd` + `syncobj_fd` + timeline value over aqueduct (via `SCM_RIGHTS`).
4. Compositor: receive both fds. `BO_IMPORT_SHARE` on its `device_fd` and `VM_BIND` into its VM (zero-copy on same vendor). Register the syncobj fd with kqueue, threshold in `data` (§5.6).
5. Compositor: when kqueue fires, the BO is ready for use as a texture in the next composite pass.

No dma-buf, no Wayland-style explicit-sync extension, no separate fence-fd protocol. Just two fds and a counter value.

### 6.2 Cross-device sharing (multiple GPUs, single process or multi-process)

*(unchanged in 2.1)*

Mesa needs this for laptops with discrete + integrated GPUs (render on discrete, display on integrated). The same `share_fd` mechanism applies, with the constraint that both vendor modules must understand the share representation. Practically this means: vendors implementing this ABI agree on a shared "physical-pages-with-vendor-tag" representation as the export format, similar to dma-buf today. The agreement is documented in this spec; vendors that opt out reject cross-vendor imports cleanly (`ENOTSUP`) rather than corrupting state.

Display cdev is treated as a "vendor" for this purpose: a BO produced by `atrium-amdgpu` and imported by `atrium-display` is a scanout buffer; the display cdev's `IMPORT_SHARE` accepts vendor-tagged buffers it can DMA from.


### 6.3 Virtualization (plan, not V1)

Two mechanisms, chosen by who is on each side:

- **Foreign host or foreign guest: API forwarding.** The guest sends Vulkan or aqueduct-gpu commands and the host executes them on its own stack. This is Carillon and aqueduct-gpu today (Atrium on macOS), and it is the only option for a Linux guest running its own DRM driver, since translating DRM uAPI into this ABI is out of scope.
- **Atrium guest on an Atrium host: native context.** The guest runs the real vendor userspace and only this ABI crosses a virtio-gpu channel. bhyve's virtio-gpu backend asks the broker for one client context per guest (§4.1), with that guest's quota and scheduler weight, so a guest is isolated and recovered exactly like an app. This works because the ABI is transport-clean (principle 5, and rule 2 of §6.4).

Two gaps must be closed before native context ships: importing guest RAM as a system-heap BO (a `BO_IMPORT_MEMORY` from a bhyve memory-segment fd; there is no userptr in this ABI), and reflecting a host device reset into the guest as its own lost-generation change (§10).

Hardware partitioning (SR-IOV, MIG, Mali AE partitions) and whole-GPU passthrough are not planned. If a partitioning GPU is targeted, a partition is selected at `CLIENT_OPEN` through a trailing field (§8), so no existing struct changes.

### 6.4 Mixed-criticality readiness (design rules, not a feature)

Atrium does not target safety-certified use. These rules keep the architecture *capable* of it (automotive-style separation of a safety-rated display from untrusted rendering) without building it now. Every future change to this ABI must keep them true:

1. **The control plane is liftable.** Everything at the Control level (§4.1), the validation duties (§5.12) and the recovery ladder (§10.1) depend only on the control/client boundary, never on sharing kernel state with clients. That is what lets the control plane move into a small separately verifiable component (an EL2 hypervisor or separation kernel) with FreeBSD becoming one client through native context (§6.3), without changing the client ABI.
2. **Every client call is transport-clean.** Ioctl structs are flat POD; every pointer field addresses a flat array of POD with an explicit count; submit blobs contain no pointers. Any client call can be marshalled across a hypervisor channel unchanged.
3. **No device-global state is client-visible.** Deadlines, priorities, resets and telemetry are per context or per queue (§5.7, §5.10, §10). A client can never change another's timing except through scheduler policy the control plane sets.
4. **Timing is reported, not assumed.** `QUERY_CAPS` will carry the device's preemption granularity and, where the vendor can state one, a worst-case preemption latency (a trailing cap record, §8). `SCHED` supports a highest-priority class reserved for the control plane's own contexts.
5. **Display planes can have separate owners.** The display lease (§7) is granted per plane. A plane can be leased to a different owner from the main compositor, with its own fences, and survives the main owner's crash (§10.2). Display output CRC readback, where the hardware has it, is exposed as a display capability so a monitor can check that what is on screen matches what was committed.
6. **The safe path does not need the GPU.** Content that must always be visible can be rendered on the CPU (Tier-2) into a dedicated plane, so a GPU fault, reset or detach (§10.1) cannot hide it.

---

## 7. Display ABI summary

Display is on a separate cdev (`/dev/atrium/display<N>`) specified in [`atrium-display-architecture.md`](atrium-display-architecture.md). 2.1 fixes the parts of the interface that belong to this ABI:

- **Scanout by object, never by address.** The atomic commit takes a `bo_fd` or a `share_fd` per plane. The display module resolves it, checks size, pitch, format and tiling against the plane (§5.12) and pins the backing for as long as it is on screen; the BO's free is deferred until it has been flipped away (§5.11). The `atrium-gpu-amd` path that writes a userspace-supplied `{vram_offset, size}` into the scanout registers, and the `BO_EXPORT_SCANOUT` ioctl that feeds it, are retired.
- **Ownership is a lease.** One process holds the display at a time: the broker (frescod) opens `/dev/atrium/display<N>` exclusively, and a second open fails with `EBUSY`. A test harness or a VT-switch takes it only through the broker. There is no DRM-master-style ambient grant. When the holder's fd closes the last frame stays on screen until the next holder commits (§10.2). The lease is granted per plane: normally the broker holds every plane, but a plane can be leased to a separate owner (§6.4 rule 5).
- **Fences.** A commit takes an optional in-fence (`syncobj_fd`, value) that the display module waits on before latching, and an out-fence (`syncobj_fd`, value) it signals when the previous buffer is released.
- **vblank and hotplug** are kqueue events on the display fd (vblank-wait ioctl retired).

No display configuration touches the GPU cdev. No GPU operation touches the display cdev. They share buffer and syncobj fds; that is the entire interface.

---

## 8. Versioning and extension model

Three layers of evolution, in order of preference:

1. **Trailing fields in input structs.** Append new fields at the end. Old userspace passes a smaller `struct_size`; the kernel zero-fills missing fields with documented defaults. New userspace passes a larger `struct_size`; old kernels see fields they don't recognize and ignore them (the documented behavior is that the kernel's effective size is `min(struct_size, kernel_known_size)`).

2. **New cap_id values in `QUERY_CAPS`.** Drivers advertise new features by emitting new cap records. Old userspace skips unknown cap_ids (TLV walks past unrecognized records). New userspace tests for cap presence before using new features.

3. **New ioctl numbers.** When a feature is not expressible as a struct extension, add a new ioctl. Older drivers return `ENOTTY`; userspace tests via `QUERY_CAPS` for the corresponding feature flag before issuing the new ioctl.

We do not version the ABI as a single monolithic number except as a coarse compatibility shibboleth (ABI_VERSION major bumps are reserved for incompatible struct layout changes that cannot be expressed with `struct_size`; this should never happen).

The "uAPI is forever" lesson from DRM (findings §1.8) applies in full: once an ioctl ships with stable userspace consumers, its signature is permanent. New behavior goes through new mechanisms, not by redefining old ones.


*2.1 additions.* Layer 1 only works if the structs carry it. Two rules are mandatory before any out-of-tree consumer (Mesa winsys, `atrium-gpu-rs`) links against the ABI:

- Every input struct begins with `uint32_t struct_size`, including the ones `atrium-gpu-amd` defines today without it. The kernel rejects a size below the 2.0 layout.
- Every fd field is `int32_t`, and `-1` means "none". The intermediate ABI's `uint32_t` fd fields are converted in the same change.

---

## 9. Worked example: a Vulkan frame on this ABI

Pseudocode for a Vulkan backend rendering one frame and presenting to a Fresco compositor. In production the "client" below is the broker acting for an app (§4.1); in a test or dev jail it can be the app itself.

```c
/* broker, one-time */
control_fd = open("/dev/atrium/gpu/amdgpu0", O_RDWR);      /* devfs: broker only */
ioctl(control_fd, ATRIUM_GPU_CLIENT_OPEN, &(struct atrium_gpu_client_open){
    .struct_size = sizeof(...), .jail_id = app_jid, .quota = app_quota });
device_fd = co.out_device_fd;

/* client, one-time */
ioctl(device_fd, ATRIUM_GPU_QUERY_CAPS, &caps);
vm_fd    = /* ATRIUM_GPU_VM_CREATE on device_fd */;
queue_fd = /* ATRIUM_GPU_QUEUE_CREATE on vm_fd */;
frame_done = /* ATRIUM_GPU_SYNCOBJ_CREATE on device_fd */;

/* allocate and bind (BO is VM-agnostic until bound) */
image_bo = /* ATRIUM_GPU_BO_CREATE on device_fd */;
cmd_bo   = /* ATRIUM_GPU_BO_CREATE on device_fd, CPU_ACCESS */;
ioctl(vm_fd, ATRIUM_GPU_VM_BIND, &bind_args);   /* image_va, cmd_va */
cmd = mmap(NULL, cmd_size, PROT_WRITE, MAP_SHARED, cmd_bo, cmd_mmap_offset);

/* per frame */
build_command_blob(cmd, image_va, ...);
ioctl(queue_fd, ATRIUM_GPU_SUBMIT, &(struct atrium_gpu_submit){
    .blob_va = cmd_va, .blob_size = n,
    .out_dep_ptr = &(struct atrium_gpu_syncobj_point){ frame_done, 0, ++frame },
    .out_dep_count = 1 });

/* hand to compositor */
share_fd = /* ATRIUM_GPU_BO_EXPORT_SHARE on image_bo */;
sendmsg(aqueduct_socket, SCM_RIGHTS={share_fd, frame_done}, frame);
close(share_fd);
```

The compositor side:

```c
/* recvmsg returns share_fd + frame_done + value */
bo_fd = /* ATRIUM_GPU_BO_IMPORT_SHARE on the compositor's device_fd */;
ioctl(comp_vm_fd, ATRIUM_GPU_VM_BIND, ...);

EV_SET(&kev, frame_done, EVFILT_READ, EV_ADD, 0, value, NULL);   /* threshold in data */
kevent(kq, &kev, 1, NULL, 0, NULL);

/* when it fires: if (kev.fflags & ATRIUM_GPU_NOTE_ERROR) drop the frame;
   else use bo_fd as a texture in the next composite SUBMIT */
```

No Wayland surface, no dma-buf, no separate event fd. Two fds and a counter value.

---

## 10. Hang recovery and fault model

*Rewritten in 2.1; the v2 text was a sketch.*

- **Detection.** Each queue has a watchdog. A kernel-submitted queue is hung when its fence slot has not advanced past an outstanding `seqno` for the watchdog period; a user-mode queue when its write pointer has stayed ahead of its fence for that period (§5.9). Default period 2 s, configurable per context by the broker (`SCHED`); compute contexts that need long-running work get a longer period or the long-running flag (§13a). Page faults and shader traps reported by interrupt are detected immediately.
- **Queue reset first.** The kernel removes the faulting queue from the hardware or firmware (MES `REMOVE_QUEUE` on AMD), resets only it, and errors its outstanding signal points with `ATRIUM_GPU_ERR_FAULT`, `ERR_HANG` or `ERR_RESET` (§5.6): the counter advances so no waiter hangs, and `SYNCOBJ_WAIT` returns `EIO`. The queue's later submits fail with `EIO` until the owner calls `QUEUE_RESET` or recreates it. Other queues, other VMs and other jails keep running.
- **Device reset only when queue reset fails.** If the firmware cannot remove the queue, the kernel resets the device. Every queue on the device has its outstanding points errored with `ERR_RESET`, the device's *lost generation* counter increments, and VRAM-resident BOs whose contents did not survive are marked lost (`BO_QUERY` reports it). Every client sees the new generation in `QUERY_CAPS` and maps it to `VK_ERROR_DEVICE_LOST`; contexts that were not running can continue after re-uploading lost BOs.
- **Who may reset.** `QUEUE_RESET` on one's own `queue_fd`; `DEVICE_RESET` only on `control_fd` (§5.10). No unprivileged ioctl clears another tenant's in-flight work.
- **Fault info.** The faulting context can read a fault record (faulting VA, engine, type) via `QUEUE_QUERY_FAULT`; other contexts see only the error code. A devcoredump-style capture is control-only.
- **Invariant.** Every signal point ever accepted is eventually signalled or errored. No waiter can be left behind by a fault, a reset, a queue close or a driver detach.


### 10.1 Escalation ladder (firmware and engine hangs)

The kernel never waits on firmware without a bound. Every firmware message (MES on AMD) has a timeout, and a timeout moves recovery one rung down. Each rung is tried once per incident.

| Rung | Action | Trigger | ABI effect |
|---|---|---|---|
| 0 | Remove and reset the one queue | watchdog, fault interrupt | That queue's points error (`ERR_HANG`/`ERR_FAULT`); nothing else changes |
| 1 | Reset the engine and reload its scheduling firmware | firmware does not answer `REMOVE_QUEUE` within 100 ms, or rung 0 fails | Device lost: every queue errors with `ERR_RESET`, lost generation increments |
| 2 | Function-level PCI reset (or the vendor's bus-level reset), full re-initialisation and firmware load | rung 1 fails or times out | As rung 1; VRAM contents assumed lost |
| 3 | Detach | rung 2 fails | Every fd on the device returns `ENXIO`; every outstanding point errors with `ERR_DEVICE_GONE`; the cdev is destroyed. The kernel stays up |

**Reset budget.** If the device needs rung 1 or worse more than a set number of times in a window (default 3 in 10 minutes, settable by the broker with `SCHED`), the kernel stops recovering and goes to rung 3. A device that cannot stay up is worse than no device.

**Falling back to the CPU.** When the broker sees rung 3 (or decides on its own that the GPU is unreliable), it switches rendering to the Tier-2 CPU path and keeps the desktop running. This is the case a Linux driver cannot take and Atrium can: the system does not need the GPU to work.

Nothing in the ladder holds a lock that a waiter or the fence-retire path needs, so `SYNCOBJ_WAIT` and kqueue keep reporting progress, errors included, throughout.

### 10.2 Broker crash

In production every client context belongs to the broker (§4.1), so a broker crash closes them all. The kernel handles that as an ordinary close of each context: queues are removed, outstanding points error, memory is released at its retire point (§5.11). Apps recover through Fresco client replay (`fresco-recovery.md` §3). Two things are added so the restart is fast and invisible:

- **The display holds the last frame.** When the display lease holder's fd closes, the display module keeps its own reference to the buffers last latched on each plane and keeps scanning them out. It drops them on the next commit from a new lease holder. Vblank continues; no modeset happens during the gap.
- **Sealed shared buffers can outlive the broker.** Sealed BOs (§5.13) are immutable, so they are safe to hand to anyone who already had them. The broker deposits their `share_fd`s with its Portcullis supervisor (the supervisor's fd store), and the restarted broker takes them back and re-imports them. Fonts, glyph atlases and shader binaries are then available without re-upload; anything not deposited is reloaded from Tessera by hash. `control_fd` is never deposited: the new broker opens the node again.

The kernel keeps no other broker state across the crash. Scene, window and slot state is the client-replay contract's job, and WM state is the WM checkpoint (`fresco-recovery.md` §4).

### 10.3 Reset while the display is flipping

Rendering and display are separate blocks, and recovery treats them separately:

- **Rung 0 and rung 1 do not touch the display block.** Scanout continues from whatever is latched.
- **A commit whose in-fence errors is dropped.** The previous buffer stays on screen, and the commit's out-fence errors with the same code, so the broker knows the frame never showed.
- **Rung 2 resets the display too.** Afterwards the display module re-applies its last committed mode and planes from its own copy. Planes whose buffers lived in VRAM are marked lost and show the boot framebuffer contents (the `atrium_bootfb` splash) until the broker commits a new frame.
- **The display fd reports it.** A `ATRIUM_DISPLAY_NOTE_RESET` kqueue event (with the device's new lost generation) tells the lease holder to re-render and commit. Registering `EVFILT_READ` with `fflags = ATRIUM_DISPLAY_NOTE_RESET` gives a reset-only event (`data` = the reset generation; vblanks never wake it); a plain vblank registration sees the flag in the returned `fflags`. `STATUS_V2` carries `ATRIUM_DISPLAY_STATUS_CONTENT_LOST` from the reset until a commit made after it reaches the screen. *Implemented 2026-11-05 in atrium-gpu-amd (the generation counts device FLRs).* frescod re-presents at once, and the gpu renderer falls back to Tier-2 until no reset has come for 5 s. Tier-2 still presents through the GPU's copy engine into a VRAM scanout buffer, so a device reset affects it too, but it only re-copies its CPU frame.
- **Memory topology decides what is lost.** Whether a reset loses the on-screen content is the vendor module's call, since it knows where scanout lives: a discrete card with VRAM scanout (today's case) raises `CONTENT_LOST`; a UMA device scanning out of system memory need not; on a hybrid (Optimus-style) laptop only the display GPU's own resets touch the screen. The broker's reaction is the same in every case. Those platforms get their handling when we support them (Shankar, 2026-11-05).
- **Rung 3** leaves the display on the firmware or boot framebuffer if the platform has one; otherwise the screen goes dark and the broker reports the failure through Tier-2 on whatever output remains.


### 10.4 Suspend and resume

Suspend is handled as a planned rung-2 reset, with notice:

- On `device_suspend` the vendor module stops accepting submits (they block until resume, they do not fail), waits a bounded time (default 1 s) for in-flight work, then preempts or removes every queue. Work that did not finish is errored with `ERR_RESET`.
- VRAM contents are lost on suspend unless the vendor module saves them. 2.1 requires saving only kernel-owned state (page tables, rings, firmware state needed to resume) and pinned scanout buffers. Other VRAM BOs are marked `LOST` (`BO_QUERY`); system-heap BOs survive.
- On resume the module re-initialises the device and firmware, restores page tables and queues, increments the lost generation only if any BO was lost, and lets blocked submits proceed. Clients that see the generation change re-upload lost BOs; sealed shared BOs are re-uploaded by the broker from Tessera (§5.13).
- The display module re-applies its last committed state on resume (as after rung 2, §10.3), so the desktop reappears without a modeset from the broker.

---

## 10.5 ioctl-surface hardening (added 2026-06-10; attestation gate)

In production this ABI is reachable only by the GPU broker (§4.1);
in test and dev jails it is reachable by whatever holds a direct
grant. Either way the ioctl boundary is, alongside the Tessera VFS,
the platform's primary kernel attack surface, and a compromised
broker must not be able to escalate through it. Two adversarial inputs deserve explicit
fuzzing, and passing them is part of the isolation attestation
(gpu-isolation.md), not optional hardening:

- **The ioctl structs themselves.** Every ioctl takes a
  `struct_size`-prefixed struct with embedded user pointers,
  counts, and fds (`atrium_gpu_submit` has `blob_va`/`blob_size`,
  `in_dep_count`/`out_dep_count`, fd arrays). A syscall-level
  fuzzer drives each ioctl with malformed sizes, overflowing
  counts, hostile pointers, bad/foreign fds, and torn concurrent
  calls. Invariant: structured error (`EINVAL`/`EFAULT`/`ENOTTY`),
  never a panic, OOB access, integer-overflow allocation, or fd
  confusion. Run under KASAN in the in-kernel test VM.
- **The submit blob.** Although the blob is "opaque to the
  kernel," the kernel's *basic structure validation* (§5.7) and
  the vendor module's pre-dispatch checks are parsing
  attacker-controlled bytes. The blob validator gets its own
  fuzz target per vendor module; a malformed blob must be rejected
  before dispatch or contained by VM/IOMMU, never escape the
  submitting context (this is the I3/I4 invariant of
  gpu-isolation.md, exercised adversarially rather than only by
  the hand-written `test_gpu_isolation` cases).

**Gate:** a kernel driver cannot reach `status = "production"` in
the jaild attestation (gpu-isolation.md "Attestation freshness")
until both fuzz targets have run 24 h with zero new crashes at the
attested commit. A new crash blocks the attestation, which fails
the `gpu` capability closed. Regression corpus is permanent: every
past crash stays a seed.

---

## 11. Userspace consumers

*Rewritten in 2.1.* The v2 text planned a Mesa/radv port first. Atrium's actual userspace is its own:

- **`atrium-gpu-rs`** — the Rust binding of this ABI (today its `amd` module speaks the intermediate `'A'`/`'D'` surface). It is regenerated from `include/atrium/gpu_abi_v2.h` and is the only library that issues these ioctls. Effort: small, mechanical.
- **frescod (the broker)** — holds `control_fd`, mints client contexts, owns the display lease, and renders the desktop. Today it renders with lavapipe and copies into a scanout BO; the decided direction is the Tier-2 CPU path, with this ABI used for scanout, sharing and, once a GPU backend exists, Tier-3 rendering.
- **`atrium-vk-icd` / `aqueduct-gpu-host`** — the Vulkan front end. A Tier-3 backend that targets this ABI directly (building vendor command blobs, binding with `VM_BIND`, submitting on queues) is the native GPU path. No such backend exists yet.
- **Mesa (optional).** A `winsys/atrium_amdgpu` port of radv onto `atrium-gpu-rs`/a C shim remains possible, and the ABI keeps Mesa's winsys contract in mind (principle 1, POD blobs). It is a fallback if writing our own RDNA4 command generation proves too costly, not the plan. Rough size if taken: 3–6 engineer-months per vendor plus WSI.

The first real-GPU Vulkan driver is our own (§13p, `atrium-rdna4-vulkan-backend.md`); Mesa is the fallback.

---

## 12. Out of scope for this document

- **Non-Vulkan APIs.** OpenGL via gallium works because Mesa internally maps it onto `winsys`; we don't design for it. OpenCL same.
- **Power management interface.** Each vendor module handles its own (PCI runtime PM, ASC firmware, etc.). The ABI does not expose explicit "device suspend" ioctls; the existing FreeBSD `device_suspend(9)` framework handles this at the cdev layer.
- **Hot-add/hot-remove.** The cdev disappears on device removal. fds opened against it return errors on subsequent ops. No special protocol.
- **Display ABI** — see separate document.
- **Capability protocols** (HDCP, content protection). Not a v2 concern.
- **Virtual GPU partitioning** (SR-IOV, MIG) and passthrough. Not planned; see §6.3 for the virtualization plan and how a partition would be selected if ever needed.
- **Safety certification.** Not a goal; §6.4 lists the rules that keep the architecture capable of it.

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


**Resolved in 2.1:**

- **(d)** The ioctl group is `'A'` (reconciliation decision; `atrium-gpu-amd` already uses it).
- **(f)** There is no `event_fd`. kqueue registrations are per process, so a syncobj fd received over `SCM_RIGHTS` is registered fresh by the receiver (§5.6).
- **(h)** No eviction in V1. Allocation beyond a jail's quota fails with `EDQUOT` (§4.2). Transparent migration with fence ordering stays the direction if eviction is added.
- **(k)** One user-mode queue per doorbell page, mapped from its `queue_fd`, revoked by pager invalidation on reset or close, hang detected from write pointer versus fence, frees never gated on user-writable fences (§5.9, §5.11).

**New in 2.1:**

m. **Firmware licensing.** Real RDNA hardware needs AMD's signed binary firmware (MES, PSP, SMU, CP microcode). `LICENSING-POLICY.md` must say whether Atrium images may ship redistributable binary firmware, or whether the user fetches it at install. This blocks any real-silicon release, not the ABI.

n. **VMID pressure.** Hardware has 15–16 VMIDs; 2.1 decouples VMs from VMIDs (§5.2) and relies on the firmware to assign them on demand. The fallback for hardware or firmware that does not do this (kernel-side VMID stealing with a TLB flush) needs a design if such a target appears.


o. **Render-target compression.** Off in 2.1 (§5.3). Revisit once a real-silicon backend can measure the bandwidth and energy gain for frescod and 3D workloads. Defining it needs: clearing compression state on allocation (I5), a validated `VM_BIND` flag where compression is a page-table bit (RDNA4), the layout carried as a modifier on `share_fd`, display validation of scanout-capable layouts, refusal of CPU mappings of compressed BOs, and a policy against compressing surfaces whose compression ratio would leak another app's or origin's pixels (the "GPU.zip" side channel).

p. **First real-GPU Vulkan driver.** *Resolved 2026-11-03:* our own RDNA4 backend, no Mesa, as a Tier-3 `Backend` in `aqueduct-gpu-host` with a compiler on the atrium-spv IR. Design: [`atrium-rdna4-vulkan-backend.md`](atrium-rdna4-vulkan-backend.md). A radv winsys port stays the fallback.
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
| Master concept | DRM-Master | n/a | `control_fd` held by the broker; clients get minted `device_fd`s (§4.1) |
| Versioning | per-driver `DRM_VERSION` | `VkApplicationInfo.apiVersion` | `struct_size` + cap TLV |
| Event delivery | DRM event queue + poll | callback / fence wait | kqueue on the `syncobj_fd` itself |

Atrium's choices are mostly Vulkan's (the API we're serving) translated into BSD primitives (fds, kqueue). DRM was an intermediate layer designed for X11-era assumptions; we collapse it.

---

## 15. Cross-BSD portability notes

*(unchanged in 2.1)*

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

*(unchanged in 2.1)*

This design is heavily indebted to:

- **Intel's Xe RFC** and the team's deliberate decisions about what to drop from i915. Many of v2's choices (VM_BIND mandatory, explicit-sync only, firmware scheduling) match Xe's.
- **AMD's MES user-mode-queue work** (mainlined in Linux 6.16 for GFX11/GFX12 — the hardware class Atrium targets first). §5.9 adopts its firmware-scheduled direct-doorbell submission mechanism and reframes it as a Capsicum/Portcullis capability rather than a permission retrofit.
- **The Asahi Linux project**, particularly Alyssa Rosenzweig and Asahi Lina, for the v2 UAPI's POD-blob submit model and the demonstration that a clean GPU driver can fit in ~30K LoC of MIT-licensed code.
- **DRM maintainers** whose 20-year evolution produced the lessons we are learning from. Specific concepts we are inheriting under different names: `dma-buf` (→ `share_fd`), `drm_syncobj` timeline (→ `syncobj_fd`), VM_BIND (→ `ATRIUM_GPU_VM_BIND`), atomic modesetting (→ display-cdev v2), DMA-BUF feedback (→ `share_fd` modifier negotiation).
- The **Mesa community**, whose `winsys` abstraction makes a port like this possible without rewriting Vulkan backends. We are explicitly designing to fit within the existing `winsys` contract rather than asking Mesa to change.
- **FreeBSD's GPU contributors** who have maintained drm-kmod for a decade. Their work proves out the constraint we are responding to: shimming Linux is possible, but it is not where we want to be.

---

## 17. What happens next

*Rewritten in 2.1.* Order of work, from the review (`../reviews/2026-11-03-gpu-kernel-and-tier2-review.md` §A8):

1. **Header and binding.** Land `include/atrium/gpu_abi_v2.h`; regenerate `atrium-gpu-rs` against it.
2. **Close the isolation breaks in `atrium-gpu-amd`** (§18 rows 1–5): per-queue state, deferred free, per-queue revocable doorbells, scanout by fd, control/client split. Each lands with its §5.12 test.
3. **Fault model.** Syncobj error state, interruptible waits, watchdog and the §10.1 ladder against gpusim's fault injection.
4. **Conformance and attestation.** `tests/gpu/` (one test per §5.12 row), `test_gpu_isolation`, and the two §10.5 fuzz targets at 24 h.
5. **Real silicon.** Replace gpusim-only register paths (firmware load, MES, VM contexts, display) with RDNA4 programming. Blocked on firmware licensing (§13m).
6. **First real-GPU Vulkan path** (§11, §13p), then frescod Tier-3.

External review (FreeBSD `freebsd-arch@`, other BSDs, Mesa winsys authors) is worth doing after step 4, when there is a conformance suite to point at.

---

## 18. Implementation status (updated 2026-11-13, 2.3)

`atrium-gpu-amd` on gpusim and on the R9700 (stages 3 to 5f, `r9700-bare-metal.md` §5). The 2026-11-03 gap list ([`../reviews/2026-11-03-gpu-kernel-and-tier2-review.md`](../reviews/2026-11-03-gpu-kernel-and-tier2-review.md) §A) is closed except where noted; the mechanisms are in `atrium-gpu-amd/ISOLATION.md`, the security picture in [`gpu-security-model.md`](gpu-security-model.md).

| Requirement | `atrium-gpu-amd` | Section |
|---|---|---|
| Queue objects as fds, per-queue fences, kqueue completion | done: `QUEUE_CREATE` on a VM fd, `QUEUE_SUBMIT` on the queue fd; MES user queues on silicon, queue fds on gpusim; the v1 ring `SUBMIT` is retired | §5.5, §5.7 |
| Deferred free / free rule | done: BO frees wait for queue drain (gpusim) or one MES preemption of the VM's queues (silicon), never a client-writable fence | §5.11 |
| Revocable per-queue doorbells, user-mode queues | done; `USER_MODE` host-root-only (§5.9, 2.3) | §5.9 |
| Scanout by BO fd, display lease | done | §7 |
| Authority split | node-based profile (§4.1); **open:** per-call `PRIV_DRIVER` check on the control node (today a passed control fd keeps its authority). Minted profile (`CLIENT_OPEN`, `SET_QUOTA`, `QUERY_USAGE`) not implemented | §4.1 |
| Accounting | per-uid VRAM and System caps (`EDQUOT`); **open:** cap on live queues per uid; VA, VM, syncobj and in-flight caps | §4.2 |
| Validated `VM_BIND`, `vmem` VA arena, read-only binds | done (ABI rev 3) | §5.4 |
| GPU W^X (`BIND_EXEC`) | **open:** client binds on silicon are always executable | §5.4 |
| Syncobj error state, interruptible waits | done | §5.6 |
| Watchdog, per-queue loss on fault or hang, reset | done; last rung mode 1 reset with bus mastering off | §10 |
| VRAM allocator with scrub-before-reuse | done (gpusim and silicon) | §5.3 |
| CPU mapping of BOs (`BO_MMAP_INFO`) | not implemented: BO contents move by `BO_READ`/`BO_WRITE` copies; queue-fd mappings (ring, writeback, doorbell) only | §5.3 |
| Sealed BOs | not implemented | §5.13 |
| Cross-device `share_fd`, telemetry ioctl, perf counters | not implemented | §5.8, §5.10, §5.15 |
| Isolation test, fuzz gate | `test_gpu_isolation` and `atrium_gpu_test` pass on gpusim on queue fds (5f); not yet on silicon; the 24 h fuzz gate has not run at an attested commit, so the jaild stanza stays `experimental` | §10.5 |

---

## 19. ioctl table (new in 2.1)

Group `'A'`, numbers from `include/atrium/gpu_abi_v2.h`. "Control" calls fail with `EPERM` on a client `device_fd`. "Holder" means anyone holding the fd. Numbers 0–27 belong to the intermediate `atrium-gpu-amd` ABI and are retired when it converges.

| # | Call | Issued on | Authority | Spec |
|---|---|---|---|---|
| 64 | `QUERY_CAPS` | `device_fd` or `control_fd` | Client | §5.1 |
| 65 | `CLIENT_OPEN` | `control_fd` | Control | §4.1 |
| 66 | `SET_QUOTA` | `control_fd` | Control | §4.2 |
| 67 | `QUERY_USAGE` | `device_fd` or `control_fd` | Client (own jail) / Control | §4.2 |
| 68 | `VM_CREATE` | `device_fd` | Client | §5.2 |
| 69 | `VM_BIND` | `vm_fd` | Client | §5.4 |
| 70 | `BO_CREATE` | `device_fd` | Client | §5.3 |
| 71 | `BO_MMAP_INFO` | `bo_fd` | Client | §5.3 |
| 72 | `BO_QUERY` | `bo_fd` | Client | §5.14 |
| 73 | `BO_WRITE` | `bo_fd` | Client (bring-up, tests) | §5.3 |
| 74 | `BO_READ` | `bo_fd` | Client (bring-up, tests) | §5.3 |
| 75 | `BO_SEAL` | `bo_fd` | Client | §5.13 |
| 76 | `BO_EXPORT_SHARE` | `bo_fd` | Client | §5.8 |
| 77 | `BO_IMPORT_SHARE` | `device_fd` | Client | §5.8 |
| 78 | *reserved: `BO_IMPORT_MEMORY`* | `device_fd` | Client | §6.3 |
| 79 | `QUEUE_CREATE` | `vm_fd` | Client | §5.5 |
| 80 | `QUEUE_RESET` | `queue_fd` | Client (own queue) | §5.14, §10 |
| 81 | `QUEUE_QUERY_FAULT` | `queue_fd` | Client (own queue) | §5.14 |
| 82 | `QUEUE_MAP` | `queue_fd` | Client with ALLOW_USER_QUEUES | §5.9 |
| 83 | `SUBMIT` | `queue_fd` | Client | §5.7 |
| 84 | `SYNCOBJ_CREATE` | `device_fd` | Client | §5.6 |
| 85 | `SYNCOBJ_SIGNAL` | `syncobj_fd` | Holder | §5.6 |
| 86 | `SYNCOBJ_WAIT` | `syncobj_fd` | Holder | §5.6 |
| 87 | `SYNCOBJ_QUERY` | `syncobj_fd` | Holder | §5.6 |
| 88 | `QUERY_TELEMETRY` | `device_fd` or `control_fd` | Client (own) / Control (device) | §5.10 |
| 89 | `CLOCK_CALIBRATE` | `device_fd` | Client | §5.15 |
| 90 | `PERF_OPEN` | `device_fd` or `control_fd` | Client with ALLOW_PERF / Control | §5.15 |
| 91 | `SCHED` | `control_fd` | Control | §5.14 |
| 92 | `POWERGATE` | `control_fd` | Control | §5.14 |
| 93 | `DEVICE_RESET` | `control_fd` | Control | §5.14, §10.1 |

---

*This document is dual-licensed under MIT or Apache-2.0, matching the rest of Atrium. Discussion welcome at the Atrium project repository.*
