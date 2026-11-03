# RDNA4 backend — the Vulkan subset Atrium uses

**Status:** inventory, 2026-11-03. Milestone M0 of
[`atrium-rdna4-vulkan-backend.md`](atrium-rdna4-vulkan-backend.md). Taken from
the source, not from the Vulkan spec: anything not listed here is out of scope
until something in the tree needs it.

## Tier A — the desktop (fresco-vulkan + the shader bundles)

This is what frescod needs to render the desktop on the GPU. It is small.

**Shaders** (`bundles/atrium-core`, `bundles/atrium-text`, Slang → SPIR-V):

| Bundle | Stage | Features |
|---|---|---|
| `op_rectangle`, `op_path`, `op_texture`, `op_glyph_run` | compute, `numthreads(64,1,1)` | `RWStructuredBuffer`/`StructuredBuffer` (storage buffers), `InterlockedAdd` (buffer atomics), `SV_DispatchThreadID` |
| `pipe_rectangle`, `pipe_path` | vertex + fragment | vertex pulling from `StructuredBuffer` by `SV_VertexID`/`SV_InstanceID` (**no vertex buffers**), `SV_Position`, one `SV_Target` |
| `pipe_texture`, `pipe_glyph_run` | vertex + fragment | as above, plus `Sampler2D` (combined image sampler) `.Sample()` |

**API** (ash calls in `fresco-vulkan/src`):

- Objects: instance, device, one graphics+compute queue, command pool/buffers,
  fences, semaphores, buffers, images and views, one sampler, descriptor
  pool/layout/sets, pipeline layout, one compute and one graphics pipeline per
  op, render pass and framebuffer, shader modules; swapchain only on the windowed
  path (headless is the frescod path).
- Descriptors: `STORAGE_BUFFER` (dominant), `UNIFORM_BUFFER`,
  `COMBINED_IMAGE_SAMPLER`.
- Commands: `cmd_dispatch`, `cmd_draw`, `cmd_begin/end_render_pass`,
  `cmd_bind_pipeline`, `cmd_bind_descriptor_sets`, `cmd_set_viewport`,
  `cmd_set_scissor`, `cmd_pipeline_barrier`, `cmd_fill_buffer`,
  `cmd_copy_buffer_to_image`, `cmd_copy_image_to_buffer`.
- State: triangle strip; one colour attachment `B8G8R8A8_SRGB`; 1 sample; blend
  `SRC_ALPHA`/`ONE_MINUS_SRC_ALPHA` on colour, `ZERO`/`ONE` on alpha; sampler
  `LINEAR`, `CLAMP_TO_EDGE`; dynamic viewport and scissor.
- Not used: vertex buffers, index buffers, depth/stencil, MSAA, multiple render
  targets, mipmaps, geometry/tessellation, subgroup ops, push constants.

**Consequence for the backend.** Tier A needs: compute with storage buffers and
atomics; a vertex-only NGG stage that pulls from storage buffers; fragment
shaders with one sampled 2D texture; sRGB colour output with fixed blending;
buffer↔image copies; barriers. That is the target of M1–M4, and it is a much
smaller compiler and state surface than general Vulkan.

## Tier B — the atrium-vk-icd example corpus

These are the regression cases the Tier-2 work already uses. They become oracle
cases (Tier-2 vs R9700 vs gpusim) and widen the backend after Tier A.

**Compute shaders** (`atrium-vk-icd/examples/shaders`, 21): `write_42`,
`per_thread`, `per_thread_triangle`, `multi_binding`, `push_scale`,
`spec_scale`, `max_const`, `ternary`, `branch`, `loop_mul`, `sum_loop`, `rmw`,
`rotate`, `bit_count`, `bit_chain`, `float_sqrt`, `atomic_add`,
`groupshared_var`, `groupshared_xor`, `subgroup_sum`, `storage_image_write`.
Adds: push constants, specialization constants, divergent control flow and
loops, LDS (`groupshared`), subgroup reductions, storage images.

**Graphics examples** (`loader_graphics_*`, 54): array, bgra, clear, cube, cull,
cull_dynamic, damage, depth, depth_bias, depth_bounds, depth_cmp, depth_dynamic,
depth_range, deriv, discard, fragdepth, frontface, half, indexed,
instance_rate, instanced, interp, lines, linestrip, lod, mipmap, mrt, mrt_blend,
msaa, multi_fs_in, multi_vbuf, multidraw, pcf, points, primid, pushc, r8,
restart, rg8, rgb10a2, roundtrip, scissor, shadow, shadow_cmp, srgb, stencil,
stencil_dynamic, strip, texture, trifan, ubo, unorm, viewport, vs_pushc.
Adds: vertex and index buffers, depth/stencil, MSAA, MRT, mipmaps and LOD,
derivatives, discard, lines and points, shadow comparison sampling, more formats.

## Order

1. Tier A compute (M1): the four `op_*` shaders.
2. Tier B compute (M2): the 21 example shaders, in roughly the listed order.
3. Tier A images (M3): `Sampler2D` sampling, buffer↔image copies.
4. Tier A graphics (M4): the four `pipe_*` pipelines → fresco-vulkan renders a frame.
5. Tier B graphics, by demand.
