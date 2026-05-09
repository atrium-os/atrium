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

## Required: BLOB-based scanout

Every Atrium-supported host backend MUST implement the virtio-gpu
BLOB resource family:

- Negotiate the **`VIRTIO_GPU_F_RESOURCE_BLOB`** feature.
- Accept **`VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB`** with
  `blob_mem = VIRTIO_GPU_BLOB_MEM_GUEST` and one `virtio_gpu_mem_entry`
  carrying the BO's contiguous physical address. (HOST3D variants are
  used by venus for VkDeviceMemory-backed buffers, separately.)
- Accept **`VIRTIO_GPU_CMD_SET_SCANOUT_BLOB`** with
  `format = VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM`, single-plane
  (`strides[0] = w * 4`, `offsets[0] = 0`).
- Accept **`VIRTIO_GPU_CMD_RESOURCE_FLUSH`** on a BLOB resource —
  redraws the rectangle. No `TRANSFER_TO_HOST_2D` is needed: with
  `BLOB_MEM_GUEST` the host reads guest RAM directly via the
  registered `mem_entry`.

The kmod uses **plain `VIRTIO_GPU_FLAG_FENCE`** (no
`INFO_RING_IDX`) for these commands — scanout is a global operation,
not per-context. (Venus's per-context blob path is separate, uses
`INFO_RING_IDX`, and is independent.)

## Why BLOB is the unifier

| Backend | BLOB support | Notes |
|---|---|---|
| Plain `virtio-gpu-pci` | ✅ since spec rev 2021 | uses `MEM_GUEST` |
| `virtio-gpu-gl-pci` + venus | ✅ required for venus | venus uses `MEM_HOST3D`; scanout uses `MEM_GUEST` |
| llvmpipe-on-virtio (smoke) | ✅ | same path as plain virtio-gpu |
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
