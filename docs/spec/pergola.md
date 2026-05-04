# Pergola — Atrium UI toolkit

> *Pergola* (Latin: ornamental garden structure with cross-beams).
> The framework apps grow widgets on. Sits above Fresco; emits scenegraph
> messages via `fresco-socket-rs` underneath. Equivalent in role to
> Qt / GTK / AppKit in current desktop stacks.

**Status**: design phase. Implementation begins D2/D3 (adapt existing
Rust toolkit) and D4+ (native crate).

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
