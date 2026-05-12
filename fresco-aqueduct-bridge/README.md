# fresco-aqueduct-bridge

Translation layer: **fresco-protocol** scene nodes → **aqueduct-gpu**
frame-command-stream records.

The bridge is the architectural seam where frescod's renderer becomes
wire-only. Pure functions; no I/O; no scene-graph walking.

## Where this fits

```
frescod (scene server, owns fresco-protocol parsing)
   │
   ▼  builds RectParams / PathParams / TextureParams / GlyphRunParams
fresco-aqueduct-bridge   ← this crate, per-node translators
   │
   ▼  emits FrameOp records into a FrameBuilder
aqueduct-gpu-client::GpuClient
   │
   ▼  Unix socket (host endpoint)
aqueduct-gpu-host (tier-1 SW or tier-3 GPU)
```

Neither end depends on this crate — it depends on both. Drop it in
when you need them to talk.

## Translators

| Function | fresco type | aqueduct-gpu pipeline |
|---|---|---|
| `translate_rect` | `RectParams` | `BUILTIN_PIPELINE_RECT` |
| `translate_texture` | `TextureParams` | `BUILTIN_PIPELINE_TEXTURED_RECT` |
| `translate_path` | `PathParams` (rotated quad) | `BUILTIN_PIPELINE_PATH` |
| `translate_glyph_run` | `GlyphRunParams` | `BUILTIN_PIPELINE_GLYPH_RUN` |
| `begin_renderpass`, `end_renderpass` | — | renderpass bookends |

Each translator emits three FrameOp records: `BindPipeline` +
`PushConstants` + `Draw`. Callers compose them into a single
frame via the FrameBuilder.

## Demo

```sh
cargo run --example demo -p fresco-aqueduct-bridge
```

Spins up an in-process `SoftwareBackend`, connects a `GpuClient`,
builds a fresco-shaped scene (rect + path + glyph_run with real
text shaped by rustybuzz/swash), submits one frame, reads back
pixels, writes `aqueduct-gpu-demo.png`.

End-to-end smoke test of the whole Phase 1.3c + 1.4 stack — no
Vulkan, no kmod, no VM required.

## Coordinate convention

fresco-protocol coordinates are screen-pixel space with top-left
origin. Atrium-native tier-1 rasterisation matches. No flip.

## Wire-format crosswalk

fresco and aqueduct-gpu both carry types named `GlyphRunParams`.
They're deliberately distinct: fresco's is part of the **consumer**
protocol (scene server ↔ apps over fresco-protocol); aqueduct-gpu's
is the **GPU** protocol (frescod ↔ GPU host endpoint). The bridge
re-shapes one into the other. Future protocol drift is independent
on each side.

## Testing

```sh
cargo test    # 3 unit tests (encoding) + 4 socket-backed e2e tests
```
