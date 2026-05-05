# DRM / GPU-Stack Research Findings

Research artifact for Atrium GPU-stack design. Reports what each layer of the
Linux DRM ecosystem does, why it exists, and what modern vendor drivers
(amdgpu / Xe / Asahi / NVIDIA open) chose to keep, drop, or replace. No design
recommendations — only source-grounded findings.

Sources are cited inline by URL on first use; a consolidated source list lives
at the end of each Part.

---

## Part 1: DRM kernel subsystems — what each layer solves

### GEM (Graphics Execution Manager)

GEM is a per-driver buffer-object framework with reference counting,
shmem-backed pageable storage, fake-offset mmap, and dma-buf integration. It
emerged in 2008 inside Intel's i915 driver as a deliberate counter-proposal to
TTM, on the position that TTM "attempted a one-size-fits-them all solution"
that became "a large, complex piece of code that turned out to be hard to
use" (https://docs.kernel.org/gpu/drm-mm.html). Rather than solving every
problem, GEM "identified common code between drivers and created a support
library to share it" (ibid.).

What it solves: lifetime, naming, mmap, and CPU/GPU coherence for buffer
objects on UMA systems where there is no separate VRAM pool to manage. What
came before: per-driver ad-hoc allocators; the older DRI1 model exposed raw
hardware to X. Limitations: GEM "has no video RAM management capabilities and
is thus limited to UMA devices" (ibid.) — discrete GPUs still need TTM
underneath, and most modern drivers (radeon, amdgpu, nouveau, xe) expose GEM
handles to userspace while delegating placement and eviction to TTM.

If you remove GEM, you lose: handle-based BO references, cross-process
sharing via PRIME (PRIME flips a GEM handle to a dma-buf fd and back), and
the standardized `mmap` offset trick that lets a userspace `mmap()` an
arbitrary BO via a single `/dev/dri/cardN` fd.

Sources: https://docs.kernel.org/gpu/drm-mm.html ;
https://en.wikipedia.org/wiki/Direct_Rendering_Manager

### TTM (Translation Table Manager)

TTM "manages memory for accelerator devices with dedicated memory," handling
"lifetime, movement and CPU mappings" of buffer objects across heterogeneous
memory domains (system RAM, VRAM, GTT/IOMMU windows)
(https://docs.kernel.org/gpu/drm-mm.html). It implements resource managers,
LRU lists, pipelined eviction (where a fence on the destination defers the
copy), bus snooping and caching attributes, and per-BO placement constraints.

Why it exists alongside GEM: TTM predates GEM. Tungsten Graphics and Intel
co-developed TTM, but Intel's integrated-graphics team (Packard, Anholt)
rejected it as too heavy for UMA hardware and shipped GEM in i915 instead
(https://en.wikipedia.org/wiki/Direct_Rendering_Manager). The compromise that
followed was: drivers for discrete GPUs (radeon, then amdgpu, nouveau, xe)
internally use TTM for placement but expose GEM ioctls outwardly for
uniformity. TTM is therefore the layer that handles the *real* hard problem
of multi-pool memory: deciding where a buffer lives, when to evict it, how to
serialize copies behind GPU-visible fences, and how to make CPU mappings
coherent.

If you remove TTM you must reinvent: LRU eviction, pipelined fenced copies,
domain-aware allocation policy, and the locking discipline that keeps
eviction deadlock-free (the dma-fence rules in Part 1.6).

### KMS (Kernel Mode Setting) and atomic modesetting

KMS centralizes display-pipeline configuration in the kernel. Pre-KMS, the X
server poked display registers from userspace, which "created instability and
security vulnerabilities" and produced flicker on VT-switch
(https://en.wikipedia.org/wiki/Direct_Rendering_Manager;
https://docs.kernel.org/gpu/drm-kms.html). KMS exposes a hierarchy:
**framebuffer → plane → CRTC → encoder → connector**, with encoders kept
visible "for backward compatibility, though they unnecessarily complicate the
API" (drm-kms.html).

Atomic modesetting layers transactional commit semantics on top: a userspace
client builds a `drm_atomic_state` containing every property change, the
kernel validates the entire set (including with `DRM_MODE_ATOMIC_TEST_ONLY`),
and only then commits. The two design invariants are all-or-nothing
validation and state-snapshot transactions, which let compositors avoid
intermediate invalid states across multi-display configurations
(drm-kms.html; https://en.wikipedia.org/wiki/Direct_Rendering_Manager).

If you remove KMS: every display server re-implements mode setting against
raw registers, VT-switch goes back to flickering, hot-plug becomes ad-hoc,
and there is no kernel-mediated arbitration when multiple processes want the
display.

Source: https://docs.kernel.org/gpu/drm-kms.html

### dma-buf and PRIME (cross-driver buffer sharing)

dma-buf is "the framework for sharing buffers for hardware (DMA) access
across multiple device drivers and subsystems"
(https://docs.kernel.org/driver-api/dma-buf.html). It exposes a buffer as a
file descriptor; an exporter creates and owns backing storage, importers
attach their device, request a per-device sg-table mapping, and the exporter
can move backing storage between domains transparently. PRIME is the GEM
convention layered on dma-buf: "GEM handle ↔ dma-buf fd" conversion ioctls
let a userspace process pass a buffer between two DRM devices (e.g. discrete
GPU rendering, integrated GPU scanout) using only an fd
(https://en.wikipedia.org/wiki/Direct_Rendering_Manager).

The hard problem it solves is not just "share a pointer." It is: (a) two
drivers from different subsystems with different IOMMU configurations need a
common contract; (b) the buffer's backing storage may need to migrate
(VRAM→system) on attach; (c) implicit-sync producers and explicit-sync
consumers must interoperate. dma-buf solves these via attachment/mapping and
via dma_resv reservation objects that carry fences alongside the buffer
(see Part 1.5).

Limitations: dma-buf attachments are coarse (whole-buffer, no sub-range), and
implicit-sync semantics encoded in dma_resv are a constant source of
deadlock-discipline bugs (the "dma-fence rules", below).

Source: https://docs.kernel.org/driver-api/dma-buf.html

### sync_file / drm_syncobj / timeline syncobj — the sync evolution

Three generations live in the tree simultaneously.

1. **dma-fence** is the underlying primitive: a one-shot, signal-once, no-op-
on-allocation-after-publish object that "represent[s] a mechanism to signal
when an asynchronous hardware operation has completed"
(https://docs.kernel.org/driver-api/dma-buf.html). Its rules are draconian:
"for normal operation no memory can be allocated in a callback" and "no
locks can be taken under which memory might be allocated"
(https://lwn.net/Articles/951811/). These rules exist because dma-fences sit
in the eviction/reclaim path; allocating memory while holding one risks
livelock under memory pressure.

2. **sync_file** wraps a single dma-fence as a file descriptor for userspace
hand-off. Used by Android, X11/Wayland presentation, and explicit-sync
clients. Limitation: a sync_file is bound to one fence at creation time and
cannot be re-armed.

3. **drm_syncobj** wraps a fence-pointer that *can be updated* — userspace
can replace the underlying fence, and timeline syncobjs (Vulkan-1.2-style)
hold a monotonically increasing 64-bit counter where each value materializes
its own fence on demand (https://docs.kernel.org/driver-api/dma-buf.html;
https://lists.freedesktop.org/archives/mesa-dev/2020-March/224228.html).
This matches Vulkan timeline-semaphore semantics directly.

The 2024 final piece was kernel ioctls (added in 5.20 / 6.0) to convert
between sync_file fences and the dma_resv on a dma-buf, letting an
explicit-sync Vulkan client present to an implicit-sync compositor without
either side rewriting:
> "As far as the compositor is concerned, we look just like an OpenGL driver
> using implicit synchronization" (Collabora,
> https://www.collabora.com/news-and-blog/blog/2022/06/09/bridging-the-synchronization-gap-on-linux/).

If you remove syncobj: Vulkan timeline semaphores cannot be honored by the
kernel; you must either fake them in userspace or build your own primitive.

### DRM scheduler (drm_sched)

drm_sched is a software arbitration layer between userspace submissions and
hardware/firmware queues. Its model: userspace pushes jobs into per-process
**entities**, entities live in **run queues** with priorities, and a
scheduler instance pulls jobs FIFO/round-robin onto a hardware ring
(https://lwn.net/Articles/951811/;
https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/scheduler/sched_main.c).

It exists because (a) hardware rings are typically a fixed small number per
engine and userspace contexts are unbounded; (b) the kernel needs a single
chokepoint to enforce dma-fence ordering, timeout/hang detection, and TDR
(timeout-driven reset) policy; and (c) "it is also used by drivers which
don't need the actual scheduling functionality" — i.e., it is the standard
place to attach the dma-fence-rule discipline and the GPU-reset state
machine (LWN 951811).

When it is needed vs hardware/firmware scheduling: amdgpu's MES and Intel's
GuC firmware schedulers do the actual context-switch / preemption decisions
in firmware. But Xe still uses drm_sched — repositioned as "a dependency /
inflight job tracker rather than a true scheduler" — and added a one-to-one
scheduler-per-entity policy (`DRM_SCHED_POLICY_SINGLE_ENTITY`) precisely
because firmware completes jobs out of order, breaking drm_sched's original
"submission order = completion order" assumption
(https://lwn.net/Articles/928310/). Asahi and Mali (Panthor) made the same
choice.

Limitations: the dma-fence rules force every job-completion path to be
allocation-free; long-running compute workloads (where a job may run for
seconds) violate the dma-fence-progress assumption and require a separate
"long-running fence" annotation (LWN 928310).

### drm_mm / drm_buddy

These are intra-driver address-space allocators, not BO managers. `drm_mm`
is a generic range allocator used to suballocate from GTT or GPU VA spaces;
`drm_buddy` is a buddy allocator more suited to large VRAM pools where
fragmentation costs matter. The drm-mm.html document does not give them a
dedicated rationale section
(https://docs.kernel.org/gpu/drm-mm.html — confirmed; the doc only mentions
GPUVA fields on GEM objects). They exist because every GPU driver needs
*some* allocator for GPU virtual address space and VRAM, and pre-2010 each
driver wrote its own; consolidating into drm_mm/drm_buddy was a code-share
move, not a design breakthrough.

GPUVA / GPUVM is a newer (2023+) helper that tracks buffer-to-VA mappings
for VM_BIND-style drivers; it is the layer Xe, Nouveau, Asahi, and Panthor
share (https://docs.kernel.org/gpu/drm-vm-bind-async.html;
https://patchwork.kernel.org/project/linux-arm-kernel/patch/20250313-agx-uapi-v2-1-59cc53a59ea3@rosenzweig.io/).

### DRM uAPI versioning

DRM enforces strict uAPI stability with a notable rider: "The Linux kernel's
'no regression' policy holds in practice only for open-source userspace of
the DRM subsystem" (https://docs.kernel.org/gpu/drm-uapi.html). Every new
ioctl requires a corresponding open-source userspace implementation reviewed
upstream, and **kernel patches must merge before the userspace patches** so
that headers do not diverge.

Versioning conventions: driver-private ioctls use the `DRM_IOCTL_` prefix
within the `DRM_COMMAND_BASE..DRM_COMMAND_END` range, are described by
`drm_ioctl_desc` arrays carrying access-flag bits (`DRM_AUTH`,
`DRM_RENDER_ALLOW`, `DRM_MASTER`, `DRM_ROOT_ONLY`), and support
zero-extension of trailing struct fields for forward compatibility
(drm-uapi.html). Two device nodes split the surface: **primary**
(`/dev/dri/cardN`, requires DRM-Master, can do KMS) and **render**
(`/dev/dri/renderDN`, unprivileged, no modesetting, render+PRIME only)
(drm-uapi.html).

Source: https://docs.kernel.org/gpu/drm-uapi.html

---

## Part 2: Modern vendor driver lessons

### What amdgpu / Xe / Asahi kept from DRM unambiguously

All three keep: GEM (handle ↔ fd via PRIME), dma-buf for cross-device
sharing, dma-fence as the universal completion primitive, drm_syncobj
(timeline form for Vulkan 1.2), KMS+atomic for any display-capable instance,
and drm_sched as at minimum a dependency-tracker / TDR-state-machine even
when firmware does real scheduling
(https://www.kernel.org/doc/html/v6.8/gpu/rfc/xe.html;
https://asahilinux.org/2022/11/tales-of-the-m1-gpu/;
https://lwn.net/Articles/928310/).

Xe additionally keeps TTM ("for better code sharing across the DRM
subsystem, particularly with components like TTM and drm/scheduler",
xe.html). Asahi reuses the DRM scheduler subsystem and "approximately 1,500
lines of safe Rust abstractions wrapping C interfaces" rather than
sidestepping the framework
(https://asahilinux.org/2022/11/tales-of-the-m1-gpu/).

### What Xe deliberately dropped or changed vs i915

Xe is a from-scratch i915 successor created as "a fresh base to work from
that is unencumbered by older platforms"
(https://www.kernel.org/doc/html/v6.8/gpu/rfc/xe.html). Specific changes:

- **VM_BIND replaces per-submit buffer lists.** The old i915 (and DRM
  classic) model had every submission carry a list of all BOs it touches,
  so the kernel could pin and fence them. VM_BIND decouples binding from
  submission: userspace explicitly maps BOs into a GPU VM and the bindings
  persist across submits (xe.html;
  https://docs.kernel.org/gpu/drm-vm-bind-async.html).
- **GuC firmware scheduling is mandatory**, not optional. i915 had
  hand-rolled execlist scheduling; Xe assumes GuC and pushes the
  scheduling complexity into firmware (xe.html;
  https://lwn.net/Articles/928310/).
- **Display code is shared with i915** rather than re-forked — explicit
  "maximum reuse" goal
  (https://www.phoronix.com/review/intel-xe-i915-linux-619).
- **Power management leverages PCI subsystem pm/runtime_pm + PCODE/GuC**
  rather than reinventing S/D/R-state plumbing (xe.html).
- **Explicit sync only** — no implicit-sync fast path; everything goes
  through syncobjs.
- **No legacy ioctls** — none of the pre-atomic, pre-render-node, or
  command-buffer-relocation surface that i915 still carries.

### What Asahi did differently from a typical DRM driver

Asahi targets the Apple M1/M2 GPU, which is structurally unlike a discrete
PCIe GPU:

- **Firmware-mediated everything.** "All communication with the GPU happens
  via the firmware, using data structures in shared memory"
  (https://asahilinux.org/2022/11/tales-of-the-m1-gpu/). The firmware
  ("ASC") runs RTKit on an ARM coprocessor and owns power, scheduling,
  preemption, fault recovery. The driver does not talk to the GPU directly.
- **GPU memory IS firmware memory.** Firmware uses the same page tables as
  the GPU MMU, so kernel + user + firmware live in interlinked address
  spaces with cross-pointers. This makes direct userspace access impossible
  — apps could corrupt each other's *firmware* state, not just their own
  rendering.
- **"Over 100 data structure types" with one init struct holding "almost
  1000 fields"** (tales-of-the-m1-gpu). The driver is mostly marshaling.
- **Rust kernel driver.** Chosen specifically to handle the concurrency and
  ownership burden — the project found two memory-safety bugs in the C
  drm_sched while writing the Rust bindings
  (https://asahilinux.org/2023/03/road-to-vulkan/).
- **VM_BIND + explicit sync only** in the v2 UAPI, modeled on Xe and
  Panthor
  (https://patchwork.kernel.org/project/linux-arm-kernel/patch/20250313-agx-uapi-v2-1-59cc53a59ea3@rosenzweig.io/).
- **Flattened submit format**: "a single contiguous blob of plain-old-data,
  no CPU pointers" — chosen explicitly to make virtgpu wire transport
  trivial (ibid.).

### Per-area design choices

| Concern             | amdgpu                                  | Xe                                   | Asahi                                |
|---------------------|------------------------------------------|---------------------------------------|---------------------------------------|
| Memory mgmt         | TTM + GEM + GPUVM + drm_buddy            | TTM + GEM + GPUVM + VM_BIND           | shmem+GEM + GPUVM + VM_BIND           |
| Sync                | drm_syncobj + dma-fence                  | drm_syncobj timeline only             | drm_syncobj timeline only             |
| Submission          | rings + drm_sched + MES firmware         | exec_queue + drm_sched + GuC firmware | submit-pipe to firmware via drm_sched |
| Scheduling          | hardware rings + MES (firmware)          | GuC (firmware)                        | RTKit/ASC (firmware)                  |
| Modeset             | DCN via DC + KMS atomic                  | shared with i915 (Display Core)       | KMS atomic                            |
| Fault/hang recovery | RAS, per-IP reset, devcoredump           | devcoredump, GuC-driven reset         | firmware fault recovery               |

### Hardware-specific concerns that any port must preserve

amdgpu: per-IP-block init/reset ordering (GMC, IH, SMU, PSP, GC, SDMA, DCN,
VCN); SMU/PSP firmware loading sequence; GFXOFF state transitions; RAS bad-
page tracking + EEPROM logging; multi-VMID GPUVM (VMID 0 = kernel)
(https://docs.kernel.org/gpu/amdgpu/index.html;
https://docs.kernel.org/gpu/amdgpu/driver-core.html).

Xe: GuC + Pcode firmware contracts; survivability mode for bad firmware;
multi-tile device topology; hardware workaround tables; multicast/replicated
register access; runtime PM via PCI subsystem
(https://docs.kernel.org/gpu/xe/index.html).

Asahi: ASC/RTKit boot and message passing; ~1000-field firmware init blob;
firmware version compatibility (each macOS version ships new firmware);
shared page tables; thermal/power coordination via separate Apple firmware
(tales-of-the-m1-gpu).

NVIDIA (open modules): GSP firmware loading and version pinning; the OS-
agnostic `nv-kernel.o_binary` blob is recompiled per-driver-release, kernel
glue is in `kernel-open/`
(https://github.com/NVIDIA/open-gpu-kernel-modules/blob/main/README.md).

Common across all: GPU page-table format and IOMMU interaction; PCIe BAR
resizing for VRAM access; ECC reporting; thermal throttling; firmware
update + integrity (for AMD/NVIDIA where firmware is signed).

---

## Part 3: Userspace contract — what Mesa requires from the kernel

### What libdrm provides

libdrm started in 1999 with the original DRI work for XFree86 and 3dfx, and
its source still lives under the Mesa umbrella for historical reasons
(https://en.wikipedia.org/wiki/Direct_Rendering_Manager). Functionally it is
"a wrapper that provides a function written in C for every ioctl of the DRM
API, as well as constants, structures and other helper elements" (ibid.;
https://manpages.debian.org/testing/libdrm-dev/drm.7.en.html). It exposes:
memory mapping, context management, DMA, vblank, fence management, output
management, plus per-vendor sub-libraries (`libdrm_amdgpu`, `libdrm_intel`,
`libdrm_radeon`, `libdrm_nouveau`).

For modern drivers (radv, anv, nvk, iris, RADV, etc.) `libdrm_<vendor>` is
where buffer-allocation, residency, and submission helpers live. RADV uses
`libdrm_amdgpu` to wrap `AMDGPU_*` ioctls (BO alloc, GEM op, CS submit,
syncobj, VM op). ANV similarly used `libdrm_intel` historically; on Xe the
intel-vendor library shrinks because more lives in the kernel.

### What kernel surface Mesa actually depends on

The minimum cross-vendor surface Mesa Vulkan WSI assumes:

1. **GEM-handle BO allocation + mmap** with an opaque handle space and a
   way to convert handle ↔ dma-buf fd (PRIME).
2. **dma-buf import semantics** on the device: an external fd resolves to a
   GPU-mappable BO with sane format/modifier negotiation.
3. **drm_syncobj (timeline form) ioctls**: create, signal, wait, transfer,
   import-from-sync_file, export-to-sync_file. This is the kernel side of
   `VkSemaphore` / `VkFence`
   (https://www.collabora.com/news-and-blog/blog/2022/06/09/bridging-the-synchronization-gap-on-linux/;
   https://9to5linux.com/mesa-24-1-linux-graphics-stack-released-with-vulkan-x11-wsi-explicit-sync-support).
4. **A submission ioctl** that takes a command stream (or ring index +
   offset) plus in/out syncobj arrays.
5. **Format/modifier query** (`AddFB2` on KMS side; `linux-dmabuf` protocol
   on Wayland side). Compositors negotiate formats including direct-scanout
   eligibility via DMA-BUF feedback
   (https://www.phoronix.com/news/DMA-BUF-FB-Mesa-VLK-Wayland;
   https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/4942).

### Could the kernel ABI be different shapes?

Searched sources do not directly answer "could it be non-ioctl?" Two
observations from what *is* there:

- The Asahi v2 UAPI deliberately structures submit payloads as "a single
  contiguous blob of plain-old-data, no CPU pointers" precisely to make
  them transportable over virtgpu — i.e., the actual contract Mesa needs
  is a self-contained byte string + a few syncobj fds, not an ioctl per se
  (Asahi UAPI v2 patchwork link, above). This implies Mesa can ride on any
  transport that gives it a byte-pipe + fd-passing.
- libdrm's role is purely an ioctl wrapper layer
  (https://en.wikipedia.org/wiki/Direct_Rendering_Manager). Replacing
  ioctls with another mechanism would mean reimplementing libdrm but not
  changing Mesa's higher logic, *provided* the semantics (handle space,
  fence semantics, VM_BIND ordering) are preserved.

What is harder to replace: the **fd-as-handle convention** is baked into
WSI (sync_file fds, dma-buf fds, KMS atomic event fds). Anything that does
not give Mesa file descriptors for these objects forces a translation layer
in WSI itself.

### Cross-vendor commonalities — architectural or convenience?

Architectural (cannot be removed without breaking Mesa):

- dma-buf import gives both GPUs a coherent view of the same backing pages
  (or transparently migrates them).
- dma-fence semantics are compatible across drivers — a fence signaled by
  GPU A can be waited on by GPU B.
- Format modifiers describe tiling/compression in a vendor-neutral
  enumeration so a compositor can ask "can I scan this out without a
  blit?" (DMA-BUF feedback).

Convenience (could be done differently):

- The exact `DRM_IOCTL_` numbering and `drm_ioctl_desc` plumbing.
- The split between primary and render nodes (an access-control
  convention, not a hardware necessity).
- libdrm's specific function names.

### Vulkan WSI and the kernel's role

Mesa's Vulkan WSI sits between `vkAcquireNextImage`/`vkQueuePresentKHR` and
the windowing protocol. Per Collabora's writeup, the historical pre-explicit-
sync semantics were "a lie" — `vkAcquireNextImage`'s semaphore was always
already signaled because the compositor used implicit dma_resv ordering
(https://www.collabora.com/news-and-blog/blog/2022/06/09/bridging-the-synchronization-gap-on-linux/).
The 2024 Mesa-24.1 + Wayland linux-drm-syncobj-v1 work replaced this with
true explicit sync end-to-end
(https://www.phoronix.com/news/Mesa-24.1-Vulkan-Wayland-Exp).

Kernel role in WSI:
- Provide dma-buf for swapchain images so the compositor can map them.
- Provide drm_syncobj timeline for acquire/release semaphore semantics.
- Provide KMS atomic + page-flip events for fullscreen / direct-scanout
  paths.
- Provide DMA-BUF feedback hints (which device + which modifiers) so the
  client can allocate scanout-eligible buffers
  (https://www.phoronix.com/news/DMA-BUF-FB-Mesa-VLK-Wayland).

The compositor does the windowing-protocol talk; the kernel does not know
about Wayland or X11.

---

## Part 4: NVIDIA's alternative model

NVIDIA's open kernel modules ship four kernel objects: `nvidia.ko`,
`nvidia-modeset.ko`, `nvidia-drm.ko`, `nvidia-uvm.ko`. The repo splits each
into a large pre-compiled "OS-agnostic" blob (`nv-kernel.o_binary`,
`nv-modeset-kernel.o_binary`) plus a thin `kernel-open/` shim that is
recompiled against the running kernel
(https://github.com/NVIDIA/open-gpu-kernel-modules/blob/main/README.md).

What replaces DRM: very little in the modules themselves — `nvidia-drm.ko`
*does* register with the DRM subsystem, but only as a thin adapter. The bulk
of GPU control (memory mgmt, command submission, contexts) lives inside the
proprietary userspace libraries shipping in the matching driver release; the
kernel-userspace ABI is private and matched per-version. The README
explicitly requires "GSP firmware and user-space NVIDIA GPU driver
components from a corresponding 595.71.05 driver release" (ibid.).

Costs of being outside the DRM ecosystem:
- No PRIME interop with other DRM drivers without bridge code.
- No standard render-node access control, so containers/sandboxing rules
  written for `/dev/dri/renderD*` need NVIDIA-specific equivalents.
- Distros must patch userspace stacks (Mesa zink, Wayland compositors) to
  speak NVIDIA's fence/buffer protocols.
- Mesa's nvk Vulkan driver and the open Nouveau/nova kernel driver are the
  community alternative that *do* fit the DRM model — implying NVIDIA's
  closed-ABI model is not viewed as the long-term direction even by NVIDIA-
  adjacent open-source efforts.

Whether viable for Atrium: this model assumes you control both the kernel
module *and* the userspace stack and can ship them as a matched pair. It
trades ecosystem compatibility (PRIME, syncobj sharing, container tooling)
for freedom from upstream constraints. Atrium's stated goal of consuming
upstream Mesa userspace runs counter to this model.

Source: https://github.com/NVIDIA/open-gpu-kernel-modules/blob/main/README.md

---

## Part 5: Synthesis questions

### a. Which DRM subsystems exist primarily because of cross-vendor uniformity?

Cross-vendor-driven (would shrink or vanish if each vendor exposed its own
ABI to its own userspace):

- **GEM as a uniform handle namespace** — vendors would happily use private
  handle types if Mesa did not expect a common `gem_handle_t`.
- **PRIME and the dma-buf ↔ GEM-handle ioctls** — needed because two
  different drivers must share buffers; a single-vendor stack can use any
  internal mechanism.
- **drm_syncobj** as a generic name — a single-vendor stack could expose
  vendor-private timeline objects with the same semantics.
- **KMS as a vendor-neutral display API** — single-vendor stacks can use
  vendor display ABIs.
- **The DRM ioctl numbering convention and `drm_ioctl_desc` plumbing**.
- **render-node vs primary-node split** — an access-control convention.

These exist because Linux historically had to support N vendors talking to
M userspaces. A stack with one vendor and one userspace-API consumer pays
the uniformity tax for nothing.

### b. Which DRM subsystems exist because of real underlying problems?

Hardware/algorithmic-driven (any GPU stack must solve these):

- **TTM-class memory management**: domain placement, LRU eviction,
  pipelined fenced copies, bus-snooping cache attributes. Discrete VRAM and
  multi-pool memory are physical realities.
- **dma-fence semantics**: GPU work is asynchronous; *something* must
  represent "this completed" without holding allocator locks. The exact
  shape (fd vs handle vs counter) is policy; the discipline is not.
- **Timeline (monotonic-counter) sync**: required by Vulkan 1.2 semantics;
  arose from real shortcomings of single-shot fences for compute.
- **VM_BIND-style separation of binding from submission**: the per-submit
  buffer-list model fundamentally cannot scale to bindless / large-context
  Vulkan workloads (xe.html).
- **GPU scheduler / TDR state machine**: hangs happen, must be recovered;
  jobs must be ordered against fences without violating dma-fence rules.
- **KMS atomic commit semantics**: hardware page-flips have all-or-nothing
  failure modes; multi-plane configs need transactional commit to avoid
  scanout corruption.

### c. Which modern primitives reduce or eliminate older DRM mechanisms?

- **Vulkan 1.2 timeline semaphores → drm_syncobj timeline**: makes most
  uses of bare sync_file obsolete; eliminates the "fence per submit"
  bookkeeping where one timeline carries an arbitrary number of points.
- **Hardware/firmware schedulers (AMD MES, Intel GuC, Apple ASC)**: shrink
  drm_sched to a dependency-tracker + TDR coordinator rather than a true
  scheduler (https://lwn.net/Articles/928310/).
- **VM_BIND**: eliminates per-submit BO lists, the relocation tables that
  libdrm carried for years, and the residency-tracking overhead of every
  ioctl (https://docs.kernel.org/gpu/drm-vm-bind-async.html).
- **Wayland linux-dmabuf + DMA-BUF feedback**: eliminates wl_drm and the
  earlier modifier-less negotiation; the compositor and client now agree
  on tiling/compression/scanout-eligibility upfront
  (https://www.phoronix.com/news/DMA-BUF-FB-Mesa-VLK-Wayland).
- **Explicit sync everywhere (Mesa 24.1 + Wayland drm-syncobj-v1)**:
  removes the implicit dma_resv-as-sync cargo cult; clients no longer rely
  on the "vkAcquireNextImage semaphore was a lie" trick
  (https://www.collabora.com/news-and-blog/blog/2022/06/09/bridging-the-synchronization-gap-on-linux/).
- **Render nodes**: eliminate the DRM-Master authentication dance for
  compute-only clients (drm-uapi.html).

Content-addressed memory is mentioned in the prompt but no source surfaced
addresses it in the GPU-driver context; the closest analog in DRM is
`dma_resv` (a per-buffer fence container), which is identity-keyed not
content-keyed. No source claim available.

### d. Hard-earned hardware quirks that a from-scratch project would re-learn

Drawn from amdgpu/xe/asahi docs and from the wider DRM TODO discourse:

- **Per-IP-block init ordering and dependency graphs** (amdgpu's `amd_ip_funcs` exists for a reason — wrong order = silent corruption or hang;
  https://docs.kernel.org/gpu/amdgpu/driver-core.html).
- **Firmware loading sequence and version pinning**: PSP/SMU/GuC/MES/GSP
  firmware images must match the driver, and the driver must validate
  signatures before letting the firmware boot.
- **GPU reset chains**: one engine hanging often requires resetting half
  the IP blocks in a specific order; some hangs only recover with a full
  PCI FLR.
- **GFXOFF / runtime-PM races**: power-gating an engine while a submission
  is in flight is a classic deadlock source.
- **PCIe BAR resizing (resizable BAR)** and the consequences when it is
  unavailable (forced 256 MiB VRAM window with an aperture cache).
- **IOMMU/SVM coherence quirks**: per-platform whether the GPU can see CPU
  page-tables atomically; whether ATS/PASID actually work.
- **Multi-tile / multi-die routing** (xe_tile, xe_gt_mcr — multicast
  registers exist because tile-level register access is not transparent).
- **Display: HDCP, audio-over-HDMI, DSC, FreeSync/Adaptive-Sync, link
  training retries, DP-MST topology, eDP backlight quirks.** Each of these
  has a panel-level workaround database somewhere in DRM.
- **Hang recovery state for in-flight syncobjs**: which fences to force-
  signal, which to leave stuck, how to inform userspace without breaking
  Vulkan device-lost semantics.
- **Firmware "wedged" detection** (Xe survivability mode, xe.html): the
  difference between "GPU hung, reset" and "firmware corrupted, stop
  trying."
- **EDID quirks**: thousands of monitors lie in their EDID; `drm/edid` has
  a quirks table.

### e. Smallest kernel-side surface that satisfies Mesa vendor backends

Per the source material (no editorial opinion added):

Mesa Vulkan backends require, at minimum: BO alloc with handle ↔ dma-buf fd
conversion; an mmap path; a VM with VM_BIND-style binding ops; a submit
that takes a self-contained command-stream blob plus syncobj-fd in/out
arrays; drm_syncobj timeline create/wait/signal/transfer + sync_file
conversion; format/modifier query; and KMS atomic + page-flip event for
display-capable contexts (Asahi UAPI v2 patchwork; xe.html;
docs.kernel.org/gpu/drm-vm-bind-async.html;
docs.kernel.org/driver-api/dma-buf.html).

Sources do not enumerate an authoritative "minimal Mesa kernel ABI"
checklist; the above is the union of what amdgpu/xe/asahi expose that Mesa
backends actually call.

### f. Specific risks / known traps for a clean-sheet GPU stack

From the sources:

1. **dma-fence allocation discipline.** Any callback in a fence path must
   be allocation-free under memory pressure (LWN 951811). Getting this
   wrong only fails under OOM, in production. drm_sched encodes this
   discipline; a fresh framework re-derives it the hard way.
2. **Long-running compute breaks dma-fence.** A fence that may take seconds
   to signal violates the forward-progress assumption used by reclaim
   (LWN 928310). Xe added long-running-fence annotations specifically for
   this.
3. **Firmware-scheduler vs in-order completion.** drm_sched assumed
   submission-order = completion-order; GuC/MES/ASC violate it. Every
   from-scratch driver hits this and either rewrites the scheduler or
   adopts the one-scheduler-per-entity hack (LWN 928310; Asahi UAPI v2).
4. **Implicit-sync interop.** Even an explicit-sync-only client must
   present to compositors that may still use implicit sync via dma_resv;
   the sync_file ↔ dma_resv ioctls (5.20+) exist precisely because this
   transition is multi-year (Collabora 2022).
5. **Reset-during-submission races.** GFXOFF/runtime-PM, hang recovery,
   and userspace fd close all interleave; getting lifecycle right requires
   the drm_sched "entity outlives process? jobs continue?" rules
   (LWN 951811).
6. **Modifier negotiation.** Allocating a swapchain image without the
   right tiling/compression modifier silently disables direct scan-out and
   forces a copy per frame; the DMA-BUF feedback protocol exists because
   this was a real performance regression (Phoronix DMA-BUF feedback).
7. **Firmware ABI is the real ABI.** Asahi shows that for firmware-mediated
   GPUs the kernel driver is mostly a marshal layer; the *firmware*
   version locks behavior. Apple ships new firmware with each macOS;
   AMD/Intel/NVIDIA likewise tie kernel to firmware
   (tales-of-the-m1-gpu; xe.html;
   github.com/NVIDIA/open-gpu-kernel-modules).
8. **Display quirks are not graphics.** EDID lies, panel timing
   workarounds, link-training retry, HDCP, audio. None of this is GPU
   architecture but all of it lives in any production driver.
9. **VM_BIND locking is non-trivial.** The drm-vm-bind-locking and
   drm-vm-bind-async docs exist as separate top-level documents because
   getting bind/unbind/exec interleaving right is its own subproject
   (https://docs.kernel.org/gpu/index.html items 37–38).
10. **uAPI is forever.** DRM enforces "kernel patches merge before
    userspace" specifically because a shipped ioctl cannot be changed
    (drm-uapi.html). A clean-sheet stack pays the same price the moment
    third-party Mesa builds start consuming it.

---

## Consolidated source list

Linux kernel docs (docs.kernel.org / dri.freedesktop.org mirrors):
- https://docs.kernel.org/gpu/index.html
- https://docs.kernel.org/gpu/drm-mm.html
- https://docs.kernel.org/gpu/drm-kms.html
- https://docs.kernel.org/gpu/drm-uapi.html
- https://docs.kernel.org/gpu/drm-internals.html
- https://docs.kernel.org/gpu/drm-vm-bind-async.html
- https://docs.kernel.org/driver-api/dma-buf.html
- https://docs.kernel.org/gpu/amdgpu/index.html
- https://docs.kernel.org/gpu/amdgpu/driver-core.html
- https://docs.kernel.org/gpu/xe/index.html
- https://www.kernel.org/doc/html/v6.8/gpu/rfc/xe.html

LWN articles:
- https://lwn.net/Articles/951811/  (DRM scheduler doc improvements)
- https://lwn.net/Articles/928310/  (Xe scheduler / long-running workloads)

Asahi:
- https://asahilinux.org/2022/11/tales-of-the-m1-gpu/
- https://asahilinux.org/2023/03/road-to-vulkan/
- https://patchwork.kernel.org/project/linux-arm-kernel/patch/20250313-agx-uapi-v2-1-59cc53a59ea3@rosenzweig.io/
- https://alyssarosenzweig.ca/blog/asahi-gpu-part-7.html

NVIDIA:
- https://github.com/NVIDIA/open-gpu-kernel-modules/blob/main/README.md

Mesa / WSI:
- https://docs.mesa3d.org/  (index — note: docs.mesa3d.org/wsi.html 404'd)
- https://www.collabora.com/news-and-blog/blog/2022/06/09/bridging-the-synchronization-gap-on-linux/
- https://www.phoronix.com/news/DMA-BUF-FB-Mesa-VLK-Wayland
- https://www.phoronix.com/news/Mesa-24.1-Vulkan-Wayland-Exp
- https://9to5linux.com/mesa-24-1-linux-graphics-stack-released-with-vulkan-x11-wsi-explicit-sync-support
- https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/4942
- https://lists.freedesktop.org/archives/mesa-dev/2020-March/224228.html

Background:
- https://en.wikipedia.org/wiki/Direct_Rendering_Manager
- https://manpages.debian.org/testing/libdrm-dev/drm.7.en.html

Sources that returned 404 / inaccessible (noted for completeness):
- https://docs.mesa3d.org/wsi.html  (404)
- https://docs.kernel.org/gpu/rfc/xe.html  (404; substituted with v6.8 mirror)
- https://gitlab.freedesktop.org/mesa/mesa/-/blob/main/src/vulkan/wsi/README.md  (Anubis access-denied)
- https://docs.mesa3d.org/vulkan/index.html  (index only; no architecture text)
