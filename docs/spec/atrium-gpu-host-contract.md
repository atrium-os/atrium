# Atrium GPU host contract

The Atrium GPU stack has two ABI boundaries:

```
  userspace (frescod, atrium-gpu-rs, …)
        ↕  Atrium GPU ABI  (atrium_gpu.h — IOC_ALLOC, IOC_SUBMIT,
                            IOC_BIND_GPU, IOC_SET_MODE, IOC_PAGE_FLIP, …)
  kmod (atrium_virtio_gpu.ko, future native drivers)
        ↕  Host contract    (this document)
  host backend  (virtio-gpu-pci, virtio-gpu-gl-pci+venus,
                 future: native AMD/Intel/Apple GPU drivers)
```

The **userspace ABI** is stable and format-agnostic — apps and frescod
allocate BOs, render into them, and call `set_mode` / `page_flip`
without knowing which backend is underneath.

This document specifies the **host contract** the kmod expects from
whatever it talks to on the other side. Standardizing here is what
lets the same kmod sit on top of venus, plain virtio-gpu, or future
native drivers without per-backend code paths inside the kmod.

## Required: BLOB-based scanout via `BLOB_MEM_HOST3D`

Every Atrium-supported host backend MUST implement the virtio-gpu
BLOB resource family AND the host-allocated (`HOST3D`) backing mode:

- Negotiate **`VIRTIO_GPU_F_RESOURCE_BLOB`** and
  **`VIRTIO_GPU_F_CONTEXT_INIT`**.
- Expose a **host-visible shared-memory region** (QEMU's `hostmem=N`
  on `virtio-gpu-gl-pci`) — the kmod allocates BAR windows from it
  and publishes them to userspace via `mmap`.
- Accept **`VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB`** with
  `blob_mem = VIRTIO_GPU_BLOB_MEM_HOST3D`,
  `blob_flags = VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE`, `blob_id = 0`,
  `nr_entries = 0`. The host allocates the backing pages on its side;
  the guest maps them via the BAR window.
- Accept **`VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE`** + 
  **`VIRTIO_GPU_CMD_RESOURCE_MAP_BLOB`** for the scanout context's
  ownership of the blob.
- Accept **`VIRTIO_GPU_CMD_SET_SCANOUT_BLOB`** with
  `format = VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM`, single-plane
  (`strides[0] = w * 4`, `offsets[0] = 0`).
- Accept **`VIRTIO_GPU_CMD_RESOURCE_FLUSH`** on a BLOB resource.

`BLOB_MEM_GUEST` is **not** an acceptable scanout backing on macOS
hosts: virglrenderer needs `udmabuf` to consume guest sglists, which
Darwin doesn't have. `HOST3D + USE_MAPPABLE` works uniformly across
all hosts and is the single path the kmod uses.

### Per-context vs. global

`HOST3D` blobs are tied to a virtio-gpu context. The kmod
lazy-creates a kmod-internal **scanout context** (capset = venus,
debug name `atrium-scanout`) to own these resources — userspace does
not need its own venus context for plain scanout. Fences for context
operations use **`VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX`**
with `ring_idx = 0` so the host routes them through the per-context
async-fence path (the legacy global path is broken under
`VIRGL_RENDERER_NO_VIRGL`, the macOS host case).

**Fence routing** — the per-context async fence path requires
virglrenderer's proxy to create a fence-eventfd that the worker
signals on retire. On Linux this uses `eventfd(2)`; the proxy
unconditionally strips `VIRGL_RENDERER_THREAD_SYNC` from its flags
when `<sys/eventfd.h>` is absent, which silently breaks fence
delivery on macOS/FreeBSD hosts. The atrium-os virglrenderer fork
provides a socketpair-based eventfd emulation (commit `52903fdb`)
that closes this gap; both venus userspace and kmod-internal scanout
contexts now retire fences correctly on macOS hosts.

## Why BLOB is the unifier

| Backend | BLOB support | Notes |
|---|---|---|
| `virtio-gpu-gl-pci` + venus | ✅ required for venus | venus and scanout both use `MEM_HOST3D` |
| Plain `virtio-gpu-pci` | ✗ on macOS hosts | `MEM_GUEST` blobs need `udmabuf` at the renderer boundary; absent on Darwin |
| llvmpipe-on-virtio (smoke harness) | ✅ | uses the same `--venus` profile but with venus userspace falling back to lavapipe — the kmod-internal scanout context still needs venus capset for HOST3D |
| Future native AMD/Intel | n/a (no virtio) | kmod's host-side translator targets the silicon's command set; the kmod's *internal* command shape stays BLOB-shaped |

Legacy `RESOURCE_CREATE_2D` + `RESOURCE_ATTACH_BACKING` +
`SET_SCANOUT` is **not** a supported scanout path. Some host backends
(notably the venus-capable `virtio-gpu-gl-pci`) silently drop or
mis-handle those commands; rather than fork the kmod's command flow
per-backend, we mandate BLOB for all.

## Failure mode

The kmod fails attach with `ENOTSUP` and a `device_printf` if the
host doesn't advertise `F_RESOURCE_BLOB`. This surfaces in `dmesg`
right after the virtio handshake. No `/dev/atrium-gpu0` is created.
Recovery: use a BLOB-capable QEMU + virglrenderer build (the
`qemu-build` repo's default config already enables this).

## Relation to other features

- **`F_CONTEXT_INIT`** — also required, for venus per-context fence
  routing. The two features are negotiated together.
- **`F_VIRGL`** — not required. Atrium's compute/3D path is venus
  via `F_CONTEXT_INIT`; legacy VirGL is unused.
- **`F_EDID`** — optional; the kmod synthesizes a virtual connector
  if the host doesn't provide one.
- **`F_RESOURCE_UUID`** — not used.

## Display ICC profiles

Out of scope for this contract. Per-monitor calibration (ICC
profiles, gamut mapping, HDR transfers) lives a layer above the host
contract — see [pergola.md §8.1](pergola.md). For now, all hosts are
assumed to drive sRGB displays; the linear→sRGB encoding is done by
the GPU's color-attachment write to a `B8G8R8A8_SRGB` swapchain.
