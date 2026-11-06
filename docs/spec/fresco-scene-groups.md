# Fresco scene groups: one scene vocabulary for Pergola and the Navigator

**Status:** design, 2026-11-06. Not built.
**Owner:** Pergola thread (UI). Wire, frescod and kernel parts need the Fresco owners' review;
the Navigator mapping needs the Navigator thread's.
**Asked by the user, 2026-11-06:** after the Pergola review, the user agreed to "one shared scene
format with groups first, then ScrollView on top of it", added without breaking today's apps.

Related: [pergola.md](pergola.md) §10 (ScrollView, DocumentView), [atrium-document-profile-v1.md](atrium-document-profile-v1.md)
§3.14 (documents and widgets share the scene graph), [atrium-navigator-backend.md](atrium-navigator-backend.md)
§4.8 (bands, scroll harness), [limen.md](limen.md) §2.1 (doc-viewer slot).

---

## 1. Why

What a window's scene is today, as frescod holds and draws it (`fresco-scene-server`
`WindowSceneState`, `frescod/src/pack.rs`, `fresco-compute`):

- **Flat.** One map per kind (rect, segment path, texture, glyph run, fill), keyed by node id.
  Every leaf is in window pixels. Only a fill carries a transform and a clip.
- **Drawn by kind, then by id.** All rects, then all paths, then fills, then textures, then
  glyphs (`fresco-compute` lib.rs header). So a rect can never cover text: a Pergola popover or
  menu drawn over a list shows the list's text through it. That is a bug today, not only a
  limit.
- **No clip** for anything but fills, so text cannot be clipped to a scroll viewport.

What sits on top of it:

- **Pergola** lays out with taffy and sends every node at its absolute window position. A scroll
  step would re-send every visible node.
- **The Navigator scene** (`navigator-render` `Scene`) has clip rectangles, a transform tree,
  nested opacity groups, scroll regions, gradients, shadows, images and fills. Its scroll harness
  draws clips, groups and transforms as if absent (`scroll_bench.rs` header).

So the document profile's promise (a document's controls *are* Pergola widgets, §3.14) has no
common format to be true in. This note adds one: a **group** node, and a way to put any leaf
inside a group.

## 2. The wire

Two new atrium-core scene ops, sent with `SCENE_NODE_SET` like the others (closed registry,
`fresco_protocol::scene_ops`):

### 2.1 `ATRIUM_CORE_GROUP` (0x1006)

```rust
pub struct GroupParams {
    /// The group this one sits in; `None` = the window's root.
    pub parent: Option<u32>,
    /// Paint order among its siblings: lower first; ties by node id.
    pub order: u32,
    /// Local space to the parent's space: x' = a x + c y + e, y' = b x + d y + f.
    /// v1: translate and positive uniform scale only (b = c = 0, a = d > 0).
    pub transform: [f32; 6],
    /// In the PARENT's space (applied after `transform`), so changing a scroll
    /// offset moves the content and leaves the viewport where it is.
    /// Intersected with every ancestor's clip. `None` = no clip of its own.
    pub clip: Option<[f32; 4]>,
    /// Multiplies every descendant's alpha (v1; see §2.4).
    pub opacity: f32,
    /// group_flags: SCROLL (bit 0), ISOLATE (bit 1, reserved).
    pub flags: u32,
}
```

### 2.2 `ATRIUM_CORE_PLACE` (0x1007)

```rust
/// Sent with the node id of an existing leaf or group: moves it.
pub struct PlaceParams { pub parent: Option<u32>, pub order: u32 }
```

A leaf's own params (`RectParams`, `GlyphRunParams`, …) are unchanged; once placed, they are read
in the parent group's local space. `PLACE` is its own op so the five leaf param structs and every
client that sends them stay as they are. A client that places nodes sends `SET` then `PLACE` in
the same frame; frescod applies a frame atomically, so nothing is drawn half placed.

### 2.3 Rules

- **Compatibility.** A window that never sends `GROUP` or `PLACE` draws exactly as today, pixel
  for pixel: its unplaced leaves form a *legacy layer* at the root, in today's kind-then-id order.
  The legacy layer draws before every placed node. Every existing app keeps working unchanged.
- **Paint order** of placed nodes is a depth-first walk: a group's children in `(order, node id)`
  order, each child's subtree before the next sibling. Kinds interleave freely. Sparse `order`
  values let a client insert between siblings without renumbering.
- **Clearing a group clears its subtree** (one `SCENE_NODE_CLEAR`). This is how a band is evicted
  (§6) and how a closed popover leaves.
- **Parent checks.** The parent must be a group in the same window, written by the same client
  (the existing `node_writers` check). A node cannot be placed under itself or a descendant.
  A leaf whose parent is cleared goes with it; no node is ever left dangling.
- **Limits** (per window, refused past them): depth ≤ 32, groups ≤ 4,096. Numbers to settle with
  the Fresco owners; the point is a stated bound per window, like the fill budget.
- **Validation** as for fills: finite values, coordinates within ±`FILL_MAX_COORD`, `0 ≤ opacity ≤ 1`,
  a v1 transform that is not translate + positive uniform scale is refused.

### 2.4 What v1 leaves out, stated

- **Rotation and skew of groups.** Rects, glyphs and textures are axis-aligned ops in the kernels,
  so a rotated group cannot be drawn without kernel work. A document that rotates content sends it
  as fills (which take a full affine already). Lifting this is a later kernel change.
- **Exact group opacity.** v1 multiplies the group's opacity into each descendant's alpha. That
  equals true group compositing only where descendants don't overlap; the Navigator's own notes say
  nested opacity groups are not the same as multiplied alphas. `ISOLATE` is reserved for the exact
  version (draw the group to its own buffer, then blend), which needs renderer work.
- **The embedded surface** (a Limen slot). A later leaf kind that names another client's surface
  by a frescod-issued handle, sized and clipped like any leaf in a group. It builds on this note.

## 3. Drawing it (pack and kernels)

Packing (`frescod/src/pack.rs`, run by the window's jailed renderer in split mode) walks the tree
once per frame:

1. Compose each node's transform with its ancestors' and apply it to the leaf's geometry. Uniform
   scale and translation map a rounded rect to a rounded rect, a glyph run to a glyph run (scaled
   glyphs need the atlas at that size: glyph runs under a scaled group are rasterised at the
   composed size, not stretched).
2. Intersect clips down the tree. A group whose clip misses the window is skipped whole, with all
   its descendants: off-screen bands cost nothing.
3. Emit kernel ops in paint order. The kernels already draw ops in list order (bin sets bit = op
   index, raster walks bits in order), so interleaving kinds needs no kernel change.
4. Clip every op to its clip rect, rounded out to whole device pixels. **One kernel change**: the
   raster kernel tests the pixel against the op's whole-pixel box (`p3`, today read only by the bin
   kernel) before shading. Fills keep their own clip, intersected with the group's. This changes
   `raster.comp`, so it goes through the Tier-2, GIR and RDNA4 oracle checks like any kernel change;
   that is the Fresco owners' call.

Damage needs nothing new: tile signatures already make unchanged tiles free, and a changed group
transform changes the signatures of exactly the tiles its content covers.

## 4. Scrolling: the SCROLL flag

A group with `SCROLL` and a clip is a scroll viewport. Its transform's translation is the scroll
offset. When between two frames the only change inside its clip is that translation, by a whole
number of device pixels smaller than the clip, the renderer:

1. copies its previous pixels inside the clip by the delta,
2. rasters the newly exposed strip,
3. rasters any tile inside the clip that something drawn *after* the group touches (a floating
   button over a list), and any tile damaged for another reason.

Conditions, any one failing means the clip is drawn in full that frame (never wrong, only slower):
the delta is a whole device pixel; it is smaller than the clip in that axis (pergola.md §10.1 as
corrected in e2054f1: shift-and-strip is never worse than a full draw, so the only full draws are a
scroll of the whole viewport or more, a jump, and the first frame); the group's first child is an
opaque rect covering its clip, so the copied pixels don't carry a background that did not move.
Pergola's ScrollView always draws that background.

This is the Navigator scroll harness's shift-then-strip (8e79bd4: 0.1% missed frames at 120 Hz on
the biggest documents, composed frames equal to a full redraw), moved from the harness into the
renderer, where every app gets it.

Not in v1: the fast-fling blur (a kernel pass, Fresco's), and scrolling while the app is busy
(declarative animation of a group's transform, pergola.md §4.2). v1 scrolling is client-driven:
Pergola sends one transform change per frame, which is a few dozen bytes.

## 5. Pergola on groups

- A `Stack` becomes a group whose transform is its layout origin; children are laid out relative
  to it. A filled stack is a group with a rect as its first child.
- `ScrollView` is a `SCROLL` group with a clip and an opaque background, holding one content group.
  A scroll is one `GROUP` update on the content group.
- The window scale (`EV_WINDOW_DPI_CHANGED`) is the root group's uniform scale; app code stays in
  logical pixels.
- Pergola's own hit testing walks the same transforms and clips, so a click lands where the pixel
  was drawn.
- Node ids become keyed (stable across renders) in the same change, so a scroll or an inserted row
  re-sends only what changed.

## 6. The Navigator on groups

| Navigator scene | Groups |
|---|---|
| transform tree (`xforms`, `xform_parents`) | a group per transform; rotation or skew → its content sent as fills (§2.4) |
| clips | a group with a clip |
| opacity groups | group opacity (multiplied in v1; `ISOLATE` later) |
| scroll regions (wide tables) | `SCROLL` groups |
| bands (backend §4.8, pergola.md §10.3) | one group per band under the document's content group; evicted with one clear |

So DocumentView in a Pergola window and the Navigator's own surface send the same nodes, and a
native control inside a document is a Pergola widget subtree under the document's group.

## 7. Work, in order

1. `fresco-protocol` + `fresco-scene-server`: the two ops, the rules in §2.3, the tests below.
   frescod packs the tree (§3 steps 1–3). Interleaved order already fixes the popover bug.
2. Kernel clip test (§3 step 4), through the oracle checks. Until it lands, clips apply to fills
   only and other ops are culled by their whole box.
3. `fresco-client`: `scene_group`, `scene_place`. Pergola moves Stack and the root onto groups.
4. `SCROLL` shift in the pixel-mode renderer; Pergola `ScrollView`; judged with the scroll harness
   against the §10.1 gates.
5. The Navigator surface sends groups (Navigator thread).

## 8. Tests

- Legacy: every existing scene test and screenshot unchanged; a window with no groups packs to the
  same ops byte for byte.
- Equivalence: a grouped scene and the same scene flattened by hand draw identical pixels in
  `fresco_compute::reference`.
- Order: a rect placed after a glyph run covers it.
- Clip: text clipped to a viewport, against the reference.
- Scroll: shifted frames equal full redraws for a scripted scroll (the harness's check), including
  a floating node in front and a transparent-background group (which must fall back to full).
- Refusals: cycles, a foreign parent, depth and count limits, a rotated v1 transform, NaN.
- Fuzz: random trees through `fresco_compute::fuzz`.

## 9. Questions for the Fresco owners

- The raster kernel's box test (§3 step 4): acceptable, and is `p3` the right place?
- Limits per window (depth, groups), and whether interleaving kinds costs Tier-2 anything measurable.
- The scroll shift for the gpu renderer (F4e), which keeps no previous pixels today: full draws
  there until it does?
