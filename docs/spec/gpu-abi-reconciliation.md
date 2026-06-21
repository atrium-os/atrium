# Atrium GPU/Display ABI — Reconciliation & Convergence Plan

> Status: **decision record** (2026-06-19). This is the single source of truth for
> *which* GPU/display ABI is canonical and *how* every component converges to it.
> It does not restate the ABIs — it points at the canonical specs, resolves the
> contradictions between the surfaces that exist today, and lays out the migration.

## 0. Why this document

Several GPU/display ABI surfaces were written and implemented at different points:

- `gpu-abi.md` (v0.1, 2026-04-28) — the first kernel↔userspace GPU ABI.
- `atrium-gpu-abi-v2.md` (2026-06-10) — the long-term ABI, written to "lock in the
  shape before a second vendor ports."
- `aqueduct-gpu.md` (2026-06-02) — transport + an early display/scanout sketch.
- `atrium-display-architecture.md` (2026-06-15) — the settled display design.
- Concrete code: `atrium-kmod/atrium_gpu.h` (group `'G'`, handle BOs — the virtio
  kmod), `atrium-gpu-amd/atrium_gpu_amd_abi.h` + `atrium_display_abi.h` (group
  `'A'`/`'D'`, fd BOs — the from-scratch driver), and `atrium-gpu-rs` (Rust
  userspace, currently mirroring the `'G'` surface).

They are **not contradictory by intent — they are a lineage** with an already-named
convergence target. This document makes that explicit so we stop adding adapters and
move every component onto one ABI.

## 1. The decision

1. **Canonical GPU ABI = `atrium-gpu-abi-v2.md`** (object model: every kernel object
   is an fd; per-process VM via `VM_CREATE`/`VM_BIND`; opaque-blob `SUBMIT`; timeline
   syncobjs; user-mode queues via doorbell mmap; TLV `QUERY_CAPS`). ioctl group `'A'`.
2. **Canonical display ABI = `atrium-display-architecture.md`** (separate cdev;
   scanout buffer crosses GPU→display as a lowered handle — `{vram_offset,size}`
   today, a `share_fd` in the general/cross-device case; **target** shape is
   atomic-commit + kqueue vblank/flip-done + syncobj in/out fences). ioctl group `'D'`.
3. **One userspace ABI, three backends.** `atrium-gpu-rs` exposes exactly the v2
   surface. **All three backends implement it**: `atrium-gpu-amd` (gpusim / real
   silicon), `atrium-virtio-gpu` (virtio transport), and Carillon (paravirt→Metal).
   Userspace is backend-agnostic; the kmod a client opened is invisible above the ABI.
4. **`atrium-gpu-amd` is the reference implementation.** It already implements ~80%
   of v2 (fd BOs, `VM_CREATE`/`VM_BIND`, timeline syncobjs that are *directly*
   kqueue-able, doorbell UMQ, TLV caps, dma-buf-style display). Where the working
   implementation improved on the v2 draft, **the implementation wins and v2 is
   amended** (see §4). The `'G'` virtio surface and `gpu-abi.md` v0.1 are superseded.

## 2. Supersession lineage

| Spec / surface | Status |
|---|---|
| `atrium-gpu-abi-v2.md` | **CANONICAL** (GPU), amended per §4 |
| `atrium-display-architecture.md` | **CANONICAL** (display) |
| `gpu-abi.md` (v0.1) | **SUPERSEDED** by v2 (was the bring-up GPU ABI) |
| `aqueduct-gpu.md` — *transport* sections | live (Carillon/venus transport) |
| `aqueduct-gpu.md` — *display* sections (§6.5.x: `BIND_GPU`, `scanout_handle`, `WAIT_VBLANK`) | **SUPERSEDED** by `atrium-display-architecture.md` |
| `atrium-kmod/atrium_gpu.h` (`'G'`, handle BOs) | **LEGACY** — migrate to `'A'` (§5) |
| `atrium-gpu-amd/*_abi.h` (`'A'`/`'D'`) | **REFERENCE** — finish v2 deltas (§5) |
| `atrium-gpu-rs` (mirrors `'G'`) | **MIGRATE** to v2 `'A'` (§5) |
| `tier2-renderer.md` (AOT software Vulkan) | **CANONICAL** for Tier-2 (§8) |
| `aqueduct-gpu.md` §6.5.1 — *"Tier-2 = deferred llvmpipe"* | **SUPERSEDED** by `tier2-renderer.md` (§8) |
| `fresco-rendering-stack.md` (tier model) | live — the Tier-1/2/3 + router model (§8) |

## 3. The unified GPU ABI (group `'A'`)

Object model (v2): `device_fd` → `VM_CREATE` → `vm_fd`; BOs/queues/syncobjs are fds;
SCM_RIGHTS-passable; Capsicum-clean (no ambient "submit to the GPU" authority — the
capability is the mapped doorbell page).

| Capability | Canonical (v2 `'A'`) | amd today | virtio `'G'` today |
|---|---|---|---|
| Caps | `QUERY_CAPS` TLV | ✓ `'A',15` | fixed struct `CAPS 'G',7` → **port to TLV** |
| Address space | `VM_CREATE` → vm_fd | ✓ `'A',12` | **absent → add** |
| Bind | `VM_BIND` (async, persistent) | ✓ `'A',13` | **absent → add** |
| BO create | `BO_CREATE` → bo_fd (VM-agnostic until bind) | `BO_ALLOC 'A',0` (fd) | `ALLOC 'G',1` → **handle → fd** |
| BO map | `BO_MMAP_INFO` → offset, `mmap(device_fd)` | xfer via `BO_WRITE/READ 'A',1/2` (+ mmap d_mmap) | `mmap_offset` in alloc |
| Submit | `SUBMIT(opaque blob)` on queue_fd | `SUBMIT 'A',4` (ring_fd + n_dwords) → **converge to queue_fd + blob** | `SUBMIT 'G',4` (cmd_handle) → **port** |
| Queues | `QUEUE_CREATE` → queue_fd; `QUEUE_MAP` (UMQ) | `QUEUE_MAP 'A',14` (no queue_fd yet) → **add queue_fd** | **absent → add** |
| Sync | timeline syncobj fd | `SYNCOBJ_CREATE/SIGNAL/QUERY/WAIT 'A',8–11` | int fences `FENCE_WAIT/QUERY 'G',5/6` → **replace** |
| Completion→kqueue | the syncobj fd is `EVFILT_READ`-able | ✓ (direct) | **absent → add** |
| Share | `BO_EXPORT_SHARE`/`BO_IMPORT_SHARE` (cross-VM/-device) | `BO_EXPORT_SCANOUT 'A',27` (display lowering) + `MINT/IMPORT` exists in `'G'` | `MINT_TOKEN/IMPORT_REGION 'G',0x47/0x48` |
| Telemetry | `QUERY_TELEMETRY` | `SCHED 'A',25`, `POWERGATE 'A',26` | **absent** |

### Deltas where v2 had open questions / the impl improved it

These are reconciled here and v2 should be amended to match:

- **Syncobj kqueue: drop `SYNCOBJ_EVENTFD`.** v2 proposed a *separate* eventfd for
  kqueue. `atrium-gpu-amd` makes the **syncobj fd itself** `EVFILT_READ`-able (threshold
  in the kevent `data` field), which is simpler and is proven working in-VM. Canonical:
  the syncobj fd is the kqueue source; no separate eventfd object.
- **BO create is VM-agnostic.** A BO is created on `device_fd` (not `vm_fd`) and gains a
  GPU-VA only via `VM_BIND`. This is what lets one BO bind into multiple VMs and matches
  cross-VM share. (v2 §5.3 implied create-on-vm_fd; amend to device_fd.)
- **`SUBMIT` blob may *be* a ring BO.** The opaque blob and amd's "ring is a BO" are the
  same idea at two fidelities: kernel-mediated `SUBMIT` copies a POD blob; the UMQ path
  `QUEUE_MAP`s a ring BO the client writes blobs into. Both are vendor-opaque. Canonical:
  `SUBMIT(blob)` on a `queue_fd`; the ring-BO is the UMQ lowering of the same.
- **ioctl group letter is `'A'`** (v2 §13d left it open; `'A'` is the de-facto and stays).

## 4. The unified display ABI (group `'D'`)

Canonical model (`atrium-display-architecture.md`): the display is a **decoupled
engine** on its own cdev that scans out a shared VRAM buffer. The GPU→display handoff
is the **lowered buffer handle**: `{vram_offset,size}` for a same-device VRAM BO today
(`BO_EXPORT_SCANOUT`), generalizing to a `share_fd` (dma-buf-equiv) for the cross-device
case. **No `BIND_GPU`, no BO-handle resolution in the display kmod** — the offset is
absolute against the shared VRAM aperture (this is the lowering of the dma-buf import,
not a different model).

**Interim (implemented, `atrium_display_abi.h`)** → **Target (`atrium-display-architecture.md` §7):**

| Aspect | Interim (today) | Target |
|---|---|---|
| Scanout handoff | `{vram_offset,size}` via `BO_EXPORT_SCANOUT` | same; + `share_fd` for cross-device |
| Modeset/flip | `SET_MODE`/`PAGE_FLIP` ioctls (`'D',3/4`) | **atomic-commit** blob (one ioctl: FB×plane×CRTC×mode) |
| Vblank | `STATUS.vblank_count` poll (`'D',5`) | **kqueue `EVFILT_READ` on the display fd** (vblank + flip-done) |
| Flip sync | `vsync` bit | **syncobj in-fence** (wait render-done) + **out-fence** (prev FB free) |
| Connectors | single (`ENUM 'D',1`) | `connector_id`-keyed, multi (encoder/PHY crossbar, MST, USB-C) — already stubbed by `CONFIG/USBC/MST/DPTRAIN 'D',6–9` |
| Flip queue | depth-1 drop-and-count (`STATUS.dropped_flips`) | same policy |

**Migration note — `connector_id`:** the canonical ABI is `connector_id`-keyed even
while only one connector exists, because `atrium-gpu-rs`/frescod already pass it and
multi-connector is the documented next step. The interim single-connector ioctls add a
`connector_id` field (ignored/validated as 0 for now).

**ioctl-number conflict to resolve:** `'D',5` is `STATUS` (amd) vs `WAIT_VBLANK`/`CURSOR`
(virtio `'G'`). In the unified `'D'` namespace: `STATUS`=`'D',5`, vblank moves to the
kqueue path (no blocking ioctl), `CURSOR` takes a fresh number. `WAIT_VBLANK` is retired
(its callers migrate to kqueue; frescod already paces with `thread::sleep`).

## 5. Per-component convergence map

**A. `atrium-gpu-amd` (reference) — finish the v2 deltas.** Add `BO_CREATE` on
device_fd + `BO_MMAP_INFO`; add `QUEUE_CREATE`→queue_fd (UMQ already has `QUEUE_MAP`);
fold `SUBMIT` to queue_fd + blob; add `BO_EXPORT_SHARE`/`IMPORT_SHARE`; align caps TLV
ids. Display: add `connector_id`; then the atomic-commit + kqueue-vblank + syncobj-fence
evolution. (The current `'A'`/`'D'` surface stays valid through the transition.)

**B. `atrium-virtio-gpu` (`atrium_gpu.h` `'G'`) — port to `'A'`.** This is the biggest
lift: replace handle BOs with fd BOs, add `VM_CREATE`/`VM_BIND`, replace int fences with
timeline syncobjs (kqueue-able), opaque-blob `SUBMIT`, TLV caps, and the unified `'D'`
display surface (drop `BIND_GPU`/`scanout_handle`/`WAIT_VBLANK`/`CURSOR` for the
offset/atomic/kqueue model). The venus/region-sharing `'G',0x40+` ioctls map to
`BO_EXPORT_SHARE`/`IMPORT_SHARE` + the opaque submit blob.

**C. `atrium-gpu-rs` — re-target to v2 `'A'`.** Rewrite `src/abi.rs` to the `'A'`/`'D'`
ioctls + structs; update `src/lib.rs`: fd BOs, `Vm`/`Queue`/`Syncobj` fd types,
`Bo::export_scanout()`, `Display` on the offset model (drop `bind`; `set_mode`/`page_flip`
take the exported `{offset,size}`; `wait_vblank` → kqueue on the display fd). This is the
surface every backend then satisfies.

**D. `frescod` — minimal.** It already allocs a SCANOUT BO and calls
`set_mode`/`page_flip`; once `atrium-gpu-rs` exports the scanout internally, frescod drops
the `dpy.bind()` line. (Its `HeadlessRenderer` Vulkan dependency is a *separate* arc —
the ABI reconciliation does not address what renders the frame.)

## 6. Phased migration plan

1. **Lock the doc** (this file) + add supersession headers to the superseded specs.
2. ✅ **`atrium-gpu-rs` → v2 `'A'`** (GPU side: fd BOs, VM, syncobj, submit, caps) against
   the `atrium-gpu-amd` reference — DONE, verified in-VM by `amd_smoke` (caps TLV, compute
   INC, syncobj-via-kqueue, display enum) against the real driver.
3. ✅ **`atrium-gpu-rs` display → offset model** (`Bo::export_scanout` + offset
   `set_mode`/`page_flip`) — DONE. Added `amd::Scanout` (System staging BO → CP `DMA_DATA`
   → VRAM scanout BO → `{vram_offset,size}`); needed a multi-PT-page VM in the kmod
   (`ATRIUM_AMD_VM_NUM_PT`=16, 32 MiB VA) so a full-screen staging+scanout pair fits.
   Verified in-VM by `display_flip`. frescod's main path migrated (drops `bind`); the
   `frescod_aqueduct`/`aqueduct_smoke` bins stay on `'G'` until D-3 (kqueue), so the legacy
   `'G'` Display can't be retired yet.
4. **`atrium-gpu-amd` v2 finish** (`BO_CREATE`/`QUEUE_CREATE`/share + caps-TLV align).
   **share — DONE:** a BO binds into multiple VMs (per-VM bindings list), so one
   buffer is shared across address spaces (compositor imports a client buffer);
   BOs are `DFLAG_PASSABLE` for SCM_RIGHTS transport + `amd::Vm::import`. Verified
   by `bo_share` (CPU + GPU cross-VM). **caps-TLV — DONE:** `QUERY_CAPS` now also
   emits `ADDRESS_SPACE` (per-VM VA window — the real 32 MiB) + `HEAPS` (VRAM size),
   decoded by `amd::Caps` (TLV-forward-compatible). `BO_CREATE`/`QUEUE_CREATE`
   renames are cosmetic-only (semantics already match v2 — see D-1/D-2).
5. **`atrium-virtio-gpu` → `'A'`** (the large port; gated on a virtio test target).
6. **Display target evolution** (atomic-commit + kqueue vblank/flip-done + syncobj
   fences) across kmod + `atrium-gpu-rs`, once steps 2–5 are stable.

Steps 2–3 unblock the original goal (frescod first light on the amd display) without the
virtio port; step 5 makes the ABI truly backend-agnostic.

## 7. Open decisions (flag before they bite)

- **D-1: keep amd's directly-kqueue-able syncobj fd over v2's separate `SYNCOBJ_EVENTFD`?**
  ✅ **RESOLVED — yes.** Implemented + verified: a submit's `RELEASE_MEM` signals the
  syncobj, and `amd_smoke` waits on it with `EVFILT_READ` directly on the syncobj fd (no
  separate eventfd). v2 to drop `SYNCOBJ_EVENTFD`.
- **D-2: `BO_CREATE` on `device_fd` (VM-agnostic) vs `vm_fd` (v2 draft)?** ✅ **RESOLVED —
  device_fd.** amd creates BOs on the device fd, independent of any VM; `VM_BIND` maps them
  (into *many* VMs — the sharing work). v2 §5.3 to follow. (Name stays `BO_ALLOC`; a
  `BO_CREATE` rename is cosmetic, not done.)
- **D-3: display vblank — kqueue-only, retiring `WAIT_VBLANK`?** 🔶 **IN PROGRESS.** The
  device vblank *interrupt* now exists and is HW-faithful: gpusim raises a DCN-like vblank
  IRQ each vertical blank through the IH ring (cause `IH_CAUSE_VBLANK`, gpusim 942d96b),
  the driver arms it on `SET_MODE` (`regDISP_VBLANK_IRQ_EN`) and the IH ISR services it —
  verified interrupt-driven (~50/s, no submits), no longer polled-only. **Remaining:** the
  per-vblank `d_kqfilter` on `/dev/atrium-display0` so userspace `EVFILT_READ`-waits on
  vblank (and `WAIT_VBLANK` retires). That's the final Phase-6 display-timing piece.
- **D-4: does Carillon implement `'A'` directly, or front a backend that does?** Defer;
  Carillon is a transport — it should present the `'A'` cdev surface like the others.

## 8. The rendering tiers (orthogonal to this ABI)

The render *tiers* are a separate axis from the kmod ABI and were drifting too, so
they're reconciled here for one mental model. **A tier is how a GPU-dispatch endpoint
turns dispatch commands into pixels; the ABI (§3–§4) is the kmod↔userspace boundary
those pixels' buffers cross.** They are orthogonal: any tier feeds a scanout buffer,
and the display ABI scans it out.

**The tier model** (`fresco-rendering-stack.md`):

- **Tier-1** — tiny-skia CPU rasteriser of *Atrium's own* bundle ops (rect/path/text).
  `aqueduct-gpu-host/src/software/`. Battery/no-GPU/CI/bring-up.
- **Tier-2** — **AOT software Vulkan** for third-party SPIR-V: SPIR-V → `atrium-spv-ir`
  → a bespoke ARM64/x86_64 backend (+ Cranelift fallback) → native `.so`, dlopen'd; no
  JIT/interpreter in the hot path. Spec: **`tier2-renderer.md`** (the `atrium-spv-*`
  crates). This is full Vulkan executed on the CPU — *not* a tiny-skia compositor.
- **Tier-3** — hardware GPU: MoltenVK→Metal on macOS bring-up, native Vulkan on Linux
  dev, the native `atrium-gpu` driver on real HW (`carillon.md`).
- The **router** (`aqueduct-gpu-host/src/router.rs`) picks Tier-2 vs Tier-3 on a cost
  model (the energy axis); Tier-1 is the no-GPU floor.

**Tier-2 reconciliation:** `tier2-renderer.md` ("Design v2," AOT software Vulkan, code
in `atrium-spv-*`) is canonical. It supersedes `aqueduct-gpu.md` §6.5.1's framing of
Tier-2 as a deferred llvmpipe/lavapipe vendoring — that approach was rejected (it would
break "Mesa only at build time"); the bespoke AOT path is the live design.

**Two-phase endpoint model** (where the tiers run, and which path uses the display ABI):

- **Bring-up (today):** a guest app → Carillon wire → the `aqueduct-gpu-host` daemon
  **on the macOS host** → Tier-1/Tier-2/Tier-3(Metal) → presented on the host. The
  `/dev/atrium-display0` ABI is **not** on this path.
- **Native (D5+):** the `atrium-gpu` kmod **is** the endpoint — same dispatch protocol,
  no host daemon. Rendering lands in a VRAM scanout BO and `/dev/atrium-display0` scans
  it out. **The gpusim + `atrium-gpu-amd` work is this path, brought up against a
  functional model instead of silicon.** `tests/firstlight.c` proves it end to end: the
  GPU renders a frame into a VRAM BO, `BO_EXPORT_SCANOUT` lowers it to `{vram_offset,
  size}`, and the display ABI flips it.

**Consequence for "run a Vulkan app on the native display":** a Vulkan app (incl.
frescod's `HeadlessRenderer`, a direct Vulkan client) reaches the native path through a
Vulkan driver that targets the `'A'` submit ABI — that is the **Tier-3 native** driver
(a from-scratch ICD / atrium-mesa fork), a large arc of its own. Tier-2 (software Vulkan)
is a backend on the *daemon* endpoint, not a driver for the `atrium-gpu-amd` kmod, so it
does not by itself put a Vulkan app on `/dev/atrium-display0`. Until that driver exists,
the native display is driven by direct GPU dispatch (as `firstlight` does).
