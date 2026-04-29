# Subsystem — Graphics (Fresco protocol + Atrium GPU ABI)

> See [NAMING.md](../NAMING.md) for component naming.

## Layered design

Three layers, each with its own scope and ABI.

### Layer 1 — Fresco protocol (userspace ↔ userspace, app ↔ server)

Retained-mode, content-addressed scenegraph protocol. Already implemented and stable; documented in `fresco-server/src/command/protocol.rs`, `libfresco/src/protocol.h`. Wire-format spec freezing is a D7 task.

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

Replacing linuxkpi+drm-kmod **with Vulkan-feature-parity native drivers** is millions of LoC per GPU. Nobody is going to do that.

Replacing it **with what fresco-server actually needs** is a much smaller surface. The server requires:

1. Allocate GPU-visible memory (one ioctl + mmap).
2. Submit a command stream (one ioctl).
3. Wait for completion (kqueue integration).
4. Modeset (handful of ioctls).
5. Get interrupts as kqueue events.

That's it. ~5 ioctls + kqueue. Not a million-line DRM compatibility shim.

Why so much less:

- **No userspace driver per process.** Mesa-equivalent doesn't exist in this stack — fresco-server is the only consumer. No need for shared shader compiler, GLX/EGL plumbing, libdrm context juggling.
- **No GEM / dma-buf.** Memory objects don't need to cross process boundaries; the server allocates, the server uses.
- **No KMS atomic modesetting framework.** Direct page-flip ioctl is fine — server is the only entity that programs displays.
- **No DRM connector / encoder / CRTC abstraction.** Talk directly to the hardware's display engine.
- **No shader compiler.** Server pre-compiles a small shader set at startup; no per-app shader compilation.

A "Fresco-sufficient" driver for one GPU family is on the order of **10–50k LoC**, not millions. That's months of focused work, not decades.

## D0 — Atrium GPU ABI design

Specced in detail in [../spec/gpu-abi.md](../spec/gpu-abi.md). Twelve ioctls covering:

- Buffer object allocation / free / mmap.
- Command-stream submission with opaque u64 fence ids.
- Modesetting (enumerate displays, set mode, page flip).
- vblank + fence completion delivered via `kqueue`.

Constants are prefixed `ATRIUM_GPU_*` (e.g. `ATRIUM_GPU_IOC_ALLOC`) and live in `<atrium/gpu.h>`. Each native driver implements the full set for its hardware.

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

Over time, the linuxkpi fallback shrinks as native drivers cover more hardware. Eventually it goes away.

## What's not Vulkan (and why it's fine)

The Fresco server-internal GPU backend is **not** a Vulkan-equivalent API. It's a much smaller set of operations:

- Triangle rendering with a fixed set of shaders.
- Texture sampling.
- Stencil-fill / cover for path rendering (NV-pathrendering style).
- Solid + linear/radial gradient + textured materials.
- Per-window FBO and screen passes.
- Cursor overlay.

Things Vulkan can do that we don't:

- Compute shaders. (Reserved as a protocol extension; likely needed for D6.)
- Raytracing.
- Mesh shaders.
- Custom shader languages. (Apps don't write shaders in Fresco; the server's renderer has a fixed shader set.)
- Indirect draws.

For ~95% of UI, document, browser, and productivity workloads, this is sufficient. AAA games and professional 3D apps remain on Vulkan/Metal — not the target audience for this stack.

## Protocol extensions for compute (future)

Reserved opcodes exist in `protocol.rs` for `CMD_SPAWN_TASK` etc. — a future "autonomous task" subsystem letting the server run client-supplied compute kernels. Sketch design (D6+):

- Client uploads a kernel blob (SHA-256-keyed, like other CAS content).
- Client sends `CMD_SPAWN_TASK` with kernel hash + input/output buffer hashes.
- Server schedules on the GPU (or CPU fallback).
- Result lands as a CAS-stored blob; server emits completion.

This makes Servo / WebRender's compute-driven tile rasterization feasible without exposing raw Vulkan.

## Cross-references

- [Wire-format spec](../spec/wire-format.md) — opcodes, payload layouts.
- [GPU ABI spec](../spec/gpu-abi.md) — kernel/userspace boundary (companion to wire-format).
- [sandbox.md](sandbox.md) — how per-app isolation interacts with GPU access.
- [transport.md](transport.md) — protocol over different transports.
