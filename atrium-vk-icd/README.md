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

**Vulkan 1.3 dispatch surface: complete.** ~140 entry points,
26 unit + 29 e2e tests, live two-process verification on
macOS/aarch64 and FreeBSD/aarch64. WSI Present round-trip
VM-verified.

### Implemented

- **Loader ABI**: `vk_icdNegotiateLoaderICDInterfaceVersion` (v7),
  `vk_icdGetInstanceProcAddr`, `vk_icdGetPhysicalDeviceProcAddr`
  (ICD v4+ fast path), `vkGetDeviceProcAddr` with instance/device
  filter.
- **Instance extensions advertised**: `VK_KHR_surface`,
  `VK_EXT_atrium_surface`, `VK_EXT_debug_utils` (no-op stubs),
  `VK_KHR_get_surface_capabilities2`.
- **Device extensions advertised**: `VK_KHR_swapchain`,
  `VK_KHR_push_descriptor`.
- **Bootstrap probes**: `vkEnumerateInstance{Version,Extension,Layer}Properties`,
  `vkEnumerateDeviceExtensionProperties`.
- **Physical-device probes** (1.0 + 1.1+ pNext variants + KHR aliases):
  - `vkGetPhysicalDevice{Properties,Features,MemoryProperties,
    QueueFamilyProperties,FormatProperties,ImageFormatProperties}`
    and their `*2{,KHR}` counterparts.
  - `VkPhysicalDeviceLimits` filled to Vulkan 1.3 §43.1
    "Required Limits" (16K × 16K image / framebuffer caps;
    sample count 1 only; tier-1 has no MSAA).
  - `vkGetPhysicalDeviceSurface{Support,Capabilities,Formats,
    PresentModes}KHR` + `Capabilities2KHR` / `Formats2KHR`
    (4 priority-ordered surface formats: R/B8G8R8A8 UNORM/SRGB).
- **Device + queue**: `vkCreate/DestroyDevice`, `vkGetDeviceQueue`,
  `vkDeviceWaitIdle` / `vkQueueWaitIdle`.
- **Resources** (all carrying daemon-side `ResourceId`s via the
  appropriate `GpuClient::create_*` call on bind):
  - `VkDeviceMemory` (host-backed Box; daemon-side region_id).
  - `VkBuffer` / `vkGetBufferMemoryRequirements{,2,2KHR}` /
    `vkBindBufferMemory`.
  - `VkImage` / `vkGetImageMemoryRequirements{,2,2KHR}` /
    `vkBindImageMemory` / `vkGetImageSubresourceLayout`.
  - `VkImageView`, `VkBufferView`.
  - `VkSampler` (with full filter / address-mode / lod fields).
  - `vkMapMemory` / `vkUnmapMemory` /
    `vkFlush/InvalidateMappedMemoryRanges`.
  - `vkGetDevice{Buffer,Image}MemoryRequirements{,KHR}` — 1.3
    handle-less sizing for sub-allocators.
- **Shader pipeline**:
  - `vkCreateShaderModule` — sha-256 the SPIR-V, call
    `resolve_shader` against the daemon's cache, fall back to
    `upload_shader` on miss.
  - `vkCreatePipelineLayout`, `vkCreateGraphicsPipelines`,
    `vkCreateComputePipelines`. (Pipeline contents currently
    opaque to the ICD; the host's bundle definition is the
    source of truth.)
- **Render pass + dynamic rendering**: legacy
  `vkCreate/DestroyRenderPass`, `vkCreate/DestroyFramebuffer`,
  `vkCmdBegin/EndRenderPass`; **plus** 1.3
  `vkCmdBeginRendering` / `vkCmdEndRendering` (inline attachment
  specs at draw time — emits the same BeginRenderPass FrameOp).
- **Descriptor sets**: `vkCreateDescriptorSetLayout`,
  `vkCreateDescriptorPool`, `vkAllocateDescriptorSets`,
  `vkFreeDescriptorSets`, `vkResetDescriptorPool`,
  `vkUpdateDescriptorSets`. 1.1 descriptor-update templates
  (`vkCreate/Destroy/UpdateDescriptorSetWithTemplate{,KHR}`) +
  `VK_KHR_push_descriptor` (`vkCmdPushDescriptorSet{,WithTemplate}
  {,KHR}`).
- **Command buffers**: full state machine
  (Initial → Recording → Executable → Initial), pool/alloc/free/
  destroy/reset/trim, secondary cmdbuf execution via
  `vkCmdExecuteCommands`.
- **vkCmd\* draw + state**: `Draw`, `DrawIndexed`, `DrawIndirect`,
  `DrawIndexedIndirect`, **`Draw{,Indexed}IndirectCount{,KHR,AMD}`**
  (1.2; forwards to non-count with max_draw_count), `Dispatch`,
  `DispatchIndirect`, `Set{Viewport,Scissor}`,
  **`Set{Viewport,Scissor}WithCount{,EXT}`** (1.3),
  **`BindVertexBuffers2{,EXT}`** (1.3),
  `PushConstants`, `BindPipeline`, `BindVertex/IndexBuffers`,
  `BindDescriptorSets`.
- **vkCmd\* extended dynamic state** (1.3 + EXT aliases):
  `Set{CullMode,FrontFace,PrimitiveTopology,DepthTest/Write/
  CompareOp/BoundsTest/Enable, Stencil{Test,Op}Enable,
  RasterizerDiscardEnable,DepthBiasEnable,
  PrimitiveRestartEnable}` — 14 entries, no-op on tier-1.
- **vkCmd\* copy + clear**: `CopyBuffer`, `CopyBufferToImage`,
  `CopyImage`, `BlitImage`, `ResolveImage`, `CopyImageToBuffer`,
  `Clear{Color,DepthStencil}Image`, `ClearAttachments`. 1.3
  sync2 variants `Cmd{Copy,Blit,Resolve}*2{,KHR}` (6 entries,
  no-op stubs).
- **vkCmd\* sync**: `PipelineBarrier` + 1.3 `PipelineBarrier2{,KHR}`
  (parses VkDependencyInfo, emits the same FrameOp). Events:
  `Set/Reset/WaitEvents` + sync2 `Set/Reset/WaitEvents2{,KHR}`.
- **Sync objects**: `VkFence` + `vkWaitForFences` / `vkResetFences`
  / `vkGetFenceStatus`. `VkSemaphore`: 1.2 timeline API stubs
  (`vkSignal/WaitSemaphores`, `vkGetSemaphoreCounterValue` —
  immediate-return given sequential submission). `VkEvent` with
  state tracking.
- **Queries**: `VkQueryPool` create/destroy + cmd entries.
  `vkCmdWriteTimestamp2{,KHR}` no-op (tier-1 has no real
  timestamps; `timestampPeriod=0` advertised).
- **Submit**: `vkQueueSubmit` + 1.3 `vkQueueSubmit2{,KHR}`
  (VkSubmitInfo2 / VkCommandBufferSubmitInfo parsing) walk queue
  → device → instance → `Mutex<GpuClient>` and call
  `submit_frame` against a per-device persistent fence +
  monotonic timeline.
- **VK_EXT_debug_utils** (no-op stubs for 11 entries: messenger
  create/destroy/submit, object name/tag, queue + cmd labels).
- **WSI** (`VK_KHR_surface`, `VK_KHR_swapchain`,
  `VK_EXT_atrium_surface`):
  - `vkCreateAtriumSurfaceEXT` (VkSurfaceKHR ≡ Fresco window-id),
    `vkDestroySurfaceKHR`.
  - `vkCreateSwapchainKHR`, `vkDestroySwapchainKHR`,
    `vkGetSwapchainImagesKHR`, `vkAcquireNextImageKHR`,
    `vkQueuePresentKHR` (emits `OP_GPU_PRESENT` to daemon).
  - frescod-aqueduct installs a `SoftwareBackend::set_present_hook`
    that lands presented pixels into the per-window
    `WindowSurface::image` (see frescod-aqueduct binary).

### Deferred

- **Tier-2 (llvmpipe) shader execution.** The tier-1 software
  renderer in `aqueduct-gpu-host` only handles Atrium-native
  bundle pipelines (rect/path/textured-rect/glyph_run);
  third-party SPIR-V uploaded through atrium-vk-icd surfaces a
  structured "tier-2 territory" warning. Tier-2 wiring is the
  next big milestone.
- `VK_KHR_buffer_device_address` (need the bufferDeviceAddress
  feature + a real address scheme on the daemon side; safer to
  leave the feature unadvertised than to stub).
- Surface capabilities: extent currently hardcoded 1280×800
  (frescod-aqueduct's typical mode). Real per-window sizing
  needs a connector-aware probe through the daemon.
- `atrium-pkg` shader-precompile integration (install-hook
  story; today shader upload is per-app first-run).

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
