# atrium-gpu-amd — reference design

**Status:** design, 2026-05-08.
**Owner:** D5 atrium-mesa track + native FreeBSD GPU drivers.
**Scope:** the AMD RDNA2 kernel module + companion display module that
implements the Atrium GPU ABI. Companion to
[`gpu-abi.md`](gpu-abi.md) (the binding ABI surface) and
[`atrium-gpu-abi-v2.md`](atrium-gpu-abi-v2.md) (next-revision ABI work).

This document is **simultaneously two things**:

1. The **binding architectural spec** for `atrium-gpu-amd.ko` —
   structural decisions, file organization, design principles.
   Code reviews check against this.

2. A **non-binding reference** for any future atrium-gpu-* vendor
   module. The Atrium GPU ABI is the only contract; vendors are
   free to architect internals however they see fit. We publish
   what good looks like, articulated, so authors choosing similar
   patterns have an example to react against. Ad-hoc convergence
   beats imposed conformance.

> **Why publish a "non-binding reference"?** Linux DRM's structural
> mess didn't come from someone forcing every driver into one
> shape — it came from *nobody articulating a shape*, so each
> vendor's driver evolved bottom-up under conflicting framework
> pressures over decades. Articulating a coherent shape, even
> without enforcement, sets a baseline. Vendors who want to do
> something different know what they're departing from.

## 1. Position

### 1.1 Framework-less, per-vendor drivers

Linux DRM is a framework. Drivers plug into it via vtable hooks
(`drm_driver` ops, `gem_funcs`, `ttm_device_funcs`, …); the
framework owns the control flow. To trace what happens when
userspace submits a command on `amdgpu`, you traverse:

```
ioctl(DRM_IOCTL_AMDGPU_CS) →
  drm_ioctl() →                      [DRM framework dispatch]
    amdgpu_cs_ioctl() →              [vendor entry]
      amdgpu_cs_parser_init() →      [vendor pre-process]
        ttm_eu_reserve_buffers() →   [TTM framework: validate BOs]
          ... sub-callbacks ...      [each calls back into vendor hooks]
      amdgpu_cs_submit() →
        amdgpu_job_submit() →
          drm_sched_job_arm() →      [scheduling framework]
            drm_sched_entity_push() →
              ... eventually ring write happens, somewhere
```

Eight layers, each hiding state behind a struct + vtable. To fix a
bug at the bottom you read all eight. Multiple frameworks overlap
(DRM core, TTM, GEM, drm-sched, gpuvm, …); state mutations cross
ownership boundaries; lock-order discipline is brittle; an
accidental change in TTM affects every vendor.

**atrium-gpu-amd has zero framework abstractions for hypothetical
other vendors.** It is not "DRM-shaped with AMD code in the hooks."
It is an AMD RDNA2 driver. Top-level ioctl handlers do the actual
work. Hardware register writes appear at leaf functions, not eight
levels of indirection deep.

### 1.2 The Atrium GPU ABI is the only contract

What's stable across the kernel/userspace boundary is exactly
what's in [`gpu-abi.md`](gpu-abi.md):

- BO alloc/map/free
- Command submit (opaque command stream + BO refs + fences)
- Fence wait/query
- Display: connector enum, EDID, mode set, page flip, vblank

Everything inside the kernel module — internal struct shapes,
helper function signatures, file organization — is private. We
refactor freely. Mesa-side `atrium-winsys` only sees ioctls. This
is the inverse of Linux DRM, where many "internal" framework
structs leak through helper functions and changes ripple across
every vendor driver.

### 1.3 Reference-design status

For atrium-gpu-amd: this document is binding. The principles in
§3, the file layout in §4, the not-doing list in §6 are
contractual; departures require justification recorded in commit
messages.

For any future atrium-gpu-* vendor module: this document is
**suggestive, not binding**. The Atrium GPU ABI is the only thing
that must match. Vendor-specific architecture is the vendor's call.

## 2. The DRM-shaped pathology, articulated

DRM's flaws aren't accidental — they're what happens when you
force uniformity on hardware that isn't uniform. The specific
pathologies we are avoiding:

**Generic abstractions that always need vendor-specific overrides.**
TTM exposes a "buffer object" API that's generic enough to host
any GPU's memory model, but every vendor overrides the eviction
policy, the placement decision, the page-pinning behavior, the
mmap path. The "generic" API is mostly hooks; the actual logic is
in vendor code that calls back into framework helpers that call
back into vendor hooks. Net cost: 4× the code, 4× the bugs, no
correctness guarantee from the framework.

**Implicit state.** DRM maintains many invariants by carefully-
ordered framework calls. Get the order wrong and things silently
break — no compile error, no clear runtime error, just "page flip
intermittently fails when there's GPU contention on a Tuesday."
Order constraints are enforced by lockdep at runtime if you
remember to enable it.

**Side effects in unrelated code.** A `vmwgfx` fix lands; it
modifies a TTM helper; a week later `i915` hits a corner case
because their assumption about TTM's behavior wasn't quite what
the helper now does. The framework is a shared mutable substrate;
vendors are coupled through it.

**Long callback chains hiding intent.** A 12-layer call stack to
do what amounts to "write a register and update a refcount."
Reading the code tells you the *control flow*; understanding the
*intent* requires reading the framework's design rationale, which
is scattered across LWN articles, mailing-list threads, and
implicit conventions.

**API churn.** The framework evolves; every vendor must catch up.
A 10-year-old vendor driver requires constant maintenance to stay
buildable, before you even get to functional changes.

We avoid all of these by *not having a framework*. atrium-gpu-amd
is one driver. Its internal APIs serve only its own implementation.
Future drivers (atrium-gpu-nvidia, etc.) write their own internal
APIs, suited to their own hardware.

The cost of this approach: code duplication. atrium-gpu-amd and
atrium-gpu-nvidia would re-implement BO refcounting, fence dispatch,
ring submit. We accept that. The duplicated code is small per
driver (~6-8K lines), self-contained, and decoupled from changes
in the other driver. The Linux-DRM-economy trade is "share code,
share bugs, share API churn"; we choose "duplicate, isolate, evolve
independently."

## 3. Architectural principles

Six principles. Each defensible. Departures must be justified.

### 3.1 Linear control flow

Every ioctl handler reads top-to-bottom, leaf-call by leaf-call.
No vtables, no driver "ops" structs, no callback chains. If the
reader wants to know what happens when userspace submits a
command, they read `amd_ioctl_submit()` start to finish in one
file:

```c
static int amd_ioctl_submit(struct amd_softc *sc, struct amd_ctx *ctx,
                            struct atrium_gpu_submit *req)
{
    /* 1. Validate request shape (sizes, BO handles in range). */
    int err = amd_validate_submit_args(sc, req);
    if (err) return err;

    /* 2. Walk BO refs; pin each in this ctx's GPUVM. Pinning here
     * is just refcount + eviction-priority bump — GPUVM mappings
     * already exist (see ioctl_bo.c::amd_ioctl_bo_alloc). */
    for (uint32_t i = 0; i < req->bo_count; i++) {
        err = amd_bo_pin_for_submit(ctx, req->bo_handles[i]);
        if (err) goto unpin;
    }

    /* 3. Acquire engine ring lock; reserve N dwords. */
    struct amd_ring *ring = amd_pick_ring(sc, req->engine);
    uint32_t *ring_buf = amd_ring_reserve(ring, req->cmd_size);
    if (!ring_buf) { err = ENOSPC; goto unpin; }

    /* 4. Copy user command bytes directly into ring buffer.
     * Userspace pre-encoded these for THIS chip; kernel doesn't
     * parse — see HARDWARE-NOTES.md "command opacity". */
    err = copyin((void*)req->cmd_handle_offset, ring_buf, req->cmd_size);
    if (err) goto unreserve;

    /* 5. Allocate a fence id, append a write-fence packet. */
    uint64_t fence_id = amd_fence_alloc(ring);
    amd_emit_fence_packet(ring_buf + req->cmd_size, fence_id);

    /* 6. Doorbell. Hardware now starts executing. */
    amd_ring_commit(ring, req->cmd_size + FENCE_PACKET_DWORDS);
    amd_ring_doorbell(ring);

    /* 7. Track BOs as in-flight on this fence (auto-unpin on retire). */
    amd_fence_track_bos(fence_id, ctx, req->bo_handles, req->bo_count);

    req->fence_out = fence_id;
    return 0;

unreserve:
    amd_ring_release(ring, req->cmd_size);
unpin:
    while (i-- > 0) amd_bo_unpin(ctx, req->bo_handles[i]);
    return err;
}
```

Thirty lines, seven numbered steps, zero indirection. A new
engineer reads this and knows what happens. Compare to
`amdgpu_cs_ioctl()` in Linux: ~200 lines of "if user passed flag X,
call helper Y," each helper its own 100-line function with
framework callbacks; total trace ~1000 lines across 8 files.

### 3.2 Explicit state machines

BOs have a defined lifecycle. State transitions are named,
single-purpose functions. The complete set:

```
amd_bo_alloc()                    →  CREATED
amd_bo_pin_for_submit()           CREATED  →  IN_FLIGHT
amd_fence_retire_pending_bos()    IN_FLIGHT  →  CREATED        (auto)
amd_bo_evict()                    CREATED  →  IN_GTT           (memory pressure)
amd_bo_pull_to_vram()             IN_GTT  →  CREATED           (re-pin path)
amd_bo_free()                     *  →  DESTROYED              (drops final ref)
```

Six transitions. Drawable on a whiteboard. Each is a named
function. There is no implicit "wait, this changes state because
of side effects in another function" surprise. Adding a new
transition requires adding a named function; the next reviewer
sees it.

### 3.3 Hardware-block-shaped abstractions

The driver's file structure mirrors the chip's actual block diagram.
RDNA2 has:

- **GMC** (Graphics Memory Controller) — page tables for GPU virtual memory
- **CP** (Command Processor) — graphics + compute ring buffers, doorbell
- **SDMA** (System DMA) — DMA copy engines (used for eviction, blits)
- **SMU** (System Management Unit) — power, clocks, thermal
- **PSP** (Platform Security Processor) — firmware loader + secure crypto
- **DCN** (Display Core Next) — display engine (separate kmod)

So files: `gmc.c`, `cp.c`, `sdma.c`, `smu.c`, `psp.c`, `dcn.c`.
Not `mem.c` and `engine.c` and `pm.c` (generic abstractions); the
file structure tells you what hardware it touches.

Where the same logical operation runs on multiple hardware blocks
(e.g., command submission via CP or SDMA), we have explicit per-
block functions, not a `submit_to_engine(engine_id, ...)` switch
dispatcher. Reading `amd_cp_submit()` tells you about CP. Reading
`amd_sdma_submit()` tells you about SDMA. They share *no code*
unless that code is *genuinely* identical (in which case it lives
in a small `ring_helpers.c` — not as a virtual method).

### 3.4 Native VM primitives, no TTM

FreeBSD has good GPU-driver-shaped VM primitives:

- `vm_object` — backing storage abstraction; anonymous-RAM,
  file-backed, or device-backed
- `vm_page` — physical page descriptor
- `vmem` — best-fit allocator for arbitrary address spaces
  (perfect for GPU virtual address space)

We use them directly. The complete BO struct:

```c
struct amd_bo {
    /* Owning context; null for kernel-internal BOs (firmware, ring
     * buffers, etc.). */
    struct amd_ctx *ctx;

    /* Size in bytes. */
    uint64_t size;

    /* Current physical placement. */
    enum {
        AMD_BO_VRAM,     /* in dedicated VRAM */
        AMD_BO_GTT,      /* in system RAM, GPU-aperture-mapped */
        AMD_BO_USWC,     /* in system RAM, uncached, GPU-visible */
    } placement;

    /* Physical backing — one of these is non-null based on placement. */
    vm_paddr_t       vram_addr;        /* VRAM BAR offset, if VRAM */
    vm_object_t      sys_obj;          /* system RAM page list, if GTT/USWC */

    /* GPU-visible address; bound into ctx's page tables.
     * Allocated via vmem from ctx->gpuvm_arena. */
    uint64_t         gpu_addr;

    /* CPU-side mmap object. Lazy: created on first IOC_BO_MAP. */
    vm_object_t      cpu_map_obj;

    /* Refcount. Held by: user handles, in-flight fences, GPUVM mappings. */
    volatile int     refs;
};
```

That's the complete struct. Compare TTM's `ttm_buffer_object`:
~30 fields, 5 sub-objects, parent class `gem_object` with another
~20 fields. TTM is generic; we are not.

Eviction is a function on the VRAM allocator: when `vmem_alloc()`
fails on the VRAM arena, walk the LRU, pick a victim, schedule a
DMA to GTT, retry. ~150 lines for the full algorithm. Linux's TTM
eviction is thousands of lines because it's parameterized over
multiple memory domains, cache types, eviction priorities, and
vendor hooks.

### 3.5 ABI as the only public contract; internal APIs change freely

The public face is `gpu-abi.md`'s ioctl set. That's stable.
Everything else — function signatures, struct layouts, internal
helpers, module-internal naming — is private. We refactor
without coordinating with Mesa.

Concretely: an internal API change in atrium-gpu-amd doesn't
require a corresponding atrium-mesa change unless it touched the
ABI. Most changes don't.

Linux DRM has the opposite property: many "internal" structs are
exposed via DRM helper functions, and changing them ripples
through every vendor driver and external out-of-tree module. The
framework leaks. Atrium's doesn't.

### 3.6 Tests co-located, exercised before merge

Each `.c` file has a corresponding `tests/test_<file>.c` exercising
every entry point and every error path. Tests are part of the
kmod build; loaded via a sysctl entry point or a privileged ioctl
(in-kernel test framework, not userspace).

When a bug is fixed, a test is added in the same commit, in the
same directory. No "PR landed; tests will come later" — that's
review policy backed by precommit hook.

This gives us local regression catching: a change to `bo.c` that
breaks `tests/test_bo.c` fails immediately. Linux DRM testing is
split between kernel selftests, KASAN reports, IGT (a userspace
test suite), and CI farms — each catching different things, none
owning the driver's correctness end-to-end.

## 4. Module organization

### 4.1 Three modules

1. **`atrium-gpu-amd-pci.ko`** (~500 lines) — PCI bring-up, BAR
   mapping, MSI-X allocation, IRQ vector routing. Provides a
   `softc` shared between the GPU module and the display module.
   Loaded first.

2. **`atrium-gpu-amd.ko`** (~6-8K lines) — the GPU compute/render
   driver. Owns `/dev/atrium-gpu0`. Depends on the PCI module.

3. **`atrium-gpu-amd-display.ko`** (~3-5K lines) — the display
   engine. Owns `/dev/atrium-display0`. Also depends on the PCI
   module. Loadable independently of the GPU module (e.g., for
   headless servers that never load display, or for debugging if
   one is broken).

This separation means:

- The compute module loads on headless servers without display
  baggage.
- A bug in DCN can't crash GPU compute.
- The display module evolves independently (HDR support landing
  doesn't risk regressing GPU stability).

### 4.2 atrium-gpu-amd file layout

```
atrium-gpu-amd/
├── README.md                # 1-page architectural overview
├── HARDWARE-NOTES.md        # AMD manual citations, errata, choices

│ # Bring-up
├── module.c                 # newbus probe/attach + cdev registration
├── firmware.c               # PSP-loaded firmware: SMU, CP, MEC, SDMA, RLC

│ # Core abstractions (each = one hardware block)
├── gmc.c                    # Graphics Memory Controller (page tables, GTT)
├── gpuvm.c                  # Per-context GPU virtual memory; VMID alloc
├── bo.c                     # Buffer object lifecycle
├── cp.c                     # Command Processor (graphics + compute rings)
├── sdma.c                   # System DMA (eviction copies, blits)
├── smu.c                    # System Management Unit (power, clocks)
├── irq.c                    # IRQ vector dispatch within the GPU module
├── fence.c                  # Fence allocation, retire-on-IRQ, kqueue
├── reset.c                  # Hang detection + ring reset

│ # Public API
├── ioctl.c                  # Single switch: ATRIUM_GPU_IOC_* → handler
├── ioctl_bo.c               # IOC_BO_ALLOC, _MAP, _FREE
├── ioctl_submit.c           # IOC_SUBMIT (the example in §3.1)
├── ioctl_fence.c            # IOC_FENCE_WAIT, _QUERY

│ # Hardware spec
├── reg/
│   ├── gc_10_3_0.h         # Graphics Core registers (RDNA2)
│   ├── mmhub_2_3_0.h       # MMHub (memory hub) registers
│   ├── nbio_2_3_0.h        # Northbridge IO registers
│   └── osssys_4_0_0.h      # OS-system interface registers

│ # Tests (co-located; see §3.6)
└── tests/
    ├── test_bo.c
    ├── test_submit.c
    ├── test_fence.c
    ├── test_irq.c
    └── test_reset.c
```

### 4.3 atrium-gpu-amd-display file layout

```
atrium-gpu-amd-display/
├── README.md
├── module.c                 # cdev register for /dev/atrium-display0
├── dcn.c                    # Display Core Next bring-up + clock tree
├── connector.c              # HDMI/DP enumeration; EDID read over AUX/I2C
├── link.c                   # DP link training, HDMI scrambling
├── mode.c                   # Mode validation + atomic set
├── plane.c                  # Primary/cursor planes
├── flip.c                   # Page flip + vblank delivery
├── reg/
│   └── dcn_3_0.h           # DCN 3.0 registers (RDNA2 era)
└── tests/
    ├── test_connector.c
    └── test_mode.c
```

V1 scope: one connector at a time, one mode (1080p/1440p/4K
common modes), no DSC, no HDR, no HDCP. Adds incrementally.

## 5. Specific design choices

### 5.1 GPUVM: per-context page tables, never shared

Each user context (one per opened `/dev/atrium-gpu0` fd) gets its
own GPUVM page tables, allocated at fd-open time. Each context
gets a VMID — a hardware identifier the command processor uses
when translating addresses. Submission specifies the context's
VMID; hardware reads the corresponding page tables.

Consequences:

- Two processes cannot see each other's BOs even at the GPU
  level. The cdev fd ownership is the first defense; per-context
  GPUVM is the second.
- A buggy process can corrupt only its own GPUVM. Memory
  protection is per-context.
- No cross-context shared mappings exist by default. Buffer
  sharing across processes goes through explicit BO export/import
  via SCM_RIGHTS — a deliberate user-visible operation.

GPUVM lives in `gpuvm.c`. ~800 lines for the page-table walker,
VMID allocator, and TLB invalidate paths.

### 5.2 Fence retire path: IRQ → kqueue, no polling

When the GPU completes a submission, hardware writes the
sequence number to a memory location and fires an MSI-X IRQ. The
IRQ handler:

1. Reads the per-engine "highest retired fence" register
   (`mmCP_RB0_RPTR` for the graphics ring, similar per engine).
2. Updates a per-engine atomic.
3. `wakeup()` on a sleep channel (for `IOC_FENCE_WAIT` waiters).
4. Posts a kqueue notification on the cdev (for `EVFILT_READ`
   subscribers).
5. Walks the in-flight BO list; unpins any whose tracking-fence
   has now retired.

Userspace either calls `IOC_FENCE_WAIT` (kernel sleeps until the
fence's counter is `≤` retired_max) or registers `EVFILT_READ`
on the fd for event-driven notification. No polling threads, no
wall-clock timers, no jiffies. Pure event-driven.

### 5.3 Power management: minimal V1, opt-in V2

V1 plan: load SMU firmware, request "default 3D performance"
power state, leave it. Performance is ~70-80% of optimal because
we're not doing workload-aware DPM (Dynamic Power Management).
Acceptable.

V2: SMU exposes per-clock-domain metrics; we add a small DPM
module that scales clocks based on engine utilization. Closer to
native performance.

V3+: Fine-grained gating, idle-state management, thermal
throttling integration. Approaching parity with the official driver.

The architectural property: power management is a *separate*
module (`smu.c` + thin `pm.c` in V2). It's not woven through
every other file. V2's DPM addition doesn't touch `bo.c` or
`submit.c`. This is deliberate; it's the opposite of Linux's
amdgpu, where power-management state checks appear inside command
submission paths because they evolved together.

### 5.4 GPU family scope: RDNA2-only V1, RDNA3 as additive module

We pick **one** GPU family and write a driver for it. RDNA2 (Navi
21/22/23) because:

- Modern enough to be a current programming target (chiplet-aware
  layouts, RT cores, mesh shaders)
- Mature enough that AMD documentation is comprehensive
- Common enough in the wild for real users to have hardware
- Old enough that SMU/PSP firmware is stable (vs. RDNA3's
  still-evolving stack)

Adding RDNA3 later is a *separate* module: `atrium-gpu-amd-rdna3.ko`.
It shares register-layout files where applicable but has its own
bring-up code, its own SMU interface, its own CP/SDMA paths, its
own DCN version. We do **not** try to abstract "AMD GPU family"
generically.

This is the *opposite* of Linux's amdgpu, which has dispatch
tables for GFX 6/7/8/9/10/11/12 with shared code paths and per-
family hooks. That's how amdgpu got to 500K lines.

The cost of our approach: ~70% of code is duplicated between the
RDNA2 and RDNA3 modules. The benefit: each is independently
readable, independently testable, and a bug in RDNA3 cannot
break RDNA2.

This is a real trade-off. We're choosing readability + isolation
over share-efficiency. For a small team supporting a few vendors,
it's right. For a 100-engineer driver org supporting 30 GPU
families, you'd want more sharing. We are not that org and don't
want to grow into one.

### 5.5 Hang recovery: timeout + ring reset, no per-context kill

When a fence hasn't retired in N seconds (N = 2s default), declare
hang. Reset the ring (CP-side reset for graphics; SDMA reset for
SDMA). Mark all in-flight fences on that engine as "lost"
(`IOC_FENCE_WAIT` returns `EIO`).

We do **not** try to identify the offending context, kill it, and
let other contexts continue. Linux does this and it's hideously
complex; the failure modes (mid-submit-but-not-yet-doorbelled,
wedged page table, partial flush, fence trapped between two CP
units) are gnarly.

For V1: ring reset = engine-wide reset; all contexts on that
engine see EIO; userspace recovers by reallocating. Less granular,
much simpler, debuggable.

V2 may add per-VMID quiesce + reset, when we have failure-mode
data showing it actually works. The bar is "we can describe the
algorithm in a one-page state machine, and exercise every state
in tests." DRM's per-context recovery does not pass that bar.

### 5.6 BO lifecycle reference

The complete BO state machine, with the conditions that trigger
each transition:

```
                 amd_bo_alloc
                       │
                       ▼
                  ┌──────────┐    amd_bo_pin_for_submit       ┌──────────┐
                  │ CREATED  │  ─────────────────────────►   │ IN_FLIGHT │
                  │          │  ◄─────────────────────────   │           │
                  └──────────┘    amd_fence_retire (auto)    └──────────┘
                    │      ▲
       amd_bo_evict │      │ amd_bo_pull_to_vram
                    ▼      │
                  ┌──────────┐
                  │  IN_GTT  │
                  └──────────┘

                  amd_bo_free (from any state, drops final ref)
                       │
                       ▼
                  ┌──────────┐
                  │DESTROYED │
                  └──────────┘
```

Six transitions. Each one is a named function. The transitions
are mutually exclusive — at any moment a BO is in exactly one
state, and only the listed transitions can move it.

Implementation pattern: each state is an enum value; each
transition function asserts the precondition state, performs the
work, and writes the new state. A single mutex per BO serializes
state transitions.

## 6. What we explicitly are NOT doing

Concrete list of things Linux DRM does that we're not. Each
removal cuts code; the cumulative cost is what makes our driver
shippable in a quarter rather than a decade.

- **No generic GPU scheduler.** No drm-sched equivalent. Userspace
  submits to its preferred ring; kernel doesn't reorder. If
  multiple contexts contend, fairness comes from ring time-slicing
  in hardware (CP supports this), not a kernel-level scheduler.

- **No DRM/KMS legacy ioctls.** No mode-set via
  `drm_mode_setcrtc`. No legacy CRTC/encoder/connector object
  model. Our display API is the Atrium GPU ABI's
  `IOC_GET_CONNECTOR` / `IOC_SET_MODE` set, which is much smaller
  than DRM-KMS.

- **No GEM-style flink names.** BO sharing is via fd export
  through SCM_RIGHTS only, never via global numeric names. Avoids
  the GEM "anyone with the name owns the BO" hole.

- **No KFD / ROCm / per-process queue exposure.** Compute uses
  the same submission path as graphics, just on the compute ring.
  ROCm-like features (per-process doorbells exposed to user,
  user-space queue management) are deferred indefinitely.

- **No AMD ATPX / hybrid graphics.** Laptop dGPU dynamic switching
  isn't supported. V1 assumes always-on dGPU.

- **No HDCP.** Content-protection path missing. Acceptable: we
  explicitly don't target streaming-DRM-protected playback in V1.

- **No PCI Atomic Ops or PASID.** Advanced address translation
  features. V1 uses simpler MMIO + MSI-X paths.

- **No SR-IOV / virtualization.** GPU virtualization for guests
  is its own world; not V1.

- **No frequency-counted fences for legacy compositors.** No
  Mesa+i915-style DRI3 token shenanigans. We have one path:
  Atrium GPU ABI fences. Period.

- **No AGP, no bus-width fallbacks, no shared system
  memory beyond GTT.** Modern GPUs handle this; we don't carry
  decade-old fallback code.

Each of these is a *visible-to-users* feature loss. We accept
that. The trade is a driver that's 90% as capable in 10% of the
code, with 10× the readability.

## 7. Code-quality-as-architecture

Some practices that turn into architecture if applied
consistently:

### 7.1 Every register access is annotated

When the driver writes `amd_mmio_write(sc, regGRBM_STATUS, 0)`,
the line carries a comment that says **which manual section**
defines that register and **what bit pattern** the value means.
Reader doesn't need to look up the manual — context is right
there.

```c
/* Reset Graphics Block (GRBM). Per RDNA2 GFX10.3 ISA Reference,
 * §28.2.4: writing 0 to GRBM_GFX_INDEX clears any per-SE/per-SH
 * targeting; subsequent register writes go to all units. */
amd_mmio_write(sc, regGRBM_GFX_INDEX, 0);
```

### 7.2 Every structural decision has a `WHY` comment

Not "what" — "why." `bo.c` opens with a comment explaining why we
don't use a generic memory framework, why eviction is per-VRAM,
why we ref-count instead of GC. This forces the architect to
articulate trade-offs at decision time and lets the next reader
update them when reality changes.

```c
/*
 * BO lifecycle: see ../README.md §5.6 for the state machine.
 *
 * WHY refcount instead of explicit ownership: BOs are referenced
 * by user handles, in-flight submissions, and GPUVM mappings, all
 * of which can outlive each other in unpredictable orders. A
 * borrow-checker pattern would require either (a) explicit
 * shared ownership types in C (clunky), or (b) a single owning
 * abstraction that mediates all access (== a framework, what
 * we're avoiding). Refcount is the smallest tool that handles
 * the actual aliasing we have.
 *
 * WHY no garbage collection: GC adds latency variance to
 * submission paths. With refcount, BO release is deterministic —
 * ref hits 0, free runs synchronously. For a kernel module, this
 * is the right trade.
 */
```

### 7.3 Co-located tests

Each `tests/test_<file>.c` exercises every entry point in
`<file>.c`, including error paths. Tests are part of the kmod
build, loaded via a sysctl entry. Tests run before merge: the
precommit hook executes the relevant test set.

### 7.4 Fault injection for hang paths

Helpers that synthesize the failure modes we care about:

```c
amd_inject_fault(sc, AMD_FAULT_RING_TIMEOUT);
amd_inject_fault(sc, AMD_FAULT_VRAM_FULL);
amd_inject_fault(sc, AMD_FAULT_PSP_NORESPONSE);
amd_inject_fault(sc, AMD_FAULT_FENCE_LOST);
```

Bug fixes for hang paths require an injection-based test that
fails before the fix and passes after. Hang recovery is the
hardest thing to test under natural conditions; injection is the
only practical way to make it routine.

### 7.5 No dead code, period

If a function isn't called, it's deleted. If a feature is
disabled, the code for it is removed (not `#ifdef`'d out). Linux
DRM has accumulated countless `#if 0` blocks and never-called
helpers; ours doesn't.

This requires reviewer discipline: every PR removes at least as
much as it adds, ideally. Concretely tracked via line-count
budget per file.

## 8. Implementation staging

D5.2 (the AMD slice) becomes:

| Week | Goal |
|---|---|
| 1-2 | PCI bring-up; BAR mapping; basic register I/O on a real RX 6700 (Navi 22). Read GRBM_STATUS over PCI; print it. |
| 3-4 | Firmware loading. PSP brings up SMU, MEC, CP. GPU is "alive" — clocks running, ring buffers initialized, no errors in IRQ. |
| 5-6 | GPUVM + BO + submit. Hand-encoded clear-the-screen command runs from kernel; VRAM contents change as expected. |
| 7-8 | Fence + IRQ + kqueue notify. End-to-end submit-and-wait works; fault injection covers timeout path. |
| 9-10 | atrium-winsys for radv. Shader-less Vulkan triangle renders. |
| 11-13 | Display engine bring-up (atrium-gpu-amd-display). Framebuffer scanning out to a monitor. |
| 14-16 | Stabilization, more fault injection, real apps (vkmark, glmark2, a Bevy demo). |

Sixteen weeks. ~4 months focused. The number is plausible because
we're not building a framework — we're building one driver that
does one thing well.

## 9. Reference-design status (revisited)

For atrium-gpu-amd: this document is the binding spec. Reviewers
check changes against §§3, 4, 6 specifically. Departures require
justification in the commit message; the burden is on the
departing change to explain why the principle should be relaxed.

For other vendor modules (atrium-gpu-nvidia, atrium-gpu-intel,
…): this document is **suggestive, not binding**. The Atrium GPU
ABI is the only thing that *must* match. Vendors are free to:

- Use a different file layout
- Make different choices about state machines, eviction, power
- Share code internally however suits their hardware
- Pursue a different architectural style entirely

But: we publish the *principles* (§3) as a reference, with the
articulated reasoning. Vendors writing atrium-gpu-* drivers can
choose to follow — or not — and they'll have a coherent example
to react against.

The DRM mess started not because everyone was forced into one
shape but because *no shape was articulated*; the framework grew
bottom-up under conflicting pressures over decades. Articulating
one good shape, even non-mandatory, sets a much better baseline.
Vendors who depart can do so deliberately, knowing what they're
departing from.

## 10. Open questions

1. **Suspend/resume.** V1 plan: not supported. Always-on power
   state. V2 needs S3/S4 path: serialize device state, repower,
   re-init firmware, restore. Significant effort; punted.

2. **Multi-GPU systems.** V1: each GPU gets its own
   `/dev/atrium-gpu<N>`; userspace picks which to use. No
   cross-GPU buffer sharing or compute distribution. V2 if real
   demand emerges.

3. **Compute-only submissions on the graphics queue.** Some apps
   want compute without the graphics state machine overhead.
   Direct submission to the compute ring is supported in V1; we
   haven't decided whether to expose graphics-ring "compute mode"
   submissions. Defer to actual workload data.

4. **Telemetry and observability.** sysctl-exposed counters for
   submissions, fence retire latency, eviction events, hang
   counts. Lower-priority than correctness; lands as it's needed.

5. **Reservation objects across BOs.** When a submission
   references multiple BOs, do we need a shared "reservation
   lock" pattern (like Linux's dma-resv)? In V1 we hold per-BO
   refs without cross-BO ordering invariants. May discover we
   need more; will re-evaluate after first wave of real apps.

## References

- [`spec/gpu-abi.md`](gpu-abi.md) — the binding ABI surface this
  driver implements.
- [`spec/atrium-gpu-abi-v2.md`](atrium-gpu-abi-v2.md) — next-
  revision ABI work; this driver tracks v0.1 of the ABI today.
- [`spec/gpu-isolation.md`](gpu-isolation.md) — security model
  for GPU access.
- [`spec/fresco-rendering-stack.md`](fresco-rendering-stack.md)
  §2 — why the bottom boundary is Vulkan; informs what userspace
  expects.
- [`spec/fresco-surfaces.md`](fresco-surfaces.md) — game/GPU
  external-surface model that consumes this driver.
- AMD RDNA2 Instruction Set Architecture (public, 2020).
- AMD GFX10.3 Register Reference (public, AMD GPUOpen).
- `feedback_no_linuxkpi`, `feedback_atrium_licensing_policy` —
  substrate constraints.
