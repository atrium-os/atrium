# Fresco external surfaces

**Status:** design, 2026-05-08.
**Owner:** Pergola toolkit + fresco-server compositor.

The mechanism for apps that render their own GPU content (games,
3D viewports, video players, scientific viz) and need fresco-server
to composite that content into a window alongside or under
toolkit-emitted scene-graph UI.

The other side of the same coin from the scene-graph wire ([`spec/
fresco-rendering-stack.md`](fresco-rendering-stack.md)): scene-graph
covers "app describes UI, server paints"; this spec covers "app
paints into a buffer the server samples." Both can coexist within
a single window — productivity apps with a 3D viewport are the
canonical case, full-screen games are the degenerate case.

> **Positioning vs. §4 of fresco-rendering-stack.** §4 says "general
> apps don't get raw Vulkan." That stands. The escape hatch *here*
> is a **sanctioned, structured channel**: the app gets to render its
> own pixels into an externally-imported `VkImage`, but it does not
> get to drive the compositor's pipeline, doesn't bypass the WM,
> doesn't see other apps' content. It's the iOS `CAMetalLayer` /
> Wayland `wl_subsurface + linux-dmabuf` shape, expressed in our
> wire format.

## 1. Architecture

```
┌──────────────────────────────────────────────────────┐
│  app process                                         │
│   ┌──────────────────────────────────────────────┐   │
│   │  Pergola view tree                           │   │
│   │   ├ toolbar()              (scene nodes)     │   │
│   │   ├ vulkan_view(renderer)  (ExternalSurface) │   │
│   │   └ statusbar()            (scene nodes)     │   │
│   └──────────┬───────────────────────────────────┘   │
│              │ scene-graph deltas                    │
│              │   +                                   │
│              │ SURFACE_CREATE/RESIZE/FRAME_READY ops │
│              │   +                                   │
│              │ SCM_RIGHTS fd-passing for VkImage,    │
│              │ semaphore                             │
└──────────────┼───────────────────────────────────────┘
               │  aqueduct (CLASS_DISPLAY)
               ▼
┌──────────────────────────────────────────────────────┐
│  fresco-server                                       │
│   ├ allocates surface backing (VkImage + semaphores) │
│   ├ exports fds, hands them to client                │
│   ├ samples surface during composite per frame       │
│   └ may take fast paths (direct scanout, VRR, HDR)   │
└──────────────────────────────────────────────────────┘
```

Each external surface is **one scene-graph node** of kind
`ExternalSurface` referencing a server-allocated `VkImage`. The
node sits in the window's scene tree like any other; z-order with
adjacent toolkit-emitted nodes is automatic.

## 2. Wire format

### 2.1 New scene-graph node kind

```
ExternalSurface {
    surface_id: u32,           // matches a CREATE'd surface
    format: PixelFormat,       // BGRA8_SRGB | RGBA8_SRGB | RGBA16F | ...
    extent: (u32, u32),        // current backing size
    sampling: SamplingMode,    // Linear | Nearest
    alpha: AlphaMode,          // Opaque | Premultiplied | Straight
}
```

Identity (`surface_id`) is allocated by the server in response to
`SURFACE_CREATE` and used by all subsequent ops + node references.

### 2.2 Surface lifecycle ops (CLASS_DISPLAY)

```
Control (client → server):
  SURFACE_CREATE      hints { initial_size, format_pref, pacing,
                              alpha, hdr_intent, vrr_intent }
                      → server replies SURFACE_CREATED with id,
                        chosen format, fd(VkImage), fd(semaphore)
                      via SCM_RIGHTS

  SURFACE_DESTROY     surface_id

  SURFACE_FRAME_READY surface_id, frame_no, signal_value
                      → "I just submitted; semaphore will reach
                         signal_value on completion. Composite
                         when ready."

  SURFACE_RECONFIG_ACK surface_id, accepted: bool, new_size
                       → response to RECONFIG; "I retargeted my
                         renderer to new_size"

Events (server → client):
  SURFACE_CREATED     surface_id, chosen_format, w, h
                      (paired with fd-passing of VkImage + sema)

  SURFACE_FRAME_REQ   surface_id, target_present_time_ns
                      (pull-mode: "render now to hit this deadline")

  SURFACE_RECONFIG    surface_id, new_w, new_h, new_format?
                      ("you're being resized / format-changed;
                        next frame should target this")

  SURFACE_LOST        surface_id, reason
                      (gpu reset, device removed, format unsupported;
                       client should DESTROY + recreate)
```

Frame pacing hint at create-time:

```rust
enum SurfacePacing {
    Pull,       // server emits FRAME_REQ; app responds within budget
    Push,       // app emits FRAME_READY whenever; server picks latest
}
```

### 2.3 Cross-process Vulkan handles

`SURFACE_CREATED` carries two file descriptors via SCM_RIGHTS:

1. **External memory fd** for the `VkImage` backing — imported on
   the client side via `VK_KHR_external_memory_fd` with
   `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT_KHR`. Same handle
   type Wayland's `linux-dmabuf-v1` uses on Linux; FreeBSD Mesa
   supports it.

2. **External timeline-semaphore fd** for completion signaling —
   imported via `VK_KHR_external_semaphore_fd`. Single timeline
   semaphore per surface; client increments on each submit, sends
   the new value with `SURFACE_FRAME_READY`; server waits for that
   value before sampling.

On `SURFACE_RECONFIG` the server may allocate a *new* VkImage; if
so, the next `SURFACE_CREATED`-shaped reply carries the new fds
and the client re-imports. Old image + fd are released after the
client's `RECONFIG_ACK`.

## 3. Frame pacing modes

### 3.1 Pull (default)

Server tracks its own composite tick (typically refresh-rate-driven).
On each tick, it emits `SURFACE_FRAME_REQ` to every visible Pull
surface a few ms before the deadline. Client must call its
`SurfaceRenderer::render_frame`, submit a Vulkan command buffer
that signals the timeline semaphore, and emit `SURFACE_FRAME_READY`
within the budget (typically 4–12 ms depending on tick rate).

If the client misses the deadline, the server uses the previous
frame's contents and re-issues `FRAME_REQ` on the next tick. App
catches up; no error condition.

Use case: **interactive apps** where "what's on screen now is what
the app would draw now" — CAD viewports, 3D editors, gameplay
where input latency matters.

### 3.2 Push

Server doesn't ask. Client emits `SURFACE_FRAME_READY` whenever it
has a fresh frame. Server picks the latest at composite time. If
the client emits multiple `FRAME_READY` between server ticks, only
the newest one is composited — older frames are skipped.

Use case: **media apps** where the app's clock differs from the
display's — video playback (decoder runs at content's frame rate;
display runs at refresh rate), live audio-locked visualizations.
Also useful for apps that genuinely want to render at
*lower-than-refresh* rates to save power (e.g. a CAD app idling
on a static scene).

### 3.3 Switching modes

Pacing is a creation-time hint. Switching modes mid-life requires
DESTROY + CREATE. Most apps pick one at startup and stay there.

## 4. Resize handshake

On window resize (user drag, layout reflow, DPI change):

```
1. server determines new content area size for the surface
2. server: SURFACE_RECONFIG surface_id new_w new_h
3. server holds onto the old VkImage; keeps composing it
   (old size, possibly stretched as a transitional state)
4. client.SurfaceRenderer.on_resize(new_size) runs
5. client allocates next frame at new size
   - if it can use the same VkImage (e.g. shrink), great
   - if not (most cases), client emits SURFACE_RECONFIG_ACK
     requesting a new backing
6. server allocates new VkImage at new size, emits a fresh
   SURFACE_CREATED with new fds
7. client imports new fds, renders next frame at new size,
   emits FRAME_READY
8. server composites new frame, frees old VkImage
```

The pacing during resize matters perceptually: between step 2 and
step 7 the server is still showing the old (stretched) backing.
Anchor primitives in the surrounding scene-graph subtree (per
[`pergola.md`](pergola.md) layout discussion) keep the rest of the
window crisp during this window — only the surface itself is
transiently stretched.

## 5. Composition with surrounding UI

`ExternalSurface` is a scene node like any other. The window's
scene tree may have:

- Toolkit-emitted nodes *above* the surface in z-order (HUD,
  overlay UI, modal dialogs over a fullscreen surface)
- Toolkit-emitted nodes *adjacent* to the surface (toolbars,
  sidebars, statusbars surrounding a viewport)
- Toolkit-emitted nodes *below* the surface (backdrop visible
  through alpha)

Server composites in z-order, sampling the surface's VkImage when
it reaches that node. No special case in the compositor pipeline.

## 6. Format negotiation

At connection setup, server announces a list of `PixelFormat`s it
can accept for ExternalSurfaces, in preference order. Typical:

```
1. BGRA8_UNORM_SRGB     (compositor default; most efficient path)
2. RGBA8_UNORM_SRGB
3. RGBA16_SFLOAT        (if HDR pipeline available)
4. BGRA8_UNORM          (linear; rare)
```

Client passes a preference order in `SURFACE_CREATE`; server picks
the first match. If no overlap, server returns `SURFACE_LOST` with
reason `format_unsupported` and the client must retry with broader
preferences.

## 7. Fast paths

The ExternalSurface wire format doesn't change between the slow
path (compositor samples the surface like any other texture) and
the fast paths described below. **Apps don't request fast paths —
they declare intent at create time, and the server picks the
strategy.** This keeps the API stable and lets the platform evolve
its rendering pipeline transparently.

### 7.1 Direct scanout

When a single `ExternalSurface`:

- fills the entire window
- the window fills the entire display
- the surface is opaque (`AlphaMode::Opaque`)
- the format matches what the kernel modesetting plane expects
- pacing is `Pull` (so the server controls timing)
- no UI is composited above the surface

…the server can hand the surface's `VkImage` directly to the kernel
DRM modesetting plane (atrium-gpu API for "primary plane scanout").
The compositing pass is *skipped*: GPU does no copies, the surface
buffer becomes the displayed framebuffer. Latency drops by one
frame; power drops because there's no read-back-and-blend pass.

This is what Wayland's `wp_presentation_feedback` + DRM direct-scanout
does for fullscreen games on Linux. For Atrium / atrium-gpu, it's a
small atrium-gpu API addition (the modesetting layer needs to accept
an externally-allocated VkImage as its scanout source).

The opposite-direction signal: if any of the conditions stop being
true (a notification overlay appears, the user un-fullscreens, an
async modal pops up), the server transparently falls back to the
compositing path on the next frame. App is unaware.

### 7.2 Variable refresh rate (VRR)

When the display + atrium-gpu driver support VRR (FreeSync /
Adaptive-Sync / G-Sync), the server can choose to clock the
display at the surface's actual frame-arrival rate rather than
forcing a fixed refresh.

Trigger:

- single `ExternalSurface` is the dominant content
- pacing is `Push` (app drives timing) OR `Pull` with `vrr_intent`
  hint at create time
- app's frame rate stays within the display's VRR range
  (typically 48–144 Hz)

The wire format addition is one optional `hints.vrr_intent` field
at `SURFACE_CREATE`:

```rust
enum VrrIntent {
    Off,        // force fixed refresh
    Auto,       // server decides (default)
    Prefer,     // request VRR if available
}
```

The server's modesetting layer (atrium-gpu) handles the VRR
mode-switch transparently when conditions are right.

### 7.3 HDR pass-through

When:

- surface format is `RGBA16_SFLOAT` (or `RGB10_A2_UNORM`)
- display + atrium-gpu support HDR scanout
- create-time hint `hdr_intent` is `Some(transfer_function)`

…the server can present the surface's pixel values directly to
the display in the chosen color space (BT.2020 PQ for HDR10, scRGB
for HDR scRGB), bypassing the SDR tone-map pass.

```rust
enum HdrIntent {
    None,            // SDR; server tone-maps if needed
    Hdr10Pq,         // BT.2020 + PQ EOTF
    HdrScRgb,        // scRGB (linear, extended range)
}
```

If the display doesn't support HDR, server tone-maps to SDR
during composite. App is unaware; rendering code stays the same.

### 7.4 Game mode (low-latency)

A composite hint at create time:

```rust
enum LatencyIntent {
    Default,      // server picks; may double-buffer for smoothness
    LowLatency,   // minimize input-to-photons; single-buffer if possible
}
```

When `LowLatency` is requested:

- server uses single-buffer direct-scanout when eligible (§7.1)
- compositing pipeline reduces internal queue depth
- input events are routed with priority (see input section, future
  spec)
- VRR is preferred (§7.2)

Cost: occasional tearing in transient states (resize, mode change),
slightly more visible compositor cost when conditions break the
fast path. Apps that ask for it accept the tradeoff.

### 7.5 The unifying principle

All four fast paths share one property: **the wire format expresses
intent, not strategy.** Apps say "I want low latency" or "I'm
producing HDR content"; the server decides whether to use direct
scanout, VRR, single-buffer, etc. based on the runtime conditions
(window state, display capabilities, what else is on screen).

Adding a future fast path (e.g. async-reproject-on-resize, or a
per-display compositor offload) doesn't require a wire-format
bump — only new internal pipeline paths in fresco-server. Apps
benefit without code changes.

## 8. Implementation order

Stage with Pergola's roadmap:

1. **`pergola-vulkan`** — raw `VkImage`/semaphore import; the small
   foundation crate. Ships with the first ExternalSurface-using app.
2. **fresco-server: SURFACE_* ops + ExternalSurface scene node** —
   slow path only (composite via blit). Required to ship the raw
   crate end-to-end.
3. **`pergola-wgpu`** — adapter atop `pergola-vulkan` exposing
   `wgpu::TextureView` to apps. Lands when first wgpu-based app
   integrates.
4. **fresco-server: direct scanout fast path** — single-window
   fullscreen-opaque optimization. Requires an atrium-gpu addition
   (primary-plane scanout from external VkImage). Ships when first
   game integrates.
5. **fresco-server: VRR + HDR + game-mode hints** — additive,
   each independently slottable. Wire format is forward-compatible;
   apps using older clients see the slow path automatically.

## 9. Open questions

1. **Multi-display surfaces.** A surface that spans two monitors
   with different refresh rates: which clock drives `FRAME_REQ`?
   Probably the slowest; punt for now.
2. **DMA-buf vs. opaque-fd.** FreeBSD has both shapes via Mesa.
   `opaque_fd` is more portable across Atrium deployments; `dmabuf`
   may unlock Wayland-compat shims. Pick one canonical (probably
   `opaque_fd`) and document.
3. **Capture / screen recording.** Surfaces should be visible to
   approved capture apps (system-side screenshot/recording). The
   compositor can read them anyway during composite; the question
   is API. Defer to a later spec.
4. **Surface sharing across apps.** Two apps cooperating on one
   GPU buffer (e.g. video pipeline producer/consumer). Out of
   scope for V0; possible via aqueduct fd-passing if/when needed.

## References

- [`spec/fresco-rendering-stack.md`](fresco-rendering-stack.md) §4
  on raw GPU access (this is the sanctioned shape)
- [`spec/pergola.md`](pergola.md) for the toolkit-side
  `vulkan_view()` widget
- [`spec/aqueduct.md`](aqueduct.md) §4.2 for fd-passing transport
- Wayland `linux-dmabuf-v1`, `wp_presentation_feedback`,
  `wp_color_management_v1` for prior art on the linux side
- Apple `CAMetalLayer` / `CAEAGLLayer` for the iOS/macOS
  equivalent shape
