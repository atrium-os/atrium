# atrium-vk-icd

Vulkan ICD (Installable Client Driver) that speaks the aqueduct-gpu
protocol. Translates Vulkan API calls into FrameOp records sent to
an aqueduct-gpu host endpoint (`aqueduct-gpu-host` standalone, or
`frescod-aqueduct` as a renderer co-process; the atrium-gpu kmod
direct at D5+).

This is not a general Vulkan driver. It targets aqueduct-gpu
specifically — the format-rules table, the bundle-pipeline model,
the install-time AOT-compiled shader cache.

## Status

**Setup-record-submit lifecycle: complete.** ~90 Vulkan entry
points, 30 tests, live two-process verification on macOS/aarch64
and FreeBSD/aarch64.

### Implemented

- **Loader interface**: `vk_icdNegotiateLoaderICDInterfaceVersion`
  (v7), `vk_icdGetInstanceProcAddr`.
- **Bootstrap probes**: `vkEnumerateInstance{Version,Extension,Layer}Properties`.
- **Instance + physical-device**:
  - `vkCreate/DestroyInstance` with aqueduct-gpu handshake.
  - `vkEnumeratePhysicalDevices` (1 device per backend reachable).
  - `vkGetPhysicalDeviceProperties` / `Features` / `MemoryProperties` /
    `FormatProperties` / `QueueFamilyProperties`.
- **Device + queue**: `vkCreate/DestroyDevice`, `vkGetDeviceQueue`,
  `vkDeviceWaitIdle` / `vkQueueWaitIdle`.
- **Resources** (all carrying daemon-side `ResourceId`s via the
  appropriate `GpuClient::create_*` call on bind):
  - `VkDeviceMemory` (host-backed Box; daemon-side region_id).
  - `VkBuffer` / `vkGetBufferMemoryRequirements` / `vkBindBufferMemory`.
  - `VkImage` / `vkGetImageMemoryRequirements` / `vkBindImageMemory`.
  - `VkImageView`, `VkBufferView`.
  - `VkSampler` (with full filter / address-mode / lod fields).
  - `vkMapMemory` / `vkUnmapMemory`.
- **Shader pipeline**:
  - `vkCreateShaderModule` — sha-256 the SPIR-V, call
    `resolve_shader` against the daemon's cache, fall back to
    `upload_shader` on miss.
  - `vkCreatePipelineLayout`, `vkCreateGraphicsPipelines`,
    `vkCreateComputePipelines`. (Pipeline contents currently
    opaque to the ICD; the host's bundle definition is the
    source of truth.)
- **Render pass**: `vkCreate/DestroyRenderPass`, `vkCreate/Destroy
  Framebuffer`.
- **Descriptor sets**: `vkCreateDescriptorSetLayout`,
  `vkCreateDescriptorPool`, `vkAllocateDescriptorSets`,
  `vkUpdateDescriptorSets` (uniform / storage buffer + sampler /
  combined-image-sampler / sampled-image / storage-image).
- **Command buffers**: full state machine
  (Initial → Recording → Executable → Initial), pool/alloc/free/
  destroy, secondary cmdbuf execution via `vkCmdExecuteCommands`
  (re-pushes secondary's FrameOps into primary).
- **vkCmd\*** (~27 opcodes): `SetViewport`, `SetScissor`,
  `PushConstants`, `BindPipeline`, `BindVertex/IndexBuffers`,
  `BindDescriptorSets`, `Draw`, `DrawIndexed`, `Draw*Indirect`,
  `Dispatch`, `DispatchIndirect`, `Begin/EndRenderPass`,
  `NextSubpass`, `CopyBuffer`, `CopyBufferToImage`, `CopyImage`,
  `BlitImage`, `ResolveImage`, `CopyImageToBuffer`,
  `Clear{Color,DepthStencil}Image`, `ClearAttachments`,
  `PipelineBarrier`, `Set{Event,LineWidth,DepthBias,
  BlendConstants}`, `ResetEvent`, `WaitEvents`.
- **Sync**: `VkFence` + `vkWaitForFences` / `vkResetFences` /
  `vkGetFenceStatus`. `VkSemaphore` (opaque handle; timeline
  serialization is via per-submit timeline). `VkEvent` with
  state tracking.
- **Queries**: `VkQueryPool` create/destroy + `vkCmdBeginQuery` /
  `EndQuery` / `WriteTimestamp` / `ResetQueryPool` (no-op today
  — tier-1 has no hardware timestamps; `vkGetQueryPoolResults`
  returns `VK_NOT_READY` truthfully).
- **Submit**: `vkQueueSubmit` walks queue → device → instance →
  `Mutex<GpuClient>` and calls `submit_frame` against a
  per-device persistent fence + monotonic timeline.

### Not yet implemented

- **WSI** (`VK_KHR_surface`, `VK_KHR_swapchain`). Apps that need
  windowed presentation must currently render to an offscreen
  image + read back. The
  [`examples/headless_triangle`](examples/headless_triangle.rs)
  shows the offscreen pattern end-to-end.
- **Tier-2 (llvmpipe) shader execution.** The tier-1 software
  renderer in `aqueduct-gpu-host` only handles Atrium-native
  bundle pipelines (rect/path/textured-rect/glyph_run);
  third-party SPIR-V uploaded through atrium-vk-icd surfaces a
  structured "tier-2 territory" warning. Tier-2 wiring is the
  next big milestone.
- `VK_EXT_debug_utils` / `VK_EXT_debug_report`.
- `atrium-pkg` shader-precompile integration (the
  install-hook story; today shader upload is per-app first-run).

## How the Khronos loader finds us

A JSON manifest in `atrium_icd.json` (next to `Cargo.toml`)
declares the cdylib path + API version. Install on Linux/FreeBSD:

```
sudo install -m 644 atrium_icd.json /usr/local/share/vulkan/icd.d/
sudo install -m 644 target/release/libatrium_vk_icd.so /usr/local/lib/
```

The Khronos loader (`libvulkan.so.1`) discovers ICDs by scanning
`*/vulkan/icd.d/*.json` and dlopens the listed library; we
export the two required entry points (`vk_icdNegotiate*` +
`vk_icdGetInstanceProcAddr`). With both files installed, a
plain Vulkan app linked against `libvulkan` finds atrium-vk-icd
automatically and routes its calls through aqueduct-gpu.

## How to run without the Khronos loader

The headless example
([`examples/headless_triangle.rs`](examples/headless_triangle.rs))
drives the ICD through its C ABI directly — useful when the
loader isn't available, or for in-tree validation:

```
# Spawn a host endpoint (separately built):
$ aqueduct-gpu-host --backend software --socket /tmp/h.sock &

# Drive the ICD against it:
$ ATRIUM_VK_ICD_SOCKET=/tmp/h.sock cargo run --example headless_triangle
```

VM-verified on FreeBSD/aarch64 — see the example's module doc for
the full expected output.

## Layout of the source

- `src/lib.rs` — every Vulkan entry point in one file. Grouped
  by Vulkan version / extension. Each entry point reads its
  `VkXxxCreateInfo` by field offset via `read_unaligned`, then
  either constructs an ICD-side `Atrium*` struct or pushes a
  `FrameOp` into the cmdbuf's `FrameBuilder`.
- `tests/end_to_end.rs` — integration tests that spawn a real
  `SoftwareBackend` listener and exercise the ICD's C ABI.
- `examples/headless_triangle.rs` — live demo against a
  separately-spawned `aqueduct-gpu-host` process.
- `atrium_icd.json` — the Khronos ICD manifest the Vulkan
  loader reads to discover us.

## License

MIT OR Apache-2.0, matching the rest of the Atrium tree.
