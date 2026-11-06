# Pergola — Atrium UI toolkit

> *Pergola* (Latin: ornamental garden structure with cross-beams).
> The framework apps grow widgets on. Sits above Fresco; emits scenegraph
> messages via `fresco-socket-rs` underneath. Equivalent in role to
> Qt / GTK / AppKit in current desktop stacks.

**Status (2026-05-09):** implementation phase. Pulled forward from
D5 to land before D2 (vestibulum) and D3 (forum) so those apps come
up on Pergola from day one rather than via direct `fresco-socket-rs`.

**Companion docs:**
- [docs/design/atrium-visual-language.md](../design/atrium-visual-language.md)
  — typography, color, spacing, shape, motion. Read first; widgets
  reference its tokens.

---

## §1. Scope

Pergola is the **only** way most Atrium apps interact with Fresco. App
developers should never need to import `fresco-socket-rs` directly for
window creation, widget composition, layout, text, animation, or
accessibility. Those concerns live in Pergola.

Pergola owns:

- Widget vocabulary and composition
- Layout engine
- Event routing and focus management
- Window lifecycle (sends Fresco WM ops on the app's behalf)
- Text shaping (via rustybuzz / HarfBuzz) — shaping happens here, not in fresco-server
- Animation API (client-driven in D2/D3, declarative-to-server in D4+)
- Accessibility tree maintenance (AX tree mirrors widget tree; sent via CLASS_AX)
- Theming (colors, typography, dark mode, scale factors)

Pergola does NOT own:

- Pixel rasterization (that's the SPIR-V bundle on the GPU, via Fresco)
- Multi-app composition (that's the scene server)
- Window placement / z-order policy (that's the scene server's WM role)
- Storage, network, audio (separate Atrium services)

---

## §2. Architectural shape

**Library, not service.** Pergola is a Rust crate linked into each app
process. No extra IPC hop between app code and toolkit.

**Retained-mode.** The widget tree is owned by Pergola, persists across
frames, and the app mutates it. This maps naturally onto Fresco's
retained-mode wire protocol — widget changes diff into
`SCENE_NODE_SET` / `SCENE_NODE_CLEAR` deltas.

**Diff-on-commit.** App calls `view.commit()` (or equivalent); Pergola
walks the dirty subtree, emits the wire deltas, and sends a single
`SCENE_FRAME_BEGIN..END` bundle.

```
       App code
          │  mutates widget properties
          ▼
   ┌─────────────────────────────┐
   │ Pergola widget tree         │  ← retained, stays across frames
   │  (Rust types, owned by app) │
   └──────────────┬──────────────┘
                  │  diff on commit
                  ▼
   ┌─────────────────────────────┐
   │ fresco-socket-rs            │  ← low-level wire client
   └──────────────┬──────────────┘
                  │  aqueduct UDS
                  ▼
            fresco-server
```

---

## §3. Crate layout (planned)

| Crate | Responsibility |
|---|---|
| `pergola` | Top-level façade: re-exports + app entry point |
| `pergola-widgets` | Widget vocabulary (Button, TextField, ScrollView, …) |
| `pergola-layout` | Layout engine (taffy or custom) |
| `pergola-text` | Shaping (rustybuzz) + glyph-run construction |
| `pergola-events` | Input routing, focus, keyboard navigation |
| `pergola-anim` | Animation API (client-driven D2/D3, declarative D4+) |
| `pergola-ax` | Accessibility tree construction (CLASS_AX wire ops) |
| `pergola-theme` | Theming, color schemes, type scale |

---

## §4. Animation

Two-phase plan:

### §4.1 Client-driven (D2/D3)

Toolkit emits per-frame state via existing `SCENE_NODE_SET`. App must
be running 60+ Hz during the animation. Simple, ships fast, no new
wire surface. Power cost on battery is real; smoothness suffers
during app GC pauses or other host-side work.

### §4.2 Declarative-to-server (D4+)

New `ANIMATION_*` op family in CLASS_DISPLAY:

```
ANIMATION_START   handle, target_node, property, from, to,
                  duration, curve, on_complete_callback
ANIMATION_CANCEL  handle
ANIMATION_FINISHED handle  (async event back to client)
```

fresco-server runs an interpolator on its own tick; the app can be
suspended and animations still play to completion. Closes the iOS
Render Server gap noted in the Fresco deck.

Pergola's animation API surface should be the same in both phases —
the app code never changes, only the implementation flips.

---

## §5. Accessibility

**Architecturally first-class**, not retrofitted. The scene server
already mirrors per-app retained-mode trees; AX is the same shape with
different node payloads.

**CLASS_AX dictionary** (sibling to CLASS_DISPLAY):

```
AX_NODE_SET        node_id, role, label, value, state, parent, rect
AX_NODE_CLEAR      node_id
AX_TREE_FOCUS_CHANGE node_id
```

Pergola maintains both trees:

- **Scene tree** (geometry, colors, glyphs) — emitted as CLASS_DISPLAY
- **AX tree** (semantic role, label, value, state) — emitted as CLASS_AX

Both mirror the widget structure but carry different payloads.

**Single semantic tree, three consumers:**

1. Screen readers / voice control / switch control (assistive tech)
2. UI testing / automation tools (the AX tree IS the test surface)
3. Scripting (drive the UI from outside the app)

This is a real differentiator vs. desktop Linux's fragmented AT-SPI.

---

## §6. Window lifecycle

Pergola owns this. Apps call `Window::new(...)`; Pergola produces the
right wire calls. App code never imports `fresco-socket-rs` directly
for window management.

**Wire ops** (in CLASS_DISPLAY — no new class):

```
Control (client → server):
  WINDOW_CREATE         id, hints (size, decorations, modal, parent)
  WINDOW_DESTROY        id
  WINDOW_SET_TITLE      id, string
  WINDOW_SET_HINTS      id, hint-flags
  WINDOW_REQUEST_CLOSE  id

Events (server → client):
  WINDOW_RESIZED         id, width, height
  WINDOW_FOCUS_CHANGED   id, gained|lost
  WINDOW_CLOSE_REQUESTED id   (user clicked X)
  WINDOW_DPI_CHANGED     id, scale
```

This is what xdg-shell solves for Wayland; Atrium gets to solve it once,
cleanly, without an extension dance because we own the protocol.

---

## §7. Text and glyph runs

Per the Fresco rendering-stack spec §3 / deck:

- **Shaping** (text → glyph IDs + positions) runs in Pergola via
  `pergola-text` (rustybuzz / HarfBuzz). CPU work. Pergola can cache
  shaped runs across frames.
- **Glyph-run construction** produces `glyph_run` scene-graph nodes
  (atrium-core op).
- **Rasterization** runs on the GPU (atrium-core texture op + atlas,
  or eventual atrium-text vector bundle).

Apps work in terms of `Text` widgets; Pergola handles shaping
internally.

---

## §8. What's deferred

- **Third-party toolkit ports** (Qt / GTK on Fresco): defer
  indefinitely. Publish the wire protocol; let the community port if
  demand exists. Not a platform burden.
- **Immediate-mode toolkit support**: possible (toolkit must
  diff frame-to-frame internally), but not D2/D3.
- **Cross-app shared widget cache**: e.g. a "system Button" rendered
  once on the GPU and reused across apps. Future optimization.

### §8.1 Color management roadmap

Color management has three layers, addressed in order:

1. **App layer (linear RGB).** Done. Pergola's `Color` stores linear
   components; blending math (alpha compositing, gradients) is
   correct.
2. **Encoding layer (linear → sRGB).** Done. fresco-vulkan's color
   attachment uses `B8G8R8A8_SRGB`, so the GPU applies the sRGB
   transfer at attachment write. App code never thinks about gamma
   encoding.
3. **Display profile layer (sRGB → monitor).** **Deferred to D5+.**
   Most consumer displays target sRGB out of the box, so #2 produces
   acceptable results today. Real ICC support requires:
   (a) frescod owning the scanout path end-to-end (currently kmod +
       virtio-gpu hardcode `B8G8R8A8_UNORM`);
   (b) an ICC profile registry per attached display
       (`xdg-color-manager`-equivalent, parsed at scanout time);
   (c) a way for windows to declare their content color space
       (sRGB / Display-P3 / BT.2020) so frescod can pick the right
       transformation; default is sRGB.
   Until D5, "everything is sRGB" is the operating assumption.

### §8.2 Near-term Pergola TODO

- **Auto-contrast text color.** Done. `Color::auto_on(bg)` /
  `theme.text_auto_on(bg)` pick black or white based on WCAG
  relative-luminance contrast.
- **Pre-WM full-screen + transparent-shell pattern for login apps.**
  Done for vestibulum. Generalization for other login-time apps is a
  later concern.
- **Framebuffer-size discovery.** Vestibulum hardcodes 1280×720 to
  match the smoke harness. Real frescod should hand framebuffer
  dimensions back to the client (proposed: `WINDOW_CREATE` with
  `width=0 height=0` → fullscreen; response carries actual size).

---

## §9. Roadmap alignment

- **D2 → D4** (frescod hardening + foundation apps): bring-up apps
  continue speaking `fresco-socket-rs` directly; Pergola is *not* on the
  critical path here. Pergola development happens in parallel.
- **D4.5** (declarative animation): `ANIMATION_*` op family lands;
  `pergola-anim` builds against it.
- **D5** (Pergola native + accessibility): the toolkit ships
  feature-complete enough to host a foundation app; `pergola-ax` lands
  the AX tree path; CLASS_AX wire ops added. atrium-edit / atrium-term
  rewritten on Pergola as canary apps.
- **D6+** (Pergola maturation, additional widgets, theming, third-party
  app uptake).

---

## §10. Scrolling and rich documents (agreed with the user, 2026-11-06)

*Asked by the user, 2026-11-06, while the Navigator's big-document work was under way:
scrolling and flings "belong ideally to the UI", and "I'm wondering if this becomes some
kind of a rich text/layout widget for the UI so other apps can get the same features".
Agreed: yes. Status: specified, not built. Pergola has no `ScrollView` yet.*

### §10.1 Scrolling is Pergola's, not the Navigator's

`ScrollView` owns everything a reader feels while scrolling, so that every app scrolls
the same way. The Navigator's document surface is one user of it. The Navigator backend
supplies content (bands, §10.3), never scroll behaviour.

- **Input to motion.** Wheel, trackpad, keyboard (page/arrow/home/end) and touch flings,
  with momentum and the system's scroll settings. One physics model, shared by all apps.
- **Shift, then draw the strip.** On a scroll the compositor moves what is already drawn
  by the scroll delta (a blit or texture offset) and draws only the newly exposed strip.
  At 4,000 px/s and 120 Hz that is about 33 px of an 800 × 600 window, roughly 6% of a
  full frame.
- **Never worse than a full draw.** Measured (atrium-navigator-backend.md §4.8, harness
  8e79bd46; Fresco damage test a5bff603): shift-plus-strip costs the full path's fixed
  per-frame work plus the exposed rows in proportion, plus a sub-millisecond pixel move,
  so it is no worse than a full draw at any strip height. The only switch is: a scroll of
  the whole viewport or more, a jump, or the first frame is drawn in full directly.
- **Motion during fast flings.** Above a speed threshold (start at about 2,500 px/s, tuned
  by eye), a vertical motion blur scaled to velocity. Strips drawn while blurred may be
  cheaper (reduced resolution, or text as greeked lines). As the fling slows below the
  threshold the blur fades out over 2–3 frames and the visible area is redrawn sharp: the
  same "approximate now, sharpen on settle" mechanism as pinch zoom.
- **Placeholders, not stalls.** If content for a region is not ready when the scroll
  reaches it, show greeked placeholder lines in the content's text colour (or a blurred
  low-detail preview), never a blank area or a frozen frame. They resolve to real content
  when it arrives.
- **Reduce motion.** The system's reduce-motion setting turns the blur off; placeholders
  stay (they are not motion).
- **Gates** (the same as the Navigator's, so one harness judges both): at 60 Hz, no stall
  over 100 ms after first display, p99.9 frame time ≤ 2 periods, missed vsyncs ≤ 1%; the
  same at 120 Hz. A placeholder counts as shown content for frame rate but not for lag:
  real content must replace it within 100 ms of the scroll settling.

Where it runs: client-driven first (§4.1), then the shift and blur move server-side with
declarative animation (§4.2), so a busy app still scrolls smoothly. The strip redraw rides
frescod's damage path (incremental drawing redraws only changed tiles), which the Fresco owners decide.

### §10.2 A document view any app can use

The Navigator's document layout (profile normalize, style, layout, SVG, zoom and text
size, tables, big-document bands, replay of recorded interactions) already outputs the
Fresco scene graph that Pergola widgets are made of (atrium-document-profile-v1.md §3.14).
So it is offered to other apps in two forms, chosen by **whose content it is**:

| content | form | why |
|---|---|---|
| untrusted (an HTML mail body, a downloaded help page, a file preview, a feed item) | **Limen `doc-viewer` role** (limen.md §2.1): the app embeds a jailed document surface | the content never runs in the app's process; the app gets layout, zoom, bands and replay without their attack surface |
| the app's own trusted text (help, release notes, a chat log, rich text it generates) | **`DocumentView` widget**, linking the same layout crates in-process | no jail hop, and the text can use the app's theme and widgets directly |

Both draw through `ScrollView` (§10.1), so scrolling feels the same everywhere.
`DocumentView` is not an editor; rich-text editing is a separate widget that may later
share the layout code.

### §10.3 What this requires of the layout code now

- The layout crates stay free of Navigator-only dependencies. Checked 2026-11-06:
  `navigator-dom`, `-style`, `-render`, `-svg`, `-normalize` and `-fonts` depend on
  each other, on Fresco's protocol and compute crates, and on third-party crates only; not
  on `navigator-backend`, `-fetch` or `-prerender`. Keep it that way. (The `navigator-`
  prefix can be renamed when `DocumentView` is built.)
- The band interface (atrium-navigator-backend.md §4.8 step 3) is the contract between a
  content source and `ScrollView`: "give me the scene for this range of the document, at
  this zoom", with an estimated total extent that may settle later. A `DocumentView`, the
  Navigator worker and any long list (a mail index, a log) implement it alike.
