# Fresco Dynamic Bundles — Wire Protocol Extension

> **Status.** Proposal. Adds two new ops to the CLASS_DISPLAY dictionary published by `fresco-protocol`. Lets clients register custom SPIR-V bundles at runtime and dispatch against the resulting op-ids exactly as they dispatch against the built-in atrium-core ops (rect, texture, path). The architectural decision behind this feature was settled in the "Path A vs Path B" discussion in May 2026 — this doc specifies the mechanics.
>
> **Companion specs.**
> - `fresco-rendering-stack.md` — the broader rendering architecture and §3.4 closed op-id registry.
> - `aqueduct.md` — the envelope substrate; CAS upload + opcode dispatch are existing primitives, this proposal does not extend the substrate.
> - `atrium-gpu-abi-v2.md` — the kernel/userspace GPU ABI. Orthogonal: dynamic bundles live above frescod, not below it. SPIR-V dispatch goes through the same Vulkan/Mesa/libatrium-gpu path the built-in bundles use.
>
> **One-line summary.** Apps register `(SPIR-V compute + vert + frag, params schema)` via aqueduct CAS upload; frescod AOT-compiles, allocates a per-client op-id, replies. App then dispatches with `OP_SCENE_NODE_SET { op_id, params }` like any other scene node. Cross-app dedup, GPU traversal, multi-app composition all apply automatically.

---

## 1. Why this exists

The original `fresco-rendering-stack.md` §3.4 specifies a closed registry of op-ids: atrium-core owns `0x1000–0x1FFF` (rect, texture, path today), atrium-text owns `0x2000–0x2FFF` (reserved for D3), and so on. The expectation was that all rendering primitives ship as first-party bundles loaded at frescod startup.

That model serves stock UI workloads (most apps need only rect + texture + path + glyph). It does not serve apps that want primitives outside the first-party bundle set:

- A particle system with a custom advection kernel.
- A custom 2D primitive (waveform display, spectrogram, graph viz, signed-distance-field UI library).
- A specialty rendering effect (custom blur, distortion, color grading) that doesn't fit a pre-baked atrium-core op.
- A vendor or first-party app that wants its own primitives without waiting for upstream bundle inclusion.

The two alternatives we considered:

1. **Reserve op-id ranges per vendor (the "2.4 GHz unlicensed band" model).** Coordination nightmare. Two unrelated apps both pick op_id 0x8000, conflict at frescod. Rejected.
2. **Ship app-specific ops as PRs against atrium-core.** Wrong granularity. atrium-core is a curated set; not every app's particle system belongs there.

Dynamic registration sidesteps both: op-ids are server-allocated handles, scoped to a single client connection, returned at registration time. No coordination across clients. No PR queue. Existing dispatch path unchanged.

The architectural commitment we settled on previously: **all custom rendering goes through this mechanism** rather than through a Path-B-style "client renders into a surface, hands the surface to the compositor" escape hatch. This proposal is the mechanism. The client-surface escape is explicitly NOT proposed here (see §12).

---

## 2. Goals and non-goals

### Goals

- A wire-format mechanism for clients to upload custom SPIR-V at runtime.
- Server-allocated op-ids with no cross-client coordination.
- Zero changes to the existing dispatch path: registered bundles dispatch identically to atrium-core's built-in bundles.
- Validation gate: malformed SPIR-V or schema-violating bundles are rejected at registration time, never dispatched.
- Per-client lifetime: bundles are dropped when the client disconnects; no zombie state.
- Reuses existing primitives (aqueduct CAS upload, postcard payload encoding, OP_SCENE_NODE_SET dispatch). No new substrate.

### Non-goals

- **System-wide / persistent bundles** that survive client disconnects. Possible later if real demand emerges; out of scope for v1.
- **Cross-client bundle sharing.** Client A's bundle is not visible to client B's op-id space. Two clients registering the "same" bundle produce two independent registrations. Acceptable redundancy; sidesteps trust + permission complexity.
- **Bundle signing or trust model.** Any client can register any well-formed SPIR-V. The GPU's hardware isolation is the only barrier; if validation passes, the bundle dispatches. Future work may add signed-bundle support if abuse appears.
- **Path B (client-rendered surfaces).** Not proposed here. See §12.
- **Hot-update / re-register on existing op_id.** Once an op_id is allocated, it's immutable for the connection's lifetime. Clients that want to update unregister + re-register; the new op_id is different.

---

## 3. Op-ID allocation model

### 3.1 ID space partitioning

Within the 32-bit op_id space (per `fresco-rendering-stack.md` §3.4):

| Range | Owner | Notes |
|---|---|---|
| `0x0000–0x0FFF` | reserved (control / system) | not for SCENE_NODE_SET |
| `0x1000–0x1FFF` | atrium-core (first-party) | rect=0x1000, texture=0x1001, path=0x1002 |
| `0x2000–0x2FFF` | atrium-text (first-party, reserved) | D3 milestone |
| `0x3000–0x3FFF` | animation (first-party, reserved) | D4 |
| `0x4000–0x4FFF` | AX (first-party, reserved) | D5 |
| `0x5000–0x7FFF` | reserved for future first-party bundles | |
| **`0x8000–0xFFFF`** | **dynamic vendor allocation** | this proposal |
| `0x10000–0xFFFFFFFF` | reserved | future expansion |

Dynamic registrations are allocated from the `0x8000–0xFFFF` range. That's 32,768 slots — comfortable for any single client's needs.

### 3.2 Per-client allocation

Op-ids are scoped to the client connection that registered them. Internally frescod keys its bundle table by `(client_id, op_id)` pairs.

Implications:

- Two clients can independently hold op_id `0x8000`. They refer to different bundles. No conflict.
- Client A's `OP_SCENE_NODE_SET { op_id: 0x8000, ... }` is dispatched against A's bundle for op_id 0x8000. Client B's identical op-code goes to B's bundle.
- A client's op_id space is its own. The first registration in a connection gets `0x8000`; the second gets `0x8001`; etc. The actual numbers are an implementation detail — clients should treat them as opaque handles.
- On client disconnect, all that client's bundles are dropped, their op_ids free for the next client.

This model has the same scoping discipline as file descriptors: the kernel doesn't ask processes to coordinate fd numbers; each process has its own fd table. The same intuition applies here.

### 3.3 First-party op-ids are global

Op_ids in the first-party ranges (`0x1000–0x7FFF`) are global, not per-client. Any client can dispatch atrium-core's `RECT` op at 0x1000 without registration. The dynamic mechanism does not apply to first-party bundles.

---

## 4. Wire ops

Two new ops in the CLASS_DISPLAY dictionary, defined alongside the existing window and scene ops in `fresco-protocol`:

```
OP_BUNDLE_REGISTER         (control op, client → frescod, request)
OP_BUNDLE_UNREGISTER       (control op, client → frescod, request)
```

Both return responses with the `IS_RESPONSE` envelope flag.

### 4.1 OP_BUNDLE_REGISTER

```rust
#[derive(Serialize, Deserialize)]
pub struct BundleRegisterPayload {
    pub manifest:           BundleManifest,
    pub spirv_compute:      Hash256,    // CAS hash of SPIR-V binary
    pub spirv_vertex:       Hash256,
    pub spirv_fragment:     Hash256,
}

#[derive(Serialize, Deserialize)]
pub struct BundleManifest {
    pub op_name:            String,         // human-readable, for debug + telemetry
    pub params_size:        u32,            // exact bytes of the params blob in
                                            // SCENE_NODE_SET payloads for this op
    pub max_instances:      u32,            // upper bound on instances per frame
                                            // (sizes the GPU instance buffer)
    pub render_state:       RenderStateHints,
    pub vulkan_extensions:  Vec<String>,    // required Vulkan extensions
}

#[derive(Serialize, Deserialize, Default)]
pub struct RenderStateHints {
    pub blend_mode:         BlendMode,      // OPAQUE | ALPHA | PREMULT_ALPHA | ADDITIVE
    pub depth_test:         bool,
    pub depth_write:        bool,
    pub cull_mode:          CullMode,       // NONE | FRONT | BACK
    pub topology:           Topology,       // TRIANGLE_STRIP | TRIANGLE_LIST
    /* Reserved trailing fields for forward compat. */
}
```

The three SPIR-V binaries are uploaded via aqueduct CAS *before* the register op (using the substrate's standard `cas::upload_blob` flow). The register op references them by hash; frescod resolves the hashes from CAS, runs the validation pipeline, and replies.

#### Reply

```rust
#[derive(Serialize, Deserialize)]
pub struct BundleRegisterReply {
    pub op_id:              u32,            // server-allocated, in 0x8000..=0xFFFF
}
```

The reply is sent with `IS_RESPONSE` flag set on the same op-code (`OP_BUNDLE_REGISTER`), matching the WINDOW_CREATE pattern already established for response-bearing ops.

#### Errors

If validation fails, the reply payload is an error variant:

```rust
#[derive(Serialize, Deserialize)]
pub enum BundleRegisterError {
    HashUnresolved { which: ShaderKind },           // CAS hash not present
    SpirvValidationFailed { which: ShaderKind, message: String },
    SpirvReflectMismatch { detail: String },        // bindings don't match manifest
    UnsupportedExtension(String),
    ResourceLimitExceeded { limit: ResourceLimit },
    PipelineCompileFailed { message: String },      // AOT compile error
    OpIdSpaceExhausted,                             // shouldn't happen with 32k slots
}
```

Errors are postcard-encoded into a separate response variant (different envelope flag bits or a discriminated union — see §13).

### 4.2 OP_BUNDLE_UNREGISTER

```rust
#[derive(Serialize, Deserialize)]
pub struct BundleUnregisterPayload {
    pub op_id:              u32,
}
```

Releases the bundle. After acknowledgment, future `SCENE_NODE_SET { op_id, ... }` calls referring to this op_id are silently dropped (rationale: a client may have nodes with stale op_ids in flight; ignoring is safer than erroring).

GPU resources (compiled pipelines, instance buffers) are freed when no in-flight frames reference the bundle's nodes — frescod waits for the next `SCENE_FRAME_END` after which the bundle's nodes have all been cleared from per-window state, then releases.

#### Reply

```rust
#[derive(Serialize, Deserialize)]
pub struct BundleUnregisterReply {
    pub ok: bool,            // false if op_id wasn't registered (or already unregistered)
}
```

---

## 5. Manifest details

### 5.1 `params_size`

This is the single most important manifest field for safety. When frescod dispatches `SCENE_NODE_SET { op_id, params }` for a dynamic bundle, it expects `params.len() == manifest.params_size`. Mismatched payloads are dropped with a log entry; they don't corrupt the GPU.

Inside the SPIR-V compute kernel, the params blob is the per-instance "scene node" record — exactly analogous to `SceneNode` in `bundles/atrium-core/compute/op_rectangle.comp`. The kernel reads from a host-mapped buffer, writes to the instance buffer, and the render pipeline draws from there.

The manifest does not specify the *layout* of the params struct (i.e., what's at offset 0 vs offset 4). That's encoded entirely in the SPIR-V's `SceneNode` struct definition; frescod sees the params as opaque bytes and copies them through. Schema mismatch between client and shader is a client bug — frescod can't validate it, but malformed SPIR-V can't escape the GPU sandbox either.

### 5.2 `max_instances`

Sizes the GPU instance buffer. Set high enough for the worst-case frame for this op; setting it too low causes drops (logged), too high wastes VRAM. atrium-core's bundle uses 65536; that's a reasonable upper bound default.

### 5.3 `render_state`

The render pipeline needs to know things the SPIR-V doesn't encode:

- Blend mode: how to combine fragment output with the framebuffer.
- Depth test / write: whether to use the depth buffer.
- Cull mode: which faces to discard.
- Topology: TRIANGLE_STRIP (atrium-core's default — 4 verts per quad) vs TRIANGLE_LIST (general meshes).

These map directly to Vulkan `VkPipelineColorBlendStateCreateInfo`, `VkPipelineDepthStencilStateCreateInfo`, etc. when frescod compiles the render pipeline.

Reserved trailing fields let new render-state knobs be added without breaking existing manifests.

### 5.4 `vulkan_extensions`

Strings like `"VK_EXT_descriptor_indexing"` or `"VK_KHR_buffer_device_address"`. frescod checks that all listed extensions were enabled when its own `VkDevice` was created; if not, registration fails with `UnsupportedExtension`. This prevents a bundle from using features the running frescod can't honor.

---

## 6. SPIR-V validation pipeline

When `OP_BUNDLE_REGISTER` arrives, frescod runs the following on each of the three SPIR-V binaries:

1. **Resolve from CAS.** `cas::resolve(hash)` — error `HashUnresolved` if missing.
2. **`spirv-val` invocation.** The Khronos-distributed validator. Mandatory; any validator error kills the registration with `SpirvValidationFailed`.
3. **Reflection.** Uses the existing `fresco-vulkan::reflect` module (the same one that processes atrium-core's bundles). Extracts: descriptor set bindings, push constant ranges, entry points, used Vulkan capabilities.
4. **Schema cross-check.** Reflected bindings must match the conventions atrium-core bundles use (compute kernel binds: scene buffer, instance buffer, counter; vertex shader binds: instance buffer, screen uniform; fragment is freeform). Mismatches → `SpirvReflectMismatch` with diagnostic detail.
5. **Resource limit check.** Per-driver limits on descriptor sets, push constant size, etc. If exceeded → `ResourceLimitExceeded`.
6. **AOT pipeline compile.** Same code path as `fresco_vulkan::pipeline::OpPipelines::create` — produces compute + render `vk::Pipeline` objects. Compile errors (which can happen for valid SPIR-V that hits driver bugs or unsupported feature combinations) → `PipelineCompileFailed`.

Only if all six steps succeed does frescod allocate an op_id and reply.

The validator + reflection + AOT compile pipeline is shared verbatim with atrium-core's startup-time bundle loading. A dynamic bundle that survives all these checks runs through *the same dispatch code* as a built-in bundle — there is no second code path.

---

## 7. Lifecycle

```
Client                                     frescod
──────                                     ────────

# Setup once per bundle.
upload SPIR-V compute              ──→     CAS::store(hash_c)
upload SPIR-V vertex               ──→     CAS::store(hash_v)
upload SPIR-V fragment             ──→     CAS::store(hash_f)
OP_BUNDLE_REGISTER {manifest,
   hash_c, hash_v, hash_f}         ──→     resolve hashes
                                            spirv-val × 3
                                            reflect × 3
                                            cross-check schema
                                            allocate op_id (e.g. 0x8000)
                                            AOT compile pipelines
                                            register in (client_id, 0x8000)
                                            allocate OpFrameResources
                                   ←──     IS_RESPONSE { op_id: 0x8000 }

# Per frame.
OP_SCENE_FRAME_BEGIN               ──→
OP_SCENE_NODE_SET { node_id: N,
   op_id: 0x8000,
   params: <params_size bytes> }   ──→     EnvelopeFrontend stores in
                                            per-window state
... more nodes ...
OP_SCENE_FRAME_END                 ──→     render walk:
                                            - rect nodes via atrium-core RECT
                                            - texture nodes via atrium-core TEXTURE
                                            - path nodes via atrium-core PATH
                                            - 0x8000 nodes via dynamic bundle
                                            single pass, GPU traversal applies
                                            scanout to BO, page-flip

# Teardown.
OP_BUNDLE_UNREGISTER {op_id: 0x8000} ──→   wait for next FRAME_END
                                            free OpFrameResources
                                            destroy compiled pipelines
                                   ←──     IS_RESPONSE { ok: true }

# Or implicit teardown on disconnect.
client closes UDS                  ──→     drop all bundles registered by
                                            this client_id; free GPU resources
```

---

## 8. Resource limits

Configurable in frescod, with defaults:

| Limit | Default | Rationale |
|---|---|---|
| Bundles per client | 64 | An app with many custom primitives might need 10–20; 64 is generous. |
| Total bundles per frescod instance | 4096 | 64 clients × 64 bundles. |
| SPIR-V size per shader | 256 KiB | Real shaders are typically 2–20 KiB; 256 KiB catches accidents. |
| Instance buffer size per bundle | derived from `max_instances` × instance record size, capped at 16 MiB | Large enough for 65k full-record instances. |
| `max_instances` per bundle | 65536 | Matches atrium-core. Higher requires explicit admin override. |
| `params_size` | 4 KiB max per node | Catches manifest typos; real ops use 16–48 bytes. |

Limits exceeded at register time → `ResourceLimitExceeded` with the specific limit named. Limits exceeded at dispatch time (e.g., more nodes than `max_instances`) → log + drop excess, render best-effort.

---

## 9. Security considerations

### 9.1 Threat model

Clients are partially trusted: they're running in a Portcullis jail with limited capabilities, but they're not signed or attested. A malicious or buggy client may submit arbitrary SPIR-V trying to:

- Read or write memory outside its bundle's allocated buffers (mitigated by GPU hardware MMU per VM)
- Cause a GPU fault or hang (mitigated by per-vendor hang recovery; see GPU ABI v2 §10)
- Exhaust GPU memory (mitigated by per-bundle resource limits, §8)
- Exfiltrate data via timing channels (acknowledged risk; out of scope for v1)
- Use unimplemented Vulkan features that crash the driver (mitigated by spirv-val + extension-allowlist in manifest)

### 9.2 Validation is not airtight

`spirv-val` catches malformed SPIR-V. It does not catch:

- Algorithmically valid SPIR-V that exhibits pathological behavior (infinite-loop compute kernels, fragment shaders that always discard).
- Driver-specific bugs triggered by valid SPIR-V (every GPU vendor has them).
- Resource-exhaustion attacks that fit within the manifest's stated limits.

Each of these requires *some* trust in the client. Per-client jail boundaries (Portcullis) are the layer that handles "this app is malicious"; the bundle dispatch layer assumes clients can be buggy but not adversarial-with-state-of-the-art-GPU-exploits.

### 9.3 No signed-bundle requirement in v1

We could require bundles to be signed by an Atrium-trusted key before frescod accepts them. This would shift the trust model from "any client" to "any client whose bundle was reviewed by a trusted party." Defer; revisit if abuse patterns appear in the wild.

### 9.4 Fault isolation

A bundle's compute kernel that GPU-faults can take down the entire GPU context (see GPU ABI v2 §10). Concretely: one buggy bundle from one app can break rendering for *all* apps until the queue is reset.

This is the same risk all GPU-shader-dispatch systems have (browsers shipping WebGL/WebGPU shaders, etc.). The mitigation strategies are the standard ones — vendor-driver hang detection + queue reset + Vulkan device-lost semantics propagating to clients. frescod's recovery path on GPU device-lost: drop all client bundles, re-create the device, ask clients to re-register. Painful but bounded.

---

## 10. Worked example: a particle system

A client wants to render 50,000 animated particles each frame with a custom GPU update kernel.

### 10.1 Build-time

The client ships:
- `particle_advect.comp.spv` — compute kernel that advances each particle's position based on per-instance velocity + global time
- `particle_quad.vert.spv` — vertex shader emitting a billboard quad per particle
- `particle_quad.frag.spv` — fragment shader applying the particle color + alpha falloff

Plus a manifest, conceptually:

```rust
BundleManifest {
    op_name: "particle".into(),
    params_size: 32,           // per-particle: pos[3], vel[3], color[4], life[1]
    max_instances: 65536,
    render_state: RenderStateHints {
        blend_mode: BlendMode::PremultAlpha,
        depth_test: true, depth_write: false,  // soft particles
        cull_mode: CullMode::None,
        topology: Topology::TriangleStrip,
    },
    vulkan_extensions: vec![],
}
```

### 10.2 Runtime registration

```rust
let conn = fresco_client::Connection::connect("/tmp/frescod.sock")?;

// Upload SPIR-V.
let h_c = conn.upload_blob(&include_bytes!("../shaders/particle_advect.comp.spv"))?;
let h_v = conn.upload_blob(&include_bytes!("../shaders/particle_quad.vert.spv"))?;
let h_f = conn.upload_blob(&include_bytes!("../shaders/particle_quad.frag.spv"))?;

// Register.
let particle_op_id = conn.bundle_register(BundleRegisterPayload {
    manifest: ...,
    spirv_compute: h_c,
    spirv_vertex:  h_v,
    spirv_fragment: h_f,
})?;
// particle_op_id is e.g. 0x8000 — opaque handle for this connection.
```

### 10.3 Per-frame dispatch

```rust
let mut frame = conn.frame()?;       // FrameBuilder helper from fresco-client

for (i, particle) in particles.iter().enumerate() {
    frame.scene_node_set(
        i as u32,
        particle_op_id,
        bytes_of(particle),          // 32 bytes per Manifest::params_size
    )?;
}

frame.finish()?;
```

That's it. frescod sees `OP_SCENE_NODE_SET { op_id: 0x8000, ... }` for each particle, stores in per-window state, and during the frame's render walk dispatches the registered bundle exactly the way it dispatches atrium-core's RECT or PATH ops.

The client wrote ~30 lines of Rust + 3 small SPIR-V files. No frescod modifications, no atrium-core PR, no upstream coordination. The particle renderer is a first-class participant in the merged-scene render pass — it benefits from cross-app composition, multi-window dispatch, and the same GPU traversal model atrium-core ops use.

---

## 11. Implementation in frescod

Touch points, by crate:

### `fresco-protocol`
- Add `OP_BUNDLE_REGISTER`, `OP_BUNDLE_UNREGISTER` constants in the `control` module.
- Add `BundleRegisterPayload`, `BundleManifest`, `RenderStateHints`, `BundleRegisterReply`, `BundleRegisterError`, `BundleUnregisterPayload`, `BundleUnregisterReply` types.
- Add op-id constants for the dynamic range bounds (`DYNAMIC_OP_ID_BASE = 0x8000`, `DYNAMIC_OP_ID_END = 0xFFFF`).

### `fresco-scene-server`
- Extend `EnvelopeFrontend` with a `dynamic_bundles: HashMap<(ClientId, u32), Arc<DynamicBundle>>` field.
- New `DynamicBundle` struct holding the manifest + a `RegisteredBundle` from fresco-vulkan (wraps the AOT-compiled pipelines + frame resources).
- Implement `handle_bundle_register` / `handle_bundle_unregister` dispatch methods.
- Allocate op_ids per-client via a small per-client counter starting at `DYNAMIC_OP_ID_BASE`.
- Extend `WindowSceneState` to handle nodes with op_ids ≥ 0x8000 (goes into a separate `dynamic_nodes: HashMap<u32, HashMap<u32, Vec<u8>>>` keyed by `(op_id, node_id) → params`).
- On client disconnect (existing cleanup_client path), drop all that client's dynamic bundles.

### `fresco-vulkan`
- Add a `register_dynamic_bundle()` API to `HeadlessRenderer` that takes the same inputs as the existing `load_bundle()` but for a single op rather than a manifest-loaded set.
- Extend the per-frame render walk to dispatch dynamic bundles alongside the built-in OP_ID_RECT / OP_ID_TEXTURE / OP_ID_PATH branches.
- Add validation pipeline: spirv-val invocation (subprocess, since libspirv-val isn't great Rust-bindable; or shaderc/SPIRV-Cross integration), reflection, schema cross-check.

### `frescod`
- Per-frame render loop: include dynamic_nodes in the merged scene gathering. Each (op_id, list_of_(node_id, params)) becomes a separate dispatch in render_to_buffer.

### `fresco-client`
- New `bundle_register()` / `bundle_unregister()` convenience methods.

Estimated total: ~500–800 LoC across these crates, plus ~100 LoC of new test coverage.

---

## 12. Out of scope: client-rendered surfaces (Path B)

This proposal does **not** include a wire op for clients to hand frescod a pre-rendered GPU surface (the Wayland/dmabuf model). The omission is deliberate:

- The architectural commitment in May 2026 was to make Path A (scene-graph dispatch) the only client-rendering path, with dynamic bundles as the escape valve for primitives outside atrium-core's set.
- Adding a Path B op (e.g., `OP_SLOT_SET_SURFACE { slot_id, share_fd, sync_fd, sync_value }`) would be a small wire-protocol change *enabled* by GPU ABI v2's `share_fd` mechanism, but it would let clients bypass the architecture for their viewport content — which defeats the cross-app dedup + GPU-traversal wins for those clients.
- 3D-engine ports (UE/Godot/bevy) are the obvious population that wants Path B. The honest answer for them today is "either express your renderer as bundles (probably impractical), or wait for a future Path B op (not currently planned)."

If/when the case for Path B becomes compelling — either because a real engine port is on the table or because the architecture wins on Path A turn out smaller than projected — the substrate already supports it (GPU ABI v2 §6.1, §6.2). Adding the op then is a follow-up proposal.

---

## 13. Open questions

a. **Reply variant encoding.** A `BundleRegisterReply` should be either `{op_id}` on success or `BundleRegisterError` on failure. Encode as `Result<Reply, Error>` (postcard supports it cleanly), or use distinct envelope flags? Current convention (e.g., WINDOW_CREATE) doesn't have a precedent for response-with-error; this is the first op to need it.

b. **CAS-resolve timing.** Does frescod resolve hashes synchronously during register-op handling (blocks the client thread for ms), or async (returns a "pending" id that becomes valid after a notification op)? Synchronous is simpler; async is needed if SPIR-V uploads are large (multi-MB).

c. **`spirv-val` integration.** Vendoring the validator, calling out to a separate process, or linking against libSPIRV-Tools? Linking is cleanest but adds a build dep; subprocess is portable but slow and security-relevant (we're running validation on untrusted SPIR-V).

d. **Bundle dedup across clients.** If two clients register byte-identical SPIR-V, frescod could share the compiled pipelines. This trades implementation complexity for memory savings. Probably defer; current per-client allocation is simpler.

e. **Render-pass compatibility.** Dynamic bundles render into the same color attachment as atrium-core. If a bundle wants its own depth/stencil buffer or multiple color attachments, it can't have them in v1. Acceptable scope for a first release; advanced bundles wait for v2.

f. **Pipeline-derivative caching.** A common pattern is "register N bundles that differ only in one parameter." Vulkan supports pipeline derivatives for cheap recompilation. Worth exposing? Probably defer to v2.

g. **Hot SPIR-V reload (debugging).** Ergonomics for developers writing bundles: edit shader, recompile, see updated render. This is unregister + re-register today (different op_id). Could add a "swap in place" op for development. Defer.

h. **Telemetry.** Should frescod expose registered bundles via sysctl for `dtrace`-style observation? Useful for debugging but adds a kernel-visible surface.

---

## 14. What happens next

1. **Round 1 (this document).** Internal Atrium review.
2. **Round 2.** Implement the v1 minimum in `fresco-protocol` + `fresco-scene-server` + `fresco-vulkan` + `fresco-client`. ~500–800 LoC. Add a sample bundle (a particle system or signed-distance-field UI primitive) as proof of life and integration test.
3. **Round 3.** End-to-end verification: a client registers a bundle, dispatches it, the result renders correctly, multi-app composition with built-in bundles still works.
4. **Round 4.** Document migration: how would today's atrium-core PATH op (built-in) look if it had been a dynamic bundle? Useful as a tutorial for prospective bundle authors.
5. **Stable.** Once shipped and a couple of real bundles are in production, freeze the v1 wire format. Future evolution via the trailing-fields extension model.

Estimated calendar time to (3): 2-4 weeks of focused work, given the substrate is in place.

---

*Dual-licensed MIT or Apache-2.0, matching the rest of Atrium. Discussion welcome at the Atrium project repository.*
