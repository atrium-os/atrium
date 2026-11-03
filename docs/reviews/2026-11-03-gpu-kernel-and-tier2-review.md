# Review: the GPU kernel driver architecture, and Tier-2 maturity

> **Date:** 2026-11-03. **Scope:** the DRM-free kernel GPU architecture
> (`spec/atrium-gpu-abi-v2.md`, `spec/gpu-abi-reconciliation.md`,
> `spec/atrium-gpu-driver-architecture.md`, `spec/atrium-gpu-amd-design.md`,
> `spec/atrium-display-architecture.md`, `spec/gpu-isolation.md`) checked
> against the reference implementation in `atrium-gpu-amd/` (every `.c` file
> read). Then the Tier-2 software Vulkan path (`atrium-spv-*`,
> `aqueduct-gpu-host` Tier-2, `atrium-vk-icd`) assessed for use as the
> renderer under frescod. Companion to `../GRAPHICS-STACK.md`.
>
> **How to read it:** §A is the kernel review. §B is Tier-2. Each finding
> cites the file and line it comes from. Severity: **[S1]** breaks isolation
> or correctness for other processes; **[S2]** a design gap that has to be
> settled before real hardware; **[S3]** quality, drift or hygiene.

## Verdict in brief

**Kernel architecture: the direction is right and worth committing to. The
implementation has not yet replaced several things DRM does for you, and the
gaps are concentrated exactly where the architecture makes its security
claims.**

- The core choices hold up. They match the post-2020 consensus (Xe, Asahi,
  Panthor) expressed in BSD primitives: fd-as-handle objects, VM_BIND,
  timeline syncobjs delivered through kqueue, an opaque submit blob, firmware
  scheduling, display on its own cdev, and a per-vendor contract with no
  shared framework. Dropping DRM, GEM, TTM and KMS doesn't lose anything
  essential. Keep all of it.
- What's missing is mostly what DRM's machinery quietly provides: knowing
  when the GPU has finished with memory before freeing it, attributing
  completions to the right submission, queue and VMID management across many
  clients, separating control from client authority, and validating in the
  kernel. Today `atrium-gpu-amd` leaves the last of these to the **gpusim
  referee**. On silicon nothing plays that role.
- Five problems break the isolation the `gpu` capability promises (§A3). Any
  two processes sharing the GPU are affected. They aren't one-line fixes:
  each needs an object the v2 spec already describes but the code doesn't
  have yet (`queue_fd`, `share_fd`, per-queue fences, a control/client
  split).
- Moving from gpusim to silicon is bigger than the backend-vtable framing
  suggests. Firmware loading, MES bring-up, the page-table format, power and
  display are all programmed through **SIM-only registers** (§A5). The
  real-silicon backend will be most of the driver.
- None of the isolation tests the spec makes a precondition for granting the
  `gpu` capability exist (§A6).

**Tier-2: a good choice, further along than its specs say, but not ready to
be the only renderer under frescod.** It already renders the atrium-core rect
pipeline through `HeadlessRenderer` → `atrium-vk-icd` → Tier-2. The text,
path and texture bundles, sRGB correctness, x86_64 support, build
reproducibility and automated testing are the remaining work. Four of its
test targets no longer compile (§B). The plan in §B5 makes it the
default with lavapipe kept as a test oracle, not shipped.

---

## A. Kernel driver architecture

### A1. What's right (keep)

| Decision | Why it's right | Where |
|---|---|---|
| Vendor-per-cdev contract, no shared kernel framework | Avoids the DRM midlayer. Each driver is readable alone. | v2 §3.1 |
| Every object is an fd (BO, VM, syncobj), `DFLAG_PASSABLE` | Lifetime = refcount, SCM_RIGHTS transport, works under Capsicum. Implemented cleanly. | `bo.c`, `vm.c`, `sync.c` |
| BO created VM-agnostic, mapped by `VM_BIND` into many VMs | The right sharing model. Removes per-submit BO lists. | `bo.c:362` |
| Timeline-only sync, the syncobj fd itself readable by kqueue | Simpler than DRM's sync_file + syncobj + dma_resv. Correct kqfilter modelled on `eventfd`. | `sync.c` |
| Opaque command blob, the kernel doesn't parse it | Keeps the kernel small and vendor-neutral. `SET_COMPUTE`/`SET_DRAW` already folded into PM4. | `cp.c` |
| Display as a separate engine and cdev, vblank via kqueue, IH demuxed by cause | Mirrors real silicon (one IH ring, DCN independent of GFX). Vblank reaches userspace through `kevent()`. | `ih.c`, `display/display_dev.c` |
| Base/pci + gpu + display as three modules with coordinated reset hooks | amdgpu's pre/post-reset shape without its code. Enables the hot-swap story. | `reset.c` |
| Backend ops vtable under a transport-neutral front-end | The right seam for the *front-end*. See §A5 for its limits. | `atrium_gpu_amd.h` |
| Firmware scheduling, no drm-sched | Right for MES/GuC-class hardware. | v2 principle 7 |

Two v2 ideas are genuine differentiators worth keeping: **doorbells as
capabilities** (§5.9) and **read-only energy telemetry** (§5.10). Both need
the fixes in §A3 before they hold.

### A2. What DRM does that we haven't replaced yet

These are design-level. Each needs a decision written into v2, not just a
code patch.

**A2.1 Memory lifetime versus in-flight GPU work [S1].**
Explicit sync tells *userspace* when a buffer is ready. The *kernel* still has
to know when the GPU has stopped touching pages before freeing them or reusing
their PTEs. Today `close(bo_fd)` unmaps and frees the backing pages
immediately (`bo.c:106-123`). `close(vm_fd)` frees the page tables and
releases the VMID (`vm.c:149`, `vm.c:56`) even while a queue is still
executing under it. On real hardware that is a device-side use-after-free:
DMA into freed host pages, or a VMID reused while its TLB entries are live.
amdgpu solves this with dma_resv fences on every BO. The design doc's
"IN_FLIGHT" state (`atrium-gpu-amd-design.md` §5.6) was meant to cover it,
but it isn't built.
*Recommendation:* no per-BO fences. Track a per-queue retired sequence number
(A2.2) and a per-VM "last submitted seqno per queue." Unbind, close and VM
destroy go onto a deferred-free list, reaped by the fence ISR once retired ≥
recorded. This costs about 100 lines. Without it, explicit sync isn't safe.

**A2.2 Completions are counted, not identified [S1].**
The ISR pops one entry from a global FIFO per end-of-pipe interrupt
(`irq.c:25-38`). It ignores the cookie's ring field and any fence value.
That's correct only if completions arrive in exactly push order across all
engines and all clients. Compute and GFX, or a ring parked on a cross-queue
WAIT, break that, and the wrong syncobj is signalled: the compositor sees
"done" for a frame still rendering. The FIFO also holds only 16 entries and
**drops the push silently when full** (`irq.c:52`). Once it's full, a
submit succeeds but its syncobj never signals.
*Recommendation:* adopt the design doc's own §5.2 model. Each queue gets a
kernel-owned fence BO. `RELEASE_MEM` writes `(queue, seqno)`. The ISR reads
each queue's seqno and retires every signal point ≤ it. Signal points are a
per-queue list with no fixed cap (or a cap that fails the submit with
`EAGAIN`). The same mechanism gives hang detection (A2.6) and deferred frees
(A2.1).

**A2.3 Queues and VMIDs are global, fixed and unvirtualized [S1/S2].**
There are exactly two hardware queues for the whole device (GFX qid 0,
compute qid 1). **Every `SUBMIT` reprograms the shared queue's base and VMID
and then rings the doorbell** (`cp.c:78-92`), with no lock held
(`ioctl.c:286-345`). Two processes submitting concurrently can interleave
these register writes, so process A's ring executes under process B's
VMID. That's a cross-address-space execution bug, the very thing
`gpu-isolation.md` I1–I3 forbid. VMs map 1:1 to hardware VMIDs, so the
system tops out at **15 live VMs** (`vm.c:56`). With per-jail apps that's 15
GPU-using processes for the whole machine.
*Recommendation:* make `queue_fd` (v2 §5.5) real. A queue is created in a VM
and owns an MQD and its own doorbell page. On MES hardware (RDNA3/4) queues
are mapped through **MES API packets** (`ADD_QUEUE`/`REMOVE_QUEUE`) and the
MES handles VMID/process switching. The kernel then needs a *process
context* per VM, not a permanent VMID. Specify that in v2 and model it in
gpusim. The kernel-mediated `SUBMIT` then targets *that* queue, and never
reprograms a shared one.

**A2.4 No separation between control and client authority [S1].**
Any process that can open `/dev/atrium-gpu0` can call `GPU_RESET` (resetting
the device under every other client), `SCHED` (adding queues, changing
global weights and deadlines) and `POWERGATE` (gating blocks other clients
are using) (`ioctl.c:389-430`). `atrium_amd_open` does no check
(`ioctl.c:13`), and the devfs rules note that the driver "gates only on node
permissions." The display cdev is the same: any opener can modeset and flip
(`display_dev.c:300-330`). v2 principle 8 is "Capsicum-clean." Done right,
that means a jailed app holds a fd that *can't* do these things.
*Recommendation:* split the authority. Either use a separate control node
(`/dev/atrium-gpu0ctl`, `/dev/atrium-display0` owned by frescod's broker
only), or derive restricted client fds with a `CLIENT_FD` ioctl and have
`cap_ioctls_limit()` enforce them. Reset becomes per-queue for clients and
device-wide only for the control fd. The display needs an explicit
owner/lease, which v2 leaves as "no master concept." Write the lease model
into the display spec.

**A2.5 Validation is delegated to the simulator [S1 on silicon].**
Several checks exist only because gpusim's "referee" enforces them:
- Scanout `{vram_offset, size}` is taken from userspace and written straight
  to `FB_BASE/FB_SIZE` (`display_dev.c:135`, `304`, `323`). The comment says
  "the referee enforces residency." On silicon, any display-fd holder can
  scan out **any VRAM**, including another client's buffers. That's the
  password-field leak `gpu-isolation.md` names as the threat.
- `VM_BIND` with a caller-supplied VA (`bo.c:375`) checks only that it falls
  in the PT window: no alignment, no overlap with existing bindings, no
  ownership.
- The submit ring length is checked against the BO, but not against the
  programmed ring size.

*Recommendation:* adopt as a written principle that **the kernel validates
everything it programs into hardware; the simulator's referee is a second
line, not the first**. Scanout should import a **BO fd or `share_fd`**, not
a raw offset. The kernel resolves it to pages, checks size, pitch and
format against the mode, and pins it while it's scanned out. The v2 spec
already says `share_fd`. Since both modules share the softc, the import is
cheap.

**A2.6 No fault or hang model in practice [S2].**
v2 §10 and design §5.5 specify a 2-second timeout, signalling with an abort
flag, `EIO` to waiters, and a device-lost epoch. None of it is built.
`GPU_RESET` sets `n_pending = 0` (`ioctl.c:408`) **without signalling
anything**, so every waiter (kqueue or `SYNCOBJ_WAIT`) hangs forever.
`SYNCOBJ_WAIT` sleeps without `PCATCH` (`ioctl.c:270`), so it can't be
interrupted, and it computes its deadline in `int` ticks, which overflows on
large timeouts.
*Recommendation:* add a syncobj error state (value reached plus an error
word, reported through kevent `fflags` or `data`), a per-queue watchdog
callout fed by A2.2, and a device-lost generation counter returned on every
ioctl after a reset.

**A2.7 Memory management is placeholder [S2].**
- VRAM is a bump allocator that **never frees** (`vram.c:21`). Each
  allocated scanout or texture permanently consumes VRAM until an FLR.
- VA per VM is a bump allocator over **32 MiB** that never reuses addresses
  (`atrium_gpu_amd.h`, `ATRIUM_AMD_VM_NUM_PT`). A long-running compositor
  runs out of VA.
- There's no CPU mmap of BOs (v2 `BO_MMAP_INFO` is missing). Every upload is
  a `copyin` through `BO_WRITE`, so frescod copies a full frame through the
  kernel every frame.
- The two-level, 4 KiB-page table is a gpusim simplification. RDNA uses
  multi-level tables with fragment/huge-page bits.
- There's no eviction policy (v2 §13h is still open).

*Recommendation:* use FreeBSD's **`vmem(9)`** for both the VRAM heap and
each VM's VA space. It's built for exactly this (arena, quantum cache, free
and coalesce) and keeps the driver BSD-native. Implement `BO_MMAP_INFO` via
`d_mmap_single` with a device-pager `vm_object`. That also gives revocation
(A2.8). Decide eviction as "none in V1: VRAM exhaustion returns `ENOMEM`,
userspace falls back to System." Write that down.

**A2.8 User-mode queues don't yet amount to a capability [S1].**
The doorbell `d_mmap` accepts **any offset inside the doorbell BAR**
(`cp.c:66-75`), so any device opener can map every queue's doorbell without
calling `QUEUE_MAP`. `QUEUE_MAP` reprograms the global queue onto the
caller's VM, taking it from whoever had it. The previous owner's existing
mapping isn't revoked, because `d_mmap` mappings can't be. "The doorbell
page is the capability" (v2 principle 11) isn't true yet.
*Recommendation:* tie doorbell pages to `queue_fd`s. Only the queue's owner
can mmap its page, through `d_mmap_single` with a per-queue pager object
that the kernel can invalidate on queue destroy or reset. That closes v2
§13k (revocation).

**A2.9 Concurrency [S2].**
One `sc->lock` covers almost everything, but multi-register MMIO sequences
that must be atomic run without it: queue programming plus doorbell
(`cp.c`), the submit-deadline register staged before the doorbell
(`ioctl.c:329`), and the display's `FB_BASE_LO/HI/SIZE` followed by flip.
`bo->bindings[]`/`n_bindings` are read and written unlocked
(`bo.c:371-405`), so concurrent `VM_BIND` of a shared BO races.
`GPU_RESET` can run while other threads are mid-submit. The TLB is
invalidated once per page under the global lock, which is O(pages) MMIO for
a large BO.
*Recommendation:* use a per-queue lock for the program, signal-point and
doorbell sequence, a per-BO lock for bindings, an `sx` lock to hold off
submits during reset, and one TLB flush per bind.

**A2.10 The implemented ABI skips v2's own evolution rules [S3].**
v2 principle 9 requires `uint32_t struct_size` as the first field of every
input struct. None of the `'A'` structs have it, and fds are declared as
`uint32_t`, so `-1` for "none" is awkward. This is cheap to fix now and
impossible to fix after there are stable userspace consumers. Do it before
anything outside the repo depends on `'A'`.

**A2.11 Firmware licensing isn't decided [S2].**
Real AMD hardware won't run without AMD's binary firmware (PSP, SMU, CP,
MES, DMCUB). `LICENSING-POLICY.md` excludes closed drivers but says nothing
about firmware. Most BSDs accept redistributable firmware blobs. Make the
decision explicitly before investing in silicon bring-up.

### A3. The isolation breaks, collected

These five block granting the `gpu` capability to more than one jailed app
on this driver:

1. Unlocked shared-queue reprogramming per submit lets one process's ring run
   under another's VMID (A2.3).
2. BO and VM teardown while the GPU is still using them (A2.1).
3. Doorbell mmap of any queue without a grant, and hijack through
   `QUEUE_MAP` (A2.8).
4. Scanout of arbitrary VRAM through a forgeable offset (A2.5).
5. Unprivileged device reset, scheduler and power control (A2.4).

The model's synchronous drain hides 1, 2 and the A2.2 mis-signalling,
because the work completes inside the doorbell write. Every one of them
appears on real asynchronous hardware.

### A4. The display side

The direction (decoupled engine, scanline-accurate model, atomic commit as
the target, kqueue vblank, connector types as queryable attributes with the
PHY routing opaque) is right. Gaps: there's no owner or lease model (A2.4),
no kernel validation (A2.5), and no flip fences yet. frescod still paces with
`sleep`, even though vblank-through-kqueue works. Sequence the atomic-commit
work behind the fd-based scanout import, so the commit takes fds from day
one.

### A5. gpusim → silicon: the seam is in the wrong place for bring-up

The backend vtable (`vm_setup`, `bo_alloc`, `map_page`, `submit`, …) is a good
seam for the *steady state*. Bring-up below it, though, is written against
registers that exist only in the model:

| Function | Today (gpusim) | Real RDNA4 |
|---|---|---|
| Firmware | `regFW_CP_VERSION`/`regFW_CP_LOAD` (SIM aperture) | PSP ring, signed TOC + per-IP ucode (PFP/ME/MEC/RLC/MES/SDMA/IMU) |
| Scheduler | `regMES_INIT`, direct `CP_HQD_*` programming | MES firmware + API packets (`SET_HW_RESOURCES`, `ADD_QUEUE`) on the MES ring |
| Per-context VM | `regVM_CTX_*` (SIM) | `GCVM_CONTEXTn_*` + multi-level PTE/PDE formats, process contexts via MES |
| IP discovery | none (fixed offsets) | IP discovery table in VRAM |
| Power | `PGATE`/`SCHED` blocks (SIM) | SMU message mailbox |
| Display | `regDISP_*` block (SIM) | DCN 4.x + DMUB firmware |

So "replace the gpusim backend with a hardware one behind the same front-end"
(`atrium-gpu-driver-architecture.md` §6) holds for the front-end, roughly
1,000 lines, and not for the 70–80% that is bring-up and per-IP programming.
*Recommendation:* (1) move gpusim toward the *real* interfaces, one block at
a time: the MES API ring first, then PTE formats, then the PSP ring, so the
driver code written against the model is the code that runs on silicon.
(2) Use the **MIT-licensed amdgpu headers** (`asic_reg/gc_12_*`, `mes_v12`
API structs, PM4 headers). `LICENSING-POLICY.md` permits them, and they're
the ground truth for the register map. (3) Budget real-silicon bring-up as
the main cost of D5, not a backend swap.

### A6. Testing

`tests/atrium_gpu_test.c` (2,040 lines) is a solid *functional* suite:
compute, draw, blend, depth, textures, DMA, user-mode queues, cross-queue,
fault/reset, VRAM, display, link training. But it's **single-process and
single-threaded, with no negative inputs**. Missing:

- **`test_gpu_isolation`**. `gpu-isolation.md` makes it the gate for the
  `gpu` capability and puts it at `atrium-kmod/test_gpu_isolation.c`. It
  doesn't exist. The attested entry in `etc/jaild.policy.toml` is for the
  *virtio* driver, with `isolation_test_commit = "0000000"`.
- Two processes submitting concurrently (it would catch A2.3 today).
- Close-while-busy for a BO, VM and syncobj (A2.1), and pending-FIFO
  overflow (A2.2).
- Negative and fuzz ioctl inputs (v2 §10.5 requires 24-hour fuzzing for
  production status).
- Waiters across a reset (A2.6).

*Recommendation:* write these tests first. Several will fail today, and that
pins down the work in A2.

### A7. Spec drift

- `atrium-gpu-abi-v2.md` was never amended with the reconciliation
  decisions D-1 and D-2 (it still specifies `SYNCOBJ_EVENTFD`, `BO_CREATE` on
  `vm_fd`, and "`'A'` is taken").
- `atrium-gpu-amd-design.md` describes per-open contexts, seqno fences, BO
  `IN_FLIGHT` pinning and a 2-second hang timeout as the design. None is
  implemented. Its §6 says "no per-process doorbells exposed to user," which
  v2 §5.9 and the code contradict.
- `gpu-isolation.md` is still written for handle namespaces and the
  virtio/venus path. Its driver table lists AMD as "spec-only."
- `atrium-gpu-driver-architecture.md` §4 versus §6 (see `GRAPHICS-STACK.md`
  §9 item 19).

### A8. Recommended order

1. **Decide in v2:** queue objects and MES process contexts (A2.3), per-queue
   seqno fences with deferred free (A2.1, A2.2), control/client split and
   display lease (A2.4), fd-based scanout import (A2.5), the error and
   device-lost model (A2.6), `struct_size` headers (A2.10), and the firmware
   policy (A2.11). Fold in D-1/D-2 at the same time.
2. **Write the failing tests** from §A6.
3. **Implement in this order:** per-queue fences → deferred free → `queue_fd`
   plus a per-queue lock → control/client split → fd-based scanout →
   `vmem(9)` allocators + `BO_MMAP_INFO` → watchdog and errors.
4. **Then** move gpusim toward real interfaces (§A5) and start the
   silicon-backed path.

---

## B. Tier-2 software Vulkan: maturity

### B1. What exists

| Piece | Size | Tests |
|---|---|---|
| `atrium-spv-frontend` (SPIR-V → IR) | 9.4K LoC | 18 unit |
| `atrium-spv-ir` | 1.5K | — |
| `atrium-spv-backend-bespoke` (ARM64) | 7.4K | 140 unit |
| `atrium-spv-backend-cranelift` | 3.3K | 33 |
| `atrium-spv-runtime`, `loader`, `blob`, `pcmap`, `compile` | 5.1K | 9 |
| `atrium-spv-tests` (SPIR-V interpreter oracle) | 2.6K | 18 |
| `atrium-spv-differential` (interpreter ↔ Cranelift ↔ bespoke) | 0.2K + tests | 54 |
| `aqueduct-gpu-host` Tier-2 backend + rasterizer | ~4.7K + | 84 |
| `atrium-vk-icd` | ~47K | 65 + **56 `loader_graphics_*` feature probes** (MSAA, mip, stencil, shadow/PCF, MRT, sRGB, points/lines, instancing, push constants, …) |

Milestones that landed: Vulkan 1.0–1.3 entry points; D.1–D.8 draws (vertex
input, depth, blend, indexed); the P1 tile-binned and damage-aware
rasterizer; P3a SIMD for large primitives; partial-update transport
(VM-verified); and **P4 level 2**, where `fresco-vulkan/examples/headless_icd.rs`
runs frescod's `HeadlessRenderer` through `atrium-vk-icd` + Tier-2 and
renders the atrium-core **rect** pipeline (compute → SSBO + atomic →
instanced draw), including the VS reading `instances[gl_InstanceIndex]`
from storage.

Measured performance (from `tier2-renderer-perf.md`, 14-core Mac): a
per-pass batched 4K frame is ~3.6–4.7 ms, inside the 8.33 ms 4K@120 budget,
for rect-heavy UI. A frame of 3,200 glyph quads is ~4.2 ms, about 2.2× slower
than tiny-skia. A damage-only frame is ~0.1 ms.

### B2. What I ran

On x86_64 Linux in this session (the repo's own `cargo test`, release mode):

| Crate | Result |
|---|---|
| `atrium-spv-frontend` | all targets pass (18) |
| `atrium-spv-tests` | **2 test targets don't compile** (`image_fetch`, `texture_sample`: `SamplerDesc` gained `compare_enable`/`compare_op`, tests weren't updated). The other 15 tests pass |
| `atrium-spv-backend-cranelift` | **2 test targets don't compile** (`emit_object`, `frontend_to_backend`: `Function` gained `scratch_bytes`). The 26 that compile **fail** with `ISA lookup: Support for this target is disabled`, because the backend only knows aarch64 FreeBSD and macOS (`src/lib.rs:86-110`) |
| `atrium-spv-backend-bespoke`, `-compile`, `-loader`, `-differential`, `aqueduct-gpu-host` Tier-2 | **don't build from a clean clone**: path dependency on `../../pptk/crates/pptk-codegen-arm64`, a sibling repo outside `atrium-os/atrium` |

Two conclusions follow. The suites aren't being run routinely (struct
changes went in without their tests being updated). And **Tier-2 is
aarch64-only**: both the bespoke and the Cranelift backends refuse x86_64,
although `tier2-renderer.md` promises "ARM64/x86_64." The RDNA4 target in §A
is a discrete card, which in practice means an **x86_64** machine, so today
Tier-2 can't run on the hardware the native driver is aimed at.

### B3. Strengths

- **The architecture is sound and tested the right way.** A three-way
  differential (independent SPIR-V interpreter as oracle, Cranelift, bespoke)
  is how shader compilers should be tested. Cross-tier certification
  compares Tier-2 pixels against Tier-3 Metal.
- **The AOT plus jailed-compiler design** (no JIT in the daemon, `.so` cache)
  fits Atrium's security model better than lavapipe's in-process LLVM JIT.
- **Feature breadth through the ICD** is wide for its age (the 56 probes).
- **Damage-first rendering** is already the dominant cost lever, which suits
  a compositor where most frames touch a few hundred pixels.

### B4. Gaps before it can be the only renderer under frescod

1. **Only the rect bundle is proven.** The `atrium-core` path and texture
   ops and the whole `atrium-text` (glyph) bundle haven't been shown
   rendering on Tier-2 through `HeadlessRenderer`. Text is most of a
   desktop's pixels.
2. **Known correctness bugs:** clears of `_SRGB` attachments are done in
   linear space; points, lines, fullscreen-fill and secondary-attachment
   stores still assume RGBA (BGRA swap only on the primary attachment). The
   doc lists these as "not blocking," but they're visible for a compositor
   that renders to `B8G8R8A8_SRGB`.
3. **Silent no-ops:** `DrawIndirect`, `DispatchIndirect`, `Blit` and
   `PipelineBarrier` fall into `_ => {}` in the frame walker
   (`aqueduct-gpu-host/src/tier2_backend.rs:1742-1751`, `3380`). An app that
   uses them gets wrong output with no error. frescod doesn't use them
   today, but any third-party app will. They should return an error until
   implemented.
4. **Test-harness shapes in production paths:** a fixed 4 KiB default
   compute-output SSBO capacity per dispatch, and a legacy "BindPipeline
   alone implies a fullscreen fill" fallback.
5. **No conformance testing.** No Vulkan CTS (dEQP-VK) subset, and the
   differential shaders are small hand-built rspirv programs. Neither the
   real bundle shaders (Slang output) nor any real-world shader corpus is
   tested differentially.
6. **Not built from a clean clone** (pptk path dependency, B2) and **not in
   CI.** The only workflow is `insula.yml`. The 56 ICD probes are
   `examples/` driven by a macOS shell script, not `cargo test`.
7. **No x86_64 at all.** The bespoke backend is ARM64-only, and the
   Cranelift "fallback" is hard-wired to aarch64 FreeBSD/macOS triples
   (B2). Adding x86_64 to Cranelift is mostly a triple and ABI-shim change,
   because Cranelift supports x86_64 natively. Do it before any x86_64 AMD
   bring-up, or the no-GPU tier won't exist there.
8. **Doc hygiene:** `tier2-renderer.md` says "No code yet." The perf doc
   cites commit hashes (`d31630d`, `71d767d`, `f86d52b`, …) and test suites
   ("106/106 differential_compute," "81/81 loader smoke," "78-rung") that
   don't resolve in the repo's history or tree.
9. **CPU cost:** on a laptop, a SW compositor at 4K burns cores that a GPU
   wouldn't. That's acceptable as a no-GPU tier and a bring-up path, and
   it's exactly why the kernel work in §A still matters.

### B5. Verdict and plan for "Tier-2 under frescod"

It's the right call, with lavapipe kept as a **test oracle** (pixel diff in
CI), not a runtime dependency. Suggested sequence:

1. **Make it buildable and continuously tested:** fix the four test
   targets that no longer compile, vendor or publish `pptk-codegen-arm64`
   (or make the bespoke backend an optional feature), enable Cranelift on
   x86_64, then add a CI job running the spv crates, the differential suite, the
   Tier-2 backend tests, and the ICD probes as `cargo test` against a
   `--backend tier2` daemon.
2. **Finish bundle parity:** texture, path and `atrium-text` glyph pipelines
   through `headless_icd`, each with a golden image and a lavapipe pixel
   diff.
3. **Fix the sRGB clear and BGRA store bugs. Turn silent no-ops into
   errors.**
4. **Run frescod in the VM with `VK_DRIVER_FILES` pointed at
   `atrium-vk-icd`,** against an in-VM `aqueduct-gpu-host --backend tier2`.
   Remove the LLVM mount from frescod's jail policy once it passes.
5. **Add a curated dEQP-VK subset** (the API frescod uses plus core
   rasterization) as the long-term correctness bar for third-party apps.
6. **Later, consider the direct route.** The perf doc notes
   `frescod-aqueduct` can drive Tier-2 without going through Vulkan. Once
   frescod main is the only compositor, decide whether the compositor should
   skip the ICD hop. Keep the ICD for third-party apps either way.

