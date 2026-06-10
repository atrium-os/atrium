# Fresco Wire Format Specification

**Version:** 0.1.0 (draft, frozen as of 2026-04-28)
**Status:** Normative for implementations targeting v0.x.

This document specifies the on-wire byte layout, opcode registry, and
semantics of the Fresco protocol. It is independent of any particular
transport (ivshmem, cdev, TCP, QUIC, future hardware) and any
particular implementation.

## 1. Scope

This specification covers:

- Record formats (Command, Completion, InputEvent).
- The opcode registry (commands, completions, events, blob types).
- Connection handshake and version negotiation.
- The retained-mode scene graph data model.
- Forward-extensibility rules and the extension mechanism.

This specification does **not** cover:

- Specific transports (covered separately per transport).
- The kernel/userspace GPU ABI on the server side (covered separately).
- The implementation of the server, client libraries, or kernel modules.

## 2. Design principles

1. **Retained-mode at the wire level.** The server holds the scene; clients send mutations. Any frame whose scene is unchanged sends zero protocol bytes.

2. **Content-addressed.** Bulk data (geometry, textures, paths, kernels, samples, clipboard contents) is identified by a 256-bit cryptographic hash. Identical content has identical hash. Cached forever (subject to GC) and reusable across clients, sessions, and machines.

3. **Fixed-shape records.** All records are fixed-size for trivially deterministic ring-buffer addressing. Records reserve trailing padding for forward compatibility; an extension never resizes an existing record.

4. **Categorized rings.** Three rings carry the wire traffic, separated by direction and size:
   - **Cmd ring** — client → server, 128-byte records.
   - **Completion ring** — server → client, 128-byte records (carry hashes).
   - **Event ring** — server → client, 64-byte records (small, frequent: input + system events).

5. **Opcode-keyed semantics.** Every record's first u16 is its opcode (or comp_type / event_type). Categories partitioned by range; vendor-reserved range at the top.

6. **Versioned with strong forward-compat.** Minor version bumps add opcodes and never remove. Major version bumps may break compatibility. Both sides advertise their supported version range during handshake.

7. **Capability-gated extensions.** Optional features beyond core (compute, ray-tracing, audio, clipboard, ...) are extensions. The handshake declares which extensions are supported and which the client requests.

8. **Endian explicit.** All multi-byte integers are **little-endian**. Floats are IEEE 754 binary32 / binary64 in little-endian byte order.

## 3. Versioning

- Major version: wire-incompatible changes (record layout, opcode space repartition, breaking semantics).
- Minor version: additive (new opcode in a reserved range, new event type, new field in a reserved padding region of an existing record).
- Patch version: clarifications, no normative changes.

A server advertises `(major.minor.patch)` during handshake. Clients pinned to major X work with any server `(X.y.z)` where `y >= client_minimum_minor`. Extensions are negotiated separately.

This spec is **0.1.0**. Major version 0 means we reserve the right to break compatibility before reaching 1.0. Once 1.0 ships, the stability promise above applies.

## 4. Memory model and concurrency

- Endianness: little-endian throughout.
- Alignment: all records 4-byte aligned; payload `u64` and `f64` fields naturally aligned within the record.
- Strings: UTF-8, no null terminator unless explicitly noted.
- Hashes: `Hash256` = 32 bytes, raw SHA-256 output.

For **shared-memory transports**:
- Single-producer-single-consumer per ring (one writer, one reader). No locks.
- Read/write head pointers are u32, monotonically increasing. Index = `head % ring_capacity`.
- Updates use volatile writes (compiler barrier). Hardware coherence assumed for x86/aarch64 hosts. Future architectures may require explicit fences; documented separately.

For **socket transports**:
- Records are framed by their fixed size. Cmd records are 128 bytes; events 64. Multiple records may be sent in one segment.
- TCP gives ordering + reliability. QUIC/UDP needs a reliability layer (ack / retransmit) defined per-transport.

## 5. Records

Three record formats. All fields are little-endian.

### 5.1 Command record (128 bytes)

Sent client → server.

```
offset  size  field
   0     2    opcode               u16   (registry below)
   2     2    flags                u16   (window_id for routable ops; opcode-specific otherwise)
   4     4    sequence_id          u32   (client's correlation tag; echoed in completion)
   8   120    payload              opaque, opcode-specific (often packed structs)
 ─── 128 bytes ───
```

`flags` semantics:
- For routable opcodes (slot/frame/scene operations): the target window_id (u16). Server validates ownership.
- For non-routable opcodes: opcode-specific sub-flags or reserved zero.

`sequence_id`: the client's choice. Used to correlate the completion when one is emitted. Never interpreted by the server. Two opcodes with the same sequence_id are not collisions; correlation is per-opcode-class.

### 5.2 Completion record (128 bytes)

Sent server → client.

```
offset  size  field
   0     2    comp_type            u16   (registry below)
   2     2    status               u16   (STATUS_*)
   4     4    id                   u32   (context-specific; e.g. window_id, upload_id)
   8    32    result_hash          Hash256  (operation result, e.g. CAS hash)
  40    88    payload              opaque, comp_type-specific
 ─── 128 bytes ───
```

`comp_type` distinguishes:
- **Responses** (correlate with a previous command's `sequence_id`, echoed in the result_hash[0..4] for some responses).
- **Async events** (server-pushed; e.g. window resize requested, capability granted).

### 5.3 Event record (64 bytes)

Sent server → client. Small, frequent, latency-sensitive.

```
offset  size  field
   0     2    event_type           u16   (registry below)
   2     2    code                 u16   (event-specific subtype, e.g. key code)
   4     4    value_a              i32   (event-specific)
   8     4    value_b              i32   (event-specific)
  12     4    target_window        u32   (server-tagged; pointer events: cursor-hit window; key events: focused window)
  16    48    payload              opaque, event-specific
 ─── 64 bytes ───
```

The 48-byte payload region accommodates extended events (multi-touch with multiple pointers; gestures with axis vectors; IME composition state).

## 6. Opcode space

### 6.1 Command opcode ranges (cmd ring)

| Range | Category | Status |
|---|---|---|
| `0x0000`–`0x00FF` | CAS upload (BEGIN, DATA, FINISH, DMA, future blob mgmt) | 0.1 done |
| `0x0100`–`0x01FF` | Scene & slot graph (legacy scene mutations + slot graph + future scene-graph extensions) | 0.1 done |
| `0x0200`–`0x02FF` | Autonomous tasks (server-resident long-lived tasks: physics, particles, GI probes) | 0.1 stubbed |
| `0x0300`–`0x03FF` | Frame / control (RENDER, FRAME_END, FENCE, QUERY) | 0.1 done |
| `0x0500`–`0x05FF` | Window lifecycle (CREATE, DESTROY, SET_*, ...) | 0.1 done |
| `0x0700`–`0x07FF` | Clipboard | future |
| `0x0800`–`0x08FF` | Compute & ray-tracing (kernel load, dispatch, AS build) | future |
| `0x0900`–`0x09FF` | Animation timeline (server-side animation, including skeletal) | future |
| `0x0A00`–`0x0AFF` | Audio (per-stream submission) | future, may move to separate cdev |
| `0x0B00`–`0x0BFF` | Capability / permission requests | future |
| `0x0C00`–`0x0CFF` | 3D rendering (lights, PBR materials, skinning, shadows, IBL) | future |
| `0x0D00`–`0x0DFF` | Render passes / multi-pass compositing (probes, reflections, post) | future |
| `0x1000`–`0x10FF` | Connection meta (handshake, version, capabilities) | reserved for v0.2 |
| `0x1100`–`0xFEFF` | Reserved (future Khronos / FreeDesktop registrations) | reserved |
| `0xFF00`–`0xFFFF` | Vendor-private / experimental | unrestricted |

Notes:
- Range `0x0400–0x04FF` is currently unallocated (frame/control lives at `0x0300+`); reserved.
- Range `0x0600–0x06FF` is currently unallocated (it had been earmarked for client→server input simulation but the design is not v0.1); reserved.
- Earlier drafts of this spec placed Meta/handshake at `0x0000–0x00FF`; the actual implementation has been using that range for CAS upload since the POC. Meta moves to `0x1000–0x10FF` to match reality.

### 6.2 Currently-allocated opcodes (0.1)

#### CAS upload — `0x0001`–`0x0004`

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| `0x0001` | `CMD_UPLOAD_BEGIN` | C→S | Start a multi-cmd inline upload. payload[0..4]: total_size; payload[8..120]: first chunk. |
| `0x0002` | `CMD_UPLOAD_DATA` | C→S | Continuation chunk. payload[0..4]: offset (advisory); payload[4..120]: bytes. |
| `0x0003` | `CMD_UPLOAD_FINISH` | C→S | Complete upload. payload[32..36]: upload_id (returned in completion.id). |
| `0x0004` | `CMD_UPLOAD_DMA` | C→S | Single-cmd large upload via per-slot staging region. payload[0..4]: length. |

Inline upload (BEGIN/DATA/FINISH) is for small blobs (<4 KiB typical); DMA path is for large blobs. Server replies `COMP_UPLOAD_COMPLETE` with the SHA-256 hash.

#### Legacy scene graph — `0x0100`–`0x0108`

The legacy scene graph predates the slot graph; clients composed scenes by adding/removing/updating nodes in a CAS-tree-shaped scene held server-side. The slot graph (`0x0110+`) is the preferred path now, but the legacy ops remain parsed for tooling and backwards compatibility.

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| `0x0100` | `CMD_SET_ROOT` | C→S | Set scene root by hash. Routable (flags = window_id). |
| `0x0101` | `CMD_SET_CAMERA` | C→S | Set scene camera by hash. Routable. |
| `0x0102` | `CMD_ADD_NODE` | C→S | Add a child node to a parent in the legacy scene tree. |
| `0x0103` | `CMD_REMOVE_NODE` | C→S | Remove a node from the legacy scene tree. |
| `0x0104` | `CMD_UPDATE_TRANSFORM` | C→S | Replace a node's transform by hash. |
| `0x0105` | `CMD_UPDATE_TRANSFORM_INLINE` | C→S | Replace a node's transform with an inline 4×4 matrix in the payload. |
| `0x0106` | `CMD_UPDATE_MATERIAL` | C→S | Replace a node's material by hash. |
| `0x0107` | `CMD_ADD_LIGHT` | C→S | Add a light to the scene. |
| `0x0108` | `CMD_UPDATE_LIGHT` | C→S | Update a previously-added light. |

All routable (flags = window_id).

#### Slot graph — `0x0110`–`0x0118`

| Opcode | Name | Purpose |
|---|---|---|
| `0x0110` | `CMD_SLOT_ALLOC` | Allocate a slot (slot_id, node_type, flags). |
| `0x0111` | `CMD_SLOT_FREE` | Release a slot. |
| `0x0112` | `CMD_SLOT_SET_XFORM` | Set slot's transform by hash. |
| `0x0113` | `CMD_SLOT_SET_CONTENT` | Set slot's content (renderable / scene subtree) by hash. |
| `0x0114` | `CMD_SLOT_SET_CHILDREN` | Set slot's children list. |
| `0x0115` | `CMD_SLOT_SET_FLAGS` | Update slot's flags + clip rect. |
| `0x0116` | `CMD_SLOT_SET_ROOT` | Designate slot as scene root. |
| `0x0117` | `CMD_SLOT_SET_TEXT` | Inline text content (font + size + string). |
| `0x0118` | `CMD_SLOT_SET_CAS_CHILDREN` | Set slot's children to a CAS subtree. |

All slot ops are routable (flags = window_id).

#### Autonomous tasks — `0x0200`–`0x0202`

Long-running server-resident tasks that produce results without per-frame client involvement. Use cases: physics simulation, particle systems, GI probe baking, audio raytracing. Today these opcodes are reserved with stub handlers; full design lands when an implementation needs them.

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| `0x0200` | `CMD_SPAWN_TASK` | C→S | Start an autonomous task. payload encodes task type, time step, configuration hash. |
| `0x0201` | `CMD_SPAWN_TASK_TARGET` | C→S | Variant that also binds the task's output to a target slot/buffer. |
| `0x0202` | `CMD_STOP_TASK` | C→S | Stop a running autonomous task. |

#### Frame / control — `0x0300`–`0x0304`

| Opcode | Name | Purpose |
|---|---|---|
| `0x0300` | `CMD_RENDER` | Force a render pass (rare; FRAME_END usually suffices). |
| `0x0301` | `CMD_FENCE` | Insert a fence; completion when prior commands have committed. |
| `0x0302` | `CMD_QUERY_HASH` | Query whether a hash is in CAS; returns COMP_QUERY_RESULT. |
| `0x0303` | `CMD_FRAME_BEGIN` | Begin a frame (advisory; helps server group commits). |
| `0x0304` | `CMD_FRAME_END` | End of frame; server traverses slots, commits scene, renders. |

`CMD_RENDER` and `CMD_FRAME_END` are routable.

#### Window lifecycle — `0x0500`–`0x0506`

| Opcode | Name | Purpose |
|---|---|---|
| `0x0500` | `CMD_CREATE_WINDOW` | (w, h, flags, short_title[16]). Server returns COMP_WINDOW_CREATED. |
| `0x0501` | `CMD_DESTROY_WINDOW` | (window_id). |
| `0x0502` | `CMD_WINDOW_SET_ROOT` | (window_id, slot_id). |
| `0x0503` | `CMD_WINDOW_SET_TITLE` | (window_id, utf8_bytes). |
| `0x0504` | `CMD_WINDOW_PRESENT` | (window_id) — per-window FRAME_END. |
| `0x0505` | `CMD_WINDOW_SET_POS` | (window_id, x:f32, y:f32). |
| `0x0506` | `CMD_WINDOW_SET_SIZE` | (window_id, w:u32, h:u32). |

### 6.3 Future opcode ranges (sketches; finalized in later spec versions)

#### Clipboard — `0x0700`–`0x07FF`

```
0x0700 CMD_CLIPBOARD_PUT       (selection_id, format_count, format_list[hash..])
       payload: which clipboard (system / primary / etc.); list of (mime_type_hash, content_hash) pairs.
0x0701 CMD_CLIPBOARD_GET       (selection_id, requested_format_hash)
       Server replies with COMP_CLIPBOARD_DATA carrying content hash.
0x0702 CMD_CLIPBOARD_LIST      (selection_id)
       Server replies with available formats (hashes).
0x0703 CMD_CLIPBOARD_SUBSCRIBE (selection_id)
       Subscribe to changes; server emits EVT_CLIPBOARD_CHANGED.
```

Clipboard contents are themselves CAS blobs (multiple format representations of the same item are individually content-addressed; the clipboard is a {mime → hash} map).

#### Compute & ray-tracing — `0x0800`–`0x08FF`

```
0x0800 CMD_KERNEL_LOAD         (kernel_blob_hash, signature)
       Server validates the kernel; returns kernel_id (in completion).
0x0801 CMD_KERNEL_DISPATCH     (kernel_id, workgroup_size, input_hash[], output_size)
       Server schedules; emits COMP_KERNEL_DONE with output_hash.
0x0802 CMD_KERNEL_FREE         (kernel_id)
0x0810 CMD_AS_BUILD            (mesh_hash[]) → acceleration structure hash
0x0811 CMD_AS_FREE             (as_hash)
0x0820 CMD_RT_DISPATCH         (as_hash, raygen_kernel_id, output_size, recursion_limit)
       Emits COMP_RT_DONE with output_hash.
```

Kernel format: SPIR-V or a Fresco-IR (TBD). The server may compile / cache.

Ray-tracing scenes can also be modeled as scene-graph nodes (`NODE_RAY_SCENE`), letting RT be expressed as a slot's content. Either is valid — the RT-as-node model fits retained-mode dedup naturally; the explicit-dispatch model fits frame-by-frame compute pipelines.

#### Animation timeline — `0x0900`–`0x09FF`

```
0x0900 CMD_TIMELINE_CREATE     (duration_ms, easing_hash)
0x0901 CMD_TIMELINE_BIND_SLOT  (timeline_id, slot_id, property)
       Server animates the slot's transform / material color / etc. across the timeline.
0x0902 CMD_TIMELINE_SET_KEYFRAMES  (timeline_id, keyframes_hash)
0x0903 CMD_TIMELINE_PLAY       (timeline_id, start_offset_ms)
0x0904 CMD_TIMELINE_PAUSE      (timeline_id)
0x0905 CMD_TIMELINE_DESTROY    (timeline_id)
```

Server-side animation lets the client send "animate from A to B over 200 ms" once instead of every frame's intermediate state.

#### Audio — `0x0A00`–`0x0AFF` (or separate cdev)

Audio may use a parallel protocol on a different cdev (`/dev/fresco-audiod0`) using the same record/ring shape. Reserved here in case it folds in.

```
0x0A00 CMD_AUDIO_OPEN_STREAM   (format, sample_rate, channels)
0x0A01 CMD_AUDIO_WRITE         (stream_id, samples_hash, presentation_time_us)
0x0A02 CMD_AUDIO_SET_VOLUME    (stream_id, volume_q15)
0x0A03 CMD_AUDIO_CLOSE_STREAM  (stream_id)
```

#### Capability / permission — `0x0B00`–`0x0BFF`

```
0x0B00 CMD_CAP_REQUEST         (capability_id, justification_hash)
       Server may emit COMP_CAP_GRANTED / COMP_CAP_DENIED (potentially after user prompt).
0x0B01 CMD_CAP_RELINQUISH      (capability_id)
       Voluntarily drop a previously-granted capability.
0x0B02 CMD_CAP_QUERY           (capability_id)
       Returns COMP_CAP_STATE.
```

#### 3D rendering — `0x0C00`–`0x0CFF`

Core today already includes the 3D primitives needed for any perspective-projected scene: `NODE_CAMERA` (fov + near/far + view matrix), `NODE_TRANSFORM` (4×4 matrix, full 3D), `NODE_MESH` (3D vertex data + index buffer). The opcodes here extend that with the parts of a modern 3D pipeline that aren't expressible as plain transform + mesh:

```
0x0C00 CMD_LIGHT_ADD          (light_hash) → light_id (within current scene/window)
       NODE_LIGHT blob carries (kind, position, direction, color, intensity, range, ...)
0x0C01 CMD_LIGHT_REMOVE       (light_id)
0x0C02 CMD_LIGHT_UPDATE       (light_id, light_hash)

0x0C10 CMD_ENVIRONMENT_SET    (cubemap_hash, irradiance_hash, specular_hash)
       IBL: ambient lighting from environment + prefiltered specular.
0x0C11 CMD_REFLECTION_PROBE_ADD (probe_hash) → probe_id
0x0C12 CMD_REFLECTION_PROBE_REMOVE (probe_id)

0x0C20 CMD_SHADOW_CONFIG      (mode, resolution, cascade_count)
       mode: NONE | SHADOW_MAP | RT_SHADOWS
0x0C21 CMD_SHADOW_CASTER_SET  (slot_id, casts: bool, receives: bool)

0x0C30 CMD_SKIN_BIND          (slot_id, skin_hash)
       Bind a NODE_SKIN (joint hierarchy + inverse bind matrices) to a skinned mesh.
0x0C31 CMD_SKIN_SET_POSE      (slot_id, pose_hash)
       Update joint transforms; may be driven by an animation track.

0x0C40 CMD_INSTANCE_ADD       (slot_id, instance_buffer_hash, count)
       Render `count` instances of the slot's mesh, per-instance data from buffer.
0x0C41 CMD_INSTANCE_UPDATE    (slot_id, instance_buffer_hash)

0x0C50 CMD_LOD_SET            (slot_id, lod_chain_hash)
       NODE_LOD_CHAIN: list of (max_screen_size, mesh_hash) pairs; server selects.

0x0C60 CMD_PARTICLE_EMITTER_SPAWN  (emitter_hash) → emitter_id
0x0C61 CMD_PARTICLE_EMITTER_UPDATE (emitter_id, params_hash)
0x0C62 CMD_PARTICLE_EMITTER_KILL   (emitter_id)
```

**Layered scope for 3D:**

| Tier | What | Targets |
|---|---|---|
| Core (today, v0.1) | NODE_CAMERA + NODE_TRANSFORM + NODE_MESH + basic materials | UI, 2.5D widgets, simple visualizations |
| 3D-basics extension | Lights + PBR materials + IBL + basic shadow maps + skinned meshes | 3D-aware UI, CAD/scientific viz, browser WebGL/WebGPU content |
| 3D-advanced extension | Reflection probes, RT shadows, GPU particles, complex post-pipelines | Game engines, AAA workloads — explicitly second-class for Fresco |

The 3D-basics tier is what Fresco needs to compete with Wayland-via-Vulkan for "modern desktop UI with 3D elements" (rotating object previews, GLTF model browsers, scientific visualizations, browser-rendered 3D). It is **not** trying to displace native Vulkan for game engines — that's an explicit non-goal. Game engines stay on Vulkan / DirectX / Metal.

#### Render passes / multi-pass compositing — `0x0D00`–`0x0DFF`

For compositing 3D content into UI, picture-in-picture, off-screen render-to-texture, mirrors / portals, and post-processing:

```
0x0D00 CMD_RT_CREATE           (width, height, format) → render_target_id
       Creates an off-screen render target (separate from window FBO).
0x0D01 CMD_RT_DESTROY          (render_target_id)
0x0D02 CMD_RT_ATTACH_SCENE     (render_target_id, scene_root_hash, camera_hash)
       Render this scene into this RT each frame. Result available as a texture.
0x0D03 CMD_RT_GET_TEXTURE      (render_target_id) → texture_hash
       Use the RT's content as a NODE_TEXTURE in another scene.

0x0D10 CMD_POST_FX_CHAIN_SET   (window_id, fx_chain_hash)
       Apply a post-processing chain (bloom, color-grade, AA) to the window's FBO.
       NODE_POST_FX_CHAIN is a list of (effect_kind, params_hash).
```

Rendering a 3D scene **into a 2D UI element** is a common pattern (think a 3D model preview embedded in a settings panel; a video element in a browser; a mirror in a 3D environment). The RT approach makes this a first-class operation: the server runs the inner scene, hands back a texture, the outer scene references it.

### 6.4 Completion type ranges (completion ring)

| Range | Category |
|---|---|
| `0x00`–`0x0F` | Generic responses (UPLOAD, FENCE, QUERY, KERNEL, RT) |
| `0x10`–`0x1F` | Window-lifecycle async events |
| `0x20`–`0x2F` | Capability / permission events |
| `0x30`–`0x3F` | Audio events |
| `0x40`–`0x4F` | Clipboard events |
| `0x50`–`0xFE` | Reserved |
| `0xFF` | `COMP_ERROR` |

#### Allocated (0.1)

| Code | Name | id field | result_hash | Purpose |
|---|---|---|---|---|
| `0x01` | `COMP_UPLOAD_COMPLETE` | upload_id | uploaded blob's SHA-256 | CAS upload finished. |
| `0x02` | `COMP_FENCE` | seq | — | Fence retired. |
| `0x03` | `COMP_QUERY_RESULT` | — | the queried hash | hash exists in CAS. (status indicates EXISTS / NOT_FOUND) |
| `0x10` | `COMP_WINDOW_CREATED` | window_id | seq echoed in [0..4] | **Synchronous** response to `CMD_CREATE_WINDOW`. The client awaits this completion. NOT a queued async event. |
| `0x11` | `COMP_WINDOW_RESIZED` | window_id | width@[0..4], height@[4..8] | Async event: resize completed (e.g. drag-end). |
| `0x12` | `COMP_WINDOW_CLOSE_REQUESTED` | window_id | — | Async event: user clicked close button. |
| `0x13` | `COMP_WINDOW_FOCUS` | window_id | — | Async event. status: 1 = focused, 0 = blurred. |
| `0xFF` | `COMP_ERROR` | seq | — | Generic error (status carries detail). |

`COMP_WINDOW_CREATED` is the only window-lifecycle completion that is a synchronous response to a client command (the matching `CMD_CREATE_WINDOW`). The other three (`RESIZED`, `CLOSE_REQUESTED`, `FOCUS`) are async events queued by the server when WM state changes; client libraries that drain the completion ring should distinguish "synchronous response — feed to await_completion" from "async event — enqueue for the application's event loop."

#### Future (sketched)

```
0x04 COMP_KERNEL_DONE           id=kernel_dispatch_seq, result_hash=output_blob
0x05 COMP_RT_DONE               id=rt_dispatch_seq,    result_hash=output_blob
0x14 COMP_WINDOW_MINIMIZED      id=window_id
0x15 COMP_WINDOW_DPI_CHANGED    id=window_id, payload=new_scale
0x20 COMP_CAP_GRANTED           id=capability_id
0x21 COMP_CAP_DENIED            id=capability_id, status=reason
0x22 COMP_CAP_REVOKED           id=capability_id   (admin / user revoked)
0x40 COMP_CLIPBOARD_DATA        id=request_seq, result_hash=content_blob
0x41 COMP_CLIPBOARD_CHANGED     (from subscribe; payload: which selection)
```

#### Status codes

| Code | Name | Status | Meaning |
|---|---|---|---|
| `0x00` | `STATUS_OK` | v0.1 | Success. |
| `0x01` | `STATUS_CAS_FULL` | v0.1 | CAS store is full; can't accept the upload. |
| `0x02` | `STATUS_INVALID_HASH` | v0.1 | Hash mismatch or invalid. |
| `0x03` | `STATUS_EXISTS` | v0.1 | Hash present (response to QUERY). |
| `0x04` | `STATUS_NOT_FOUND` | v0.1 | Hash absent. |
| `0x05` | `STATUS_DENIED` | reserved (v0.2) | Capability not granted. |
| `0x06` | `STATUS_INVALID_ARG` | reserved (v0.2) | Malformed request. |
| `0x07` | `STATUS_TIMEOUT` | reserved (v0.2) | Operation timed out server-side. |
| `0x08` | `STATUS_RESOURCE_EXHAUSTED` | v0.2 (allocated 2026-06-10) | Per-client limit hit: out of slots / windows / CAS budget (`fresco-recovery.md` §5.2). Per-op error; the connection stays healthy. Distinct from `STATUS_CAS_FULL`, which is the server-global signal. |
| `0xFF` | `STATUS_INTERNAL_ERROR` | reserved (v0.2) | Server-side fault. |

v0.1 servers signal "unrecognized opcode" via `COMP_ERROR` with status `0xFF` (other status fields will be assigned in v0.2; clients should treat any non-zero status on `COMP_ERROR` as fatal in v0.1).

### 6.5 Event type ranges (event ring)

| Range | Category |
|---|---|
| `0x01`–`0x0F` | Input devices (key, mouse, scroll, gestures) |
| `0x10`–`0x1F` | Touch / pen / stylus |
| `0x20`–`0x2F` | Window state (resize, dpi, expose) |
| `0x30`–`0x3F` | System (power, network, time-zone) |
| `0x40`–`0x4F` | IME composition |
| `0x50`–`0xFE` | Reserved |
| `0xFF` | Reserved sentinel |

#### Allocated (0.1)

| Code | Name | code | value_a | value_b | Purpose |
|---|---|---|---|---|---|
| `0x01` | `EVT_KEY` | HID usage | pressed (1/0) | 0 | Keyboard. |
| `0x02` | `EVT_MOUSE_MOVE` | 0 | x (logical px) | y (logical px) | Cursor. |
| `0x03` | `EVT_MOUSE_BUTTON` | button (0=L, 1=R, 2=M) | pressed (1/0) | 0 | Click. |
| `0x04` | `EVT_SCROLL` | 0 | dx | dy | Scroll wheel / touchpad. |
| `0x05` | `EVT_RESIZE` | 0 | width | height | Window-system size change (today: server window). |

`target_window` field carries the routing destination as set by the server.

#### Future (sketched)

```
0x10 EVT_TOUCH_DOWN     code=touch_id  value_a=x  value_b=y
0x11 EVT_TOUCH_MOVE     code=touch_id  value_a=x  value_b=y  payload: pressure
0x12 EVT_TOUCH_UP       code=touch_id
0x13 EVT_PEN            code=pen_id    value_a=x  value_b=y  payload: pressure, tilt, button
0x14 EVT_GESTURE_PINCH  code=phase     value_a=scale_q16     value_b=center
0x15 EVT_GESTURE_ROTATE code=phase     value_a=angle_q16     value_b=center
0x20 EVT_WIN_DPI        target_window  value_a=scale_q16
0x21 EVT_WIN_EXPOSE     target_window  value_a/b=damage_rect
0x30 EVT_POWER          code=phase    (suspend|resume|low_battery|unplugged)
0x31 EVT_NETWORK        code=phase    (online|offline|metered)
0x32 EVT_RESYNC_REQUIRED code=reason  (state_loss|evicted_bulk) — server requests full client replay without a transport drop; client treats as epoch change (fresco-recovery.md §3.2)
0x40 EVT_IME_COMPOSE    code=cursor    payload: composition_text
0x41 EVT_IME_COMMIT     code=cursor    payload: committed_text
```

## 7. CAS blob types (NODE_*)

Content blobs in CAS carry a 4-byte header: `(type_u16 LE, version_u16 LE)`, then the payload. Type registry:

### 7.1 Currently allocated (v0.1)

| Type | Name | Purpose |
|---|---|---|
| `0x0001` | `NODE_SCENE_ROOT` | Top-level scene root (legacy CAS-tree scene). |
| `0x0002` | `NODE_SCENE_NODE` | Interior node in the legacy scene tree. |
| `0x0003` | `NODE_CAMERA` | (fov, aspect, near, far, view_xform_hash) |
| `0x0004` | `NODE_TRANSFORM` | 4x4 matrix. |
| `0x0005` | `NODE_RENDERABLE` | mesh_hash + material_hash. |
| `0x0009` | `NODE_NODE_LIST` | List-of-children container (used by slot CAS-children paths). |
| `0x0100` | `NODE_MESH` | vertex_count, index_count, flags, vertex_data_hash, index_data_hash. |
| `0x0101` | `NODE_PATH` | Vector-graphics path header; references segment data. |
| `0x0102` | `NODE_PATH_SEGMENTS` | Raw segment bytes for a `NODE_PATH`. |
| `0x0110` | `NODE_VERTEX_DATA` | Vertex buffer bytes. |
| `0x0111` | `NODE_INDEX_DATA` | Index buffer bytes. |
| `0x0200` | `NODE_MATERIAL_SOLID` | RGBA + flags. |
| `0x0201` | `NODE_MATERIAL_GRADIENT` | linear/radial + stops. |
| `0x0202` | `NODE_MATERIAL_PBR` | Standard PBR (metallic-roughness). Header allocated; full payload finalized as the 3D-basics extension lands. |
| `0x0203` | `NODE_MATERIAL_TEXTURED` | albedo_tex_hash + tint. |
| `0x0300` | `NODE_TEXT` | Inline text node (font_hash, size, utf8 bytes). |
| `0x0301` | `NODE_FONT` | TTF/OTF bytes. |
| `0x0400` | `NODE_TEXTURE` | width, height, format, pixel_data_hash. |
| `0x0401` | `NODE_PIXEL_DATA` | Raw pixel bytes. |

Notes:
- `NODE_PATH` is sometimes referred to as `NODE_PATH_HEADER` in older docs; the canonical name is `NODE_PATH`.
- `NODE_VERTEX_DATA` and `NODE_INDEX_DATA` may be either pre- or post-tessellation; the producer's intent is implicit in usage. A future spec version may distinguish raw vs. tessellated with separate types.

### 7.2 Reserved for future allocations

The currently-allocated range fills `0x0000–0x0009` (sparsely), `0x0100–0x0102`, `0x0110–0x0111`, `0x0200–0x0203`, `0x0300–0x0301`, `0x0400–0x0401`. Future reservations carefully avoid those:

**3D rendering primitives** (`0x0006`–`0x000F`, top-level scene nodes; `0x0006–0x0008` available, `0x0009` is in use):

```
0x0006 NODE_LIGHT             (kind, position, direction, color, intensity, range, inner/outer cone, shadow flags)
                              kind: DIRECTIONAL | POINT | SPOT | AREA | IBL_AMBIENT
0x0007 NODE_LIGHT_LIST        (count, light_hash[]) — multiple lights bound to a scene
0x0008 NODE_ENVIRONMENT       (cubemap_hash, irradiance_hash, specular_hash) — IBL
0x000A NODE_REFLECTION_PROBE  (position, extent, captured_cubemap_hash)
0x000B NODE_RAY_SCENE         (acceleration_structure_hash + scene_root_hash for RT)
0x000C NODE_PARTICLE_SYSTEM   (emitter_params, particle_kernel_hash)
0x000D NODE_TIMELINE          (duration, easing, keyframe_track_hash[])
0x000E NODE_LOD_CHAIN         (count, (max_screen_size_q16, mesh_hash)[])
0x000F NODE_RENDER_TARGET     (width, height, format, attached_scene_hash, camera_hash)
0x0010 NODE_POST_FX_CHAIN     (effect_count, (kind, params_hash)[])
```

**Materials — extended PBR pipeline** (`0x0210`–`0x021F`, avoiding implemented `0x0200–0x0203`):

```
0x0210 NODE_MATERIAL_PBR_EXT  PBR with extensions:
                              clearcoat (intensity, roughness, normal_tex)
                              transmission (factor, tex)
                              volume (thickness, attenuation)
                              anisotropy (strength, rotation, tex)
                              sheen (color, roughness)
                              iridescence
                              specular (factor, color)
0x021F NODE_MATERIAL_CUSTOM_SHADER  (shader_kernel_hash, params_hash)
                              Escape hatch for custom pipelines (gated by extension grant).
```

**Skinning & animation** (`0x0220`–`0x023F`):

```
0x0220 NODE_SKIN              (joint_count, joint_hierarchy[parent_index], inverse_bind_matrices[])
0x0221 NODE_SKINNED_MESH      (mesh_hash, skin_hash, blend_weights_attr_idx)
0x0222 NODE_POSE              (joint_count, joint_transforms[])  // a snapshot
0x0223 NODE_ANIMATION_TRACK   (target_path, interpolation, keyframes[time, value])
0x0224 NODE_ANIMATION_CLIP    (duration, track_count, track_hash[])
0x0225 NODE_BLEND_TREE        (node graph blending multiple clips by weight)
0x0226 NODE_MORPH_TARGETS     (target_count, vertex_delta_hash[])
                              For facial animation, blendshapes.
```

**Text & fonts — extended** (`0x0302`–`0x03FF`, avoiding implemented `0x0300–0x0301`):

```
0x0302 NODE_GLYPH_PATH        (vector path for one glyph at a given size)
0x0303 NODE_TEXT_LAYOUT       (laid-out paragraph: glyph runs + positions)
```

**Compute & raytracing** (`0x0500`–`0x05FF`):

```
0x0500 NODE_KERNEL            (compiled compute kernel; format byte selects SPIR-V / IR)
0x0501 NODE_KERNEL_SOURCE     (uncompiled source, for cache miss path)
0x0502 NODE_AS                (acceleration structure for ray-tracing)
0x0503 NODE_BUFFER            (typed GPU buffer — input or output of a kernel dispatch)
```

**Audio** (`0x0700`–`0x07FF`):

```
0x0700 NODE_AUDIO_SAMPLES     (PCM samples; format declared)
0x0701 NODE_AUDIO_GRAPH       (mixer / effect chain definition)
```

**Clipboard / IPC** (`0x0800`–`0x08FF`):

```
0x0800 NODE_CLIPBOARD_ITEM    ({mime_hash → content_hash} map; multi-format clipboard entry)
0x0801 NODE_MIME_TYPE         (UTF-8 string of MIME type)
```

**Vendor / experimental** (`0xFF00`–`0xFFFF`): unrestricted for vendor extensions.

Future blob types follow the same `(type_u16, version_u16)` header. The version field allows in-place evolution of a blob type's payload — older readers see an older version and can either upgrade their parser or fall back.

### 3D pipeline composition — example

A complete 3D-aware scene in slot terms:

```
slot 1  → NODE_RENDERABLE { mesh: NODE_SKINNED_MESH(...), material: NODE_MATERIAL_PBR(...) }
        + NODE_TRANSFORM (world matrix)
        + NODE_POSE bound via CMD_SKIN_SET_POSE (animated by a NODE_ANIMATION_CLIP through the timeline subsystem)

scene_root → NODE_LIGHT_LIST { directional sun, IBL ambient, three accent point lights }
           + NODE_ENVIRONMENT { skybox cubemap + prefiltered radiance }
           + child slots [skinned characters, static meshes, particle emitters]

window's camera → NODE_CAMERA { perspective, fov 60°, near 0.1, far 1000 }
                + view matrix from a NODE_TRANSFORM updated each frame

shadow config → CMD_SHADOW_CONFIG { SHADOW_MAP, 2048, 4 cascades }
              CMD_SHADOW_CASTER_SET on slots that should cast.

reflection probe → CMD_REFLECTION_PROBE_ADD captured at scene init.
```

Per-frame mutations are tiny: maybe a transform update for the camera, a pose update for animated characters, a particle emitter parameter tweak. The static parts (mesh, materials, environment, lights) stay resident across frames; the protocol just references them by hash. **A static 3D scene with animated camera and characters sends roughly the same per-frame bytes as a static 2D UI** — proportional to mutations, not scene complexity.

## 8. Connection handshake (0.2)

(Reserved for v0.2; current v0.1 implementations skip this and assume mutual support of v0.1.)

Sequence:
1. **Client → server: `CMD_HELLO`** (in the cmd ring's first slot).
   - payload: `(client_min_major, client_min_minor, client_max_major, client_max_minor)`,
     followed by an extension-want list (each: `(extension_id_u32, version_u16)`).
2. **Server → client: `COMP_HELLO`** in the comp ring.
   - status: 0 = OK, non-zero = version unsupported.
   - result_hash[0..8]: `(server_major, server_minor, server_patch, padding...)`.
   - result_hash[8..16]: `server_epoch` (u64 LE) — random value drawn once at server start. A reconnecting client compares epochs: same epoch = server state survived; new epoch = fresh instance, full replay required. See `fresco-recovery.md` §2.
   - payload: extension-grant list (subset of requested + any server-mandatory extensions).
3. Both sides commit to the negotiated `(major.minor)` and the granted extension set.

If the client doesn't send `CMD_HELLO`, it's assumed to speak version 0.1 (the bootstrap version); future spec versions may make handshake mandatory.

## 9. Extensions

Extensions add opcodes, completion types, event types, and/or blob types beyond the core spec. They're negotiated during handshake.

Extension structure:
- **Extension ID**: u32, registered. (Until a registry exists, use `0xVVVVxxxx` where `VVVV` is a vendor prefix you control.)
- **Version**: u16. Independent of core spec version.
- **Documentation**: separate spec doc per extension (e.g. `spec/ext-compute-1.0.md`).

An extension declares its opcode reservations within the `0xFF00`–`0xFFFF` vendor-private range OR (once registered with the central registry) within an officially-allocated range.

Vendors building Fresco-aware hardware will likely want to declare their own extensions for hardware-specific features. The `0xFF00`+ range is reserved for this without coordination.

## 10. Backwards-compatibility rules

Within a major version:

1. New opcodes may be allocated only in reserved ranges. Existing opcodes may not be removed.
2. Existing record fields may not be repurposed. New fields go in reserved padding.
3. Extension grants may be added; never withdrawn within a major version.
4. New status codes / event subtypes may be added. Existing ones may not be repurposed.
5. Servers receiving an unknown opcode in the cmd ring respond with `COMP_ERROR` (status `STATUS_INVALID_ARG`); they don't crash.
6. Clients receiving an unknown completion / event ignore it; they don't crash.
7. Server crash/restart semantics: clients implementing the shadow-state contract (`fresco-recovery.md` §3) reconnect and replay; v0.1 clients and legacy scene-graph (delta-op) clients terminate on server death — the status quo, and permitted. Servers MUST NOT assume clients are recovery-capable.

## 11. Conformance

A conformant implementation:

- Implements all opcodes in the core spec at the supported `(major.minor)` level.
- Honors record byte layouts exactly (no field rearrangement, no size change).
- Reports its supported version via the handshake (when handshake is mandatory).
- Returns `COMP_ERROR` for unsupported opcodes, not crashes.
- Honors the content-addressed semantics: identical input bytes hash identically, blobs deduplicate.

A conformance test suite is part of `fresco-os/fresco-spec` (D7 deliverable). It will exercise:
- Each opcode's record layout (positive and malformed cases).
- Hash determinism across implementations.
- Window-lifecycle semantics (CREATE → DESTROY round trips, ownership, focus).
- Slot-graph transformation invariants.
- Error propagation.

## 12. Stability statement

Until v1.0:
- Major version 0 means breaking changes are permitted between minor revisions.
- Each release explicitly documents what changed.

From v1.0 onward:
- Major version bumps are rare and announced well in advance.
- Minor version bumps are additive only.
- Patch version bumps are clarifications only.

Implementations claiming "Fresco v1" support all v1.0 core features and accept all v1.x clients.

## 13. Cross-references

- Architecture overview: [../ARCHITECTURE.md](../ARCHITECTURE.md)
- Roadmap: [../ROADMAP.md](../ROADMAP.md)
- Graphics subsystem: [../subsystems/graphics.md](../subsystems/graphics.md)
- Transport bindings: [../subsystems/transport.md](../subsystems/transport.md)
- Repo organization: [../ORGANIZATION.md](../ORGANIZATION.md)

## Appendix A — Implementation reference

Current authoritative source for opcode constants:

- `fresco-server/src/command/protocol.rs` (Rust)
- `libfresco/include/fresco.h` and `libfresco/src/protocol.h` (C)
- `fresco-kmod/fresco.c` (kernel constants for control register layout)

These three sources must agree byte-for-byte. CI gate: any change to one without matching changes to the others fails.

## Appendix B — Changelog

- **0.1.0** (2026-04-28) — Initial spec freeze, post-implementation-audit. Documents the wire format as implemented in the multi-client desktop POC. Reserves opcode ranges for compute, ray-tracing, clipboard, audio, capabilities, animation, and the full 3D pipeline (lights, PBR materials, skinning, IBL, shadows, render-to-texture, post-processing). Handshake reserved for 0.2.

  Spec corrections in this revision (vs. the pre-audit draft):
  1. Documented in-use cmd opcodes the original draft omitted: legacy scene-mutation ops (`0x0102–0x0108`) and autonomous-task ops (`0x0200–0x0202`).
  2. Realigned the cmd opcode range table to match the implementation: CAS upload at `0x0000–0x00FF`, scene & slot at `0x0100–0x01FF`, autonomous tasks at `0x0200–0x02FF`. Connection-meta moved to reserved range `0x1000–0x10FF`.
  3. Rewrote the NODE_* registry from real implementation: added `NODE_SCENE_ROOT (0x0001)`, `NODE_SCENE_NODE (0x0002)`, `NODE_NODE_LIST (0x0009)`, `NODE_PATH_SEGMENTS (0x0102)`, `NODE_TEXT (0x0300)`, `NODE_FONT (0x0301)`, `NODE_TEXTURE (0x0400)`. Removed conflicting future-reserved blob types and shifted future allocations to non-conflicting addresses.
  4. Renamed `NODE_PATH_HEADER` → `NODE_PATH` to match implementation.
  5. Marked status codes `0x05–0x08` and `0xFF` as v0.2-reserved (current implementations only emit `0x00–0x04`).
  6. Clarified that `COMP_WINDOW_CREATED` is a synchronous response (matched via `await_completion`); other window-lifecycle completions are async events.
