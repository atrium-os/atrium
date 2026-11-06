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
- Clipboard, app-to-app messaging and notifications (decided 2026-11-06, asked by the user).
  Each is a system service outside any app jail, on the Aqueduct envelope; Pergola carries only
  a thin client and the widgets that use it:

  | concern | where it runs | wire | Pergola's part |
  |---|---|---|---|
  | Clipboard and drag-and-drop | **Tabula** (`tabulad`), user-scoped service | `CLASS_CLIPBOARD` (2); entries are CAS blobs with format lists (aqueduct.md §6.4) | Ctrl-C/X/V in `TextField` and `DocumentView`, a drag source/target API |
  | App-to-app messaging | no general app bus: typed, brokered roles. **Limen** for embedding, the **share sheet** (a powerbox, insula.md §5.2) for handing one item over, both on `CLASS_BROKER` (4) | broker-minted fds, one operation each | the share button, receiving a share |
  | Notifications | **Praeco** (`praecod`); remote pushes arrive through **Tabellarius** into Praeco | `CLASS_NOTIFY` (3); `praeco-macos` v0 is the wire reference | none in the app: toasts and history are drawn by **Forum** (a Pergola app in the trusted shell), never by the posting app |

  Why outside Pergola: each crosses jails, so each needs a trusted party that applies policy
  (per-jail clipboard scoping, aqueduct.md §6.5; Portcullis manifest `clipboard = true` /
  `notify = true`), and none can be in the app's process. **A paste is a user gesture**
  (insula.md §5.3: "clipboard read: one paste"): frescod owns input and focus, so it vouches for
  the paste keystroke or menu click, and Tabula hands the focused app that one entry. An app
  cannot read the clipboard at will. Likewise a notification's look and placement are the
  shell's, so an app cannot draw a fake system prompt.

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
Agreed: yes. Status: `ScrollView` prototype built 2026-11-06 (below, §10.5); the document band
source built 2026-11-06 (`navigator_render::source`, the §10.4 contract in process; backend
§4.8 step 3 phase 2), and its messages out of process through the Navigator broker
(`Request::Want` / `Event::Bands`, phase 4; `Viewport` drives a jailed worker in
navigator-backend's tests); `DocumentView` and the Limen doc-viewer channel not yet.*

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
The scene format it needs (a group node with transform, clip and a `SCROLL` flag, so a scroll is
one transform change and the renderer shifts and draws the strip) is designed in
[fresco-scene-groups.md](fresco-scene-groups.md) (2026-11-06, agreed with the user).

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

**As built (2026-11-06).** Crate `pergola-document` (Pergola itself stays free of the layout
crates). `Doc` holds a `navigator_render::source::ContentSource` (`Doc::html` builds the
`DocumentSource`), Pergola's `scroll::Viewport` and the resident groups; `DocumentView` is a
`ScrollView` whose content is one free stack as tall as the document (`FlexStyle::free`: layout
never looks inside), holding a free stack per resident band keyed by band id, then the overlay.
The source's sink converts each band's scene to Pergola nodes once (`convert`): rects, rounded
and ringed rects as fills, runs as `Node::Glyphs` (rastered by the app into its atlas, not shaped
by frescod), SVG paths and linear gradients as `Node::Fill`, images through the app's resolver;
every band is clipped to its range (a cut box's height can be unknown). Not drawn yet, counted in
`Doc::skipped`: box shadows (no Fresco op), image tiling and fit, and non-translation transforms
on anything but a fill; opacity groups multiply into their nodes. While the height is an
estimate each render lays out two more bands and asks for another. Tested
(`pergola-document/tests/document.rs`): a 67,000 px document settles in 54 renders, keeps about
800 nodes resident wherever it is scrolled, a band keeps its node ids while resident, and a
scroll frame costs 0.5 ms (render plus diff, release build) before frescod. Not run on frescod
yet.

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

### §10.4 The band interface, zoom, and accessibility (settled 2026-11-06)

*Asked by the Navigator thread, 2026-11-06, so the content side (navigator-render bands,
5a86d41e) and `ScrollView` are not built twice. Built on scene groups
([fresco-scene-groups.md](fresco-scene-groups.md)).*

**Who owns what.** `ScrollView` owns the viewport: scroll position, fling physics, zoom
gestures, prefetch, and anchoring. The content source (the Navigator worker, a
`DocumentView`, a long list) owns content: where bands are cut (at checkpoints), what each
band's scene is, and how much it keeps resident. The source never scrolls; `ScrollView`
never lays out.

**Coordinates.** Document space, vertical, in 1/64 px at zoom 1 (the Navigator's `U`).
`ScrollView` maps document space to its content group with one transform: translation for
scroll, uniform scale for zoom.

**The contract** (host-agnostic; in-process it is a Rust trait, out of process the same
messages over the doc-viewer role's channel, limen.md §2.1):

| direction | message | meaning |
|---|---|---|
| view → source | `want { range, zoom, prefetch }` | keep `range` (document space) ready at `zoom`; `prefetch` is a hint, about one viewport above and below |
| source → view | `extent { height, settled }` | estimated total height; `settled` once it is exact. Sent whenever it changes |
| source → view | `band_ready { band, range }` | band `band` covers `range` and its scene is in the window, as one group the source wrote |
| source → view | `band_updated { band, range }` | its content or extent changed (images arriving, a restyle) |
| source → view | `band_evicted { band }` | gone; anything of it still visible shows placeholders (§10.1) |

- **Scenes go straight to Fresco, not through `ScrollView`.** Each band is one scene group
  (backend cuts bands only at checkpoints; leaves crossing a band edge go in the source's
  overlay group, fresco-scene-groups.md §6). The source places band groups under a content
  group the view names; in-process, `DocumentView` writes through Pergola's surface, out of
  process the worker writes into its own embedded surface. `ScrollView` moves the content
  group; it never copies scene data.
- **Anchoring.** When an estimate above the viewport changes (`band_updated` or `extent`),
  `ScrollView` keeps the first visible band and its offset fixed on screen and adjusts the
  scroll position, so reading never jumps. Uses `navigator_render::anchor` semantics.
- **Residency** is the source's: it may evict anything outside the last `want` range plus
  prefetch. `ScrollView` treats an evicted band in view as not ready (placeholders), never
  as an error.

**Zoom and pinch.** `ScrollView` owns the gesture (trackpad pinch, Ctrl+wheel, Ctrl +/−/0).
During it, the content group's scale changes (approximate: existing bands stretched,
"approximate now, sharpen on settle"). When the gesture ends and input has been quiet for
100 ms, `ScrollView` sends `want` with the new zoom and the anchor to keep; the source
re-lays out at that zoom and replaces bands; `ScrollView` swaps them in keeping the anchor.
The Navigator broker's per-session `set_zoom` becomes what that settle calls. Text size is
a content setting, not a zoom, and goes to the source directly.

**Accessibility with bands** (§5). The AX tree cannot depend on residency. The source sends
a **semantic outline of the whole document** once layout knows it (headings, landmarks,
tables, links and form controls, each with its document-space range), independent of
which bands are resident. Resident bands add their full AX nodes under the outline. A
screen reader moving to a node in a non-resident range makes `ScrollView` request that
range, exactly as a scroll would. Find-in-page searches the source's text, not the scene.

**Two engines, on purpose.** Pergola's own layout (taffy flexbox, widgets, a few hundred
nodes) and document layout (profile, bands, tables, millions of nodes) stay separate. They
meet only through scene groups: a `DocumentView` is a widget whose content group the
document engine fills; a native control in a document is a Pergola widget subtree in a
document group (profile §3.14).

**Animation.** Client-driven animation (§4.1) is enough for scrolling now: with groups a
scroll frame is one transform change (a few dozen bytes), and the renderer shifts what is
drawn. Declarative animation (§4.2) is still needed so a busy app keeps scrolling, and for
the fling blur; it is not blocking ScrollView.

**The Navigator's own UI is a Pergola app** (decided by the user, 2026-11-06): toolbar, tabs,
find bar and zoom controls run on Pergola in the Navigator's trusted UI process, and each
page is a jailed document surface embedded in it (Limen doc-viewer). frescod's origin strip
stays frescod's, drawn outside any app.

### §10.5 ScrollView prototype, as built (2026-11-06)

- **Code.** `pergola/src/scroll.rs` holds the pure parts on a caller's clock: `Physics` (drag,
  fling with 0.998/ms decay, eased jumps for keys and wheel notches, bounds), `Zoom` (pinch,
  Ctrl-wheel, Ctrl + − 0, settles after 100 ms quiet) and `Viewport` (the band interface's view
  side: when to send `want`, placeholder ranges, anchoring). `widgets/scroll_view.rs` is the
  widget; `Node::Scroll` is its node.
- **Wire shape.** A `SCROLL` group at the view's origin, clipped to its box; an opaque backdrop
  rect at order 0 (theme window background unless the app picks another); the content group at
  order 1, whose translation is the offset rounded to whole device pixels. This is exactly the
  shape frescod shifts (fresco-scene-groups.md §4, 442cd94). frescod's pack test `pergolas_scroll_view_is_a_viewport`
  (b19349d) checks this exact shape is recognised and a 7 px scroll is shifted. A scroll frame sends one `GROUP`
  message: the surface skips the viewport and backdrop when they are unchanged, and the content's
  nodes keep their content-group coordinates (tested: each fling frame's delta list is the
  scroll node alone).
- **Input.** `Event::Scroll` (from fresco `PointerScroll`, at the last pointer position). Deltas of
  30 px or more are wheel notches and ease; smaller ones follow the finger and fling on release.
  The wire has no gesture phases, so release is inferred after 30 ms without a delta; a late
  delta coasts at the drag's speed and the fling takes over from the coast, so release neither
  pauses nor jumps (tested at 1 ms steps). Arrows, Page Up/Down, Space, Home/End go to the
  focused scroll view, else the one under the pointer. A press catches a fling.
- **Frames.** Client-driven (§4.1): `on_frame` handlers run every frame while something moves;
  `Window` waits at most one 120 Hz frame while the app animates and blocks otherwise.
- **Zoom.** Content is laid out at zoom 1 and sent at the settled zoom (payloads and text sizes
  multiplied); during a gesture the content group stretches it, and text keeps its size until the
  settle re-sends it. Ctrl-wheel keeps the point under the pointer still.
- **Measured** with the Navigator scroll harness (`scroll_bench --pattern pergola`, which compiles
  `pergola/src/scroll.rs` in and drives a 4,000 px/s swipe every 1.5 s through it), strips and
  bands on, a generated 305,000 px document (2,400 paragraphs), 8 s, after first display:

  | | 60 Hz: p99.9 | missed | stalls > 100 ms | 120 Hz: p99.9 | missed |
  |---|---|---|---|---|---|
  | scripted flings | 12.7 ms | 0.2% | 0 | 13.2 ms | 5.3% |
  | Pergola physics | 11.4 ms | 0.0% | 0 | 11.4 ms | 3.9% |

  The 60 Hz gates pass. At 120 Hz the misses are the harness compositor's frame cost (p99 about
  10 ms against an 8.3 ms period), the same with either motion; the gate applies once the
  compositor redraws by damage, which frescod now does (442cd94) but the harness does not model.
  Runs on a shared 4-core cloud machine vary by a point or two of missed frames.
- **Not yet.** Horizontal scrolling, a scrollbar, blur on the wire (no renderer op;
  `Physics::motion_blur_px` gives the amount), placeholders drawn (`Viewport::placeholders` gives
  the ranges, used once a band source exists), `DocumentView`, gesture phases on the wire (would
  replace the inferred release), and a live run of a Pergola app on frescod.


## §11. Vector drawing: `Canvas` (asked by the user, 2026-11-06)

For apps that draw their own 2D content (vector drawing, flowcharts, diagrams). Built
2026-11-06 in `pergola/src/shape.rs` and `widgets/canvas.rs`.

- **Retained, keyed shapes.** The app holds `Vec<CanvasItem>` (key, `Shape`). A `Shape` is a
  `PathData` (Fresco's verbs: move, line, quad, cubic, close; builders for rects, rounded rects,
  ellipses), an optional fill (`Paint`: solid, linear, radial), an optional `Stroke` (width, cap,
  join, miter limit, dashes), any affine (rotation and skew included) and an opacity. Each draws
  as one or two `Node::Fill`s; a stroke's outline, dashes applied, is made once by tiny-skia's
  stroker, the one the Navigator's SVG uses (Fresco never strokes). Nodes are keyed by item, so
  replacing one item resends that item alone (tested).
- **Pan and zoom** are `ScrollView`'s: the drawing is a free stack in its content, so scrolling
  and zoom move one group and resend no shape (tested). Horizontal scroll waits for `Physics`
  to gain an x axis.
- **Input.** `on_pointer(CanvasEvent { phase: Start | Move | End, at, hit })`: canvas
  coordinates with scroll and zoom undone, and the topmost shape whose ink is under the
  pointer, tested on the fill or the stroke's outline, not on bounds (`Shape::hit`). Selection,
  handles and tools are the app's.
- **Not yet:** text on the canvas (use `Node::Glyphs` from a shaper; a shaping helper comes with
  Pergola's own client-side text), images on the canvas (`Node::Image` exists, `Item` doesn't
  carry one yet), a pixel buffer for paint programs (a texture slot updated by region; Fresco
  supports it, no widget until an app needs it), and box-shadow-like effects (no Fresco op).

## §12. Layout of controls, grids and groups (asked by the user, 2026-11-06)

Controls used to be placed only by hand (`.at(x, y)`), so they could not
sit in a stack and nothing lined their labels up. As built:

- **`Ctx::place(widget)`** puts any control in the current stack or grid.
  A control that implements `view::Widget` says the size it wants
  (`natural_size`, 0 on an axis meaning "stretch") and where its label's
  baseline is. Layout gives its slot a rect (grow, `align_self`, min/max
  via `place_with`), and the control then draws in that rect, so a grown
  or stretched field draws at its grown size and its handlers and menus
  use the right coordinates. Placed controls draw after the layout pass
  (`Ctx::flush_overlays` runs layout → placements → overlays, round by
  round), and what they draw is laid out in turn.
  Button, TextField, Checkbox, Toggle, RadioGroup, Segmented, Tabs,
  Dropdown, Slider and CredentialField implement it; `.at()` still works.
- **Alignment**: stacks keep justify (start, centre, end, space-between),
  align (stretch, start, centre, end), padding, gap, grow/shrink/basis,
  min/max and absolute insets, and gain **`Align::Baseline`**: a row's
  children line up on their label baselines (the widget's, a text leaf's
  own, or the bottom). Labels themselves are optically centred (cap height
  on the box's middle, `text::wire_top`), so controls of one height agree
  even when centred.
- **Grids**: `Ctx::grid(stack, Grid::new([Track::Auto, Track::Fr(1.0)]))`
  lays a stack out as a grid (taffy's), children filling cells row by
  row; tracks are px, auto or fr; `FlexStyle::span` spans columns,
  `justify_self` aligns in a cell; `Align::Baseline` aligns each row. A
  form is a two-column grid: labels auto, fields fr.
- **Grouping**: `GroupBox::new(title).show(ctx, |ctx| …)` frames related
  controls on a `bg-elevated` surface with a 1px `border-default` edge,
  radius-md, padding 16, its title `sm` semibold `text-secondary`;
  `Section::new(title).description(…)` heads controls with an `md`
  semibold title and no frame. Both are vertical stacks, content `sm`
  apart; sections `lg` apart. A Stack is also a group on the wire (moves,
  clips and fades as one); RadioGroup, Segmented and Tabs are one Tab stop.
- Tab order is now tree order (reading order), not node id order.

- Rows wrap (`FlexStyle::wrap`): what does not fit carries on on the
  next line, `spacing` apart. Dialog is laid out this way (a centring
  column, a padded body, a right-justified button row of placed buttons).

- Text wraps: `Ctx::paragraph(text, style)` adds text that breaks into
  lines at the width layout gives it (greedy, at spaces and `\n`, a too
  long word on its own line), with body leading (`line-height.body`); it
  is as tall as its lines and pushes what follows down. After layout it
  becomes a free column of one-line runs, which is what the wire draws
  (frescod still shapes each run). Dialog bodies and Section
  descriptions use it.

Not yet: a grid's explicit row/column placement, RTL, hyphenation and
breaking inside long words, ellipsis truncation.
