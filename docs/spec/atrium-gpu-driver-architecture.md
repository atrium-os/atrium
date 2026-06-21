# Atrium GPU driver architecture — stance & open deltas

**Status:** notes, 2026-06-08. Non-binding. Feeds the binding spec
[`atrium-gpu-abi-v2.md`](atrium-gpu-abi-v2.md); grounded by
[`drm-research-findings.md`](drm-research-findings.md). Companion to the AMD
module design [`atrium-gpu-amd-design.md`](atrium-gpu-amd-design.md) and the
gpusim functional model (separate repo).

This captures the driver-architecture position settled in discussion, the
parts the existing v2 ABI already gets right, and the few **deltas** worth
folding into the next ABI revision. It is deliberately short — the full
design lives in the two documents above.

## 1. Principle: evidence, not authority

amdgpu (and the post-2020 Xe / Asahi consensus) is *evidence* of what a mature
multi-vendor stack converges on — not a model to copy. We take an idea because
it serves Atrium's goals (energy, capability isolation, BSD-native), and reject
it otherwise. We are not porting amdgpu; we want "amdgpu's good ideas expressed
through Portcullis capabilities + kqueue + the energy router."

**Neutrality test.** The kernel ABI must be vendor-neutral. The brutal check is
Apple AGX (tile-based, deferred, a totally different command model from AMD's
immediate-mode PM4). If the ABI survives a hypothetical AGX backend unchanged,
it is neutral enough. The proven boundary: **the kernel ABI owns
resources/VM/queues/sync; the command-stream encoding is opaque to it** and
lives in the per-vendor userspace driver. Never put a command IR in the kernel.

## 2. What v2 already gets right (re-validated)

The discussion re-derived much of `atrium-gpu-abi-v2.md`; that convergence is a
good signal. Already designed there, keep as-is:

- **Opaque POD-blob submit** (§7 of v2): the kernel validates structure and
  hands the vendor command stream to hardware/firmware; it never parses it.
- **No kernel scheduler** (v2 principle 7): MES / GuC / ASC schedule in
  firmware; the kernel surfaces submissions and tracks completions. Vendors
  with older hardware do SW scheduling *inside their module*, not in the ABI.
- **kqueue-native sync** (v2 §6, §9): timeline syncobj `event_fd` registered
  with `EVFILT_READ`; a frescod compositor waits on GPU-done + socket + input
  in one `kevent()`. This is the BSD-native answer and it is already the design
  — superseding any blocking-wait ioctl.
- **Capsicum/capability-clean** (v2 principles 8, 2): fd-as-handle, every op
  reachable from the caller's fd table, no lookup-by-name/pid. This is the
  substrate Portcullis needs.
- **VM_BIND, explicit-sync only, vendor-per-cdev, no DRM/GEM/TTM/KMS.**

## 3. Deltas to fold into the next ABI revision

Three things the v2 draft does not yet cover (it predates them or they live in
a sibling subsystem):

### 3a. User-mode queues = doorbell-as-capability

amdgpu landed user-mode queues in Linux 6.16 (RDNA3/GFX11 + **RDNA4/GFX12**,
our target). Userspace writes its command stream into its own ring, bumps the
write pointer, and **rings the doorbell directly** — bypassing a per-submit
ioctl and the in-kernel path. The kernel still owns memory validation,
VMID/security, MQD translation, run-list integration, and reset
(docs.kernel.org/gpu/amdgpu/userq.html).

Take this, for two Atrium-specific reasons (not because AMD did it):

- **Energy.** A syscall + scheduler trip per submit is overhead the Tier-2/3
  router cannot reclaim. Doorbell submission removes it from the hot path.
- **It *is* a capability.** A GPU queue is a VM-isolated context (its own VMID
  + page tables). Mapping that queue's doorbell page into a jail **is** the
  capability grant; the jail can ring only its own doorbell and reach only its
  own VM — default-deny by construction. amdgpu retrofits user-queues onto a
  permission model that predates them; we have the capability model first and
  user-queues fall out of it.

**ABI implication:** support **two submit paths behind one ABI** — (1)
kernel-mediated submit (the bring-up path, and the fallback for hardware
without user-queues) and (2) a user-mode-queue path that `mmap`s a doorbell
page (via the existing `d_mmap_single` + `vm_object_t` mechanism v2 already
uses for BOs) into the holder. Same resource/VM/sync ABI underneath. v2's
submit section should describe the doorbell-mmap path as a first-class option,
gated by a capability bit, not just the ioctl submit.

### 3b. Energy-router telemetry

The GPU is one tier in the energy router (Tier-2 software renderer vs Tier-3
GPU; see [`energy-policy.md`](energy-policy.md), "coordinated, not coupled").
The router must be able to decide *whether to use the GPU at all*. Nothing in
the GPU ABI exposes this today.

**ABI implication:** a small **read-only** telemetry surface (queue occupancy,
power/clock state, a tier-cost hint) the router can sample. Read-only and
advisory — it shares energy *intent*, it does not let the router drive the
driver (the coupling the energy policy forbids). amdgpu has no equivalent
because a Linux driver has no notion of "a cheaper non-GPU path exists." This
is Atrium-specific differentiation.

## 4. Implementation reality (what exists vs the v2 target)

The `atrium-gpu-amd` kmod (M1–M8, developed against the gpusim functional
model; see `atrium-gpu-amd/`) implements a **bring-up ABI**, not v2:
`/dev/atrium-gpu0` with integer BO handles, `BO_ALLOC/WRITE/READ`,
`SET_COMPUTE`, `SET_DRAW`, `SUBMIT`, `GET_IRQS`, `WAIT_FENCE`. It proved the
full stack end-to-end (PCI → firmware/MES → GPUVM → compute → graphics → MSI-X
→ blocking fence-wait), which is its job: develop the kernel mechanism against
a model with no silicon.

It is **not** the v2 surface, and should converge to it:

| current bring-up ABI | converges to (v2) |
| --- | --- |
| integer BO handles | **fd-as-handle** (bo_fd / vm_fd / syncobj_fd; SCM_RIGHTS) |
| `WAIT_FENCE` blocking ioctl | **timeline syncobj `event_fd`** + `EVFILT_READ` |
| implicit single VM | **VM_BIND** explicit address-space ops |
| `SET_COMPUTE` / `SET_DRAW` ioctls | **folded into the opaque submit blob** |
| kernel rings doorbell in `SUBMIT` | + **doorbell-mmap user-mode-queue path** (§3a) |

The `SET_COMPUTE`/`SET_DRAW` ioctls deserve a specific note: they exist only
because **gpusim models shader/draw state as MMIO registers** (a model
simplification). On real hardware that state goes *into the command stream*
(e.g. PM4 `SET_SH_REG`), not separate ioctls. So generalizing toward v2 (and
toward multi-vendor) actually **shrinks** the ABI — `SUBMIT` with an opaque
blob becomes the only submission path. These two ioctls are a gpusim-ism to
delete at the model→silicon / v1→v2 boundary, not an ABI to preserve.

## 5. Summary position

- Kernel owns **memory + isolation** (non-negotiable; it is the Portcullis
  boundary). It gets **out of the per-submit hot path** where hardware allows
  (doorbell capabilities) — for energy and for clean capability semantics.
- The ABI is **vendor-neutral around resources/VM/queues/sync**; the command
  stream is **opaque**. Validate neutrality against AGX, not just AMD.
- Build kernel-mediated submit first (done, M1–M8), add user-mode queues as a
  capability-gated path, converge the bring-up ABI to v2 (fd-handles, timeline
  `event_fd`, VM_BIND, opaque-blob submit).
- Differentiate where amdgpu structurally cannot: **kqueue-native sync**
  (already in v2) and **energy-router telemetry** (delta §3b).

## 6. Backends behind the shared ABI — virtio is a stepping stone, not a separate driver

§4's convergence has **landed** in `atrium-gpu-amd`: fd-as-handle BOs/VMs/syncobjs
(SCM_RIGHTS-passable), explicit `VM_BIND` (now into *multiple* VMs — sharing),
kqueue-able syncobjs (no blocking `WAIT_FENCE`/eventfd), opaque PM4-blob submit
(`SET_COMPUTE`/`SET_DRAW` deleted), and a caps TLV (`ADDRESS_SPACE`/`HEAPS`). So
`/dev/atrium-gpu0` already *is* the v2 `'A'` surface, validated against the gpusim
model of real AMD silicon.

**The backend seam's primary purpose is the gpusim → real-silicon swap, NOT
virtio.** `gpusim`+`atrium-gpu-amd` already *is* the full hardware-driver path
end to end: the real `'A'`/`'D'` driver drives gpusim (a functional RDNA model),
and gpusim's QEMU device registers a graphic console (`graphic_console_init` +
`gpusim_gfx_update`, which bulk-reads the VRAM scanout and blits it), so with
`run-vm.sh --display` the driver produces a **visible** display with no guest
paravirt driver anywhere. That is the whole reason gpusim exists — stock QEMU
has no gpusim, so virtio/vmwgfx were only ever needed in its absence. When real
AMD silicon arrives, you replace the gpusim backend (the `amd_*` ops driving the
model socket) with a hardware one **behind the same front-end** — same ABI, same
`amd_smoke`. That is the convergence the seam serves.

**A virtio backend is therefore OPTIONAL — a parallel/backup transport, not a
required deliverable.** It happens to fit the same vtable for free (a second ops
table over `CTX_INIT`/blob/`SUBMIT_3D`), so it can be added if we ever want to
run on a stock hypervisor without gpusim, or as a fallback. But the main path is
the real driver over gpusim (today) / real silicon (later); we do not need to
build virtio to have a full hardware driver end to end.

### 6.1 The seam — shared front-end over a backend-ops vtable

What is **transport-neutral** (the shared `'A'` front-end — today this code lives
in `atrium-gpu-amd` and is extracted for reuse):

- the cdev + `d_ioctl` dispatch (`/dev/atrium-gpu0`, the `'A'` switch);
- the fd-object lifecycle — `BO`/`VM`/`Syncobj` as `struct file`s, `DFLAG_PASSABLE`,
  the `*_fget` helpers, refcount/teardown;
- the per-BO **bindings list** bookkeeping (a BO bound into N VMs);
- the **syncobj timeline** + `knlist` (kqueue delivery);
- `BO_WRITE`/`BO_READ` CPU copies; the **caps TLV** assembly.

What is **per-backend** (the `atrium_gpu_backend_ops` vtable — the *only* thing a
new transport/vendor implements):

| op | amd (gpusim → real AMD) | virtio-gpu |
| --- | --- | --- |
| `vm_create` / `vm_destroy` | alloc VMID + GPUVM page tables | `CTX_INIT` (a 3D context) |
| `bo_alloc(size, flags)` | `bus_dma` System / VRAM pages | `RESOURCE_CREATE_BLOB` (guest pages) |
| `bo_map(bo, vm, *va)` | write PTEs into the VM's page table | attach resource to context, host assigns VA |
| `bo_unmap(bo, vm)` | clear PTEs | detach resource |
| `submit(vm, blob, len, engine, signal)` | lay PM4 ring + ring doorbell | `SUBMIT_3D` on the ctrl virtqueue |
| `syncobj_signal` / fence retire | `RELEASE_MEM` fence + IH IRQ | virtio-gpu fence retirement |
| `scanout(fb)` | display module `FB_BASE` | `SET_SCANOUT` |
| caps values | RDNA/gpusim heaps, 32 MiB VA | virtio-gpu blob heaps, host VA |

The two facts that make this hold: the **submit blob is opaque** (amd packs PM4;
virtio/venus packs a Vulkan command stream — the ABI never inspects it), and the
**GPU-VA is backend-assigned** (amd from its page-table allocator; virtio from the
host's device-address space). `VM_BIND`-into-many-VMs is a front-end concept;
each backend realizes it its own way (amd: extra PTE sets; virtio: extra context
attaches).

### 6.2 Extraction plan (incremental, each step verifiable)

1. ✅ **DONE** — **`atrium_gpu_backend_ops`** defined and the gpusim/PM4/GPUVM
   hardware paths moved behind it (amd = the first backend): `vm_setup`/teardown,
   `bo_alloc`/free, `map`/`unmap_page`, `submit`, `export_scanout`, `mmap`,
   `get_caps`, + optional `queue_program`/`gpu_reset`/`sched`/`powergate`. The
   front-end (`ioctl.c` + cdevsw, the fd objects, bindings, syncobj, caps) calls
   ONLY the vtable. Verified by `amd_smoke`/`bo_share`/`display_flip` throughout.
2. (optional, good hygiene) **Split the front-end into a shared source set** so a
   second backend kmod can build it — only worth doing if/when a second backend
   is actually built.
3. **(OPTIONAL — a virtio backend is a backup transport, not required; see §6
   above.)** Write it as a second ops table (`vm_create`→`CTX_INIT`,
   `bo_alloc`→blob, …) only if we want to run without gpusim. The *required* next
   step is instead the **gpusim → real-silicon** backend swap when hardware
   exists — same front-end, a hardware ops table replacing the model-socket one.
4. Retire the legacy `'G'` surface once no caller remains (the aqueduct smokes,
   per the reconciliation doc D-3).
