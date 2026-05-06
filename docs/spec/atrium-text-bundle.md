# atrium-text bundle — Design

> **Status.** Draft for review. Specifies the atrium-text SPIR-V bundle (op-id range `0x2000-0x2FFF`) and the wire-protocol additions to dispatch text rendering as scene-graph nodes rather than per-glyph TEXTURE-quad hacks. Foundation for D3 (real text rendering) and a prerequisite for Pergola (M7) once it needs labels / buttons / paragraphs.
>
> **Scope.** This document covers M6.1 (the foundational atlas-based glyph_run op with client-side shaping). M6.2 (migrate the four shipping socket apps), M6.3 (move shaping into frescod for cross-app dedup + multi-script), and M6.4 (atlas eviction + regeneration) are scoped here as future work.
>
> **Companion specs.**
> - `fresco-rendering-stack.md` §3.4 (closed op-id registry — atrium-text owns `0x2000-0x2FFF`)
> - `fresco-protocol` source (existing OP_SCENE_NODE_SET, OP_SLOT_SET dispatch)
> - `bundles/atrium-core/` (reference implementation pattern — rect, texture, path)
> - `fresco-dynamic-bundles.md` (vendor-bundle registration; atrium-text is first-party so it ships at startup like atrium-core, but the SPIR-V shape is the same)

---

## 1. The problem we're solving

Today, every shipping demo that displays text uses a hack:

- `atrium-test-client/text_demo.rs`: shapes a string with rustybuzz, then issues one `OP_SLOT_SET` (texture upload) + one `OP_SCENE_NODE_SET(TextureParams)` *per glyph*. "Hello, FreeBSD!" becomes 14 slots + 14 texture nodes.
- `atrium-edit-socket`: pre-shapes 94 ASCII chars at startup into 94 slots; per frame emits one TEXTURE node per visible character. Burns 94 GPU texture bindings per process; can't render anything outside the pre-baked set; no kerning, ligatures, RTL, or complex scripts.
- `atrium-term-socket`, `atrium-find-socket`: same pattern.

Limitations of the current approach:

1. **One slot per glyph** wastes per-window slot table entries. With 94 ASCII chars, four open apps eat ~400 slots before any other texture work.
2. **One TEXTURE node per visible glyph** means a 24-row x 80-col terminal page can emit ~1900 envelope sets per frame just for text. The wire isn't a problem at envelope size, but the per-frame dispatch overhead in EnvelopeFrontend grows linearly with character count.
3. **No real shaping** — what we have is "lay out one glyph per ASCII byte at fixed cell width." Kerning, ligatures, complex scripts, bidi all unimplemented.
4. **No cross-app dedup** — every app uploads its own glyph bitmaps. The same `'a'` rendered by atrium-edit-socket and atrium-term-socket is two CAS uploads of two identical bitmaps.

The atrium-text bundle replaces this. The wire surface for a "draw this text" operation becomes a single `OP_SCENE_NODE_SET(GlyphRunParams)` carrying a pre-shaped glyph run that references a shared atlas. Per-frame dispatch shrinks to one node per text run, the GPU does one indirect-draw per atlas, and (in M6.3) the atlas is shared across apps via CAS.

---

## 2. Design decisions

### 2.1 Atlas-based, not vector-outline

We considered Pathfinder-style stencil-and-cover or Slug-style analytic-AA Bézier rasterization. Both produce sharper results at any size and avoid atlas memory limits. Both are research-grade engineering: Pathfinder is ~5000 LoC of dense GPU code that took the team multiple years to ship.

Atlas-with-regeneration is what every production compositor uses (CoreText, DirectWrite, Skia, FreeType-on-Wayland). The pattern is well-understood, the GPU code is tiny (~one quad-instance per glyph, identical to the existing atrium-core TEXTURE op), and the cost model is predictable.

**Decision: atlas-based for v1. Vector outlines are explicitly out of scope.** Future work can add a vector op alongside atrium-text without breaking glyph_run.

Specific atlas choices:
- **Single shared atlas per font + size.** Apps requesting the same (font_hash, size) get the same atlas slot. M6.1 ships one-atlas-per-(font,size); M6.3 dedups across apps via CAS.
- **R8 single-channel** (not RGBA) for grayscale coverage. 4× memory reduction vs the current `(A,A,A,A)` premultiplied hack.
- **2048×2048 default size**, configurable via bundle manifest. Holds ~1500 ASCII glyphs at 18px or ~600 CJK glyphs at the same size.
- **Sub-rectangles per glyph** stored in a side metadata buffer (UV + advance + bearing), keyed by glyph index within the atlas.

### 2.2 Client-side shaping for M6.1, server-side later

In M6.1, the client (or a small `fresco-text` library it links) does shaping via rustybuzz + swash, builds a glyph run as `Vec<(glyph_index, x_offset, y_offset)>`, and submits it. The server doesn't need rustybuzz to render.

This keeps M6.1 scoped — the wire-format work and GPU bundle are decoupled from the host-side font infrastructure. M6.3 moves shaping into frescod for cross-app dedup; the wire format chosen here accommodates that future change without breaking.

### 2.3 Atlas as a slot binding

The atlas is just a texture, bound via the existing `OP_SLOT_SET` mechanism. No new "atlas binding" op. The glyph run references an atlas slot the same way TextureParams references a texture slot today. This means:

- The atlas's lifecycle is the existing slot lifecycle.
- The atlas can be uploaded once and reused across many glyph runs.
- Atlas regen (M6.4) is just `OP_SLOT_SET` again with new bytes — no special "atlas update" path.

### 2.4 Pre-shaped runs are immutable on the wire

A glyph run is a snapshot of "what to draw," produced by client-side shaping. The wire carries:

- The atlas slot id (which font + size)
- A list of glyph indices + per-glyph (x, y) offsets relative to the run's origin
- The run's origin (x, y) in window coordinates
- Color (RGBA)

If text changes, the client re-shapes and emits a new glyph run via `OP_SCENE_NODE_SET` (replacing the previous node by id). No partial updates.

### 2.5 Subpixel positioning vs pixel snapping

For M6.1: pixel-snap. Glyph (x, y) offsets are integer pixels relative to run origin. This is the ASCII-monospace path that today's atrium-edit-socket / atrium-term-socket use; converting them to glyph_run is a 1:1 swap.

For M6.3+ (proportional fonts, real shaping): sub-pixel positioning becomes meaningful. The wire format already accommodates it (the offsets are f32, not i16); we just don't exploit it in M6.1.

### 2.6 No font management on the wire

The wire never carries font names, file paths, or font binary data. The client-side `fresco-text` library handles font loading (memory-mapped TTF/OTF), shaping, and atlas generation. The wire only sees the *result* (glyph indices into a specific atlas).

This means: server doesn't need to know about fonts at all in M6.1. M6.3 adds a server-side font registry but it's purely additive — clients can keep doing client-side shaping if they want.

---

## 3. Wire op

One new payload type, one new op-id.

### 3.1 Op-id

```
ATRIUM_TEXT_GLYPH_RUN  = 0x2000   (in scene_ops module, alongside ATRIUM_CORE_RECT, etc.)
```

`0x2001-0x2FFF` reserved for future atrium-text ops (atlas update? text-flow shaping? composite glyph runs?).

### 3.2 GlyphRunParams

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlyphRunParams {
    /// Origin of the run in window coordinates (top-left).
    pub x: f32,
    pub y: f32,

    /// Slot containing the glyph atlas (R8 texture, established
    /// via OP_SLOT_SET with SlotKind::Texture { format: R8Unorm }).
    pub atlas_slot_id: u32,

    /// Atlas dimensions in pixels. Carried in the run rather than
    /// queried from the slot to avoid a round-trip; client knows
    /// these from when it built the atlas.
    pub atlas_width:  u32,
    pub atlas_height: u32,

    /// Foreground color (straight RGBA in [0, 1]). Atlas alpha is
    /// multiplied by this to produce the fragment color.
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,

    /// The glyphs in the run.
    pub glyphs: Vec<GlyphInstance>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GlyphInstance {
    /// Position offset from the run's origin, in pixels.
    pub dx: f32,
    pub dy: f32,

    /// Atlas sub-rectangle in atlas pixel coordinates (top-left).
    pub atlas_u: u32,
    pub atlas_v: u32,
    pub atlas_w: u32,
    pub atlas_h: u32,

    /// Glyph metrics for placement. The vertex shader uses these
    /// to position the destination quad relative to the baseline.
    pub bearing_x: f32,
    pub bearing_y: f32,
}
```

Per-glyph payload is 32 bytes (4 floats + 4 u32 + 2 floats). A single line of 80 ASCII chars is ~2.5 KiB on the wire; a full screen of text is ~150 KiB. Postcard's varint encoding of u32 fields shrinks this further for typical small atlases.

For comparison, today's per-glyph TEXTURE-node approach burns ~32 bytes envelope + 24 bytes TextureParams payload = ~56 bytes per glyph in *envelope overhead alone*, before counting the per-frame dispatch overhead in EnvelopeFrontend. The new shape collapses it.

### 3.3 Slot semantics

The atlas slot is a `SlotKind::Texture(TextureDesc { width, height, format: TextureFormat::R8Unorm })`. The atlas bytes are uploaded once via `OP_SLOT_SET` with the existing CAS-upload flow. M6.1 has no atlas update / regen flow — atlas is fixed at slot-set time.

### 3.4 Node lifecycle

GlyphRunParams nodes follow the same per-window lifecycle as RectParams / TextureParams / PathParams nodes today:
- `OP_SCENE_NODE_SET { op_id: 0x2000, node_id, params }` — install or replace
- `OP_SCENE_NODE_CLEAR { node_id }` — remove
- Dropped on window destroy / client disconnect (existing EnvelopeFrontend cleanup)

A node_id can switch op_ids freely (a node that was a rect can become a glyph_run, displacing it from the rect_nodes map).

---

## 4. SPIR-V bundle structure

Mirrors `bundles/atrium-core/` layout exactly:

```
bundles/atrium-text/
├── manifest.json
├── compute/
│   ├── op_glyph_run.comp
│   └── op_glyph_run.comp.spv (gitignored)
├── pipelines/
│   ├── pipe_glyph_run.vert
│   ├── pipe_glyph_run.vert.spv (gitignored)
│   ├── pipe_glyph_run.frag
│   └── pipe_glyph_run.frag.spv (gitignored)
└── build.sh
```

`manifest.json`:

```json
{
  "name":    "atrium-text",
  "version": 1,
  "ops": [
    {
      "id":              8192,
      "name":            "glyph_run",
      "compute_entry":   "compute/op_glyph_run.comp.spv:main",
      "render_pipeline": "pipelines/pipe_glyph_run"
    }
  ],
  "depends_on":   [],
  "gpu_resources": {
    "max_instances": 65536
  }
}
```

### 4.1 Compute kernel: `op_glyph_run.comp`

Reads `SceneNode` records (one per glyph_run node), expands each into one `InstanceRecord` per glyph, writes to instance buffer. Atomic counter tracks total instances.

```glsl
#version 460

struct GlyphInstance {
    vec2 d_offset;      /* dx, dy from run origin */
    vec4 atlas_uv;      /* u, v, w, h in atlas coords */
    vec2 bearing;       /* bearing_x, bearing_y */
};

struct SceneNode {
    vec4 origin;        /* x, y, _, _ — run origin in window coords */
    vec4 atlas_dim;     /* width, height, _, _ — atlas pixel dims */
    vec4 color;         /* r, g, b, a */
    uint glyph_count;
    uint glyph_offset;  /* into the per-frame glyphs[] storage */
    /* glyphs are stored separately to keep SceneNode fixed-size */
};

struct InstanceRecord {
    vec4 dst_rect;      /* x, y, w, h in window coords */
    vec4 src_rect;      /* u0, v0, u1, v1 in atlas UV [0,1] */
    vec4 color;
};

layout(set = 0, binding = 0) readonly buffer SceneBuf {
    uint  node_count;
    uint  _pad[3];
    SceneNode nodes[];
} scene;

layout(set = 0, binding = 1) readonly buffer GlyphsBuf {
    GlyphInstance glyphs[];
} glyph_storage;

layout(set = 0, binding = 2) writeonly buffer InstanceBuf {
    InstanceRecord instances[];
} instance_buf;

layout(set = 0, binding = 3) buffer CounterBuf {
    uint count;
} counter;

layout(local_size_x = 64) in;

void main() {
    uint id = gl_GlobalInvocationID.x;
    if (id >= scene.node_count) return;
    SceneNode n = scene.nodes[id];

    /* Reserve instance-buffer slots for this run's glyphs. */
    uint base = atomicAdd(counter.count, n.glyph_count);

    for (uint i = 0; i < n.glyph_count; i++) {
        GlyphInstance g = glyph_storage.glyphs[n.glyph_offset + i];

        /* Destination rect in window coords. dx/dy are run-relative
         * offsets to the glyph's left edge; bearing_y is the
         * baseline-to-top offset (positive y goes down). */
        vec4 dst = vec4(
            n.origin.x + g.d_offset.x + g.bearing.x,
            n.origin.y + g.d_offset.y - g.bearing.y,
            g.atlas_uv.z,  /* atlas_w == dst_w */
            g.atlas_uv.w   /* atlas_h == dst_h */
        );

        /* Atlas UVs normalized to [0, 1]. */
        vec4 src = vec4(
            g.atlas_uv.x / n.atlas_dim.x,
            g.atlas_uv.y / n.atlas_dim.y,
            (g.atlas_uv.x + g.atlas_uv.z) / n.atlas_dim.x,
            (g.atlas_uv.y + g.atlas_uv.w) / n.atlas_dim.y
        );

        instance_buf.instances[base + i].dst_rect = dst;
        instance_buf.instances[base + i].src_rect = src;
        instance_buf.instances[base + i].color    = n.color;
    }
}
```

### 4.2 Vertex shader: `pipe_glyph_run.vert`

Builds 4-vertex triangle-strip quad per instance. Standard pattern.

```glsl
#version 460

struct InstanceRecord {
    vec4 dst_rect;      /* x, y, w, h */
    vec4 src_rect;      /* u0, v0, u1, v1 */
    vec4 color;
};

layout(set = 0, binding = 0) readonly buffer InstanceBuf {
    InstanceRecord instances[];
} instance_buf;

layout(set = 0, binding = 1) uniform Screen {
    vec2 size;
} screen;

layout(location = 0) out vec2 v_uv;
layout(location = 1) out vec4 v_color;

void main() {
    InstanceRecord inst = instance_buf.instances[gl_InstanceIndex];
    vec2 unit = vec2(
        float((gl_VertexIndex & 1) != 0),
        float((gl_VertexIndex & 2) != 0)
    );

    vec2 pos = inst.dst_rect.xy + unit * inst.dst_rect.zw;
    vec2 clip = (pos / screen.size) * 2.0 - 1.0;
    /* No Y flip — Vulkan default clip is Y-down, our wire convention
     * is top-left pixels. Identity mapping. */

    gl_Position = vec4(clip, 0.0, 1.0);
    v_uv = mix(inst.src_rect.xy, inst.src_rect.zw, unit);
    v_color = inst.color;
}
```

### 4.3 Fragment shader: `pipe_glyph_run.frag`

Samples R8 atlas (single-channel coverage), multiplies by color.

```glsl
#version 460

layout(set = 0, binding = 2) uniform sampler2D atlas;

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 0) out vec4 frag_color;

void main() {
    float coverage = texture(atlas, v_uv).r;
    frag_color = vec4(v_color.rgb * coverage, v_color.a * coverage);
    /* Premultiplied output. The renderer's blend state is set to
     * premultiplied-over (one * src + one_minus_src_alpha * dst),
     * so this composites correctly over arbitrary backgrounds. */
}
```

---

## 5. Implementation breakdown

By crate / file:

### 5.1 `bundles/atrium-text/`
New directory mirroring `bundles/atrium-core/`. `manifest.json`, three GLSL files, `build.sh` (single line: `glslangValidator -V -o "$f.spv" "$f"` for each .comp/.vert/.frag).

### 5.2 `fresco-protocol`
- `scene_ops::ATRIUM_TEXT_GLYPH_RUN = 0x2000` constant.
- `GlyphRunParams` and `GlyphInstance` structs with serde derives.
- Add `R8Unorm` to the `TextureFormat` enum if not present (atlas is single-channel).

### 5.3 `fresco-vulkan`
- `GlyphRunNode` struct mirroring the GPU's `SceneNode` (fixed size; glyphs go in a separate buffer).
- `GlyphInstance` mirror of the GPU struct.
- `OpKind::GlyphRun` variant in `pipeline.rs`; `op_kind(0x2000) → GlyphRun`.
- `OpFrameResources` extended for a separate "glyphs" storage buffer (per-frame variable size, sized at submit time from the total glyph count across all runs).
- `HeadlessRenderer::set_glyph_run_nodes(Vec<GlyphRunNode>, Vec<GlyphInstance>)` pair.
- The render dispatch branch: same shape as TEXTURE op (texture binding from the slot table), but with the extra glyphs storage buffer.

### 5.4 `fresco-scene-server` (EnvelopeFrontend)
- `WindowSceneState::glyph_run_nodes: HashMap<u32, GlyphRunParams>`.
- Dispatch arm for `ATRIUM_TEXT_GLYPH_RUN` in `handle_scene_node_set`.
- `extract_glyph_runs() -> (Vec<GlyphRunNode>, Vec<GlyphInstance>)` (returns the SoA-style split the GPU expects).

### 5.5 `frescod` main.rs
- Per-frame merge: walk windows in z-order, gather glyph_run nodes alongside rects/paths/textures, translate by window position, dispatch via `HeadlessRenderer::set_glyph_run_nodes`.

### 5.6 `fresco-client`
- `scene_node_glyph_run(node_id, params)` convenience method.

### 5.7 `fresco-text` (existing crate)
- Add an "emit-glyph-run-from-shaped-text" helper: take the existing `GlyphAtlas` (already produced by `shape_and_rasterize`) and convert it to a `GlyphRunParams` + `Vec<GlyphInstance>` ready to ship.
- This isolates the atlas-construction work from the rendering work; clients call `shape_and_rasterize` then `to_glyph_run(&atlas, position, color)`.

### 5.8 First migration: `atrium-test-client/text_demo.rs`
Replace the per-glyph slot + per-glyph TEXTURE node hack with: one slot for the atlas + one glyph_run node per text line. Visually verify in VM.

### 5.9 (Deferred to M6.2) Migrate the four socket apps
Same pattern. Each app's `glyph_cache.rs` collapses from 94 separate slots to 1 atlas slot; renderer becomes one glyph_run per text line (or per status row, etc.) instead of one TEXTURE per character.

---

## 6. Worked example — text_demo migration

Today (per-glyph hack):
```rust
let atlas = shape_and_rasterize(&font, "Hello, FreeBSD!", 64.0)?;
conn.scene_frame_begin()?;
for (i, _) in atlas.glyphs.iter().enumerate() {
    let (rgba, gw, gh, (dx0, dy0, dx1, dy1)) = extract_glyph(&atlas, i)?;
    let hash = conn.upload_blob(&rgba)?;
    let slot = 100 + i as u32;
    conn.slot_set_texture(slot, hash, gw, gh, TextureFormat::Rgba8UnormSrgb)?;
    conn.scene_node_texture(slot, TextureParams {
        x: 80.0 + dx0, y: 400.0 + dy0,
        w: dx1 - dx0, h: dy1 - dy0,
        slot_id: slot,
    })?;
}
conn.scene_frame_end()?;
```

After M6.1 (one atlas + one glyph_run):
```rust
let atlas = shape_and_rasterize(&font, "Hello, FreeBSD!", 64.0)?;
let atlas_hash = conn.upload_blob(&atlas.r8_pixels())?;   /* new atlas-to-R8 helper */
conn.slot_set_texture(100, atlas_hash, atlas.width, atlas.height, TextureFormat::R8Unorm)?;

let run = atlas.to_glyph_run(/*x*/ 80.0, /*y*/ 400.0, /*color*/ [1.0, 1.0, 1.0, 1.0]);
conn.scene_frame_begin()?;
conn.scene_node_glyph_run(/*node_id*/ 1, run)?;
conn.scene_frame_end()?;
```

Slots: 14 → 1. SCENE_NODE_SETs: 14 → 1. Wire bytes: ~ 2 KiB → ~700 bytes. Same visual result.

---

## 7. Open questions

a. **R8 vs R8G8 for sub-pixel-AA.** R8 is grayscale coverage. macOS / Windows do sub-pixel-AA (RGB sub-pixel separation) which needs R8G8 or RGB. For M6.1 we ship R8 grayscale (matches what fresco-text produces today); sub-pixel-AA is a future enhancement.

b. **Per-glyph storage buffer indexing.** SceneNode carries `glyph_offset` into a separate `glyphs[]` buffer. This means the kernel sees two storage buffers and the host has to manage their layout per frame. Alternative: inline the glyphs in the SceneNode (variable-size records). Inline is simpler but breaks the atomic-counter dispatch pattern. Sticking with the two-buffer design for parallelism; revisit if measured to be a bottleneck.

c. **Multiple atlases in one frame.** A frame may contain text in multiple fonts/sizes → multiple atlas slots. Each atlas needs its own descriptor set binding (current TEXTURE op binds one texture per dispatch). Either (i) one atrium-text dispatch per atlas, batched per atlas slot id, or (ii) bindless texture array (Vulkan extension `VK_EXT_descriptor_indexing`). M6.1 ships (i); M6.4-ish migrates to (ii) if measured.

d. **Atlas update / regen.** When a missed glyph needs to be added to the atlas (e.g., user types a Unicode char not yet rasterized), the client re-uploads the whole atlas. This is fine for the M6.1 scope (small ASCII atlases); larger atlases need partial-update support, which means a new `OP_SLOT_PATCH_TEXTURE` op or equivalent. Punted to M6.4.

e. **Coordinate convention for `bearing_y`.** Currently we follow the FreeType convention (positive bearing_y = ascender-relative, glyph drawn above baseline). The compute kernel does `dst.y = origin.y + d_offset.y - bearing.y` to account for this. Worth a comment in the wire docs to avoid client-side confusion.

f. **Color emoji.** Color emoji require RGBA atlases (not R8). Plumbing: a second op `ATRIUM_TEXT_GLYPH_RUN_COLOR` or a flag in GlyphRunParams that switches the bound atlas to RGBA. Defer to a later atrium-text op-id (`0x2001`).

g. **Subpixel positioning.** Wire format already supports sub-pixel offsets (f32). Renderer does pixel-snap today; sub-pixel positioning requires either (i) atlas with 4 versions per glyph (0/0.25/0.5/0.75 phase) or (ii) trilinear-filtered atlas + careful UV math. Defer to M6.3+.

---

## 8. Implementation plan + done-when

### 8.1 First commit: bundle + protocol
- Create `bundles/atrium-text/` with manifest, GLSL, build.sh
- Add scene_ops constant + GlyphRunParams + GlyphInstance to fresco-protocol
- Add R8Unorm to TextureFormat if missing
- Build the .spv files; verify with spirv-val
- **Done when:** `bundles/atrium-text/build.sh` produces .spv files that pass spirv-val

### 8.2 Second commit: fresco-vulkan integration
- OpKind::GlyphRun + op_kind() routing
- GlyphRunNode + GlyphInstance Rust mirrors
- OpFrameResources extension for glyphs storage buffer
- HeadlessRenderer::set_glyph_run_nodes API
- Render dispatch branch with R8 atlas binding
- **Done when:** standalone unit test in fresco-vulkan dispatches a single glyph_run and reads back expected pixels (PNG comparison)

### 8.3 Third commit: scene-server + frescod + client integration
- WindowSceneState::glyph_run_nodes
- handle_scene_node_set dispatch arm
- extract_glyph_runs()
- frescod main.rs render-loop merge
- fresco-client::scene_node_glyph_run helper
- fresco-text::to_glyph_run + r8_pixels helpers
- **Done when:** atrium-test-client/text_demo.rs migrated to one-atlas + one-node; smoke binary in VM produces a PNG showing "Hello, FreeBSD!" rendered via the new pipeline; pixel-comparison against the per-glyph-hack version is "visually equivalent" (sub-pixel differences acceptable)

### 8.4 M6.1 done-when (overall)
ASCII text rendering via the new pipeline works end-to-end in QEMU+lavapipe, produces the same visual result as the current per-glyph hack, with the wire-format and slot-table-usage benefits described in §1.

---

## 9. Future work (M6.2 — M6.4)

### M6.2: Migrate the four shipping socket apps
atrium-edit-socket, atrium-term-socket, atrium-find-socket, atrium-clock-socket (the last has no text but the migration is trivial). Each app's `glyph_cache.rs` collapses from "94 slots + 94 materials" to "1 atlas + 1 atlas slot", and each renderer's per-frame work collapses from "N TEXTURE nodes" to "K glyph_run nodes" (K = number of distinct text lines per frame).

Estimated work: ~200-400 LoC removed per app, ~50 LoC added per app (the new glyph_run dispatch).

### M6.3: Server-side shaping + cross-app dedup
Move rustybuzz/swash from client-side `fresco-text` into frescod (or a new `fresco-shaper` crate that frescod links). New ops:
- `OP_TEXT_SHAPE_REQUEST { utf8_text, font_hash, size_px }` → server replies with a `GlyphRun` and a slot id for the atlas.
- Server caches `(text, font, size) → shaped_run` by content hash. Two apps requesting the same shape get the same atlas, the same glyph indices, the same UVs.

Adds:
- Server-side font registry (FONT_REGISTER op, similar to bundle registration but for TTF/OTF data)
- Multi-script support (Devanagari, Arabic, CJK) via rustybuzz's existing capabilities
- BiDi handling (rustybuzz integrates with unicode-bidi)
- Cross-app text dedup: every app rendering "Hello" shares the cached shaping result + atlas

Estimated work: ~2-3 weeks. Architecturally meaningful — completes the spec's §7 vision.

### M6.4: Atlas eviction + partial-update
For long-running sessions where the atlas accumulates many glyphs and may need to evict.
- LRU eviction policy on glyph slots within the atlas
- New op `OP_SLOT_PATCH_TEXTURE { slot_id, x, y, w, h, bytes }` for partial atlas updates without re-uploading the whole texture
- Coordination: client tracks atlas-side glyph cache + signals frescod when an entry must be evicted

Estimated work: ~1-2 weeks. Important for production but not blocking M6.1 / M6.2 / M6.3.

---

## 10. What this doesn't change

- The atrium-core bundle (rect, texture, path) is unaffected. Apps continue to use those for non-text rendering.
- The wire envelope format (aqueduct CLASS_DISPLAY) is unchanged. atrium-text just adds new payload types, dispatched through the existing `OP_SCENE_NODE_SET`.
- Pergola (M7) is unblocked once M6.1 ships — its first label widgets will use glyph_run directly via fresco-client.
- The Atrium GPU ABI v2 work is orthogonal. atrium-text runs on whichever Vulkan backend frescod is built against.

---

*Dual-licensed MIT or Apache-2.0, matching the rest of Atrium. Discussion welcome at the project repository.*
