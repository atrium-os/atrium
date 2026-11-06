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
- **Limits** (per window, refused past them): depth ≤ 32, groups ≤ 4,096 (agreed with the Fresco
  owners 2026-11-06). Transforms are composed in f64 and the tree is walked iteratively.
- **Op budget.** A window frame holds at most `MAX_OPS` = 65,536 kernel ops after culling, and every
  glyph is an op. Today pack does not cap, so a frame past it fails validation and the window does
  not draw at all (a bug now). Pack culls by clip first, then drops ops past the cap and reports
  them, through the dropped-fill event generalised to any node (Fresco owners).
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
   index, raster walks bits in order, branching on kind per op), so interleaving kinds needs no
   kernel change and costs Tier-2 nothing. `fresco_compute::Frame::pack(&Scene)` takes per-kind
   vectors; an ordered entry point is added beside it, and `Frame::pack` keeps producing identical
   bytes. The fill-index-to-node-id mapping of `pack_report` and the `FillCache` path keep working,
   since dropped-fill reports depend on them.
4. Clip every op to its clip rect, rounded out to whole device pixels. **One kernel change**: the
   raster kernel tests the pixel against the op's whole-pixel box (`p3`) before shading. Pack sets
   `p3` = the op's box ∩ its clip rounded out; fills keep their own clip in `p0` as today. Confirmed
   by the Fresco owners 2026-11-06: bin already culls by `p3`, damage signatures hash it, and
   `reference::alpha_at` tests it; over 300 fuzz scenes no op paints outside its own `p3`, so the
   test changes no legacy pixel. It goes through the Tier-2, GIR and RDNA4 oracle checks.

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
opaque rect covering its clip (exactly the clip, alpha 1, radius 0), so the copied pixels don't
carry a background that did not move. Pergola's ScrollView always draws that background.

**Tile signatures move with the pixels** (Fresco review, 2026-11-06). The pixel-mode renderer tracks
damage by 16×16 tile signatures, so after the copy, tiles wholly inside the clip whose source was
valid take the new frame's signatures; tiles straddling the clip edge, the exposed strip, and tiles
touched by ops after the group are rastered. To tell "only the translation changed" without
re-deriving it, **pack's output carries, per `SCROLL` group, a content hash of its subtree in local
space (its own translation excluded) and the box of everything drawn after it**.

**GPU renderer** (F4e): each window's VRAM surface persists and only dirty tiles are rastered, but
there is no in-VRAM copy that handles overlap yet, so v1 draws the clip in full there (damage
already confines that to the clip's tiles); a shift kernel comes later.

**Fill flattening.** Composing a scroll translation into fill transforms would change the
`FillCache` key every frame and re-flatten every fill in a band. The cache keeps flattened lines
in group-local space and only re-tiles (Fresco owners).

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

Revised after the Navigator thread's review (2026-11-06,
/mnt/project-files/navigator/scene-groups-review-navigator.md). The Navigator scene is **not a
tree**: clip, transform and opacity group are independent flat attributes per leaf, clips and
transforms are already absolute, and paint order is re-sorted for stacking contexts. So:

| Navigator scene | Groups |
|---|---|
| leaves | **one group per run of consecutive leaves (in paint order) with the same clip, transform and opacity**, under the band group; depth stays about 3 |
| bands (backend §4.8, pergola.md §10.3) | one group per band, cut **only at checkpoint boundaries**; evicted with one clear; a band stays resident while its painted bounds meet the window |
| leaves crossing a band edge (positioned, z-index, shadows) | an **overlay group** after the bands, so paint order holds |
| transforms with rotation (the profile admits `rotate()`) | shapes → fills; a rotated subtree with text or images → one worker-drawn bitmap (texture) |
| opacity groups | multiplied when their leaves don't overlap; the worker flags overlapping ones (a faded card: background plus text) and sends them as a bitmap until `ISOLATE` exists |
| scroll regions (wide tables) | `SCROLL` groups; the worker adds the opaque backdrop rect when it knows it; a sticky column is a sibling group after the `SCROLL` group |
| shadows | no Fresco op; drawn by the worker into a texture leaf at the shadow's place in paint order |
| an SVG's bitmap fallback (dropped fills) | takes the dropped paths' place in the same group and order |

The Navigator thread measures, on the corpus, groups per window against the 4,096 limit, and how
often overlapping opacity groups, rotations and overlay leaves occur.

So DocumentView in a Pergola window and the Navigator's own surface send the same nodes, and a
native control inside a document is a Pergola widget subtree under the document's group.

## 7. Work, in order

Steps 2 and the renderer half of 4 are the Fresco owners' (kernels, Tier-2); the rest is the
Pergola thread's, except 5 and 6.

1. `fresco-protocol` + `fresco-scene-server`: the two ops, the rules in §2.3, the tests below.
   frescod packs the tree (§3 steps 1–3). Interleaved order already fixes the popover bug.
2. Kernel clip test (§3 step 4), through the oracle checks. Until it lands, clips apply to fills
   only and other ops are culled by their whole box.
3. `fresco-client`: `scene_group`, `scene_place`. Pergola moves Stack and the root onto groups.
4. `SCROLL` shift in the pixel-mode renderer; Pergola `ScrollView`; judged with the scroll harness
   against the §10.1 gates.
5. `ISOLATE` (exact group opacity: draw the group to its own buffer, then blend), moved up at the
   Navigator thread's request: a faded card is the commonest case and multiplied alpha gets it
   wrong. Renderer work, Fresco owners; bitmaps stand in until it lands.
6. The Navigator surface sends groups (Navigator thread).

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

## 9. Review

Fresco owners (2026-11-06): approved; `p3` clip test, limits, interleaving and the GPU-renderer
v1 answered in §2.3, §3 and §4. Navigator thread (2026-11-06): approved with the §6 changes.
