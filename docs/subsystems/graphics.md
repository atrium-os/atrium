# Subsystem — Graphics (Fresco protocol + Atrium GPU ABI)

> See [NAMING.md](../NAMING.md) for component naming.

## Layered design

Three layers, each with its own scope and ABI.

### Layer 1 — Fresco protocol (userspace ↔ userspace, app ↔ server)

Retained-mode, content-addressed scenegraph protocol. Already implemented and stable; normative spec at [../spec/wire-format.md](../spec/wire-format.md) (v0.1; freeze for D2 at rollout M1, full standardization at D7). Reference implementations: `fresco-server/src/command/protocol.rs`, `libfresco/src/protocol.h`.

Salient properties:

- **Retained-mode**: server holds the scene tree. Clients send mutations, not full re-uploads.
- **Content-addressed**: blobs (vertex data, textures, materials, paths) are SHA-256-keyed. Identical content = same hash = stored once. Cross-process, cross-client.
- **Slot graph**: clients allocate slots; each slot holds a transform + content hash + flags + clip rect. Mutation = update one field of one slot.
- **Per-client isolation**: each client open() of `/dev/fresco0` gets a slot index from a kmod bitmap. Per-slot cmd/comp/input rings in shmem. No cross-client data leak.
- **Window ownership**: server tracks window owner = client_id. All window-affecting ops checked against ownership.

The protocol is **transport-agnostic** — see [transport.md](transport.md). Today it runs on ivshmem (QEMU). Native FreeBSD will use a host-local cdev. Remote desktop will use TCP / QUIC.

### Layer 2 — fresco-server (privileged userspace process)

Owns the scene tree, the GPU, the input devices. Single process for the whole desktop. Equivalent in role to Wayland compositor + X server + Mesa userspace driver, **fused into one**.

- Receives Fresco protocol commands from clients.
- Maintains scene tree + content-addressed store.
- Runs WM (decorations, focus, drag/resize/close).
- Compositing: per-window FBO + screen pass.
- Backend: emits commands native to the target GPU.

Architecturally this is a `GpuBackend` trait with multiple implementations:

- `metal_backend` — today, macOS host (development env).
- `vulkan_backend` — transitional, runs on FreeBSD with Vulkan-via-linuxkpi-amdgpu. Useful as a fallback for hardware D0 doesn't cover.
- `virtio_backend` — D0 target. Native FreeBSD, no linuxkpi.
- `<vendor>_backend` — eventually per-GPU (AMD, ARM Mali, Apple, etc.).

### Layer 3 — Native FreeBSD kernel GPU drivers

This is the layer that **replaces linuxkpi+drm-kmod for our targets**. See "Why this is human-scale work" below.

Two cdevs:

- **`/dev/atrium-gpu0`** — submission, memory, fences. Per-client GPU access mediated by capability (post-D2.5).
- **`/dev/atrium-display0`** — modesetting, vblank, page flip. Owned by fresco-server.

Each is fronted by a per-vendor driver: `atrium-virtio-gpu`, `atrium-mali`, `atrium-amdgpu`, ...

## Why this is human-scale work (replacing linuxkpi)

> Restated 2026-06-10. The original (D0-era) version of this section argued the whole stack was small because "fresco-server is the only GPU consumer, fixed shader set, no shader compiler, no buffer sharing." The platform has since grown a Vulkan-class third-party path (aqueduct-gpu, Tier-2, atrium-mesa), so the *userspace* surface is no longer tiny. The human-scale claim survives because it was always really about the **kernel** boundary — restated precisely below.

Replacing linuxkpi+drm-kmod **with Vulkan-feature-parity native kernel drivers in the Linux DRM shape** is millions of LoC per GPU. Nobody is going to do that.

Atrium's resolution splits the problem across the kernel/userspace fault line:

- **The kernel ABI stays small and vendor-neutral** ([../spec/atrium-gpu-abi-v2.md](../spec/atrium-gpu-abi-v2.md)): seven fd-backed object types (device, VM, BO, queue, timeline syncobj, share handle, event fd), VM_BIND, **opaque POD-blob submit** (the kernel never parses command streams), kqueue delivery, optional user-mode doorbell queues as capability grants. A per-vendor kernel module is explicit-state-machine C with linear control flow — atrium-gpu-amd targets **~6–8K LoC for the GPU module** ([../spec/atrium-gpu-amd-design.md](../spec/atrium-gpu-amd-design.md)). The display engine module is the bigger unknown (display is the largest part of vendor DRM drivers); the scanline-accurate device model in [../spec/atrium-display-architecture.md](../spec/atrium-display-architecture.md) exists to de-risk exactly that.
- **The userspace complexity is inherited, not authored.** The atrium-mesa fork (D5) keeps Mesa's Vulkan drivers and compiler stack — ~10 engineering-years of GPU-compiler IP under MIT — with libdrm coupling replaced by Atrium GPU ABI calls, running at build/install time and inside trusted daemons, never as a per-app runtime driver instance.
- **No DRM framework in the kernel.** No GEM, no TTM, no KMS helper stack, no connector/encoder/CRTC midlayer. The ABI is a documented contract (ioctl numbers + structs), not shared kernel code. Cross-process / cross-device buffer sharing exists but as a *vendor-neutral fd* (`share_fd`, dma-buf-equivalent), and display modesetting is an atomic-commit ABI on its own cdev — both specified as contracts, not frameworks.
- **No per-app shader compilation at runtime.** All shaders — frescod's internal set and third-party SPIR-V bundles alike — are AOT-compiled at install time (jailed compiler sub-process; [../spec/tier2-renderer.md](../spec/tier2-renderer.md)) with an on-demand cold path. No JIT in the hot path, no app-supplied shaders inside the scene server's trust boundary.

So the per-vendor *kernel* work is months, not decades; the userspace Vulkan stack is a pruning-and-retargeting of Mesa, not a rewrite.

## Atrium GPU ABI

Two generations:

- **Bring-up ABI** ([../spec/gpu-abi.md](../spec/gpu-abi.md)) — the original D0 twelve-ioctl set (int handles, `WAIT_FENCE`); what `atrium-virtio-gpu` shipped against. Constants prefixed `ATRIUM_GPU_*` in `<atrium/gpu.h>`.
- **ABI v2** ([../spec/atrium-gpu-abi-v2.md](../spec/atrium-gpu-abi-v2.md)) — the convergence target: fd-as-handle (Capsicum-clean, `SCM_RIGHTS`-shareable), explicit per-process GPU VM + VM_BIND, timeline-only sync with kqueue event fds, opaque submit blobs, firmware scheduling, energy telemetry. The bring-up ABI's `SET_COMPUTE`/`SET_DRAW`-style ops fold into the opaque submit blob as drivers converge.

## Backend strategy by target

| Target | Layer-3 driver | Layer-2 backend | Effort |
|---|---|---|---|
| QEMU dev (current) | (transport-only kmod) | metal_backend (host macOS) | done |
| QEMU prod | atrium-virtio-gpu (D0) | virtio_backend (D0) | 3–4 months |
| Raspberry Pi 5 / VideoCore | atrium-vc7 | per-target backend | ~6 months |
| ARM Mali (via Panfrost docs) | atrium-mali | per-target backend | ~6 months |
| AMD RDNA (with vendor cooperation) | atrium-amdgpu | per-target backend | ~12 months |
| Intel Xe | (via partnership) | per-target backend | ~12 months |
| Apple Silicon (Asahi-style RE) | atrium-agx | per-target backend | multi-year |
| NVIDIA (RE-only, no vendor) | atrium-nvidia | per-target backend | multi-year |

Until each target has a native driver, that hardware uses the **Vulkan-via-linuxkpi fallback** — fresco-server uses linuxkpi+amdgpu+Vulkan as a transitional path. This is **not** the architectural target, but it lets users with covered-only-by-linuxkpi hardware run Fresco today.

Over time, the linuxkpi fallback shrinks as native drivers cover more hardware. It is excised at D5 (atrium-mesa + atrium-gpu-amd) — drm-kmod is the runtime's only GPL dependency and is grandfathered exactly until then ([../LICENSING-POLICY.md](../LICENSING-POLICY.md)).

## Where Vulkan fits (updated 2026-06-10)

Two distinct surfaces, often conflated:

**1. The scene path (Fresco protocol).** Apps emit scenegraph
commands — rects, paths, textures, glyph runs, transforms — never
GPU commands. The server-internal renderer for this path is a
small fixed set: triangle rendering, texture sampling,
stencil-fill/cover path rendering, solid/gradient/textured
materials, per-window FBO + screen passes, cursor. This covers
the UI / document / productivity workloads, and is the path with
the retained-mode bandwidth and isolation properties.

**2. The engine path (aqueduct-gpu).** Vulkan-class workloads —
game engines, Servo/WebRender, compute — are *not* excluded; they
ride [../spec/aqueduct-gpu.md](../spec/aqueduct-gpu.md):
frame-batched submission over the same envelope wire, **SPIR-V
bundles AOT-compiled at install time** through a universal
sandbox (static validation, per-bundle descriptor namespaces,
hardware TDR, per-connection quotas), rendering into surfaces the
compositor composes. Execution routes between Tier-2 (bespoke
BSD-native software codegen, [../spec/tier2-renderer.md](../spec/tier2-renderer.md))
and Tier-3 (hardware via atrium-mesa) by the energy-aware router
([../spec/energy-policy.md](../spec/energy-policy.md)).

What remains genuinely out: apps shipping custom shaders *into
the scene path* (frescod's renderer stays a closed set; engine
extensions are curated SPIR-V bundles, not arbitrary app
uploads), and unmodified ports that demand a system Vulkan ICD
loader in every process — structurally precluded, intentionally
(see fresco-rendering-stack.md §1 for the boundary argument).

The old "compute as a future `CMD_SPAWN_TASK` protocol extension"
sketch is realized by aqueduct-gpu; the autonomous-task opcodes
(`0x0200`–`0x0202` in wire-format.md) remain for scene-coupled
compute, while engine compute goes through aqueduct-gpu bundles.

## Cross-references

- [Wire-format spec](../spec/wire-format.md) — opcodes, payload layouts.
- [GPU ABI spec](../spec/gpu-abi.md) — bring-up kernel/userspace boundary; [ABI v2](../spec/atrium-gpu-abi-v2.md) — the convergence target.
- [aqueduct-gpu](../spec/aqueduct-gpu.md) — the engine path (Vulkan-class workloads over the envelope wire).
- [Rendering stack](../spec/fresco-rendering-stack.md) — the scene-level boundary argument and the three-layer contract.
- [Crash recovery + CAS budgets](../spec/fresco-recovery.md) — frescod restartability, per-client CAS budgets.
- [sandbox.md](sandbox.md) — how per-app isolation interacts with GPU access.
- [transport.md](transport.md) — protocol over different transports.
